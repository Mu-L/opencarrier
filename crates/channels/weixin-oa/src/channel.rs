//! SessionWatcher and message processing for WeChat OA channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};
use types::channel::{Channel, ChannelError, RoutingMode};
use types::plugin::{PluginContent, PluginMessage};

use crate::api;
use crate::models::{OaMessage, WeixinOaSessionFile};

// --- Runtime state ---

pub struct OaAccountState {
    pub app_id: String,
    pub app_secret: String,
    pub name: String,
    pub bind_agent: Option<String>,
    /// Cached access_token + expiry Instant.
    pub token: tokio::sync::Mutex<Option<(String, Instant)>>,
    pub http: reqwest::Client,
}

pub struct WeixinOaState {
    pub accounts: DashMap<String, Arc<OaAccountState>>,
}

impl WeixinOaState {
    pub fn new() -> Self {
        WeixinOaState {
            accounts: DashMap::new(),
        }
    }
}

impl Default for WeixinOaState {
    fn default() -> Self {
        Self::new()
    }
}

/// Token cache TTL with early-expiry margin (300s before actual expiry).
const TOKEN_MARGIN_SECS: u64 = 300;

impl OaAccountState {
    pub fn from_session(session: &WeixinOaSessionFile) -> Self {
        OaAccountState {
            app_id: session.app_id.clone(),
            app_secret: session.app_secret.clone(),
            name: session.name.clone(),
            bind_agent: session.bind_agent.clone(),
            token: tokio::sync::Mutex::new(None),
            http: reqwest::Client::new(),
        }
    }

    /// Get a valid access_token, refreshing if needed.
    pub async fn get_token(&self) -> Result<String, String> {
        let mut guard = self.token.lock().await;
        if let Some((ref token, expiry)) = *guard {
            if expiry > Instant::now() {
                return Ok(token.clone());
            }
        }
        let resp = api::get_access_token(&self.http, &self.app_id, &self.app_secret).await?;
        let token = resp.access_token;
        let token = token.ok_or_else(|| {
            format!(
                "No access_token in response (errcode={:?}, errmsg={:?})",
                resp.errcode, resp.errmsg
            )
        })?;
        let expires_in = resp.expires_in.unwrap_or(7200);
        let margin = expires_in.saturating_sub(TOKEN_MARGIN_SECS);
        let expiry = Instant::now() + std::time::Duration::from_secs(margin);
        *guard = Some((token.clone(), expiry));
        Ok(token)
    }

    /// Invalidate the cached token (on 40001 errors).
    pub async fn invalidate_token(&self) {
        let mut guard = self.token.lock().await;
        *guard = None;
    }
}

// --- SessionWatcher ---

/// Global shared OA state — the SessionWatcher and the send_image tool both
/// read from this Arc so runtime-added accounts are visible to both.
pub static WEIXIN_OA_STATE: std::sync::LazyLock<Arc<WeixinOaState>> =
    std::sync::LazyLock::new(|| Arc::new(WeixinOaState::new()));

pub struct SessionWatcher {
    pub state: Arc<WeixinOaState>,
    pub shutdown: Arc<AtomicBool>,
}

