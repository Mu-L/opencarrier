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

use crate::types::*;

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
    /// Per-bot state keyed by user_id (stable unique identifier for WeChat).
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

        Self {
            bots: DashMap::new(),
            token_dir,
            http: Client::new(),
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
                                    let user_id = match &tf.user_id {
                                        Some(uid) if !uid.is_empty() => uid.clone(),
                                        _ => {
                                            info!(
                                                bot_id = %tf.bot_id,
                                                "Skipping iLink token without user_id"
                                            );
                                            continue;
                                        }
                                    };
                                    if now >= tf.expires_at {
                                        info!(
                                            user_id = %user_id,
                                            "Skipping expired iLink token"
                                        );
                                        continue;
                                    }
                                    info!(
                                        user_id = %user_id,
                                        expires_in = tf.expires_at - now,
                                        "Loaded iLink token"
                                    );
                                    let state = BotSession {
                                        bot_id: tf.bot_id.clone(),
                                        bot_token: tf.bot_token,
                                        baseurl: tf.baseurl,
                                        ilink_bot_id: tf.ilink_bot_id,
                                        user_id: Some(user_id.clone()),
                                        expires_at: AtomicI64::new(tf.expires_at),
                                        http: Client::new(),
                                        context_tokens: Mutex::new(HashMap::new()),
                                        typing_tickets: Mutex::new(HashMap::new()),
                                        cursor: Mutex::new(String::new()),
                                        active: AtomicBool::new(false),
                                        bind_agent: tf.bind_agent,
                                    };
                                    self.bots.insert(user_id, state);
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

        // Insert/update in-memory, keyed by user_id
        let key = user_id.unwrap_or(bot_id);
        if let Some(mut existing) = self.bots.get_mut(key) {
            // Preserve cursor from existing session if possible
            let old_cursor = existing.cursor.lock().unwrap().clone();
            *state.cursor.lock().unwrap() = old_cursor;
            *existing = state;
        } else {
            self.bots.insert(key.to_string(), state);
        }

        info!(user_id = ?user_id, bot_id = bot_id, "Registered iLink bot from QR scan");
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

        // Use user_id as filename (stable unique key for WeChat)
        let filename_key = state.user_id.as_deref().unwrap_or(&state.bot_id);
        let path = dir.join(format!("{}.json", filename_key));
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

    /// Get a bot session by user_id.
    pub fn get_session(
        &self,
        user_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, BotSession>> {
        self.bots.get(user_id)
    }

    /// Find a bot session for sending a message. Uses user_id as primary key,
    /// falls back to scanning by bot_id if needed.
    pub fn get_session_for_send(
        &self,
        _bot_id: &str,
        user_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, BotSession>> {
        // Direct lookup by user_id (most common path)
        if let Some(state) = self.bots.get(user_id) {
            return Some(state);
        }
        // Fallback: find the key for a session with matching bot_id, then get() it
        let found_key = self
            .bots
            .iter()
            .find(|entry| entry.value().bot_id == _bot_id)
            .map(|entry| entry.key().clone())?;
        self.bots.get(&found_key)
    }

    /// List all active (non-expired) user IDs.
    pub fn active_user_ids(&self) -> Vec<String> {
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
            let user_id = match &tf.user_id {
                Some(uid) if !uid.is_empty() => uid.clone(),
                _ => continue,
            };
            // Refresh existing bot only if a new bot_token was written (re-scan).
            if let Some(mut existing) = self.bots.get_mut(&user_id) {
                if existing.bot_token != tf.bot_token {
                    info!(user_id = %user_id, "Refreshing iLink bot from updated token file (new bot_token)");
                    existing.bot_token = tf.bot_token.clone();
                    existing.baseurl = tf.baseurl;
                    existing.ilink_bot_id = tf.ilink_bot_id;
                    existing.user_id = tf.user_id;
                    existing.expires_at.store(tf.expires_at, Ordering::Relaxed);
                    existing.active.store(true, Ordering::Relaxed);
                    existing.bind_agent = tf.bind_agent.clone();
                    self.save_session(&existing);
                }
                continue;
            }
            if now >= tf.expires_at {
                continue;
            }
            info!(user_id = %user_id, "Dynamic watcher loaded new iLink bot");
            let state = BotSession {
                bot_id: tf.bot_id.clone(),
                bot_token: tf.bot_token,
                baseurl: tf.baseurl,
                ilink_bot_id: tf.ilink_bot_id,
                user_id: Some(user_id.clone()),
                expires_at: AtomicI64::new(tf.expires_at),
                http: Client::new(),
                context_tokens: Mutex::new(HashMap::new()),
                typing_tickets: Mutex::new(HashMap::new()),
                cursor: Mutex::new(String::new()),
                active: AtomicBool::new(false),
                bind_agent: tf.bind_agent,
            };
            self.bots.insert(user_id, state);
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
