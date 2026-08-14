//! WeCom channel adapter — webhook server for inbound/outbound messages.

use std::collections::HashMap;

use axum::extract::Query;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use types::channel::Channel;
use types::error::{CarrierError, CarrierResult};
use types::plugin::{PluginContent, PluginMessage};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::crypto;
use crate::token;

// ---------------------------------------------------------------------------
// Callback parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct CallbackParams {
    msg_signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    echostr: Option<String>,
}

// ---------------------------------------------------------------------------
// WeCom Channel
// ---------------------------------------------------------------------------

/// A WeCom channel that receives messages via webhook and sends via API.
pub struct WeComChannel {
    bot_id: String,
    webhook_port: u16,
    encoding_aes_key: Option<String>,
    callback_token: Option<String>,
}

impl WeComChannel {
    pub fn new(
        bot_id: String,
        webhook_port: u16,
        encoding_aes_key: Option<String>,
        callback_token: Option<String>,
    ) -> Self {
        Self {
            bot_id,
            webhook_port,
            encoding_aes_key,
            callback_token,
        }
    }
}

impl Channel for WeComChannel {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    fn supports_proactive_push(&self) -> bool {
        // App and Kf modes support proactive push. SmartBot mode does not,
        // but that case is handled by SmartBotChannel (a separate impl).
        true
    }

    fn name(&self) -> &str {
        "WeChat Work"
    }

    fn bot_id(&self) -> &str {
        &self.bot_id
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> CarrierResult<()> {
        let bot_id = self.bot_id.clone();
        let encoding_aes_key = self.encoding_aes_key.clone();
        let callback_token = self.callback_token.clone();
        let port = self.webhook_port;

        // Spawn in its own thread with dedicated runtime
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for WeCom webhook");
            rt.block_on(async move {
                run_webhook_server(
                    bot_id,
                    encoding_aes_key,
                    callback_token,
                    port,
                    sender,
                )
                .await;
            });
        });

        info!(
            bot = %self.bot_id,
            port = self.webhook_port,
            "WeCom channel started"
        );

        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> CarrierResult<()> {
        let bot = crate::token::WECOM_STATE
            .get_session_for_send(bot_id)
            .ok_or_else(|| CarrierError::InvalidInput(bot_id.to_string()))?;

        match &bot.entry.mode {
            token::WecomMode::App { .. } => {
                token::send_app_message(&bot.entry, user_id, text)?;
            }
            token::WecomMode::Kf { .. } => {
                token::send_kf_message(&bot.entry, user_id, text)?;
            }
            token::WecomMode::SmartBot { .. } => {
                return Err(CarrierError::InvalidInput(
                    "SmartBot mode does not support send via channel (use response_url)".to_string(),
                ));
            }
        }

        Ok(())
    }

    fn stop(&mut self) {
        // Webhook server runs until process exit; no graceful shutdown needed.
    }
}

// ---------------------------------------------------------------------------
// Webhook server
// ---------------------------------------------------------------------------

async fn run_webhook_server(
    bot_id: String,
    encoding_aes_key: Option<String>,
    callback_token: Option<String>,
    port: u16,
    tx: mpsc::Sender<PluginMessage>,
) {
    let state = WebhookState {
        bot_id,
        encoding_aes_key,
        callback_token,
        tx,
    };

    // Public URL is https://<host>/wecom/kf (nginx). Also keep /wecom/webhook
    // so the existing rewrite `proxy_pass …/wecom/webhook` still works.
    let app = Router::new()
        .route("/wecom/kf", get(webhook_get))
        .route("/wecom/kf", post(webhook_post))
        .route("/wecom/webhook", get(webhook_get))
        .route("/wecom/webhook", post(webhook_post))
        .with_state(std::sync::Arc::new(state));

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Failed to bind webhook port {}: {e}", port);
            return;
        }
    };

    info!("WeCom webhook server listening on port {}", port);
    if let Err(e) = axum::serve(listener, app).await {
        warn!("Webhook server error: {e}");
    }
}

