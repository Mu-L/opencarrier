//! Token storage and management for the WeChat iLink Bot plugin.
//!
//! Manages per-bot bot_tokens (24h expiry) and per-user context_tokens.
//! When DB-backed persistence is available (via set_persist_fn),
//! sessions and context_tokens are stored in SQLite instead of JSON files.

use dashmap::DashMap;
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

use crate::models::*;

/// Throttle for persisting renewed sessions. The poll loop renews `expires_at`
/// in memory on every successful (often empty) getUpdates — we must not hit the
/// DB/JSON on each ~2s tick. Persist at most this often, so on restart load sees
/// an `expires_at` at most this stale (well inside the 24h window) instead of a
/// value from the last inbound message hours/days ago.
const SESSION_SAVE_INTERVAL_SECS: i64 = 1800; // 30 min

// ---------------------------------------------------------------------------
// DB persistence callbacks (set from server.rs when memory is available)
// ---------------------------------------------------------------------------

/// Persist a session (BotTokenFile) to the database.
pub type SessionPersistFn = Arc<dyn Fn(&BotTokenFile) + Send + Sync>;
/// Load all persisted sessions from the database.
pub type SessionsLoadFn = Arc<dyn Fn() -> Vec<BotTokenFile> + Send + Sync>;

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
    /// get_updates_buf cursor for long-polling.
    pub cursor: Mutex<String>,
    /// Whether the polling loop is active.
    pub active: AtomicBool,
    /// Wall-clock (secs) of the last `save_session`. Used to throttle
    /// persistence during empty-poll renewal so we don't write every tick.
    pub last_saved: AtomicI64,
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
        self.context_tokens.lock().unwrap_or_else(|e| e.into_inner()).get(user_id).cloned()
    }
}

// ---------------------------------------------------------------------------
// Global state manager
// ---------------------------------------------------------------------------

/// Global state manager for all iLink bots.
pub struct WeixinState {
    /// Per-bot state keyed by user_id (stable unique identifier for WeChat).
    pub bots: DashMap<String, BotSession>,
    /// Shared HTTP client for API routes (QR code login).
    pub http: Client,
    /// DB-backed session persist callback. When set, save_session writes to DB instead of JSON.
    pub session_persist: Mutex<Option<SessionPersistFn>>,
    /// DB-backed session load callback. When set, load_from_dir reads from DB.
    pub sessions_load: Mutex<Option<SessionsLoadFn>>,
}

impl WeixinState {
    fn new() -> Self {
        Self {
            bots: DashMap::new(),
            http: crate::build_http_client(),
            session_persist: Mutex::new(None),
            sessions_load: Mutex::new(None),
        }
    }

    /// Set DB-backed persistence callbacks. Called once at startup from server.rs.
    pub fn set_persist_fns(
        &self,
        persist: SessionPersistFn,
        load: SessionsLoadFn,
    ) {
        *self.session_persist.lock().unwrap() = Some(persist);
        *self.sessions_load.lock().unwrap() = Some(load);
    }

