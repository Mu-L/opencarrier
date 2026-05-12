//! DingTalk channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/dingtalk-sessions/*.json`,
//! spawns per-bot WebSocket connections, and handles message dispatch.

pub mod api;
pub mod channel;
pub mod token;
pub mod types;
pub mod ws;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use carrier_types::channel::Channel;
use carrier_types::plugin::PluginMessage;
use dashmap::DashMap;
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Runtime bot entry
// ---------------------------------------------------------------------------

/// Runtime entry stored in DINGTALK_STATE — config + pre-built token cache.
pub struct DingTalkBotEntry {
    pub config: types::DingTalkBotConfig,
    pub token_cache: Arc<token::AccessTokenCache>,
    pub active: AtomicBool,
}

impl DingTalkBotEntry {
    pub fn new(config: types::DingTalkBotConfig) -> Self {
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
/// Discovers bots by scanning `~/.opencarrier/dingtalk-sessions/*.json`.
pub struct DingTalkState {
    pub bots: DashMap<String, DingTalkBotEntry>, // key: app_key
    pub session_dir: std::path::PathBuf,
    pub http: Client,
}

impl DingTalkState {
    fn new() -> Self {
        let home = carrier_types::config::home_dir();
        let session_dir = home.join("dingtalk-sessions");
        Self {
            bots: DashMap::new(),
            session_dir,
            http: Client::new(),
        }
    }

    /// Resolve the effective app_secret: try env var first, fall back to inline value.
    fn resolve_secret(sf: &types::DingTalkSessionFile) -> String {
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
    fn build_entry(sf: &types::DingTalkSessionFile) -> Option<DingTalkBotEntry> {
        let app_key = sf.app_key.clone();
        let app_secret = Self::resolve_secret(sf);
        if app_key.is_empty() || app_secret.is_empty() {
            warn!(name = %sf.name, "Skipping DingTalk session: missing app_key or app_secret");
            return None;
        }
        let cfg = types::DingTalkBotConfig {
            name: sf.name.clone(),
            bot_uuid: String::new(), // no longer used
            app_key,
            app_secret,
        };
        Some(DingTalkBotEntry::new(cfg))
    }

    /// Load all sessions from the session directory (initial load at startup).
    pub fn load_from_dir(&self) {
        if !self.session_dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&self.session_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %self.session_dir.display(), "Failed to read session directory: {e}");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %path.display(), "Failed to read session file: {e}");
                    continue;
                }
            };
            let sf = match serde_json::from_str::<types::DingTalkSessionFile>(&content) {
                Ok(s) => s,
                Err(e) => {
                    warn!(path = %path.display(), "Failed to parse session file: {e}");
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

    /// Load new sessions (skips already-loaded bots). Called by watcher loop.
    pub fn load_new_from_dir(&self) {
        if !self.session_dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&self.session_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let sf = match serde_json::from_str::<types::DingTalkSessionFile>(&content) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if sf.app_key.is_empty() {
                continue;
            }
            if let Some(mut existing) = self.bots.get_mut(&sf.app_key) {
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
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, app_key = %sf.app_key, "Dynamic watcher loaded new DingTalk session");
            self.bots.insert(sf.app_key.clone(), entry);
        }
    }

    /// Save a session file to disk.
    pub fn save_session(&self, sf: &types::DingTalkSessionFile) {
        if let Err(e) = std::fs::create_dir_all(&self.session_dir) {
            warn!(dir = %self.session_dir.display(), "Failed to create session directory: {e}");
            return;
        }
        let path = self.session_dir.join(format!("{}.json", sf.app_key));
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
/// Scans `~/.opencarrier/dingtalk-sessions/*.json` every 5 seconds.
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

    fn name(&self) -> &str {
        "DingTalk Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        let shutdown = self.shutdown.clone();

        // Initial load
        DINGTALK_STATE.load_from_dir();

        // Spawn watcher loop
        std::thread::Builder::new()
            .name("dingtalk-watcher".to_string())
            .spawn(move || {
                let mut spawned: HashSet<String> = HashSet::new();
                spawn_new_bots(&sender, &mut spawned);

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        info!("DingTalk session watcher shutting down");
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    DINGTALK_STATE.load_new_from_dir();
                    spawn_new_bots(&sender, &mut spawned);
                }
            })
            .map_err(|e| format!("Failed to spawn DingTalk watcher thread: {e}"))?;

        info!("DingTalk session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        let entry = DINGTALK_STATE
            .get_session(bot_id)
            .ok_or_else(|| format!("Unknown DingTalk bot: {bot_id}"))?;

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
                let robot_code = token_cache.app_key().to_string();

                api::send_direct_message(&http, &token, &robot_code, &user_id, &text).await
            });
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| format!("Send thread disconnected: {e}"))?
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

/// Spawn channel threads for newly discovered bots.
fn spawn_new_bots(sender: &mpsc::Sender<PluginMessage>, spawned: &mut HashSet<String>) {
    for entry in DINGTALK_STATE.bots.iter() {
        let app_key = entry.key().clone();
        if spawned.contains(&app_key) {
            continue;
        }

        let session = entry.value();
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

        spawned.insert(app_key);
    }
}