impl Default for SessionWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionWatcher {
    pub fn new() -> Self {
        SessionWatcher {
            state: WEIXIN_OA_STATE.clone(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Load session files from `senders/*/session.json`.
    pub fn load_from_dir(&self, senders_dir: &std::path::Path) {
        if let Ok(entries) = std::fs::read_dir(senders_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let session_path = path.join("session.json");
                if let Ok(data) = std::fs::read_to_string(&session_path) {
                    if let Ok(session) = serde_json::from_str::<WeixinOaSessionFile>(&data) {
                        if session.channel == "weixin-oa" && !session.app_id.is_empty() {
                            let app_id = session.app_id.clone();
                            info!(
                                app_id = %app_id,
                                name = %session.name,
                                "weixin-oa: loaded session"
                            );
                            let account = Arc::new(OaAccountState::from_session(&session));
                            self.state.accounts.insert(app_id, account);
                        }
                    }
                }
            }
        }
    }

    /// Look up an account by app_id.
    pub fn get_account(&self, app_id: &str) -> Option<Arc<OaAccountState>> {
        self.state.accounts.get(app_id).map(|a| a.clone())
    }

    /// Return (app_id → bind_agent) mappings for all loaded sessions.
    ///
    /// Called by the server bootstrap to register routes with the SenderRouter
    /// so inbound messages (route_key = app_id) reach the bound agent.
    pub fn route_mappings(&self) -> Vec<(String, String)> {
        self.state
            .accounts
            .iter()
            .filter_map(|entry| {
                entry
                    .bind_agent
                    .as_ref()
                    .map(|agent| (entry.app_id.clone(), agent.clone()))
            })
            .collect()
    }
}

/// Extract `[SEND_IMAGE:media_id]` markers from agent reply text.
///
/// The agent emits these markers in its reply to request image sends without
/// needing a discoverable tool (which the LLM struggles to call reliably).
/// Returns (list of media_ids, text with markers stripped).
fn extract_image_markers(text: &str) -> (Vec<String>, String) {
    let marker = "[SEND_IMAGE:";
    let mut media_ids = Vec::new();
    let mut cleaned = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(marker) {
        cleaned.push_str(&rest[..start]);
        let after = &rest[start + marker.len()..];
        if let Some(end) = after.find(']') {
            let media_id = after[..end].trim().to_string();
            if !media_id.is_empty() {
                media_ids.push(media_id);
            }
            rest = &after[end + 1..];
        } else {
            // Malformed (no closing ]), emit as-is and stop.
            cleaned.push_str(marker);
            cleaned.push_str(after);
            rest = "";
        }
    }
    cleaned.push_str(rest);
    (media_ids, cleaned)
}

/// Does this inbound message need an agent reply?
///
/// Pure-receipt / log events (template delivery receipts, unsubscribe, link
/// clicks) are dropped at the channel level so they never reach the agent —
/// zero token cost, no `[no reply needed]` round-trip.
pub fn needs_reply(msg: &OaMessage) -> bool {
    match msg.msg_type.as_str() {
        // Real user messages always need a reply
        "text" | "image" | "voice" | "video" | "shortvideo" | "location" | "link" => true,
        "event" => match msg.event.as_str() {
            // Interactive events the agent should respond to
            "subscribe" | "SCAN" | "CLICK" => true,
            // Receipts / passive events — drop silently
            "unsubscribe"
            | "TEMPLATESENDJOBFINISH"
            | "MASSSENDJOBFINISH"
            | "VIEW"
            | "LOCATION" => false,
            // Unknown events: let the agent decide (conservative)
            _ => true,
        },
        // Unknown message types: let the agent handle it
        _ => true,
    }
}

/// Convert an OaMessage to a PluginMessage ready for the bridge.
pub fn build_plugin_message(msg: &OaMessage, app_id: &str) -> PluginMessage {
    // Image messages carry a PicUrl — pass it as PluginContent::Image so the
    // bridge downloads + vision-describes it (instead of a useless "[图片消息]").
    let content = if msg.msg_type == "image" && !msg.pic_url.is_empty() {
        PluginContent::Image {
            url: msg.pic_url.clone(),
            caption: None,
            data: None,
        }
    } else {
        PluginContent::Text(build_message_text(msg))
    };

    PluginMessage {
        channel_type: "weixin-oa".to_string(),
        platform_message_id: msg.msg_id.clone(),
        sender_id: msg.from_user.clone(),
        sender_name: msg.from_user.clone(),
        bot_id: app_id.to_string(),
        content,
        timestamp_ms: if msg.create_time > 0 {
            msg.create_time * 1000
        } else {
            0
        },
        is_group: false,
        thread_id: None,
        metadata: Default::default(),
    }
}

/// Build the text content to send to agent from an OaMessage.
fn build_message_text(msg: &OaMessage) -> String {
    let msg_type = &msg.msg_type;

    match msg_type.as_str() {
        "text" | "INPUT" | "" => {
            // Plain text message
            msg.content.clone()
        }
        "event" => {
            match msg.event.as_str() {
                "subscribe" => {
                    if msg.event_key.is_empty() {
                        "[关注事件] 用户关注了服务号".to_string()
                    } else {
                        let scene = msg.event_key.trim_start_matches("qrscene_");
                        format!("[扫码关注] 场景值: {scene}")
                    }
                }
                "unsubscribe" => "[取关事件] 用户取消了关注".to_string(),
                "CLICK" => format!("[菜单点击] 菜单key: {}", msg.event_key),
                "SCAN" => format!("[扫码事件] 场景值: {}", msg.event_key),
                _ => format!(
                    "[事件] type={} event={} key={}",
                    msg_type, msg.event, msg.event_key
                ),
            }
        }
        "image" => "[图片消息]".to_string(),
        "voice" => {
            if !msg.recognition.is_empty() {
                msg.recognition.clone()
            } else {
                "[语音消息]".to_string()
            }
        }
        "video" => "[视频消息]".to_string(),
        _ => format!("[未知消息类型: {}]", msg_type),
    }
}

// --- Channel trait impl ---

impl Channel for SessionWatcher {
    fn channel_type(&self) -> &str {
        "weixin-oa"
    }

    fn name(&self) -> &str {
        "WeChat OA Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    /// One-to-one channel: a single OA binds to one fixed agent.
    /// No per-user clones, naming, or switching.
    fn routing_mode(&self) -> RoutingMode {
        RoutingMode::DirectBind
    }

    fn start(
        &mut self,
        _sender: mpsc::Sender<PluginMessage>,
    ) -> Result<(), ChannelError> {
        info!("weixin-oa: channel started (webhook mode, no polling)");
        Ok(())
    }

    fn send(
        &self,
        bot_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<(), ChannelError> {
        let account = self
            .get_account(bot_id)
            .ok_or_else(|| ChannelError::UnknownBot(bot_id.to_string()))?;

        let http = account.http.clone();
        let app_id = account.app_id.clone();
        let app_secret = account.app_secret.clone();
        let user_id = user_id.to_string();
        let text = text.to_string();

        // Spawn a thread for the async send (channel.send() is synchronous)
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async move {
                let token = match api::get_access_token(&http, &app_id, &app_secret).await {
                    Ok(t) => t.access_token.unwrap_or_default(),
                    Err(e) => {
                        warn!(%app_id, error=%e, "weixin-oa: send failed to get token");
                        return;
                    }
                };
                // Parse [SEND_IMAGE:media_id] markers — the agent emits these in its
                // reply text to request image sends without needing a discoverable tool.
                let (media_ids, text_only) = extract_image_markers(&text);
                for media_id in &media_ids {
                    if let Err(e) =
                        api::custom_send_image(&http, &token, &user_id, media_id).await
                    {
                        warn!(%app_id, %user_id, error=%e, "weixin-oa: image send failed");
                    } else {
                        info!(%app_id, %user_id, "weixin-oa: image sent via marker");
                    }
                }
                // Send any remaining text (after stripping markers) if non-empty
                if !text_only.trim().is_empty() {
                    if let Err(e) = api::custom_send_text(&http, &token, &user_id, &text_only).await
                    {
                        warn!(%app_id, %user_id, error=%e, "weixin-oa: send failed");
                    }
                }
            });
        });

        Ok(())
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        info!("weixin-oa: channel stopped");
    }

    fn start_sender(
        &self,
        sender_id: &str,
        _sender: mpsc::Sender<PluginMessage>,
    ) -> Result<(), ChannelError> {
        info!(sender_id, "weixin-oa: start_sender called (no dynamic spawn needed)");
        Ok(())
    }

    fn supports_proactive_push(&self) -> bool {
        true // Customer service message API allows proactive replies within 48h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_message_text_subscribe() {
        let msg = OaMessage {
            msg_type: "event".into(),
            event: "subscribe".into(),
            event_key: "qrscene_86XQ90593".into(),
            ..Default::default()
        };
        let text = build_message_text(&msg);
        assert!(text.contains("扫码关注"));
        assert!(text.contains("86XQ90593"));
    }

    #[test]
    fn test_build_message_text_click() {
        let msg = OaMessage {
            msg_type: "event".into(),
            event: "CLICK".into(),
            event_key: "menu_5_39470".into(),
            ..Default::default()
        };
        let text = build_message_text(&msg);
        assert!(text.contains("菜单点击"));
        assert!(text.contains("menu_5_39470"));
    }

    #[test]
    fn test_build_message_text_plain() {
        let msg = OaMessage {
            msg_type: "text".into(),
            content: "巴士路线".into(),
            from_user: "oTest".into(),
            ..Default::default()
        };
        let text = build_message_text(&msg);
        assert_eq!(text, "巴士路线");
    }

    #[test]
    fn test_parse_xml_text_message() {
        let xml = r#"<xml>
<ToUserName><![CDATA[gh_test]]></ToUserName>
<FromUserName><![CDATA[oUser123]]></FromUserName>
<CreateTime>1719936000</CreateTime>
<MsgType><![CDATA[text]]></MsgType>
<Content><![CDATA[你好巴士]]></Content>
<MsgId>1234567890</MsgId>
</xml>"#;
        let msg = crate::models::parse_xml_message(xml).unwrap();
        assert_eq!(msg.msg_type, "text");
        assert_eq!(msg.content, "你好巴士");
        assert_eq!(msg.from_user, "oUser123");
        assert_eq!(msg.to_user, "gh_test");
    }

    #[test]
    fn test_parse_xml_subscribe_event() {
        let xml = r#"<xml>
<ToUserName><![CDATA[gh_test]]></ToUserName>
<FromUserName><![CDATA[oUser456]]></FromUserName>
<CreateTime>1719936000</CreateTime>
<MsgType><![CDATA[event]]></MsgType>
<Event><![CDATA[subscribe]]></Event>
<EventKey><![CDATA[qrscene_86XQ90593]]></EventKey>
</xml>"#;
        let msg = crate::models::parse_xml_message(xml).unwrap();
        assert_eq!(msg.msg_type, "event");
        assert_eq!(msg.event, "subscribe");
        assert_eq!(msg.event_key, "qrscene_86XQ90593");
    }
}