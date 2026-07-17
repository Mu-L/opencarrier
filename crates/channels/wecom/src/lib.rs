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
pub mod tools;

pub use tools::{
    WecomSendFileTool, WecomSendImageTool, WecomSendLinkTool, WecomSendMenuTool,
    WecomSendMiniprogramTool, WecomSendVideoTool, WecomSendVoiceTool,
};

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