#[derive(Clone)]
struct WebhookState {
    bot_id: String,
    encoding_aes_key: Option<String>,
    callback_token: Option<String>,
    tx: mpsc::Sender<PluginMessage>,
}

// ---------------------------------------------------------------------------
// GET handler — callback URL verification
// ---------------------------------------------------------------------------

async fn webhook_get(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<WebhookState>>,
    Query(params): Query<CallbackParams>,
) -> axum::response::Response {
    let msg_signature = match params.msg_signature.as_deref() {
        Some(s) => s,
        None => {
            return (axum::http::StatusCode::BAD_REQUEST, "missing msg_signature").into_response()
        }
    };
    let timestamp = match params.timestamp.as_deref() {
        Some(s) => s,
        None => return (axum::http::StatusCode::BAD_REQUEST, "missing timestamp").into_response(),
    };
    let nonce = match params.nonce.as_deref() {
        Some(s) => s,
        None => return (axum::http::StatusCode::BAD_REQUEST, "missing nonce").into_response(),
    };
    let echostr = match params.echostr.as_deref() {
        Some(s) => s,
        None => return (axum::http::StatusCode::BAD_REQUEST, "missing echostr").into_response(),
    };

    // Verify signature if callback_token is configured
    if let Some(ref token) = state.callback_token {
        if !crypto::is_valid_wecom_signature(token, timestamp, nonce, echostr, msg_signature) {
            return (axum::http::StatusCode::FORBIDDEN, "invalid signature").into_response();
        }
    }

    // Decrypt echostr if encoding_aes_key is configured
    let response = if let Some(ref aes_key) = state.encoding_aes_key {
        match crypto::decode_wecom_payload(aes_key, echostr) {
            Ok(decrypted) => decrypted,
            Err(e) => {
                warn!("Failed to decrypt echostr: {e}");
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "decrypt error",
                )
                    .into_response();
            }
        }
    } else {
        echostr.to_string()
    };

    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        response,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST handler — incoming messages
// ---------------------------------------------------------------------------

