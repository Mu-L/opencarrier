//! DingTalk channel adapter.
//!
//! `SessionWatcher` discovers bots from `~/.opencarrier/senders/{app_key}/session.json`,
//! spawns per-bot WebSocket connections, and handles message dispatch.
//! New bots are started via `start_sender()` (event-driven), not polling.

pub mod api;
pub mod channel;
pub mod models;
pub mod token;
pub mod ws;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{info, warn};
use types::channel::Channel;
use types::error::{CarrierError, CarrierResult};
use types::plugin::PluginMessage;

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
// DingTalkBot — ChannelBot marker + DingTalkState alias
// ---------------------------------------------------------------------------

impl channels_common::BotEntry for DingTalkBotEntry {
    fn name(&self) -> &str {
        &self.config.name
    }
    fn secret(&self) -> &str {
        &self.config.app_secret
    }
    fn active(&self) -> &AtomicBool {
        &self.active
    }
}

/// Zero-sized marker parameterizing `BotRegistry` for DingTalk.
pub struct DingTalkBot;

impl channels_common::ChannelBot for DingTalkBot {
    type Entry = DingTalkBotEntry;
    type Session = models::DingTalkSessionFile;
    const CHANNEL: &'static str = "dingtalk";
    const LABEL: &'static str = "DingTalk";

    fn key(sf: &models::DingTalkSessionFile) -> &str {
        &sf.app_key
    }

    fn build_entry(sf: &models::DingTalkSessionFile) -> Option<DingTalkBotEntry> {
        let app_key = sf.app_key.clone();
        let app_secret = channels_common::resolve_secret(&sf.secret_env, &sf.app_secret);
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

    fn status_extra(
        entry: &DingTalkBotEntry,
        out: &mut serde_json::Map<String, serde_json::Value>,
    ) {
        out.insert("app_key".to_string(), entry.config.app_key.clone().into());
    }
}

/// Global state manager for all DingTalk bots (generic registry over `DingTalkBot`).
///
/// Discovers bots by scanning `~/.opencarrier/senders/{app_key}/session.json`.
pub type DingTalkState = channels_common::BotRegistry<DingTalkBot>;

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

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> CarrierResult<()> {
        // Initial load + spawn all discovered bots
        DINGTALK_STATE.load_from_dir();
        spawn_inactive_bots(&sender);
        info!("DingTalk session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> CarrierResult<()> {
        let entry = DINGTALK_STATE
            .get_session(bot_id)
            .ok_or_else(|| CarrierError::InvalidInput(bot_id.to_string()))?;

        let token_cache = entry.token_cache.clone();
        let user_id = user_id.to_string();
        let text = text.to_string();

        types::channel::block_on_detached(async move {
            let token = token_cache.get_token().await?;
            let http = token_cache.http().clone();
            let robot_code = token_cache.app_key().to_string();

            api::send_direct_message(&http, &token, &robot_code, &user_id, &text).await
        })
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn start_sender(
        &self,
        sender_id: &str,
        sender: mpsc::Sender<PluginMessage>,
    ) -> CarrierResult<()> {
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
            let mut ch =
                channel::DingTalkChannel::new(bot_name.clone(), app_key_for_ws, token_cache);
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
            let mut ch =
                channel::DingTalkChannel::new(bot_name.clone(), app_key_for_ws, token_cache);
            if let Err(e) = ch.start(tx) {
                warn!(bot = %bot_name, "DingTalk channel start error: {e}");
            }
        });
    }
}
