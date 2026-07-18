//! WeCom (Enterprise WeChat) channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/senders/{sender_id}/session.json`,
//! spawns per-bot connections (SmartBot WS, App/Kf webhook), and handles message dispatch.
//! New bots are started via `start_sender()` (event-driven), not polling.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use types::channel::{Channel, ChannelError};
use types::plugin::PluginMessage;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub mod channel;
pub mod crypto;
pub mod smartbot;
pub mod token;

// tools module removed — rich content is delivered via the unified
// `Channel::deliver` path and `[DELIVER:key]` markers.

// ---------------------------------------------------------------------------
// SessionWatcher — unified watcher for all WeCom modes
// ---------------------------------------------------------------------------

/// Watcher that discovers WeCom bots from session files and spawns connections.
///
/// On startup, scans `senders/*/session.json` and spawns all matching bots.
/// New bots added after startup are started via `start_sender()`.
pub struct SessionWatcher {
    shutdown: Arc<AtomicBool>,
}

impl SessionWatcher {
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// `(route_key, bind_agent)` for every wecom session that declares a
    /// bind_agent. route_key = sender_id (session `name` for app/kf, `bot_id`
    /// for smartbot) — the same value channel-wecom puts in
    /// `PluginMessage.bot_id`, which bridge uses as the route_key. Lets
    /// server.rs register sender routing so inbound wecom messages reach the
    /// bound agent (mirrors weixin-oa's route_mappings).
    pub fn route_mappings(&self) -> Vec<(String, String)> {
        let home = types::config::home_dir();
        let mut out = Vec::new();
        for (_sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("wecom") {
                continue;
            }
            let sf: token::WecomSessionFile = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let sid = sf.sender_id();
            if let Some(agent) = sf.bind_agent {
                if !agent.is_empty() {
                    out.push((sid, agent));
                }
            }
        }
        out
    }
}

impl Default for SessionWatcher {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Rich content delivery (Kf mode)
// ---------------------------------------------------------------------------

/// Resolve a [`MediaRef`] to a wecom kf `media_id`: use a pre-uploaded
/// `media_id`, else download `url` / read `file_path` and `upload_kf_media`.
async fn resolve_kf_media_id(
    http: &reqwest::Client,
    token: &str,
    media_type: &str,
    media: &types::content::MediaRef,
    default_filename: &str,
) -> Result<String, String> {
    if let Some(mid) = &media.media_id {
        return Ok(mid.clone());
    }
    let bytes = if let Some(url) = &media.url {
        let resp = http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("download media: {e}"))?;
        resp.bytes()
            .await
            .map_err(|e| format!("read media body: {e}"))?
            .to_vec()
    } else if let Some(fp) = &media.file_path {
        let resolved = if fp.starts_with('/') {
            std::path::PathBuf::from(fp)
        } else {
            types::config::home_dir().join(fp)
        };
        std::fs::read(&resolved).map_err(|e| format!("read media {resolved:?}: {e}"))?
    } else {
        return Err(format!("media has no media_id, url, or file_path (for {media_type})"));
    };
    let filename = media
        .url
        .as_deref()
        .and_then(|u| u.rsplit('/').next())
        .unwrap_or(default_filename)
        .to_string();
    token::upload_kf_media(http, token, media_type, bytes, &filename).await
}