async fn webhook_post(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<WebhookState>>,
    Query(params): Query<CallbackParams>,
    body: String,
) -> &'static str {
    let fields = if let Some(ref aes_key) = state.encoding_aes_key {
        // Encrypted payload — extract encrypted content for signature verification
        let xml_fields = match crypto::parse_wecom_xml_fields(&body) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to parse XML: {e}");
                return "success";
            }
        };

        let encrypted = match xml_fields.get("Encrypt") {
            Some(e) => e.clone(),
            None => {
                warn!("No Encrypt field in XML");
                return "success";
            }
        };

        // Verify signature if callback_token is configured
        if let Some(ref token) = state.callback_token {
            if let (Some(ts), Some(nonce), Some(sig)) = (
                params.timestamp.as_deref(),
                params.nonce.as_deref(),
                params.msg_signature.as_deref(),
            ) {
                if !crypto::is_valid_wecom_signature(token, ts, nonce, &encrypted, sig) {
                    warn!("WeCom POST webhook: invalid signature");
                    return "success";
                }
            }
        }

        // Decrypt
        match crypto::decode_wecom_payload(aes_key, &encrypted) {
            Ok(decrypted_xml) => match crypto::parse_wecom_xml_fields(&decrypted_xml) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Failed to parse decrypted XML: {e}");
                    return "success";
                }
            },
            Err(e) => {
                warn!("Failed to decrypt payload: {e}");
                return "success";
            }
        }
    } else {
        // Unencrypted payload
        match crypto::parse_wecom_xml_fields(&body) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to parse XML: {e}");
                return "success";
            }
        }
    };

    let msg_type = fields.get("MsgType").map(|s| s.as_str()).unwrap_or("");
    let from_user = fields.get("FromUserName").cloned().unwrap_or_default();
    let msg_id = fields.get("MsgId").cloned().unwrap_or_default();
    let event = fields.get("Event").map(|s| s.as_str()).unwrap_or("");

    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Build bot_id for routing
    let bot_id = state.bot_id.clone();

    // Build content based on message type
    let content = match msg_type {
        "text" => {
            let text = fields.get("Content").cloned().unwrap_or_default();
            PluginContent::Text(text)
        }
        "image" => {
            let pic_url = fields.get("PicUrl").cloned().unwrap_or_default();
            let image_data = if !pic_url.is_empty() {
                match reqwest::Client::new().get(&pic_url).send().await {
                    Ok(resp) => match resp.bytes().await {
                        Ok(b) => Some(b.to_vec()),
                        Err(_) => None,
                    },
                    Err(_) => None,
                }
            } else {
                None
            };
            PluginContent::Image { url: pic_url, caption: None, data: image_data }
        }
        "voice" => {
            let recognition = fields.get("Recognition").cloned().unwrap_or_default();
            if recognition.is_empty() {
                PluginContent::Voice { url: String::new(), duration_seconds: 0 }
            } else {
                PluginContent::Text(recognition)
            }
        }
        "video" | "shortvideo" => {
            PluginContent::Video { url: String::new(), duration_seconds: None, caption: None }
        }
        "event" if event == "subscribe" || event == "enter_agent" => {
            PluginContent::Command { name: event.to_string(), args: vec![] }
        }
        "event" if event == "kf_msg_or_event" => {
            // WeCom 微信客服: the callback carries only Token + OpenKfId (NO
            // message body). Pull the real messages via sync_msg in a spawned
            // task so we return "success" within WeCom's 5s limit.
            let cb_token = fields.get("Token").cloned().unwrap_or_default();
            let open_kfid_cb = fields.get("OpenKfId").cloned().unwrap_or_default();
            let tx = state.tx.clone();
            let bot_id = state.bot_id.clone();
            // Extract owned data before spawning — DashMap Ref isn't Send.
            let (http, access_token, bind_wecom_url) = match token::WECOM_STATE.get_session_for_send(&bot_id) {
                Some(bot) => match bot.entry.get_access_token_async().await {
                    Ok(tok) => (bot.entry.http.clone(), tok, bot.entry.bind_wecom_url.clone()),
                    Err(e) => {
                        warn!(bot = %bot_id, error = %e, "kf: get_access_token failed");
                        return "success";
                    }
                },
                None => {
                    warn!(bot = %bot_id, "kf callback: bot session not found");
                    return "success";
                }
            };
            tokio::spawn(async move {
                let mut cursor = token::get_kf_cursor(&bot_id);
                loop {
                    let (list, next_cursor, has_more) = match token::sync_kf_msg(
                        &http,
                        &access_token,
                        &cursor,
                        &cb_token,
                        &open_kfid_cb,
                        1000,
                    )
                    .await
                    {
                        Ok(r) => {
                            if let Some(first) = r.0.first() {
                                info!(
                                    bot = %bot_id,
                                    cb_open_kfid = %open_kfid_cb,
                                    msg_open_kfid = %first["open_kfid"],
                                    pulled = r.0.len(),
                                    "kf sync_msg pulled messages"
                                );
                            }
                            r
                        }
                        Err(e) => {
                            warn!(bot = %bot_id, error = %e, "sync_kf_msg failed");
                            return;
                        }
                    };
                    for m in &list {
                        // origin 3 = customer-sent; 4/5 = system/our own reply
                        if m["origin"].as_i64() != Some(3) {
                            continue;
                        }
                        let Some(content) =
                            materialize_kf_inbound(&http, &access_token, m).await
                        else {
                            continue;
                        };
                        info!(
                            bot = %bot_id,
                            msgtype = m["msgtype"].as_str().unwrap_or(""),
                            msgid = m["msgid"].as_str().unwrap_or(""),
                            "kf inbound customer message"
                        );
                        let ext = m["external_userid"].as_str().unwrap_or("").to_string();
                        // Auto-bind: resolve this customer's unionid via batchget
                        // and POST {wecom_external_id, unionid} to the backend so
                        // it can map the wecom customer to a member. Fire-and-
                        // forget; cached per customer so a chatty session triggers
                        // at most one batchget+bind per 30min. Only when this kf
                        // bot has bind_wecom_url configured (86助手 today).
                        if let Some(ref url) = bind_wecom_url {
                            if !ext.is_empty() {
                                let (http_b, tok_b, url_b, ext_b) =
                                    (http.clone(), access_token.clone(), url.clone(), ext.clone());
                                tokio::spawn(async move {
                                    if let Err(e) = token::bind_kf_customer_unionid(
                                        &http_b, &tok_b, &ext_b, &url_b,
                                    )
                                    .await
                                    {
                                        warn!(external_userid = %ext_b, error = %e, "kf bind-wecom failed");
                                    }
                                });
                            }
                        }
                        let _ = tx
                            .send(PluginMessage {
                                channel_type: "wecom".to_string(),
                                platform_message_id: m["msgid"].as_str().unwrap_or("").to_string(),
                                sender_id: ext.clone(),
                                sender_name: ext,
                                bot_id: bot_id.clone(),
                                content,
                                timestamp_ms: m["send_time"].as_i64().unwrap_or(0) as u64 * 1000,
                                is_group: false,
                                thread_id: None,
                                metadata: HashMap::new(),
                            })
                            .await;
                    }
                    cursor = next_cursor;
                    token::save_kf_cursor(&bot_id, &cursor);
                    if !has_more {
                        break;
                    }
                }
            });
            return "success";
        }
        _ => {
            return "success";
        }
    };

    let message = PluginMessage {
        channel_type: "wecom".to_string(),
        platform_message_id: msg_id,
        sender_id: from_user.clone(),
        sender_name: from_user.clone(),
        bot_id,
        content,
        timestamp_ms,
        is_group: false,
        thread_id: None,
        metadata: HashMap::new(),
    };

    let _ = state.tx.send(message).await;

    "success"
}

