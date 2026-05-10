//! WeChat iLink session watcher — dynamic session discovery, polling, and send.

use crate::plugin::channels::weixin::api;
use crate::plugin::channels::weixin::token::WEIXIN_STATE;
use crate::plugin::channels::weixin::types::*;
use carrier_types::plugin::{PluginContent, PluginMessage};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::plugin::BuiltinChannel;

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
            .map(|s| s.cursor.lock().unwrap().clone())
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
                            *state.cursor.lock().unwrap() = new_cursor.clone();
                        }
                    }
                }

                if let Some(msgs) = resp.msgs {
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
                        process_inbound_message(&bot_id, session_key, &msg, &sender);
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

fn process_inbound_message(
    bot_id: &str,
    session_key: &str,
    msg: &ILnkMessage,
    sender: &mpsc::Sender<PluginMessage>,
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

    let text = msg
        .item_list
        .as_ref()
        .and_then(|items| {
            items.iter().find_map(|item| {
                if item.type_ == Some(ITEM_TYPE_TEXT) {
                    item.text_item.as_ref()?.text.clone()
                } else {
                    None
                }
            })
        })
        .unwrap_or_default();

    if let Some(ctx_token) = &msg.context_token {
        if let Some(state) = WEIXIN_STATE.bots.get(session_key) {
            state.store_context_token(&from_user_id, ctx_token);
        }
    }

    info!(
        bot_id = bot_id,
        from = %from_user_id,
        text_len = text.len(),
        "Inbound WeChat message"
    );

    let plugin_msg = PluginMessage {
        channel_type: "weixin".to_string(),
        platform_message_id: msg.message_id.map(|id| id.to_string()).unwrap_or_default(),
        sender_id: from_user_id.clone(),
        sender_name: from_user_id.clone(),
        bot_id: bot_id.to_string(),
        content: PluginContent::Text(text),
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

/// Dynamic session watcher that polls `WEIXIN_STATE` for new bots and
/// starts polling threads for them. Handles outbound `send()` for any bot.
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

impl BuiltinChannel for SessionWatcher {
    fn channel_type(&self) -> &str {
        "weixin"
    }

    fn name(&self) -> &str {
        "__watcher__"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        let shutdown = self.shutdown.clone();
        let handle = std::thread::Builder::new()
            .name("weixin-session-watcher".to_string())
            .spawn(move || {
                watcher_loop(sender, shutdown);
            })
            .map_err(|e| format!("Failed to spawn watcher thread: {e}"))?;
        self.thread_handle = Some(handle);
        info!("WeChat SessionWatcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        let state = WEIXIN_STATE
            .get_session_for_send(bot_id, user_id)
            .ok_or_else(|| format!("No session for bot {bot_id}, user {user_id}"))?;

        if state.is_expired() {
            return Err(format!("Token expired for bot {bot_id}"));
        }

        let context_token = state.get_context_token(user_id).ok_or_else(|| {
            format!("No context_token for user {user_id} — can only reply to received messages")
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
                    let _ = tx.send(Err(format!("Failed to create send runtime: {e}")));
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
            });
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| format!("Send thread disconnected: {e}"))?
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
}

fn watcher_loop(sender: mpsc::Sender<PluginMessage>, shutdown: Arc<AtomicBool>) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            error!("Failed to create watcher tokio runtime: {e}");
            return;
        }
    };

    rt.block_on(async move {
        let mut spawned: HashSet<String> = HashSet::new();

        loop {
            if shutdown.load(Ordering::Relaxed) {
                info!("SessionWatcher shutdown signal received");
                return;
            }

            WEIXIN_STATE.load_new_from_dir();

            for entry in WEIXIN_STATE.bots.iter() {
                let user_id = entry.key().clone();
                let state = entry.value();
                if spawned.contains(&user_id) {
                    // Poll thread exited (e.g. session expired) but token is now valid again
                    if !state.active.load(Ordering::Relaxed) && !state.is_expired() {
                        spawned.remove(&user_id);
                    } else {
                        continue;
                    }
                }
                if state.is_expired() {
                    continue;
                }
                if state.active.load(Ordering::Relaxed) {
                    spawned.insert(user_id);
                    continue;
                }
                state.active.store(true, Ordering::Relaxed);
                spawned.insert(user_id.clone());
                let s = sender.clone();
                let sh = shutdown.clone();
                let thread_name = user_id.clone();
                let poll_key = user_id.clone();
                info!(user_id = %user_id, "SessionWatcher spawning poll thread for new bot");
                if let Err(e) = std::thread::Builder::new()
                    .name(format!("weixin-dyn-{thread_name}"))
                    .spawn(move || {
                        run_poll_loop(&poll_key, s, &sh);
                    })
                {
                    error!(user_id = %user_id, "Failed to spawn poll thread: {e}");
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