/// Deliver rich content to a wecom kf customer. Priority:
/// miniprogram > file > video > image > link > text.
async fn deliver_kf_rich(
    http: &reqwest::Client,
    token: &str,
    open_kfid: &str,
    external_userid: &str,
    content: &types::content::ContentDescriptor,
) -> Result<(), String> {
    if let Some(mp) = content.miniprogram.as_ref().filter(|m| m.is_complete()) {
        // thumb: OA's thumb_media_id is INVALID on wecom (separate media
        // library) - always re-upload from thumb_url/thumb_file.
        let thumb_media = types::content::MediaRef {
            url: mp.thumb_url.clone(),
            file_path: mp.thumb_file.clone(),
            media_id: None,
        };
        let thumb = resolve_kf_media_id(http, token, "image", &thumb_media, "thumb.jpg").await?;
        let body = serde_json::json!({
            "msgtype": "miniprogram",
            "miniprogram": {
                "appid": mp.appid,
                "pagepath": mp.pagepath,
                "title": mp.title,
                "thumb_media_id": thumb,
            }
        });
        return token::send_kf_msg(http, token, open_kfid, external_userid, body).await;
    }
    if let Some(f) = content.file.as_ref() {
        if !f.is_empty() {
            let mid = resolve_kf_media_id(http, token, "file", f, "file").await?;
            let body = serde_json::json!({ "msgtype": "file", "file": { "media_id": mid } });
            return token::send_kf_msg(http, token, open_kfid, external_userid, body).await;
        }
    }
    if let Some(v) = content.video.as_ref() {
        if !v.is_empty() {
            let mid = resolve_kf_media_id(http, token, "video", v, "video.mp4").await?;
            let body = serde_json::json!({ "msgtype": "video", "video": { "media_id": mid } });
            return token::send_kf_msg(http, token, open_kfid, external_userid, body).await;
        }
    }
    if let Some(img) = content.image.as_ref() {
        if !img.is_empty() {
            let mid = resolve_kf_media_id(http, token, "image", img, "image.jpg").await?;
            let body = serde_json::json!({ "msgtype": "image", "image": { "media_id": mid } });
            return token::send_kf_msg(http, token, open_kfid, external_userid, body).await;
        }
    }
    if let Some(l) = content.link.as_ref() {
        let body = serde_json::json!({
            "msgtype": "link",
            "link": {
                "title": l.title,
                "desc": l.desc,
                "url": l.url,
                "pic_url": l.pic_url.clone().unwrap_or_default(),
            }
        });
        return token::send_kf_msg(http, token, open_kfid, external_userid, body).await;
    }
    if let Some(text) = content.as_text() {
        let body = serde_json::json!({ "msgtype": "text", "text": { "content": text } });
        return token::send_kf_msg(http, token, open_kfid, external_userid, body).await;
    }
    Err("wecom kf: content has no representation".into())
}

impl Channel for SessionWatcher {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    fn supports_proactive_push(&self) -> bool {
        // WeCom App and Kf modes support proactive push; SmartBot mode does not.
        // SessionWatcher mixes all modes — return true and rely on send() to
        // fall back to buffering when a SmartBot bot's send fails.
        true
    }

    fn name(&self) -> &str {
        "WeCom Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        // Initial load + spawn all discovered bots
        token::WECOM_STATE.load_from_dir();
        spawn_inactive_bots(&sender);
        info!("WeCom session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError> {
        let session = token::WECOM_STATE
            .get_session_for_send(bot_id)
            .ok_or_else(|| ChannelError::UnknownBot(bot_id.to_string()))?;

        match &session.entry.mode {
            token::WecomMode::App { .. } => {
                token::send_app_message(&session.entry, user_id, text)
                    .map_err(ChannelError::SendFailed)?;
            }
            token::WecomMode::Kf { .. } => {
                token::send_kf_message(&session.entry, user_id, text)
                    .map_err(ChannelError::SendFailed)?;
            }
            token::WecomMode::SmartBot { .. } => {
                // SmartBot uses response_url mechanism
                let key = format!("{}:{}", bot_id, user_id);
                let response_url = smartbot::RESPONSE_URLS
                    .get()
                    .ok_or_else(|| ChannelError::Config("RESPONSE_URLS not initialized".to_string()))?
                    .lock()
                    .unwrap()
                    .remove(&key)
                    .ok_or_else(|| {
                        ChannelError::NotSupported("No response_url available. SmartBot can only reply within callback context.".to_string())
                    })?;

                let http = session.entry.http.clone();
                let text = text.to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let result = match rt {
                        Ok(rt) => rt.block_on(token::send_smartbot_response_async(&http, &response_url, &text))
                            .map_err(ChannelError::SendFailed),
                        Err(e) => Err(ChannelError::Other(format!("Runtime creation failed: {e}"))),
                    };
                    let _ = tx.send(result);
                });
                rx.recv()
                    .map_err(|e| ChannelError::Other(format!("SmartBot send thread disconnected: {e}")))??;
            }
        }

