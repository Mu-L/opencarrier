//! Feishu/Lark channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/senders/{app_id}/session.json`,
//! spawns per-bot WebSocket connections, and handles message dispatch.
//! New bots are started via `start_sender()` (event-driven), not polling.

pub mod api;
pub mod channel;
pub mod pbbp2;
pub mod token;
pub mod models;
pub mod ws;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use types::channel::Channel;
use types::plugin::PluginMessage;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Runtime bot entry
// ---------------------------------------------------------------------------

/// Runtime entry stored in FEISHU_STATE — config + pre-built token cache.
pub struct FeishuBotEntry {
    pub config: models::FeishuBotConfig,
    pub token_cache: Arc<token::BotTokenCache>,
    pub active: AtomicBool,
}

impl FeishuBotEntry {
    pub fn new(config: models::FeishuBotConfig) -> Self {
        let api_base = config.api_base().to_string();
        let token_cache = Arc::new(token::BotTokenCache::new(
            config.app_id.clone(),
            config.app_secret.clone(),
            &api_base,
        ));
        Self {
            config,
            token_cache,
            active: AtomicBool::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// FeishuState — global state manager
// ---------------------------------------------------------------------------

/// Global state manager for all Feishu bots.
///
/// Discovers bots by scanning `~/.opencarrier/senders/{app_id}/session.json`.
pub struct FeishuState {
    pub bots: DashMap<String, FeishuBotEntry>, // key: app_id
}

impl FeishuState {
    fn new() -> Self {
        Self {
            bots: DashMap::new(),
        }
    }

    /// Resolve the effective app_secret: try env var first, fall back to inline value.
    fn resolve_secret(sf: &models::FeishuSessionFile) -> String {
        if let Some(ref env_name) = sf.secret_env {
            if let Ok(s) = std::env::var(env_name) {
                if !s.is_empty() {
                    return s;
                }
            }
        }
        sf.app_secret.clone().unwrap_or_default()
    }

    /// Build a FeishuBotEntry from a session file.
    fn build_entry(sf: &models::FeishuSessionFile) -> Option<FeishuBotEntry> {
        let app_id = sf.app_id.clone();
        let app_secret = Self::resolve_secret(sf);
        if app_id.is_empty() || app_secret.is_empty() {
            warn!(name = %sf.name, "Skipping Feishu session: missing app_id or app_secret");
            return None;
        }
        let cfg = models::FeishuBotConfig {
            name: sf.name.clone(),
            app_id,
            app_secret,
            brand: sf.brand.clone(),
        };
        Some(FeishuBotEntry::new(cfg))
    }

    /// Load all sessions from senders/*/session.json (initial load at startup).
    /// Only loads files where channel == "feishu".
    pub fn load_from_dir(&self) {
        let home = types::config::home_dir();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("feishu") {
                continue;
            }
            let sf: models::FeishuSessionFile = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(e) => {
                    warn!(sender_id = %sender_id, "Failed to parse feishu session: {e}");
                    continue;
                }
            };
            if sf.app_id.is_empty() {
                continue;
            }
            if self.bots.contains_key(&sf.app_id) {
                continue;
            }
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, app_id = %sf.app_id, "Loaded Feishu session");
            self.bots.insert(sf.app_id.clone(), entry);
        }
    }

