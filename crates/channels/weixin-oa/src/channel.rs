//! SessionWatcher and message processing for WeChat OA channel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};
use types::channel::{Channel, RoutingMode};
use types::error::{CarrierError, CarrierResult};
use types::plugin::{PluginContent, PluginMessage};

use crate::api;
use crate::models::{OaMessage, WeixinOaSessionFile};
use crate::tools::{is_token_expired, refresh_token};

// --- Runtime state ---

pub struct OaAccountState {
    pub app_id: String,
    pub app_secret: String,
    pub name: String,
    pub bind_agent: Option<String>,
    /// Template-message fallback config (45015 window-closed replies), read
    /// through from the session file.
    pub fallback_template_id: Option<String>,
    pub fallback_template_field: Option<String>,
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

impl OaAccountState {
    pub fn from_session(session: &WeixinOaSessionFile) -> Self {
        OaAccountState {
            app_id: session.app_id.clone(),
            app_secret: session.app_secret.clone(),
            name: session.name.clone(),
            bind_agent: session.bind_agent.clone(),
            fallback_template_id: session.fallback_template_id.clone(),
            fallback_template_field: session.fallback_template_field.clone(),
            http: reqwest::Client::new(),
        }
    }

    /// Get a valid access_token. Delegates to the central `wechat-oa` core
    /// cache (2026-08-18 three-shell convergence) — the per-account Mutex
    /// cache is gone; every token in the process now flows through one
    /// single-flight cache keyed by app_id.
    pub async fn get_token(&self) -> CarrierResult<String> {
        wechat_oa::token::get_token(&self.http, &self.app_id, &self.app_secret).await
    }

