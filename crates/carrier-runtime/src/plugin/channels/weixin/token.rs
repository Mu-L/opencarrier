//! Token storage and management for the WeChat iLink Bot plugin.
//!
//! Manages per-bot bot_tokens (24h expiry) and per-user context_tokens.
//! Tokens are persisted to `~/.opencarrier/weixin-sessions/<bot_id>.json`.

use dashmap::DashMap;
use reqwest::Client;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::plugin::channels::weixin::types::*;

// ---------------------------------------------------------------------------
// Per-bot runtime state
// ---------------------------------------------------------------------------

/// Runtime state for a single iLink bot session (one scanned WeChat account).
pub struct BotSession {
    /// Bot ID (used as routing key).
    pub bot_id: String,
    /// iLink bot_token (from QR scan, valid 24h).
    pub bot_token: String,
    /// iLink base URL (from QR scan, usually same as ILINK_API_BASE).
    pub baseurl: String,
    /// The bot's iLink ID (e.g. "xxx@im.bot").
    pub ilink_bot_id: String,
    /// The WeChat user ID who scanned the QR code.
    pub user_id: Option<String>,
    /// Unix timestamp (seconds) when this token expires.
    pub expires_at: AtomicI64,
    /// Shared HTTP client.
    pub http: Client,
    /// Per-user context_token cache: user_id → context_token.
    context_tokens: Mutex<HashMap<String, String>>,
    /// Per-user typing_ticket cache: user_id → (ticket, cached_at).
    typing_tickets: Mutex<HashMap<String, (String, Instant)>>,
    /// get_updates_buf cursor for long-polling.
    pub cursor: Mutex<String>,
    /// Whether the polling loop is active.
    pub active: AtomicBool,
    /// Optional agent name to bind this channel to.
    pub bind_agent: Option<String>,
}

impl BotSession {
    /// Check if this bot's token has expired.
    pub fn is_expired(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        now >= self.expires_at.load(Ordering::Relaxed)
    }

    /// Check if this bot's token will expire within the given number of seconds.
    pub fn is_near_expiry(&self, within_secs: i64) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        now >= self.expires_at.load(Ordering::Relaxed) - within_secs
    }

    /// Seconds remaining until expiry.
    pub fn remaining_secs(&self) -> i64 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        (self.expires_at.load(Ordering::Relaxed) - now).max(0)
    }

    /// Store a context_token for a user (from an inbound message).
    pub fn store_context_token(&self, user_id: &str, token: &str) {
        self.context_tokens
            .lock()
            .unwrap()
            .insert(user_id.to_string(), token.to_string());
    }

    /// Get the cached context_token for a user.
    pub fn get_context_token(&self, user_id: &str) -> Option<String> {
        self.context_tokens.lock().unwrap().get(user_id).cloned()
    }

    /// Cache a typing_ticket for a user (valid 24h, we cache for 23h).
    pub fn store_typing_ticket(&self, user_id: &str, ticket: &str) {
        self.typing_tickets
            .lock()
            .unwrap()
            .insert(user_id.to_string(), (ticket.to_string(), Instant::now()));
    }

    /// Get a cached typing_ticket for a user (if fresh enough).
    pub fn get_typing_ticket(&self, user_id: &str) -> Option<String> {
        self.typing_tickets
            .lock()
            .unwrap()
            .get(user_id)
            .and_then(|(ticket, cached_at)| {
                // Cache for 23 hours (typing_ticket valid for 24h)
                if cached_at.elapsed().as_secs() < 23 * 3600 {
                    Some(ticket.clone())
                } else {
                    None
                }
            })
    }
}

// ---------------------------------------------------------------------------
// Global state manager
// ---------------------------------------------------------------------------

/// Global state manager for all iLink bots.
pub struct WeixinState {
    /// Per-bot state keyed by bot_id.
    pub bots: DashMap<String, BotSession>,
    /// Directory for persisting token files.
    pub token_dir: PathBuf,
    /// Shared HTTP client for API routes (QR code login).
    pub http: Client,
}

impl WeixinState {
    fn new() -> Self {
        let home = carrier_types::config::home_dir();
        let token_dir = home.join("weixin-sessions");

        // Auto-migrate from old weixin-tokens/ directory
        let old_dir = home.join("weixin-tokens");
        if old_dir.exists() && !token_dir.exists() {
            Self::migrate_old_tokens(&old_dir, &token_dir);
        }

        Self {
            bots: DashMap::new(),
            token_dir,
            http: Client::new(),
        }
    }