// ---------------------------------------------------------------------------
// Kf sync_msg inbound: every official customer msgtype (origin=3)
// ---------------------------------------------------------------------------

/// Media that must be fetched via `cgi-bin/media/get` before we can hand it
/// to the bridge (image/voice/video/file all arrive as `media_id` only).
#[derive(Debug)]
enum KfMediaKind {
    Image,
    Voice,
    Video,
    File,
}

#[derive(Debug)]
enum KfInbound {
    Ready(PluginContent),
    FetchMedia { media_id: String, kind: KfMediaKind },
}

fn json_str<'a>(v: &'a serde_json::Value, path: &[&str]) -> &'a str {
    let mut cur = v;
    for key in path {
        cur = &cur[*key];
    }
    cur.as_str().unwrap_or("")
}

/// Classify a customer `sync_msg` item. Pure — no I/O — so every official
/// msgtype has a fixture test. Unknown types become a text placeholder
/// (never silent-drop).
fn parse_kf_inbound(m: &serde_json::Value) -> Option<KfInbound> {
    let msgtype = m.get("msgtype")?.as_str()?;
    Some(match msgtype {
        "text" => {
            let text = json_str(m, &["text", "content"]);
            let menu_id = json_str(m, &["text", "menu_id"]);
            let body = if menu_id.is_empty() {
                text.to_string()
            } else {
                format!("{text}\n[menu_id: {menu_id}]")
            };
            KfInbound::Ready(PluginContent::Text(body))
        }
        "image" => KfInbound::FetchMedia {
            media_id: json_str(m, &["image", "media_id"]).to_string(),
            kind: KfMediaKind::Image,
        },
        "voice" => KfInbound::FetchMedia {
            media_id: json_str(m, &["voice", "media_id"]).to_string(),
            kind: KfMediaKind::Voice,
        },
        "video" => KfInbound::FetchMedia {
            media_id: json_str(m, &["video", "media_id"]).to_string(),
            kind: KfMediaKind::Video,
        },
        "file" => KfInbound::FetchMedia {
            media_id: json_str(m, &["file", "media_id"]).to_string(),
            kind: KfMediaKind::File,
        },
        "location" => {
            let lat = m["location"]["latitude"].as_f64().unwrap_or(0.0);
            let lon = m["location"]["longitude"].as_f64().unwrap_or(0.0);
            let name = json_str(m, &["location", "name"]);
            let address = json_str(m, &["location", "address"]);
            KfInbound::Ready(PluginContent::Text(format!(
                "[位置] {name}\n{address}\n纬度 {lat} 经度 {lon}"
            )))
        }
        "link" => {
            let title = json_str(m, &["link", "title"]);
            let desc = json_str(m, &["link", "desc"]);
            let url = json_str(m, &["link", "url"]);
            KfInbound::Ready(PluginContent::Text(format!(
                "[链接] {title}\n{desc}\n{url}"
            )))
        }
        "business_card" => {
            let userid = json_str(m, &["business_card", "userid"]);
            KfInbound::Ready(PluginContent::Text(format!("[名片] userid={userid}")))
        }
        "miniprogram" => {
            let title = json_str(m, &["miniprogram", "title"]);
            let appid = json_str(m, &["miniprogram", "appid"]);
            let pagepath = json_str(m, &["miniprogram", "pagepath"]);
            KfInbound::Ready(PluginContent::Text(format!(
                "[小程序] {title}\nappid={appid}\npagepath={pagepath}"
            )))
        }
        "msgmenu" => {
            let head = json_str(m, &["msgmenu", "head_content"]);
            KfInbound::Ready(PluginContent::Text(format!("[菜单消息] {head}")))
        }
        "channels" | "channels_shop_product" | "channels_shop_order" => {
            KfInbound::Ready(PluginContent::Text(format!(
                "[视频号] {msgtype}\n{m}"
            )))
        }
        "merged_msg" => KfInbound::Ready(PluginContent::Text("[聊天记录]".into())),
        "meeting" | "schedule" => {
            KfInbound::Ready(PluginContent::Text(format!("[{msgtype}]")))
        }
        "event" => {
            let ev = json_str(m, &["event", "event_type"]);
            let ev = if ev.is_empty() { "event" } else { ev };
            KfInbound::Ready(PluginContent::Command {
                name: ev.to_string(),
                args: vec![],
            })
        }
        other => {
            warn!(msgtype = other, "kf unknown msgtype — forwarding as text");
            KfInbound::Ready(PluginContent::Text(format!(
                "[未识别的客服消息类型: {other}]"
            )))
        }
    })
}

