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
use types::error::{CarrierError, CarrierResult};
use types::plugin::PluginMessage;
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
// FeishuBot — ChannelBot marker + FeishuState alias
// ---------------------------------------------------------------------------

impl channels_common::BotEntry for FeishuBotEntry {
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

/// Zero-sized marker parameterizing `BotRegistry` for Feishu.
pub struct FeishuBot;

impl channels_common::ChannelBot for FeishuBot {
    type Entry = FeishuBotEntry;
    type Session = models::FeishuSessionFile;
    const CHANNEL: &'static str = "feishu";
    const LABEL: &'static str = "Feishu";

    fn key(sf: &models::FeishuSessionFile) -> &str {
        &sf.app_id
    }

    fn build_entry(sf: &models::FeishuSessionFile) -> Option<FeishuBotEntry> {
        let app_id = sf.app_id.clone();
        let app_secret = channels_common::resolve_secret(&sf.secret_env, &sf.app_secret);
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

    fn status_extra(entry: &FeishuBotEntry, out: &mut serde_json::Map<String, serde_json::Value>) {
        out.insert("app_id".to_string(), entry.config.app_id.clone().into());
        out.insert("brand".to_string(), entry.config.brand.clone().into());
    }
}

/// Global state manager for all Feishu bots (generic registry over `FeishuBot`).
///
/// Discovers bots by scanning `~/.opencarrier/senders/{app_id}/session.json`.
pub type FeishuState = channels_common::BotRegistry<FeishuBot>;

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

    fn supports_proactive_push(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "Feishu Session Watcher"
    }

    fn bot_id(&self) -> &str {
        ""
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> CarrierResult<()> {
        // Initial load + spawn all discovered bots
        FEISHU_STATE.load_from_dir();
        spawn_inactive_bots(&sender);
        info!("Feishu session watcher started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> CarrierResult<()> {
        let entry = FEISHU_STATE
            .get_session(bot_id)
            .ok_or_else(|| CarrierError::InvalidInput(bot_id.to_string()))?;

        let content = serde_json::json!({ "text": text }).to_string();
        let token_cache = entry.token_cache.clone();
        let user_id = user_id.to_string();

        types::channel::block_on_detached(async move {
            let token = token_cache
                .get_token()
                .await?;
            let http = token_cache.http().clone();
            let base = token_cache.api_base().to_string();
            let resp =
                api::send_message(&http, &token, &base, &user_id, "open_id", "text", &content)
                    .await?;

            if resp.code != 0 {
                return Err(CarrierError::Network(format!(
                    "Feishu send error: code={} msg={}",
                    resp.code, resp.msg
                )));
            }
            Ok(())
        })
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    fn start_sender(&self, sender_id: &str, sender: mpsc::Sender<PluginMessage>) -> CarrierResult<()> {
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
