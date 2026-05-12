//! WeCom (Enterprise WeChat) channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/wecom-sessions/*.json`,
//! spawns per-bot connections (SmartBot WS, App/Kf webhook), and handles
//! message dispatch.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use carrier_types::channel::Channel;
use carrier_types::plugin::PluginMessage;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub mod channel;
pub mod crypto;
pub mod smartbot;
pub mod token;

// ---------------------------------------------------------------------------
// SessionWatcher — unified watcher for all WeCom modes
// ---------------------------------------------------------------------------

/// Watcher that discovers WeCom bots from session files and spawns connections.
///
/// Scans `~/.opencarrier/wecom-sessions/*.json` every 5 seconds.
/// Supports SmartBot (WS), App (webhook), and Kf (webhook) modes.
pub struct SessionWatcher {
    shutdown: Arc<AtomicBool>,
}

impl SessionWatcher {
    pub fn new() -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
        }
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

    fn name(&self) -> &str {
        "WeCom Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        let shutdown = self.shutdown.clone();

        // Initial load
        token::WECOM_STATE.load_from_dir();

        // Spawn watcher loop
        std::thread::spawn(move || {
            let mut spawned: HashSet<String> = HashSet::new();

            // Spawn initial bots
            spawn_new_bots(&sender, &mut spawned);

            loop {
                if shutdown.load(Ordering::Relaxed) {
                    info!("WeCom session watcher shutting down");
                    return;
                }
                std::thread::sleep(Duration::from_secs(5));
                token::WECOM_STATE.load_new_from_dir();
                spawn_new_bots(&sender, &mut spawned);
            }
        });

        info!("WeCom session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        let session = token::WECOM_STATE
            .get_session_for_send(bot_id)
            .ok_or_else(|| format!("Unknown WeCom bot: {bot_id}"))?;

        match &session.entry.mode {
            token::WecomMode::App { .. } => {
                token::send_app_message(&session.entry, user_id, text)?;
            }
            token::WecomMode::Kf { .. } => {
                token::send_kf_message(&session.entry, user_id, text)?;
            }
            token::WecomMode::SmartBot { .. } => {
                // SmartBot uses response_url mechanism
                let key = format!("{}:{}", bot_id, user_id);
                let response_url = smartbot::RESPONSE_URLS
                    .get()
                    .ok_or_else(|| "RESPONSE_URLS not initialized".to_string())?
                    .lock()
                    .unwrap()
                    .remove(&key)
                    .ok_or_else(|| {
                        "No response_url available. SmartBot can only reply within callback context.".to_string()
                    })?;

                let http = session.entry.http.clone();
                let text = text.to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build();
                    let result = match rt {
                        Ok(rt) => rt.block_on(token::send_smartbot_response_async(&http, &response_url, &text)),
                        Err(e) => Err(format!("Runtime creation failed: {e}")),
                    };
                    let _ = tx.send(result);
                });
                rx.recv()
                    .map_err(|e| format!("SmartBot send thread disconnected: {e}"))??;
            }
        }

        Ok(())
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Spawn channel threads for newly discovered bots.
fn spawn_new_bots(sender: &mpsc::Sender<PluginMessage>, spawned: &mut HashSet<String>) {
    for entry in token::WECOM_STATE.bots.iter() {
        let name = entry.key().clone();
        if spawned.contains(&name) {
            continue;
        }

        let session = entry.value();
        match &session.entry.mode {
            token::WecomMode::SmartBot { .. } => {
                let bot_name = session.entry.name.clone();
                let corp_id = session.entry.corp_id.clone();
                let bot_id = session.entry.bot_id().unwrap_or("").to_string();
                let secret = session.entry.bot_secret().unwrap_or("").to_string();

                let tx = sender.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for SmartBot");
                    rt.block_on(async {
                        let mut ch = smartbot::SmartBotChannel::new(
                            bot_name.clone(),
                            corp_id,
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
                let is_kf = session.entry.open_kfid().is_some();
                let bot_id = session.entry.name.clone();
                let corp_id = session.entry.corp_id.clone();
                let webhook_port = session.entry.webhook_port;
                let encoding_aes_key = session.entry.encoding_aes_key.clone();
                let callback_token = session.entry.callback_token.clone();

                let tx = sender.clone();
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("Failed to create tokio runtime for WeCom channel");
                    rt.block_on(async {
                        let mut ch = channel::WeComChannel::new(
                            bot_id.clone(),
                            corp_id,
                            webhook_port,
                            encoding_aes_key,
                            callback_token,
                            is_kf,
                        );
                        if let Err(e) = ch.start(tx) {
                            warn!(bot = %bot_id, "WeCom channel start error: {e}");
                        }
                    });
                });
            }
        }

        spawned.insert(name);
    }
}
