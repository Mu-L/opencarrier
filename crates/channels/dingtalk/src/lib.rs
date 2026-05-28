//! DingTalk channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/senders/{app_key}/session.json`,
//! spawns per-bot WebSocket connections, and handles message dispatch.
//! New bots are started via `start_sender()` (event-driven), not polling.

pub mod api;
pub mod channel;
pub mod token;
pub mod models;
pub mod ws;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use types::channel::{Channel, ChannelError};
use types::plugin::PluginMessage;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Runtime bot entry
// ---------------------------------------------------------------------------

/// Runtime entry stored in DINGTALK_STATE — config + pre-built token cache.
pub struct DingTalkBotEntry {
    pub config: models::DingTalkBotConfig,
    pub token_cache: Arc<token::AccessTokenCache>,
    pub active: AtomicBool,
}

impl DingTalkBotEntry {
    pub fn new(config: models::DingTalkBotConfig) -> Self {
        let token_cache = Arc::new(token::AccessTokenCache::new(
            config.app_key.clone(),
            config.app_secret.clone(),
        ));
        Self {
            config,
            token_cache,
            active: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// DingTalkState — global state manager
// ---------------------------------------------------------------------------

/// Global state manager for all DingTalk bots.
///
/// Discovers bots by scanning `~/.opencarrier/senders/{app_key}/session.json`.
pub struct DingTalkState {
    pub bots: DashMap<String, DingTalkBotEntry>, // key: app_key
}

impl DingTalkState {
    fn new() -> Self {
        Self {
            bots: DashMap::new(),
        }
    }

    /// Resolve the effective app_secret: try env var first, fall back to inline value.
    fn resolve_secret(sf: &models::DingTalkSessionFile) -> String {
        if let Some(ref env_name) = sf.secret_env {
            if let Ok(s) = std::env::var(env_name) {
                if !s.is_empty() {
                    return s;
                }
            }
        }
        sf.app_secret.clone().unwrap_or_default()
    }

    /// Build a DingTalkBotEntry from a session file.
    fn build_entry(sf: &models::DingTalkSessionFile) -> Option<DingTalkBotEntry> {
        let app_key = sf.app_key.clone();
        let app_secret = Self::resolve_secret(sf);
        if app_key.is_empty() || app_secret.is_empty() {
            warn!(name = %sf.name, "Skipping DingTalk session: missing app_key or app_secret");
            return None;
        }
        let cfg = models::DingTalkBotConfig {
            name: sf.name.clone(),
            app_key,
            app_secret,
        };
        Some(DingTalkBotEntry::new(cfg))
    }

    /// Load all sessions from senders/*/session.json (initial load at startup).
    /// Only loads files where channel == "dingtalk".
    pub fn load_from_dir(&self) {
        let home = types::config::home_dir();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("dingtalk") {
                continue;
            }
            let sf: models::DingTalkSessionFile = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(e) => {
                    warn!(sender_id = %sender_id, "Failed to parse dingtalk session: {e}");
                    continue;
                }
            };
            if sf.app_key.is_empty() {
                continue;
            }
            if self.bots.contains_key(&sf.app_key) {
                continue;
            }
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, app_key = %sf.app_key, "Loaded DingTalk session");
            self.bots.insert(sf.app_key.clone(), entry);
        }
    }

    /// Load new sessions from senders/*/session.json (skips already-loaded).
    /// Only loads files where channel == "dingtalk".
    pub fn load_new_from_dir(&self) {
        let home = types::config::home_dir();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("dingtalk") {
                continue;
            }
            // Refresh existing bot if session file changed
            if let Some(mut existing) = self.bots.get_mut(&sender_id) {
                let sf: models::DingTalkSessionFile = match serde_json::from_value(json) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let new_entry = match Self::build_entry(&sf) {
                    Some(e) => e,
                    None => continue,
                };
                if existing.config.app_secret != new_entry.config.app_secret {
                    info!(app_key = %sf.app_key, "Refreshing DingTalk session from updated file");
                    *existing = new_entry;
                }
                continue;
            }
            let sf: models::DingTalkSessionFile = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if sf.app_key.is_empty() {
                continue;
            }
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, app_key = %sf.app_key, "Dynamic watcher loaded new DingTalk session");
            self.bots.insert(sf.app_key.clone(), entry);
        }
    }

    /// Save a session file to senders/{app_key}/session.json.
    pub fn save_session(&self, sf: &models::DingTalkSessionFile) {
        let sender_id = &sf.app_key;
        if sender_id.is_empty() {
            warn!("Cannot save dingtalk session with empty app_key");
            return;
        }
        let home = types::config::home_dir();
        let dir = home.join("senders").join(sender_id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), "Failed to create sender directory: {e}");
            return;
        }
        let path = dir.join("session.json");
        match serde_json::to_string_pretty(sf) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(path = %path.display(), "Failed to write session file: {e}");
                }
            }
            Err(e) => {
                warn!("Failed to serialize session file: {e}");
            }
        }
    }

    /// Get a bot session by app_key.
    pub fn get_session(
        &self,
        app_key: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, DingTalkBotEntry>> {
        self.bots.get(app_key)
    }

    /// Get status of all bots for the API.
    pub fn status_list(&self) -> Vec<serde_json::Value> {
        self.bots
            .iter()
            .map(|entry| {
                let s = entry.value();
                serde_json::json!({
                    "name": s.config.name,
                    "app_key": s.config.app_key,
                    "active": s.active.load(Ordering::Relaxed),
                })
            })
            .collect()
    }
}