        Ok(())
    }

    fn deliver(
        &self,
        content: &types::content::ContentDescriptor,
        bot_id: &str,
        user_id: &str,
    ) -> Result<(), ChannelError> {
        // Resolve Kf creds as owned data and drop the DashMap ref before the
        // blocking thread spawn (avoids holding a shard lock across recv()).
        // Non-Kf modes (App/SmartBot) degrade rich content to text via send().
        let kf_creds = {
            let session = token::WECOM_STATE
                .get_session_for_send(bot_id)
                .ok_or_else(|| ChannelError::UnknownBot(bot_id.to_string()))?;
            match &session.entry.mode {
                token::WecomMode::Kf { .. } => {
                    let open_kfid = session
                        .entry
                        .open_kfid()
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            ChannelError::NotSupported("kf session missing open_kfid".into())
                        })?;
                    let token = session
                        .entry
                        .get_access_token()
                        .map_err(ChannelError::TokenFailed)?;
                    Some((session.entry.http.clone(), token, open_kfid))
                }
                _ => None,
            }
        };

        let Some((http, token, open_kfid)) = kf_creds else {
            let text = content.as_text().ok_or_else(|| {
                ChannelError::NotSupported(
                    "wecom app/smartbot: no text representation for this content".into(),
                )
            })?;
            return self.send(bot_id, user_id, &text);
        };

        let ext = user_id.to_string();
        let content = content.clone();
        types::channel::block_on_detached(async move {
            deliver_kf_rich(&http, &token, &open_kfid, &ext, &content)
                .await
                .map_err(ChannelError::SendFailed)
        })
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn start_sender(&self, sender_id: &str, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        token::WECOM_STATE.load_new_from_dir();
        spawn_bot_by_id(sender_id, &sender);
        info!(sender_id = %sender_id, "WeCom: started new sender");
        Ok(())
    }
}

/// Spawn channel threads for all bots that are loaded but not yet active.
fn spawn_inactive_bots(sender: &mpsc::Sender<PluginMessage>) {
    for entry in token::WECOM_STATE.bots.iter() {
        let key = entry.key().clone();
        let session = entry.value();
        if session.active.load(Ordering::Relaxed) {
            continue;
        }
        spawn_single_bot(&key, session, sender);
        session.active.store(true, Ordering::Relaxed);
    }
}

/// Spawn a specific bot by sender_id (if loaded and not yet active).
fn spawn_bot_by_id(sender_id: &str, sender: &mpsc::Sender<PluginMessage>) {
    if let Some(session) = token::WECOM_STATE.bots.get(sender_id) {
        if session.active.load(Ordering::Relaxed) {
            return;
        }
        spawn_single_bot(sender_id, session.value(), sender);
        session.active.store(true, Ordering::Relaxed);
    }
}

/// Spawn a single bot's channel thread.
fn spawn_single_bot(
    key: &str,
    session: &token::WecomBotSession,
    sender: &mpsc::Sender<PluginMessage>,
) {
    match &session.entry.mode {
        token::WecomMode::SmartBot { .. } => {
            let bot_name = session.entry.name.clone();
            let bot_id = session.entry.bot_id().unwrap_or("").to_string();
            let secret = session.entry.bot_secret().unwrap_or("").to_string();

            let tx = sender.clone();
            info!(sender_id = %key, "Spawning SmartBot thread");
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime for SmartBot");
                rt.block_on(async {
                    let mut ch = smartbot::SmartBotChannel::new(
                        bot_name.clone(),
                        bot_id,
                        secret,
                    );
                    if let Err(e) = ch.start(tx) {
                        warn!(bot = %bot_name, "SmartBot channel start error: {e}");
                    }
                });
            });
        }
        token::WecomMode::App { .. } | token::WecomMode::Kf { .. } => {
            let bot_id = session.entry.name.clone();
            let webhook_port = session.entry.webhook_port;
            let encoding_aes_key = session.entry.encoding_aes_key.clone();
            let callback_token = session.entry.callback_token.clone();

            let tx = sender.clone();
            info!(sender_id = %key, "Spawning WeCom App/Kf thread");
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("Failed to create tokio runtime for WeCom channel");
                rt.block_on(async {
                    let mut ch = channel::WeComChannel::new(
                        bot_id.clone(),
                        webhook_port,
                        encoding_aes_key,
                        callback_token,
                    );
                    if let Err(e) = ch.start(tx) {
                        warn!(bot = %bot_id, "WeCom channel start error: {e}");
                    }
                });
            });
        }
    }
}
