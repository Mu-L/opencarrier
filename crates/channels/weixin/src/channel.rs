//! WeChat iLink session watcher — dynamic session discovery, polling, and send.

use crate::api;
use crate::token::WEIXIN_STATE;
use crate::models::*;
use crate::crypto;
use types::plugin::{PluginContent, PluginMessage};
use base64::Engine;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use types::channel::{Channel, ChannelError};

/// Main polling loop (runs in a dedicated thread with its own runtime).
/// `session_key` is the user_id used as the DashMap key in WEIXIN_STATE.bots.
fn run_poll_loop(session_key: &str, sender: mpsc::Sender<PluginMessage>, shutdown: &AtomicBool) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!(session_key = session_key, "Failed to create tokio runtime: {e}");
            return;
        }
    };

    rt.block_on(async move {
        poll_loop_inner(session_key, sender, shutdown).await;
    });
}

async fn poll_loop_inner(
    session_key: &str,
    sender: mpsc::Sender<PluginMessage>,
    shutdown: &AtomicBool,
) {
    info!(session_key = session_key, "Poll loop started");

    loop {
        if shutdown.load(Ordering::Relaxed) {
            info!(
                session_key = session_key,
                "Shutdown signal received, exiting poll loop"
            );
            return;
        }

        let (bot_token, baseurl, http, bot_id) = {
            let state = match WEIXIN_STATE.bots.get(session_key) {
                Some(s) => s,
                None => {
                    for _ in 0..10 {
                        if shutdown.load(Ordering::Relaxed) {
                            info!(session_key = session_key, "Shutdown during wait, exiting");
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                    continue;
                }
            };

            if !state.active.load(Ordering::Relaxed) || state.is_expired() {
                for _ in 0..10 {
                    if shutdown.load(Ordering::Relaxed) {
                        info!(
                            session_key = session_key,
                            "Shutdown during inactive wait, exiting"
                        );
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                continue;
            }

            (
                state.bot_token.clone(),
                state.baseurl.clone(),
                state.http.clone(),
                state.bot_id.clone(),
            )
        };

        let cursor = WEIXIN_STATE
            .bots
            .get(session_key)
            .map(|s| s.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone())
            .unwrap_or_default();

        match api::get_updates(&http, &bot_token, &baseurl, &cursor).await {
            Ok(resp) => {
                if resp.errcode == Some(SESSION_EXPIRED_ERRCODE)
                    || resp.ret == Some(SESSION_EXPIRED_ERRCODE)
                {
                    warn!(session_key = session_key, "Session expired, stopping poll");
                    if let Some(state) = WEIXIN_STATE.bots.get(session_key) {
                        state.active.store(false, Ordering::Relaxed);
                        state.expires_at.store(0, Ordering::Relaxed);
                    }
                    continue;
                }

                if let Some(ret) = resp.ret {
                    if ret != 0 {
                        warn!(
                            session_key = session_key,
                            ret, "getUpdates returned non-zero ret"
                        );
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        continue;
                    }
                }

                if let Some(new_cursor) = &resp.get_updates_buf {
                    if !new_cursor.is_empty() {
                        if let Some(state) = WEIXIN_STATE.bots.get(session_key) {
                            *state.cursor.lock().unwrap_or_else(|e| e.into_inner()) = new_cursor.clone();
                        }
                    }
                }

                if let Some(msgs) = resp.msgs {
                    // Log raw item types for debugging
                    for msg in &msgs {
                        if let Some(items) = &msg.item_list {
                            for item in items {
                                info!(session_key = session_key, item_type = item.type_.unwrap_or(-1i32 as u32), has_file = item.file_item.is_some(), has_image = item.image_item.is_some(), has_text = item.text_item.is_some(), "Raw WeChat item");
                            }
                        } else {
                            info!(session_key = session_key, "WeChat message with no item_list");
                        }
                    }
                    // Renew session expiry on every successful getUpdates
                    if let Some(state) = WEIXIN_STATE.bots.get(session_key) {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        state.expires_at.store(
                            now + SESSION_DURATION_SECS,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                        state.active.store(true, std::sync::atomic::Ordering::Relaxed);
                        WEIXIN_STATE.save_session(&state);
                    }
                    for msg in msgs {
                        process_inbound_message(&bot_id, session_key, &msg, &sender, &http).await;
                    }
                } else {
                    // No messages but successful poll — still renew to keep session alive
                    if let Some(state) = WEIXIN_STATE.bots.get(session_key) {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        state.expires_at.store(
                            now + SESSION_DURATION_SECS,
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                }
            }
            Err(e) => {
                error!(session_key = session_key, "getUpdates error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}

/// Download a CDN media file, AES-decrypt it, and return as data URI.
async fn download_cdn_as_data_uri(
    http: &reqwest::Client,
    media: &CDNMedia,
) -> Result<String, String> {
    let eqp = media.encrypt_query_param.as_deref().ok_or("No encrypt_query_param")?;
    let aes_key_b64 = media.aes_key.as_deref().ok_or("No aes_key")?;
    let key = crypto::parse_aes_key(aes_key_b64).ok_or("Invalid AES key")?;

    let url = crypto::cdn_download_url(eqp);
    let data = crypto::cdn_download(http, &url, &key).await?;

    let mime = types::media::detect_image_mime(&data);

    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// Download a CDN media file, AES-decrypt it, and return raw bytes.
async fn download_cdn_raw(
    http: &reqwest::Client,
    media: &CDNMedia,
) -> Result<Vec<u8>, String> {
    let eqp = media.encrypt_query_param.as_deref().ok_or("No encrypt_query_param")?;
    let aes_key_b64 = media.aes_key.as_deref().ok_or("No aes_key")?;
    let key = crypto::parse_aes_key(aes_key_b64).ok_or("Invalid AES key")?;

    let url = crypto::cdn_download_url(eqp);
    crypto::cdn_download(http, &url, &key).await
}

async fn process_inbound_message(
    bot_id: &str,
    session_key: &str,
    msg: &ILnkMessage,
    sender: &mpsc::Sender<PluginMessage>,
    http: &reqwest::Client,
) {
    if msg.message_type != Some(MSG_TYPE_USER) {
        return;
    }
    if msg.message_state != Some(MSG_STATE_FINISH) {
        return;
    }

    let from_user_id = match &msg.from_user_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => return,
    };

    if let Some(ctx_token) = &msg.context_token {
        if let Some(state) = WEIXIN_STATE.bots.get(session_key) {
            state.store_context_token(&from_user_id, ctx_token);
        }
    }

    // Build content from the first item in item_list
    let content = match msg.item_list.as_ref() {
        Some(items) if !items.is_empty() => {
            let item = &items[0];
            let item_type = item.type_.unwrap_or(0);
            match item_type {
                ITEM_TYPE_TEXT => {
                    let text = item
                        .text_item
                        .as_ref()
                        .and_then(|t| t.text.clone())
                        .unwrap_or_default();
                    PluginContent::Text(text)
                }
                ITEM_TYPE_IMAGE => {
                    let image_url = match item.image_item.as_ref().and_then(|i| i.media.as_ref()) {
                        Some(media) => {
                            match download_cdn_as_data_uri(http, media).await {
                                Ok(uri) => uri,
                                Err(e) => {
                                    warn!(error = %e, "Failed to download WeChat image from CDN");
                                    String::new()
                                }
                            }
                        }
                        None => String::new(),
                    };
                    PluginContent::Image { url: image_url, caption: None }
                }
                ITEM_TYPE_VOICE => {
                    // If voice has text transcription, use it directly
                    if let Some(text) = item.voice_item.as_ref().and_then(|v| v.text.clone()) {
                        if !text.is_empty() {
                            PluginContent::Text(text)
                        } else {
                            PluginContent::Voice { url: String::new(), duration_seconds: 0 }
                        }
                    } else {
                        PluginContent::Voice { url: String::new(), duration_seconds: 0 }
                    }
                }
                ITEM_TYPE_FILE => {
                    let file_item = item.file_item.as_ref();
                    let filename = file_item
                        .and_then(|f| f.file_name.clone())
                        .unwrap_or_default();
                    info!(filename = %filename, has_media = file_item.and_then(|f| f.media.as_ref()).is_some(), "WeChat file message received");
                    let data = match file_item.and_then(|f| f.media.as_ref()) {
                        Some(media) => {
                            match download_cdn_raw(http, media).await {
                                Ok(bytes) => {
                                    info!(filename = %filename, size = bytes.len(), "WeChat file downloaded from CDN");
                                    Some(bytes)
                                }
                                Err(e) => {
                                    warn!(filename = %filename, error = %e, "Failed to download WeChat file from CDN");
                                    None
                                }
                            }
                        }
                        None => None,
                    };
                    PluginContent::File { url: String::new(), filename, data }
                }
                ITEM_TYPE_VIDEO => {
                    PluginContent::Video {
                        url: String::new(),
                        duration_seconds: item.video_item.as_ref().and_then(|v| v.play_length).map(|d| d as u32),
                        caption: None,
                    }
                }
                _ => {
                    warn!(item_type, "Unknown WeChat item type, treating as empty text");
                    PluginContent::Text(String::new())
                }
            }
        }
        _ => PluginContent::Text(String::new()),
    };

    info!(
        bot_id = bot_id,
        from = %from_user_id,
        item_type = match msg.item_list.as_ref() {
            Some(items) if !items.is_empty() => items[0].type_.unwrap_or(0),
            _ => 0,
        },
        "Inbound WeChat message"
    );

    let plugin_msg = PluginMessage {
        channel_type: "weixin".to_string(),
        platform_message_id: msg.message_id.map(|id| id.to_string()).unwrap_or_default(),
        sender_id: from_user_id.clone(),
        sender_name: from_user_id.clone(),
        bot_id: bot_id.to_string(),
        content,
        timestamp_ms: msg.create_time_ms.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64
        }),
        is_group: msg.group_id.is_some(),
        thread_id: msg.group_id.clone(),
        metadata: Default::default(),
    };

    if let Err(e) = sender.try_send(plugin_msg) {
        warn!(error = %e, "Plugin message channel full, dropping message");
    }
}

// ---------------------------------------------------------------------------
// SessionWatcher — monitors for new bots added after plugin startup
// ---------------------------------------------------------------------------

/// Dynamic session watcher that starts poll threads for bots and handles
/// respawn of inactive-but-valid sessions. New bots are started via
/// `start_sender()` (event-driven), not polling.
pub struct SessionWatcher {
    shutdown: Arc<AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl Default for SessionWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionWatcher {
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
        }
    }
}

impl Channel for SessionWatcher {
    fn channel_type(&self) -> &str {
        "weixin"
    }

    fn supports_proactive_push(&self) -> bool {
        // iLink replies require a context_token from a recent inbound message.
        false
    }

    fn name(&self) -> &str {
        "__watcher__"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        // Initial load + spawn all discovered bots
        WEIXIN_STATE.load_from_dir();
        spawn_all_bots(&sender);

        // Start respawn watcher (handles reconnection of inactive-but-valid bots)
        let shutdown = self.shutdown.clone();
        let handle = std::thread::Builder::new()
            .name("weixin-respawn-watcher".to_string())
            .spawn(move || {
                respawn_watcher_loop(sender, shutdown);
            })
            .map_err(|e| ChannelError::Other(format!("Failed to spawn respawn watcher thread: {e}")))?;
        self.thread_handle = Some(handle);
        info!("WeChat SessionWatcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError> {
        let state = WEIXIN_STATE
            .get_session_for_send(bot_id, user_id)
            .ok_or_else(|| ChannelError::UnknownBot(format!("No session for bot {bot_id}, user {user_id}")))?;

        if state.is_expired() {
            return Err(ChannelError::TokenFailed(format!("Token expired for bot {bot_id}")));
        }

        let context_token = state.get_context_token(user_id).ok_or_else(|| {
            ChannelError::NotSupported(format!("No context_token for user {user_id} — can only reply to received messages"))
        })?;

        let client_id = format!("openclaw-weixin-{}", Uuid::new_v4().as_simple());
        let bot_token = state.bot_token.clone();
        let baseurl = state.baseurl.clone();
        let http = state.http.clone();
        let user_id = user_id.to_string();
        let context_token = context_token.to_string();
        let text = text.to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(ChannelError::Other(format!("Failed to create send runtime: {e}"))));
                    return;
                }
            };
            let result = rt.block_on(async {
                api::send_message(
                    &http,
                    &bot_token,
                    &baseurl,
                    &user_id,
                    &context_token,
                    &client_id,
                    &text,
                )
                .await
                .map_err(ChannelError::SendFailed)
            });
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| ChannelError::Other(format!("Send thread disconnected: {e}")))?
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            match handle.join() {
                Ok(()) => info!("SessionWatcher thread joined cleanly"),
                Err(e) => error!("SessionWatcher thread panicked: {e:?}"),
            }
        }
        info!("SessionWatcher stopped");
    }

    fn start_sender(&self, sender_id: &str, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        WEIXIN_STATE.load_new_from_dir();
        spawn_bot_by_id(sender_id, &sender);
        info!(sender_id = %sender_id, "WeChat: started new sender");
        Ok(())
    }
}

/// Spawn poll threads for all bots that are loaded but not yet active.
fn spawn_all_bots(sender: &mpsc::Sender<PluginMessage>) {
    for entry in WEIXIN_STATE.bots.iter() {
        let user_id = entry.key().clone();
        let state = entry.value();
        if state.active.load(Ordering::Relaxed) || state.is_expired() {
            continue;
        }
        state.active.store(true, Ordering::Relaxed);
        let s = sender.clone();
        let poll_key = user_id.clone();
        info!(user_id = %user_id, "Spawning poll thread for bot");
        if let Err(e) = std::thread::Builder::new()
            .name(format!("weixin-dyn-{user_id}"))
            .spawn(move || {
                let shutdown = Arc::new(AtomicBool::new(false));
                run_poll_loop(&poll_key, s, &shutdown);
            })
        {
            error!(user_id = %user_id, "Failed to spawn poll thread: {e}");
        }
    }
}

/// Spawn a specific bot by user_id (if loaded and not yet active).
fn spawn_bot_by_id(sender_id: &str, sender: &mpsc::Sender<PluginMessage>) {
    if let Some(state) = WEIXIN_STATE.bots.get(sender_id) {
        if state.active.load(Ordering::Relaxed) || state.is_expired() {
            return;
        }
        state.active.store(true, Ordering::Relaxed);
        let s = sender.clone();
        let poll_key = sender_id.to_string();
        info!(user_id = %sender_id, "Spawning poll thread for new sender");
        if let Err(e) = std::thread::Builder::new()
            .name(format!("weixin-dyn-{sender_id}"))
            .spawn(move || {
                let shutdown = Arc::new(AtomicBool::new(false));
                run_poll_loop(&poll_key, s, &shutdown);
            })
        {
            error!(user_id = %sender_id, "Failed to spawn poll thread: {e}");
        }
    }
}

/// Background loop that respawns poll threads for inactive-but-valid bots.
/// This handles the case where a bot's poll loop exits (e.g. session expired
/// in iLink) but the session file has been refreshed with a new token.
fn respawn_watcher_loop(sender: mpsc::Sender<PluginMessage>, shutdown: Arc<AtomicBool>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("Failed to create respawn watcher tokio runtime: {e}");
            return;
        }
    };

    rt.block_on(async move {
        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("Respawn watcher shutdown signal received");
                return;
            }

            for entry in WEIXIN_STATE.bots.iter() {
                let user_id = entry.key().clone();
                let state = entry.value();
                // Respawn: inactive but not expired (session refreshed with new token)
                if !state.active.load(Ordering::Relaxed) && !state.is_expired() {
                    state.active.store(true, Ordering::Relaxed);
                    let s = sender.clone();
                    let poll_key = user_id.clone();
                    info!(user_id = %user_id, "Respawning poll thread for inactive bot");
                    if let Err(e) = std::thread::Builder::new()
                        .name(format!("weixin-dyn-{user_id}"))
                        .spawn(move || {
                            let shutdown = Arc::new(AtomicBool::new(false));
                            run_poll_loop(&poll_key, s, &shutdown);
                        })
                    {
                        error!(user_id = %user_id, "Failed to respawn poll thread: {e}");
                    }
                }
            }

            for _ in 0..10 {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    });
}