/// Global singleton for DingTalk state management.
pub static DINGTALK_STATE: std::sync::LazyLock<DingTalkState> =
    std::sync::LazyLock::new(DingTalkState::new);

// ---------------------------------------------------------------------------
// SessionWatcher — unified watcher for all DingTalk bots
// ---------------------------------------------------------------------------

/// Watcher that discovers DingTalk bots from session files and spawns WS connections.
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
}

impl Default for SessionWatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Channel for SessionWatcher {
    fn channel_type(&self) -> &str {
        "dingtalk"
    }

    fn supports_proactive_push(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "DingTalk Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        // Initial load + spawn all discovered bots
        DINGTALK_STATE.load_from_dir();
        spawn_inactive_bots(&sender);
        info!("DingTalk session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError> {
        let entry = DINGTALK_STATE
            .get_session(bot_id)
            .ok_or_else(|| ChannelError::UnknownBot(bot_id.to_string()))?;

        let token_cache = entry.token_cache.clone();
        let user_id = user_id.to_string();
        let text = text.to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(ChannelError::Other(format!("Runtime creation failed: {e}"))));
                    return;
                }
            };
            let result = rt.block_on(async {
                let token = token_cache
                    .get_token()
                    .await
                    .map_err(|e| ChannelError::TokenFailed(e.to_string()))?;
                let http = token_cache.http().clone();
                let robot_code = token_cache.app_key().to_string();

                api::send_direct_message(&http, &token, &robot_code, &user_id, &text)
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
    }

    fn start_sender(&self, sender_id: &str, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        DINGTALK_STATE.load_new_from_dir();
        spawn_bot_by_id(sender_id, &sender);
        info!(sender_id = %sender_id, "DingTalk: started new sender");
        Ok(())
    }
}

/// Spawn channel threads for all bots that are loaded but not yet active.
fn spawn_inactive_bots(sender: &mpsc::Sender<PluginMessage>) {
    for entry in DINGTALK_STATE.bots.iter() {
        let app_key = entry.key().clone();
        let session = entry.value();
        if session.active.load(Ordering::Relaxed) {
            continue;
        }

        let bot_name = session.config.name.clone();
        let token_cache = session.token_cache.clone();
        session.active.store(true, Ordering::Relaxed);

        let tx = sender.clone();
        let app_key_for_ws = app_key.clone();
        std::thread::spawn(move || {
            let mut ch = channel::DingTalkChannel::new(bot_name.clone(), app_key_for_ws, token_cache);
            if let Err(e) = ch.start(tx) {
                warn!(bot = %bot_name, "DingTalk channel start error: {e}");
            }
        });
    }
}

/// Spawn a specific bot by app_key (if loaded and not yet active).
fn spawn_bot_by_id(sender_id: &str, sender: &mpsc::Sender<PluginMessage>) {
    if let Some(session) = DINGTALK_STATE.bots.get(sender_id) {
        if session.active.load(Ordering::Relaxed) {
            return;
        }
        let bot_name = session.config.name.clone();
        let token_cache = session.token_cache.clone();
        session.active.store(true, Ordering::Relaxed);

        let tx = sender.clone();
        let app_key_for_ws = sender_id.to_string();
        std::thread::spawn(move || {
            let mut ch = channel::DingTalkChannel::new(bot_name.clone(), app_key_for_ws, token_cache);
            if let Err(e) = ch.start(tx) {
                warn!(bot = %bot_name, "DingTalk channel start error: {e}");
            }
        });
    }
}
