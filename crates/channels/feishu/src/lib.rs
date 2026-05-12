//! Feishu/Lark channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/feishu-sessions/*.json`,
//! spawns per-bot WebSocket connections, and handles message dispatch.

pub mod api;
pub mod api_ext;
pub mod channel;
pub mod pbbp2;
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

/// Runtime entry stored in FEISHU_STATE — config + pre-built token cache.
pub struct FeishuBotEntry {
    pub config: types::FeishuBotConfig,
    pub token_cache: Arc<token::BotTokenCache>,
    pub active: AtomicBool,
}

impl FeishuBotEntry {
    pub fn new(config: types::FeishuBotConfig) -> Self {
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
/// Discovers bots by scanning `~/.opencarrier/feishu-sessions/*.json`.
pub struct FeishuState {
    pub bots: DashMap<String, FeishuBotEntry>, // key: app_id
    pub session_dir: std::path::PathBuf,
    pub http: Client,
}

impl FeishuState {
    fn new() -> Self {
        let home = carrier_types::config::home_dir();
        let session_dir = home.join("feishu-sessions");
        Self {
            bots: DashMap::new(),
            session_dir,
            http: Client::new(),
        }
    }

    /// Resolve the effective app_secret: try env var first, fall back to inline value.
    fn resolve_secret(sf: &types::FeishuSessionFile) -> String {
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
    fn build_entry(sf: &types::FeishuSessionFile) -> Option<FeishuBotEntry> {
        let app_id = sf.app_id.clone();
        let app_secret = Self::resolve_secret(sf);
        if app_id.is_empty() || app_secret.is_empty() {
            warn!(name = %sf.name, "Skipping Feishu session: missing app_id or app_secret");
            return None;
        }
        let cfg = types::FeishuBotConfig {
            name: sf.name.clone(),
            bot_uuid: String::new(), // no longer used
            app_id,
            app_secret,
            brand: sf.brand.clone(),
        };
        Some(FeishuBotEntry::new(cfg))
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
            let sf = match serde_json::from_str::<types::FeishuSessionFile>(&content) {
                Ok(s) => s,
                Err(e) => {
                    warn!(path = %path.display(), "Failed to parse session file: {e}");
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
            let sf = match serde_json::from_str::<types::FeishuSessionFile>(&content) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if sf.app_id.is_empty() {
                continue;
            }
            if let Some(mut existing) = self.bots.get_mut(&sf.app_id) {
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
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, app_id = %sf.app_id, "Dynamic watcher loaded new Feishu session");
            self.bots.insert(sf.app_id.clone(), entry);
        }
    }

    /// Save a session file to disk.
    pub fn save_session(&self, sf: &types::FeishuSessionFile) {
        if let Err(e) = std::fs::create_dir_all(&self.session_dir) {
            warn!(dir = %self.session_dir.display(), "Failed to create session directory: {e}");
            return;
        }
        let path = self.session_dir.join(format!("{}.json", sf.app_id));
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
/// Scans `~/.opencarrier/feishu-sessions/*.json` every 5 seconds.
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
        let shutdown = self.shutdown.clone();

        // Initial load
        FEISHU_STATE.load_from_dir();

        // Spawn watcher loop
        std::thread::Builder::new()
            .name("feishu-watcher".to_string())
            .spawn(move || {
                let mut spawned: HashSet<String> = HashSet::new();
                spawn_new_bots(&sender, &mut spawned);

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        info!("Feishu session watcher shutting down");
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(5));
                    if shutdown.load(Ordering::Relaxed) {
                        return;
                    }
                    FEISHU_STATE.load_new_from_dir();
                    spawn_new_bots(&sender, &mut spawned);
                }
            })
            .map_err(|e| format!("Failed to spawn Feishu watcher thread: {e}"))?;

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
}

/// Spawn channel threads for newly discovered bots.
fn spawn_new_bots(sender: &mpsc::Sender<PluginMessage>, spawned: &mut HashSet<String>) {
    for entry in FEISHU_STATE.bots.iter() {
        let app_id = entry.key().clone();
        if spawned.contains(&app_id) {
            continue;
        }

        let session = entry.value();
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

        spawned.insert(app_id);
    }
}