    /// Load new sessions from senders/*/session.json (skips already-loaded).
    /// Only loads files where channel == "feishu".
    pub fn load_new_from_dir(&self) {
        let home = types::config::home_dir();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("feishu") {
                continue;
            }
            // Refresh existing bot if session file changed
            if let Some(mut existing) = self.bots.get_mut(&sender_id) {
                let sf: models::FeishuSessionFile = match serde_json::from_value(json) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let new_entry = match Self::build_entry(&sf) {
                    Some(e) => e,
                    None => continue,
                };
                if existing.config.app_secret != new_entry.config.app_secret {
                    info!(app_id = %sf.app_id, "Refreshing Feishu session from updated file");
                    *existing = new_entry;
                }
                continue;
            }
            let sf: models::FeishuSessionFile = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if sf.app_id.is_empty() {
                continue;
            }
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, app_id = %sf.app_id, "Dynamic watcher loaded new Feishu session");
            self.bots.insert(sf.app_id.clone(), entry);
        }
    }

    /// Save a session file to senders/{app_id}/session.json.
    pub fn save_session(&self, sf: &models::FeishuSessionFile) {
        let sender_id = &sf.app_id;
        if sender_id.is_empty() {
            warn!("Cannot save feishu session with empty app_id");
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

    /// Get a bot session by app_id.
    pub fn get_session(
        &self,
        app_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, FeishuBotEntry>> {
        self.bots.get(app_id)
    }

    /// Get status of all bots for the API.
    pub fn status_list(&self) -> Vec<serde_json::Value> {
        self.bots
            .iter()
            .map(|entry| {
                let s = entry.value();
                serde_json::json!({
                    "name": s.config.name,
                    "app_id": s.config.app_id,
                    "brand": s.config.brand,
                    "active": s.active.load(Ordering::Relaxed),
                })
            })
            .collect()
    }
}

/// Global singleton for Feishu state management.
pub static FEISHU_STATE: std::sync::LazyLock<FeishuState> =
    std::sync::LazyLock::new(FeishuState::new);

// ---------------------------------------------------------------------------
// SessionWatcher — unified watcher for all Feishu bots
// ---------------------------------------------------------------------------

/// Watcher that discovers Feishu bots from session files and spawns WS connections.
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
        "feishu"
    }

    fn name(&self) -> &str {
        "Feishu Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        // Initial load + spawn all discovered bots
        FEISHU_STATE.load_from_dir();
        spawn_inactive_bots(&sender);
        info!("Feishu session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        let entry = FEISHU_STATE
            .get_session(bot_id)
            .ok_or_else(|| format!("Unknown Feishu bot: {bot_id}"))?;

        let content = serde_json::json!({ "text": text }).to_string();
        let token_cache = entry.token_cache.clone();
        let user_id = user_id.to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("Runtime creation failed: {e}")));
                    return;
                }
            };
            let result = rt.block_on(async {
                let token = token_cache
                    .get_token()
                    .await
                    .map_err(|e| format!("Token error: {e}"))?;
                let http = token_cache.http().clone();
                let base = token_cache.api_base().to_string();
                let resp =
                    api::send_message(&http, &token, &base, &user_id, "open_id", "text", &content)
                        .await?;

                if resp.code != 0 {
                    return Err(format!(
                        "Feishu send error: code={} msg={}",
                        resp.code, resp.msg
                    ));
                }
                Ok(())
            });
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| format!("Send thread disconnected: {e}"))?
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn start_sender(&self, sender_id: &str, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        FEISHU_STATE.load_new_from_dir();
        spawn_bot_by_id(sender_id, &sender);
        info!(sender_id = %sender_id, "Feishu: started new sender");
        Ok(())
    }
}

/// Spawn channel threads for all bots that are loaded but not yet active.
fn spawn_inactive_bots(sender: &mpsc::Sender<PluginMessage>) {
    for entry in FEISHU_STATE.bots.iter() {
        let app_id = entry.key().clone();
        let session = entry.value();
        if session.active.load(Ordering::Relaxed) {
            continue;
        }

        let bot_name = session.config.name.clone();
        let token_cache = session.token_cache.clone();
        session.active.store(true, Ordering::Relaxed);

        let tx = sender.clone();
        let app_id_for_ws = app_id.clone();
        std::thread::spawn(move || {
            let mut ch = channel::FeishuChannel::new(bot_name.clone(), app_id_for_ws, token_cache);
            if let Err(e) = ch.start(tx) {
                warn!(bot = %bot_name, "Feishu channel start error: {e}");
            }
        });
    }
}

/// Spawn a specific bot by app_id (if loaded and not yet active).
fn spawn_bot_by_id(sender_id: &str, sender: &mpsc::Sender<PluginMessage>) {
    if let Some(session) = FEISHU_STATE.bots.get(sender_id) {
        if session.active.load(Ordering::Relaxed) {
            return;
        }
        let bot_name = session.config.name.clone();
        let token_cache = session.token_cache.clone();
        session.active.store(true, Ordering::Relaxed);

        let tx = sender.clone();
        let app_id_for_ws = sender_id.to_string();
        std::thread::spawn(move || {
            let mut ch = channel::FeishuChannel::new(bot_name.clone(), app_id_for_ws, token_cache);
            if let Err(e) = ch.start(tx) {
                warn!(bot = %bot_name, "Feishu channel start error: {e}");
            }
        });
    }
}