    /// Load persisted tokens from the database (preferred) or JSON files.
    pub fn load_from_dir(&self) {
        // Try DB first
        if let Some(ref load_fn) = *self.sessions_load.lock().unwrap() {
            let tfs = load_fn();
            if !tfs.is_empty() {
                self.load_from_bot_token_files(tfs);
                return;
            }
        }
        // Fallback: scan JSON files
        let home = types::config::home_dir();

        let mut tfs = Vec::new();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("weixin") {
                continue;
            }
            let tf: BotTokenFile = match serde_json::from_value(json) {
                Ok(t) => t,
                Err(e) => {
                    warn!(sender_id = %sender_id, "Failed to parse weixin session: {e}");
                    continue;
                }
            };
            tfs.push(tf);
        }
        if !tfs.is_empty() {
            self.load_from_bot_token_files(tfs);
        }
    }

    /// Shared logic to load BotTokenFiles into the in-memory bot cache.
    fn load_from_bot_token_files(&self, tfs: Vec<BotTokenFile>) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let mut count = 0;
        for tf in tfs {
            let user_id = match &tf.user_id {
                Some(uid) if !uid.is_empty() => uid.clone(),
                _ => continue,
            };
            if now >= tf.expires_at {
                continue;
            }
            let persisted_ctx = tf.context_tokens.clone();
            count += 1;
            let state = BotSession {
                bot_id: tf.bot_id.clone(),
                bot_token: tf.bot_token,
                baseurl: tf.baseurl,
                ilink_bot_id: tf.ilink_bot_id,
                user_id: Some(user_id.clone()),
                expires_at: AtomicI64::new(tf.expires_at),
                http: crate::build_http_client(),
                context_tokens: Mutex::new(persisted_ctx),
                cursor: Mutex::new(String::new()),
                active: AtomicBool::new(false),
                last_saved: AtomicI64::new(tf.expires_at - SESSION_DURATION_SECS),
                bind_agent: tf.bind_agent,
            };
            self.bots.insert(user_id, state);
        }
        if count > 0 {
            info!(count, "Loaded iLink bot sessions");
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
            http: crate::build_http_client(),
            context_tokens: Mutex::new(HashMap::new()),
            cursor: Mutex::new(String::new()),
            active: AtomicBool::new(true),
            last_saved: AtomicI64::new(now),
            bind_agent: bind_agent.map(|s| s.to_string()),
        };

        // Persist to disk
        self.save_session(&state);

        // Insert/update in-memory, keyed by user_id
        let key = user_id.unwrap_or(bot_id);
        if let Some(mut existing) = self.bots.get_mut(key) {
            // Preserve cursor from existing session if possible
            let old_cursor = existing.cursor.lock().unwrap_or_else(|e| e.into_inner()).clone();
            *state.cursor.lock().unwrap_or_else(|e| e.into_inner()) = old_cursor;
            *existing = state;
        } else {
            self.bots.insert(key.to_string(), state);
        }

        info!(user_id = ?user_id, bot_id = bot_id, "Registered iLink bot from QR scan");
    }

    /// Save a bot session's state. Uses DB if persistence callback is set,
    /// otherwise falls back to JSON file.
    pub fn save_session(&self, state: &BotSession) {
        let merged_ctx = state.context_tokens.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let tf = BotTokenFile {
            channel: "weixin".to_string(),
            sender_key: "openid".to_string(),
            bot_id: state.bot_id.clone(),
            bot_token: state.bot_token.clone(),
            baseurl: state.baseurl.clone(),
            ilink_bot_id: state.ilink_bot_id.clone(),
            user_id: state.user_id.clone(),
            expires_at: state.expires_at.load(Ordering::Relaxed),
            bind_agent: state.bind_agent.clone(),
            context_tokens: merged_ctx,
        };

        // Try DB first
        if let Some(ref persist) = *self.session_persist.lock().unwrap() {
            persist(&tf);
            return;
        }

        // Fallback: JSON file
        let filename_key = state.user_id.as_deref().unwrap_or(&state.bot_id);
        let dir = types::config::home_dir().join("senders").join(filename_key);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), "Failed to create sender directory: {e}");
            return;
        }
        let path = dir.join("session.json");
        match serde_json::to_string_pretty(&tf) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, &json) {
                    warn!(path = %path.display(), "Failed to write session file: {e}");
                } else {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                    }
                }
            }
            Err(e) => {
                warn!("Failed to serialize bot token: {e}");
            }
        }
    }

    /// Persist `state` only if it was last saved more than
    /// `SESSION_SAVE_INTERVAL_SECS` ago. Called from the poll loop's renewal
    /// branches (both message and empty-poll) so a long-idle bot's on-disk
    /// `expires_at` stays current without writing on every ~2s tick.
    ///
    /// Without this, idle bots' disk `expires_at` stays frozen at the time of
    /// their last *inbound message*; after 24h without a message a restart
    /// loads them as expired and drops them — the "iLink 老是断开 on restart" bug.
    pub fn persist_if_due(&self, state: &BotSession) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        if now - state.last_saved.load(Ordering::Relaxed) >= SESSION_SAVE_INTERVAL_SECS {
            self.save_session(state);
            state.last_saved.store(now, Ordering::Relaxed);
        }
    }

    /// Get a bot session by user_id.
    pub fn get_session(
        &self,
        user_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, BotSession>> {
        self.bots.get(user_id)
    }

    /// Find a bot session for sending a message to `user_id`.
    ///
    /// Routing model (verified in production 2026-08-19): iLink delivery is
    /// account-to-account and clone-agnostic — any clone can push to any
    /// recipient, provided the SENDING account has a relationship with the
    /// recipient. context_tokens doubles as the relationship ledger.
    /// Three paths, in order:
    /// 1. **Direct**: `user_id` IS a logged-in account (the 号池). Deliver
    ///    via its own session — effectively self-chat. Works for every pool
    ///    account (admin alerts ride this path); bare send, no token needed.
    /// 2. **Relationship scan**: the peer chatted with exactly one of our
    ///    accounts before — its token lives in THAT account's session. iLink
    ///    peer ids are per-account namespaces, so at most one holder; no
    ///    ambiguity.
    /// 3. **Legacy bot_id fallback**: caller-asserted explicit route. CAVEAT:
    ///    sends to peers with NO relationship upstream (never chatted, not
    ///    in the pool) are silently dropped by iLink — HTTP still returns a
    ///    message_id, so success here is best-effort, never a receipt.
    pub fn get_session_for_send(
        &self,
        _bot_id: &str,
        user_id: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, BotSession>> {
        // 1. Direct lookup by user_id — the target is itself a scanned account.
        if let Some(state) = self.bots.get(user_id) {
            return Some(state);
        }
        // 2. The one session whose account actually holds this peer's token.
        //    Mutex pairs only with read refs elsewhere (store/get/save), so
        //    locking inside the shard-read iteration cannot deadlock.
        let holder = self
            .bots
            .iter()
            .find(|entry| {
                entry
                    .value()
                    .context_tokens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(user_id)
            })
            .map(|entry| entry.key().clone());
        if let Some(key) = holder {
            return self.bots.get(&key);
        }
        // 3. Legacy fallback: first session with a matching bot_id.
        let found_key = self
            .bots
            .iter()
            .find(|entry| entry.value().bot_id == _bot_id)
            .map(|entry| entry.key().clone())?;
        self.bots.get(&found_key)
    }

    /// Load new bots — from DB if available, otherwise scan JSON files.
    /// Used by the dynamic session watcher to pick up QR-scanned bots.
    pub fn load_new_from_dir(&self) {
        // Try DB first: reload and insert any sessions not yet in memory
        if let Some(ref load_fn) = *self.sessions_load.lock().unwrap() {
            for tf in load_fn() {
                let sender_id = tf.user_id.as_deref().unwrap_or("");
                if sender_id.is_empty() || self.bots.contains_key(sender_id) {
                    continue;
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64;
                if now >= tf.expires_at {
                    continue;
                }
                info!(sender_id = %sender_id, ctx_count = tf.context_tokens.len(), "Dynamic watcher loaded new iLink bot from DB");
                let state = BotSession {
                    bot_id: tf.bot_id.clone(),
                    bot_token: tf.bot_token,
                    baseurl: tf.baseurl,
                    ilink_bot_id: tf.ilink_bot_id,
                    user_id: Some(sender_id.to_string()),
                    expires_at: AtomicI64::new(tf.expires_at),
                    http: crate::build_http_client(),
                    context_tokens: Mutex::new(tf.context_tokens),
                    cursor: Mutex::new(String::new()),
                    active: AtomicBool::new(false),
                    last_saved: AtomicI64::new(tf.expires_at - SESSION_DURATION_SECS),
                    bind_agent: tf.bind_agent,
                };
                self.bots.insert(sender_id.to_string(), state);
            }
            return;
        }

        // Fallback: scan JSON files
        let home = types::config::home_dir();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some("weixin") {
                continue;
            }
            let tf: BotTokenFile = match serde_json::from_value(json) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if let Some(mut existing) = self.bots.get_mut(&sender_id) {
                if existing.bot_token != tf.bot_token {
                    info!(sender_id = %sender_id, "Refreshing iLink bot from updated session file (new bot_token)");
                    existing.bot_token = tf.bot_token.clone();
                    existing.baseurl = tf.baseurl;
                    existing.ilink_bot_id = tf.ilink_bot_id;
                    existing.user_id = tf.user_id;
                    existing.expires_at.store(tf.expires_at, Ordering::Relaxed);
                    existing.active.store(true, Ordering::Relaxed);
                    existing.bind_agent = tf.bind_agent.clone();
                    self.save_session(&existing);
                }
                {
                    let mut ctx = existing.context_tokens.lock().unwrap_or_else(|e| e.into_inner());
                    for (uid, tok) in &tf.context_tokens {
                        ctx.entry(uid.clone()).or_insert_with(|| tok.clone());
                    }
                }
                continue;
            }
            if now >= tf.expires_at {
                continue;
            }
            info!(sender_id = %sender_id, "Dynamic watcher loaded new iLink bot");
            let persisted_ctx = tf.context_tokens.clone();
            let state = BotSession {
                bot_id: tf.bot_id.clone(),
                bot_token: tf.bot_token,
                baseurl: tf.baseurl,
                ilink_bot_id: tf.ilink_bot_id,
                user_id: Some(sender_id.clone()),
                expires_at: AtomicI64::new(tf.expires_at),
                http: crate::build_http_client(),
                context_tokens: Mutex::new(persisted_ctx),
                cursor: Mutex::new(String::new()),
                active: AtomicBool::new(false),
                last_saved: AtomicI64::new(tf.expires_at - SESSION_DURATION_SECS),
                bind_agent: tf.bind_agent,
            };
            self.bots.insert(sender_id, state);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Register a fresh scanner account (never expired: register_from_qr
    /// stamps now + SESSION_DURATION_SECS).
    fn register(state: &WeixinState, scanner: &str) {
        state.register_from_qr(
            "default",
            "tok",
            "https://example.invalid",
            "bot@im.bot",
            Some(scanner),
            Some(&format!("agent-{scanner}")),
        );
    }

    /// Path 2: a peer's context_token lives in the ONE session whose account
    /// talked to them — a send must resolve to that session, not to an
    /// arbitrary "default"-bot_id session (all sessions share bot_id
    /// "default" in production, so the legacy fallback is a coin flip).
    #[test]
    fn get_session_for_send_resolves_the_token_holder() {
        let state = WeixinState::new();
        register(&state, "scanner-a");
        register(&state, "scanner-b");
        // peer-p only ever talked to scanner-b.
        state
            .bots
            .get("scanner-b")
            .unwrap()
            .store_context_token("peer-p", "ctx");
        let resolved = state.get_session_for_send("default", "peer-p").unwrap();
        assert_eq!(resolved.key(), "scanner-b");
        assert_eq!(resolved.get_context_token("peer-p").as_deref(), Some("ctx"));
    }

    /// Path 1 unchanged: an admin entry whose id IS a scanner account
    /// resolves directly (the self-keyed-token pattern that makes the
    /// daily admin brief deliver for scanner-id admins).
    #[test]
    fn get_session_for_send_direct_hit_for_scanner_id() {
        let state = WeixinState::new();
        register(&state, "scanner-a");
        state
            .bots
            .get("scanner-a")
            .unwrap()
            .store_context_token("scanner-a", "self-ctx");
        let resolved = state.get_session_for_send("default", "scanner-a").unwrap();
        assert_eq!(resolved.key(), "scanner-a");
    }

    /// Path 3 unchanged: a peer with no token anywhere still yields SOME
    /// session via the bot_id fallback, so callers surface the accurate
    /// "No context_token" error rather than "No session".
    #[test]
    fn get_session_for_send_unknown_peer_falls_back_to_bot_id() {
        let state = WeixinState::new();
        register(&state, "scanner-a");
        let resolved = state.get_session_for_send("default", "stranger").unwrap();
        assert_eq!(resolved.key(), "scanner-a");
        assert!(resolved.get_context_token("stranger").is_none());
    }
}
