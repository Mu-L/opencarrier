//! WeCom channel adapter — webhook server for inbound/outbound messages.

use std::collections::HashMap;

use axum::extract::Query;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use types::channel::{Channel, ChannelError};
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

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
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

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError> {
        let bot = crate::token::WECOM_STATE
            .get_session_for_send(bot_id)
            .ok_or_else(|| ChannelError::UnknownBot(bot_id.to_string()))?;

        match &bot.entry.mode {
            token::WecomMode::App { .. } => {
                token::send_app_message(&bot.entry, user_id, text)
                    .map_err(ChannelError::SendFailed)?;
            }
            token::WecomMode::Kf { .. } => {
                token::send_kf_message(&bot.entry, user_id, text)
                    .map_err(ChannelError::SendFailed)?;
            }
            token::WecomMode::SmartBot { .. } => {
                return Err(ChannelError::NotSupported(
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

    let app = Router::new()
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
            let (http, access_token) = match token::WECOM_STATE.get_session_for_send(&bot_id) {
                Some(bot) => match bot.entry.get_access_token_async().await {
                    Ok(tok) => (bot.entry.http.clone(), tok),
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
                        Ok(r) => r,
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
                        let content = match m["msgtype"].as_str() {
                            Some("text") => PluginContent::Text(
                                m["text"]["content"].as_str().unwrap_or("").to_string(),
                            ),
                            _ => continue,
                        };
                        let ext = m["external_userid"].as_str().unwrap_or("").to_string();
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