async fn materialize_kf_inbound(
    http: &reqwest::Client,
    access_token: &str,
    m: &serde_json::Value,
) -> Option<PluginContent> {
    let parsed = parse_kf_inbound(m)?;
    let (media_id, kind) = match parsed {
        KfInbound::Ready(c) => return Some(c),
        KfInbound::FetchMedia { media_id, kind } => (media_id, kind),
    };

    let (bytes, filename) = if media_id.is_empty() {
        warn!(
            msgid = m["msgid"].as_str().unwrap_or(""),
            "kf media message missing media_id"
        );
        (None, None)
    } else {
        match token::download_kf_media(http, access_token, &media_id).await {
            Ok((b, f)) => (Some(b), f),
            Err(e) => {
                warn!(
                    media_id = %media_id,
                    error = %e,
                    "kf media download failed — still forwarding placeholder"
                );
                (None, None)
            }
        }
    };

    Some(match kind {
        KfMediaKind::Image => PluginContent::Image {
            url: String::new(),
            caption: None,
            data: bytes,
        },
        KfMediaKind::Voice => PluginContent::File {
            url: String::new(),
            filename: filename.unwrap_or_else(|| "voice.amr".into()),
            data: bytes,
        },
        KfMediaKind::Video => PluginContent::File {
            url: String::new(),
            filename: filename.unwrap_or_else(|| "video.mp4".into()),
            data: bytes,
        },
        KfMediaKind::File => PluginContent::File {
            url: String::new(),
            filename: filename.unwrap_or_else(|| "file".into()),
            data: bytes,
        },
    })
}