    /// Migrate token files from weixin-tokens/ to weixin-sessions/,
    /// converting the old `"name"` field to `"bot_id"`.
    fn migrate_old_tokens(old_dir: &Path, new_dir: &Path) {
        if let Err(e) = std::fs::create_dir_all(new_dir) {
            warn!(dir = %new_dir.display(), "Failed to create weixin-sessions dir for migration: {e}");
            return;
        }

        let entries = match std::fs::read_dir(old_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %old_dir.display(), "Failed to read old weixin-tokens dir: {e}");
                return;
            }
        };

        let mut migrated = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut tf: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Rename "name" → "bot_id"
            let bot_id = tf
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !bot_id.is_empty() {
                if let Some(o) = tf.as_object_mut() {
                    o.remove("name");
                    o.insert("bot_id".into(), serde_json::Value::String(bot_id.clone()));
                }
            }

            let new_path = new_dir.join(format!("{}.json", bot_id));
            match serde_json::to_string_pretty(&tf) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&new_path, &json) {
                        warn!(path = %new_path.display(), "Failed to write migrated token file: {e}");
                        continue;
                    }
                }
                Err(_) => continue,
            }
            migrated += 1;
        }

        if migrated > 0 {
            info!(count = migrated, "Migrated weixin-tokens → weixin-sessions");
            // Remove old directory after successful migration
            if let Err(e) = std::fs::remove_dir_all(old_dir) {
                warn!(dir = %old_dir.display(), "Failed to remove old weixin-tokens dir: {e}");
            }
        }
    }

    /// Load persisted tokens from the token directory.
    pub fn load_from_dir(&self, dir: &Path) {
        if !dir.exists() {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let entries = std::fs::read_dir(dir);
        match entries {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("json") {
                        continue;
                    }
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            match serde_json::from_str::<BotTokenFile>(&content) {
                                Ok(tf) => {
                                    if now >= tf.expires_at {
                                        info!(
                                            bot_id = %tf.bot_id,
                                            "Skipping expired iLink token"
                                        );
                                        continue;
                                    }
                                    info!(
                                        bot_id = %tf.bot_id,
                                        expires_in = tf.expires_at - now,
                                        "Loaded iLink token"
                                    );
                                    let state = BotSession {
                                        bot_id: tf.bot_id.clone(),
                                        bot_token: tf.bot_token,
                                        baseurl: tf.baseurl,
                                        ilink_bot_id: tf.ilink_bot_id,
                                        user_id: tf.user_id,
                                        expires_at: AtomicI64::new(tf.expires_at),
                                        http: Client::new(),
                                        context_tokens: Mutex::new(HashMap::new()),
                                        typing_tickets: Mutex::new(HashMap::new()),
                                        cursor: Mutex::new(String::new()),
                                        active: AtomicBool::new(false), // Will be set to true when channel starts
                                        bind_agent: tf.bind_agent,
                                    };
                                    self.bots.insert(tf.bot_id, state);
                                }
                                Err(e) => {
                                    warn!(path = %path.display(), "Failed to parse token file: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            warn!(path = %path.display(), "Failed to read token file: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(dir = %dir.display(), "Failed to read token directory: {e}");
            }
        }
    }

    /// Register a new bot from a successful QR scan.
    pub fn register_from_qr(
        &self,
        bot_id: &str,
        bot_token: &str,
        baseurl: &str,
        ilink_bot_id: &str,
        user_id: Option<&str>,
        bind_agent: Option<&str>,
    ) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let state = BotSession {
            bot_id: bot_id.to_string(),
            bot_token: bot_token.to_string(),
            baseurl: baseurl.to_string(),
            ilink_bot_id: ilink_bot_id.to_string(),
            user_id: user_id.map(|s| s.to_string()),
            expires_at: AtomicI64::new(now + SESSION_DURATION_SECS),
            http: Client::new(),
            context_tokens: Mutex::new(HashMap::new()),
            typing_tickets: Mutex::new(HashMap::new()),
            cursor: Mutex::new(String::new()),
            active: AtomicBool::new(true),
            bind_agent: bind_agent.map(|s| s.to_string()),
        };

        // Persist to disk
        self.save_session(&state);

        // Insert/update in-memory
        if let Some(mut existing) = self.bots.get_mut(bot_id) {
            // Preserve cursor and context_tokens from existing session if possible
            let old_cursor = existing.cursor.lock().unwrap().clone();
            *state.cursor.lock().unwrap() = old_cursor;
            *existing = state;
        } else {
            self.bots.insert(bot_id.to_string(), state);
        }

        info!(bot_id = bot_id, "Registered iLink bot from QR scan");
    }

    /// Save a bot session's state to disk.
    pub fn save_session(&self, state: &BotSession) {
        let dir = &self.token_dir;
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!(dir = %dir.display(), "Failed to create token directory: {e}");
            return;
        }

        let tf = BotTokenFile {
            bot_id: state.bot_id.clone(),
            bot_token: state.bot_token.clone(),
            baseurl: state.baseurl.clone(),
            ilink_bot_id: state.ilink_bot_id.clone(),
            user_id: state.user_id.clone(),
            expires_at: state.expires_at.load(Ordering::Relaxed),
            bind_agent: state.bind_agent.clone(),
        };

        let path = dir.join(format!("{}.json", state.bot_id));
        match serde_json::to_string_pretty(&tf) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(path = %path.display(), "Failed to write token file: {e}");
                }
            }
            Err(e) => {
                warn!("Failed to serialize bot token: {e}");
            }
        }
    }

    /// Get a bot session by bot_id.
    pub fn get_session(
        &self,
        bot_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, BotSession>> {
        self.bots.get(bot_id)
    }

    /// List all active (non-expired) bot IDs.
    pub fn active_bot_ids(&self) -> Vec<String> {
        self.bots
            .iter()
            .filter(|e| !e.value().is_expired())
            .map(|e| e.key().clone())
            .collect()
    }

    /// Load new bots from the token directory (skips already-loaded bots).
    /// Used by the dynamic session watcher to pick up QR-scanned bots.
    pub fn load_new_from_dir(&self) {
        if !self.token_dir.exists() {
            return;
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let entries = match std::fs::read_dir(&self.token_dir) {
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
            let tf = match serde_json::from_str::<BotTokenFile>(&content) {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Refresh existing bot only if a new bot_token was written (re-scan).
            // Do NOT refresh just because in-memory is expired — the iLink server
            // may have invalidated the session while the local file still looks valid.
            if let Some(mut existing) = self.bots.get_mut(&tf.bot_id) {
                if existing.bot_token != tf.bot_token {
                    info!(bot_id = %tf.bot_id, "Refreshing iLink bot from updated token file (new bot_token)");
                    existing.bot_token = tf.bot_token;
                    existing.baseurl = tf.baseurl;
                    existing.ilink_bot_id = tf.ilink_bot_id;
                    existing.user_id = tf.user_id;
                    existing.expires_at.store(tf.expires_at, Ordering::Relaxed);
                    existing.active.store(true, Ordering::Relaxed);
                    existing.bind_agent = tf.bind_agent;
                }
                continue;
            }
            if now >= tf.expires_at {
                continue;
            }
            info!(bot_id = %tf.bot_id, "Dynamic watcher loaded new iLink bot");
            let state = BotSession {
                bot_id: tf.bot_id.clone(),
                bot_token: tf.bot_token,
                baseurl: tf.baseurl,
                ilink_bot_id: tf.ilink_bot_id,
                user_id: tf.user_id,
                expires_at: AtomicI64::new(tf.expires_at),
                http: Client::new(),
                context_tokens: Mutex::new(HashMap::new()),
                typing_tickets: Mutex::new(HashMap::new()),
                cursor: Mutex::new(String::new()),
                active: AtomicBool::new(false), // Will be set true when poll thread starts
                bind_agent: tf.bind_agent,
            };
            self.bots.insert(tf.bot_id, state);
        }
    }

    /// Get status of all bots for the API.
    pub fn status_list(&self) -> Vec<serde_json::Value> {
        self.bots
            .iter()
            .map(|entry| {
                let state = entry.value();
                serde_json::json!({
                    "bot_id": state.bot_id,
                    "ilink_bot_id": state.ilink_bot_id,
                    "user_id": state.user_id,
                    "expires_at": state.expires_at.load(Ordering::Relaxed),
                    "remaining_secs": state.remaining_secs(),
                    "expired": state.is_expired(),
                    "active": state.active.load(Ordering::Relaxed),
                    "bind_agent": state.bind_agent,
                })
            })
            .collect()
    }
}

/// Global singleton for iLink state management.
pub static WEIXIN_STATE: std::sync::LazyLock<WeixinState> =
    std::sync::LazyLock::new(WeixinState::new);