    /// Invalidate the cached token (on 40001 errors).
    pub async fn invalidate_token(&self) {
        wechat_oa::token::invalidate(&self.app_id);
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
            // Receipts / passive events — drop silently.
            // `view_miniprogram` = menu item that opens a mini-program
            // (WeChat's VIEW-for-miniprogram): zero conversational intent.
            // Without this, every menu→小程序 click (86bus's main CTA)
            // burned a full reasoning-model turn to earn a
            // "[no reply needed]" (2026-08-16: 5 such turns, each ~4s,
            // all correctly silent — pure LLM cost).
            "unsubscribe"
            | "TEMPLATESENDJOBFINISH"
            | "MASSSENDJOBFINISH"
            | "VIEW"
            | "view_miniprogram"
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
        "event" => match msg.event.as_str() {
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
        },
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

// --- Rich content delivery ---

/// Resolve a [`MediaRef`] to an OA permanent material `media_id`: use a
/// pre-uploaded `media_id` directly, else download `url` / read `file_path`
/// and upload via `upload_media_permanent`.
async fn resolve_oa_media_id(
    account: &OaAccountState,
    token: &str,
    media: &types::content::MediaRef,
    default_filename: &str,
) -> CarrierResult<String> {
    if let Some(mid) = &media.media_id {
        return Ok(mid.clone());
    }
    let bytes = if let Some(url) = &media.url {
        let resp = account
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| CarrierError::Network(format!("download media: {e}")))?;
        resp.bytes()
            .await
            .map_err(|e| CarrierError::Network(format!("read media body: {e}")))?
            .to_vec()
    } else if let Some(fp) = &media.file_path {
        let resolved = if fp.starts_with('/') {
            std::path::PathBuf::from(fp)
        } else {
            types::config::home_dir().join(fp)
        };
        std::fs::read(&resolved)
            .map_err(|e| CarrierError::Network(format!("read media {resolved:?}: {e}")))?
    } else {
        return Err(CarrierError::InvalidInput(
            "media has no media_id, url, or file_path".to_string(),
        ));
    };
    let filename = media
        .url
        .as_deref()
        .and_then(|u| u.rsplit('/').next())
        .unwrap_or(default_filename)
        .to_string();
    let (mid, _url) = api::upload_media_permanent(&account.http, token, bytes, &filename).await?;
    Ok(mid)
}

/// Resolve a miniprogram card thumb to a permanent `media_id`: use
/// `thumb_media_id` directly (OA permanent), else upload from
/// `thumb_url`/`thumb_file`.
async fn resolve_oa_thumb(
    account: &OaAccountState,
    token: &str,
    mp: &types::content::MiniprogramContent,
) -> CarrierResult<String> {
    if let Some(mid) = &mp.thumb_media_id {
        return Ok(mid.clone());
    }
    let media = types::content::MediaRef {
        url: mp.thumb_url.clone(),
        file_path: mp.thumb_file.clone(),
        media_id: None,
    };
    resolve_oa_media_id(account, token, &media, "thumb.png").await
}

/// Run `send` once; on token-expired (40001) refresh and retry once.
async fn with_token_retry<F, Fut>(account: &Arc<OaAccountState>, send: F) -> CarrierResult<()>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = CarrierResult<()>>,
{
    let token = account.get_token().await?;
    match send(token).await {
        Ok(()) => Ok(()),
        Err(e) if is_token_expired(&e.to_string()) => {
            let token = refresh_token(account).await?;
            send(token).await
        }
        Err(e) => Err(e),
    }
}

/// Truncate a 45015-fallback text for a template field: template values are
/// display-capped (~20 chars visible), so ship a compact summary + a nudge to
/// come back into the account instead of a raw truncated reply.
fn fallback_summary(text: &str) -> String {
    const CAP: usize = 40;
    let head: String = text.chars().take(CAP).collect();
    if text.chars().count() > CAP {
        format!("{head}…（进入公众号回复可继续查看）")
    } else {
        format!("{head}（进入公众号回复可继续查看）")
    }
}

/// Deliver rich content to a weixin-oa follower. Priority: miniprogram, then
/// template, then image, then text (degrades via `as_text`, incl. a formatted
/// link). Template sits above text so a text+template payload uses template as
/// the carrier (it works outside the 48h window; text does not). Each send
/// retries once on a 40001 (expired token) by refreshing; a text send that
/// hits the 48h window (45015) falls back to a template message when the
/// account has `fallback_template_id`/`fallback_template_field` configured.
async fn deliver_oa(
    account: &Arc<OaAccountState>,
    openid: &str,
    content: &types::content::ContentDescriptor,
) -> CarrierResult<()> {
    if let Some(mp) = content.miniprogram.as_ref().filter(|m| m.is_complete()) {
        // Resolve thumb once (uses the current token); then send with retry.
        let token = account.get_token().await?;
        let thumb = resolve_oa_thumb(account, &token, mp).await?;
        let title = mp.title.clone();
        let pagepath = mp.pagepath.clone();
        let appid = mp.appid.clone();
        return with_token_retry(account, |token| {
            let http = account.http.clone();
            let openid = openid.to_string();
            let thumb = thumb.clone();
            let title = title.clone();
            let pagepath = pagepath.clone();
            let appid = appid.clone();
            async move {
                api::custom_send_miniprogrampage(
                    &http, &token, &openid, &title, &pagepath, &thumb, &appid,
                )
                .await
            }
        })
        .await;
    }
    if let Some(tpl) = content.template.as_ref().filter(|t| t.is_complete()) {
        let template_id = tpl.template_id.clone();
        let data = tpl.data.clone();
        let url = tpl.url.clone();
        let mp = tpl.miniprogram.clone();
        return with_token_retry(account, |token| {
            let http = account.http.clone();
            let openid = openid.to_string();
            let template_id = template_id.clone();
            let data = data.clone();
            let url = url.clone();
            let mp = mp.clone();
            async move {
                api::template_send(
                    &http,
                    &token,
                    &openid,
                    &template_id,
                    url.as_deref(),
                    mp.as_ref(),
                    &data,
                )
                .await
                .map(|_| ())
            }
        })
        .await;
    }
    if let Some(img) = content.image.as_ref() {
        if !img.is_empty() {
            let token = account.get_token().await?;
            let media_id = resolve_oa_media_id(account, &token, img, "image.png").await?;
            return with_token_retry(account, |token| {
                let http = account.http.clone();
                let openid = openid.to_string();
                let media_id = media_id.clone();
                async move { api::custom_send_image(&http, &token, &openid, &media_id).await }
            })
            .await;
        }
    }
    if let Some(text) = content.as_text() {
        let result = with_token_retry(account, |token| {
            let http = account.http.clone();
            let openid = openid.to_string();
            let text = text.clone();
            async move { api::custom_send_text(&http, &token, &openid, &text).await }
        })
        .await;
        return match result {
            Ok(()) => Ok(()),
            // 48h customer-service window closed (45015) or per-user reply
            // quota exhausted (45047) — both mean the customer-service path
            // is unavailable. If the account configured a fallback template,
            // retry as a template message (no window/quota limit) — the
            // 86bus-era failure mode (~49/day) lands here.
            Err(e)
                if matches!(
                    wechat_oa::api::extract_errcode(&e.to_string()),
                    Some(45015) | Some(45047)
                ) =>
            {
                template_fallback(account, openid, &text, e).await
            }
            Err(e) => Err(e),
        };
    }
    Err(CarrierError::InvalidInput(
        "weixin-oa: content has no miniprogram, template, image, or text representation"
            .to_string(),
    ))
}

/// 45015 fallback: re-send the text as a template message using the account's
/// configured `fallback_template_id` + `fallback_template_field`. Unconfigured
/// accounts keep the original error (log-and-drop behavior unchanged).
async fn template_fallback(
    account: &Arc<OaAccountState>,
    openid: &str,
    text: &str,
    original_err: CarrierError,
) -> CarrierResult<()> {
    let (Some(template_id), Some(field)) = (
        account
            .fallback_template_id
            .as_deref()
            .filter(|s| !s.is_empty()),
        account
            .fallback_template_field
            .as_deref()
            .filter(|s| !s.is_empty()),
    ) else {
        warn!(
            app_id = %account.app_id,
            openid,
            error = %original_err,
            "45015 window closed and no fallback template configured (set fallback_template_id + fallback_template_field in session.json)"
        );
        return Err(original_err);
    };
    let data = serde_json::json!({ field: { "value": fallback_summary(text) } });
    let token = account.get_token().await?;
    match api::template_send(
        &account.http,
        &token,
        openid,
        template_id,
        None,
        None,
        &data,
    )
    .await
    {
        Ok(_) => {
            info!(app_id = %account.app_id, openid, template_id, "45015 fallback delivered via template message");
            Ok(())
        }
        Err(e) => {
            warn!(app_id = %account.app_id, openid, error = %e, "45015 fallback template send failed");
            Err(e)
        }
    }
}

/// Deliver a `ContentDescriptor` to an OA follower by `app_id`. Public entry
/// point for the webhook callback (inbound automation rules) and the future
/// cron `Push` path - reuses the private `deliver_oa` (token + 40001 retry).
pub async fn deliver_content(
    app_id: &str,
    openid: &str,
    content: &types::content::ContentDescriptor,
) -> CarrierResult<()> {
    let account = WEIXIN_OA_STATE
        .accounts
        .get(app_id)
        .map(|a| a.clone())
        .ok_or_else(|| {
            CarrierError::InvalidInput(format!("no OA account loaded for app_id {app_id}"))
        })?;
    deliver_oa(&account, openid, content).await
}

/// Execute a stored automation rule's push against the triggering user.
/// `task_payload` is a `ContentDescriptor`-shaped JSON object
/// (`{"text":"..."}` or `{"miniprogram":{appid,pagepath,title,thumb_media_id}}`).
/// Shared by the inbound callback (Phase 1) and the future cron `Push` path
/// (Phase 2) so both triggers converge on one executor.
pub async fn execute_push(
    app_id: &str,
    openid: &str,
    task_payload: &serde_json::Value,
) -> CarrierResult<()> {
    let content: types::content::ContentDescriptor =
        serde_json::from_value(task_payload.clone())
            .map_err(|e| CarrierError::Serialization(format!("bad task_payload: {e}")))?;
    deliver_content(app_id, openid, &content).await
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

    fn start(&mut self, _sender: mpsc::Sender<PluginMessage>) -> CarrierResult<()> {
        info!("weixin-oa: channel started (webhook mode, no polling)");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> CarrierResult<()> {
        let account = self
            .get_account(bot_id)
            .ok_or_else(|| CarrierError::InvalidInput(bot_id.to_string()))?;

        let http = account.http.clone();
        let app_id = account.app_id.clone();
        let account = account.clone(); // Arc — used for the cached get_token() below
        let user_id = user_id.to_string();
        let text = text.to_string();

        // Spawn a thread for the async send (channel.send() is synchronous)
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Runtime::new() {
                Ok(rt) => rt,
                Err(e) => {
                    warn!(%app_id, error=%e, "weixin-oa: failed to create send runtime");
                    return;
                }
            };
            rt.block_on(async move {
                // Use the cached token (300s margin + 40001 invalidate) instead of
                // hitting the token endpoint on every send — the previous direct
                // api::get_access_token() call re-fetched each message.
                let token = match account.get_token().await {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(%app_id, error=%e, "weixin-oa: send failed to get token");
                        return;
                    }
                };
                // Parse [SEND_IMAGE:media_id] markers — the agent emits these in its
                // reply text to request image sends without needing a discoverable tool.
                let (media_ids, text_only) = extract_image_markers(&text);
                for media_id in &media_ids {
                    if let Err(e) = api::custom_send_image(&http, &token, &user_id, media_id).await
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

    fn deliver(
        &self,
        content: &types::content::ContentDescriptor,
        bot_id: &str,
        user_id: &str,
    ) -> CarrierResult<()> {
        let account = self
            .get_account(bot_id)
            .ok_or_else(|| CarrierError::InvalidInput(bot_id.to_string()))?;
        let openid = user_id.to_string();
        let content = content.clone();
        // Dedicated thread + runtime: safe from Tokio workers / spawn_blocking.
        // Returning the real Result lets the marker handler fall back to text.
        types::channel::block_on_detached(
            async move { deliver_oa(&account, &openid, &content).await },
        )
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        info!("weixin-oa: channel stopped");
    }

    fn start_sender(
        &self,
        sender_id: &str,
        _sender: mpsc::Sender<PluginMessage>,
    ) -> CarrierResult<()> {
        info!(
            sender_id,
            "weixin-oa: start_sender called (no dynamic spawn needed)"
        );
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
    fn test_needs_reply_filters_passive_events() {
        // Menu→mini-program click: no conversational intent — must NOT burn an
        // agent turn (2026-08-16: five such turns each spent a reasoning-model
        // call to answer "[no reply needed]").
        for ev in [
            "VIEW",
            "view_miniprogram",
            "unsubscribe",
            "TEMPLATESENDJOBFINISH",
        ] {
            let msg = OaMessage {
                msg_type: "event".into(),
                event: ev.into(),
                ..Default::default()
            };
            assert!(!needs_reply(&msg), "event {ev} should be dropped");
        }
        // Interactive events and real messages still reach the agent.
        for ev in ["subscribe", "SCAN", "CLICK"] {
            let msg = OaMessage {
                msg_type: "event".into(),
                event: ev.into(),
                ..Default::default()
            };
            assert!(needs_reply(&msg), "event {ev} should reach the agent");
        }
        let text = OaMessage {
            msg_type: "text".into(),
            content: "巴士路线".into(),
            ..Default::default()
        };
        assert!(needs_reply(&text));
    }

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