#[cfg(test)]
mod kf_inbound_tests {
    use super::*;

    fn msg(msgtype: &str, body: serde_json::Value) -> serde_json::Value {
        let mut v = serde_json::json!({
            "msgid": "m1",
            "origin": 3,
            "msgtype": msgtype,
        });
        if let serde_json::Value::Object(map) = body {
            for (k, val) in map {
                v[k] = val;
            }
        }
        v
    }

    fn ready_text(m: &serde_json::Value) -> String {
        match parse_kf_inbound(m) {
            Some(KfInbound::Ready(PluginContent::Text(t))) => t,
            other => panic!("expected Ready text, got {other:?}"),
        }
    }

    fn media_id_of(m: &serde_json::Value) -> String {
        match parse_kf_inbound(m) {
            Some(KfInbound::FetchMedia { media_id, .. }) => media_id,
            other => panic!("expected FetchMedia, got {other:?}"),
        }
    }

    #[test]
    fn text_and_menu_id() {
        let t = ready_text(&msg(
            "text",
            serde_json::json!({"text": {"content": "hello", "menu_id": "101"}}),
        ));
        assert!(t.contains("hello"));
        assert!(t.contains("menu_id: 101"));
    }

    #[test]
    fn image_voice_video_file_fetch() {
        assert_eq!(
            media_id_of(&msg(
                "image",
                serde_json::json!({"image": {"media_id": "MID_IMG"}})
            )),
            "MID_IMG"
        );
        assert_eq!(
            media_id_of(&msg(
                "voice",
                serde_json::json!({"voice": {"media_id": "MID_VOC"}})
            )),
            "MID_VOC"
        );
        assert_eq!(
            media_id_of(&msg(
                "video",
                serde_json::json!({"video": {"media_id": "MID_VID"}})
            )),
            "MID_VID"
        );
        assert_eq!(
            media_id_of(&msg(
                "file",
                serde_json::json!({"file": {"media_id": "MID_FIL"}})
            )),
            "MID_FIL"
        );
    }

    #[test]
    fn location_link_card_miniprogram_menu() {
        let loc = ready_text(&msg(
            "location",
            serde_json::json!({"location": {
                "latitude": 23.1,
                "longitude": 113.3,
                "name": "媒体港",
                "address": "海珠区"
            }}),
        ));
        assert!(loc.contains("媒体港") && loc.contains("海珠区") && loc.contains("23.1"));

        let link = ready_text(&msg(
            "link",
            serde_json::json!({"link": {"title": "T", "desc": "D", "url": "https://x"}}),
        ));
        assert!(link.contains("T") && link.contains("https://x"));

        let card = ready_text(&msg(
            "business_card",
            serde_json::json!({"business_card": {"userid": "zhangsan"}}),
        ));
        assert!(card.contains("zhangsan"));

        let mp = ready_text(&msg(
            "miniprogram",
            serde_json::json!({"miniprogram": {
                "title": "班次",
                "appid": "wxAPP",
                "pagepath": "pages/index"
            }}),
        ));
        assert!(mp.contains("wxAPP") && mp.contains("pages/index"));

        let menu = ready_text(&msg(
            "msgmenu",
            serde_json::json!({"msgmenu": {"head_content": "满意吗"}}),
        ));
        assert!(menu.contains("满意吗"));
    }

    #[test]
    fn unknown_is_placeholder_not_drop() {
        let t = ready_text(&msg("totally_new", serde_json::json!({})));
        assert!(t.contains("totally_new"));
    }

    #[test]
    fn missing_msgtype_is_none() {
        assert!(parse_kf_inbound(&serde_json::json!({"origin": 3})).is_none());
    }
}
