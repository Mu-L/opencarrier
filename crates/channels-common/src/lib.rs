//! Shared components for OpenCarrier channel adapters.
//!
//! Consolidates logic that was copy-pasted across channel crates:
//! - [`InboundDedup`]: idempotency filter for inbound messages (was a per-channel
//!   `DashMap<String, Instant>` + TTL constant + `evict_old_entries()`).
//! - [`get_cached_token`] / [`CachedToken`]: cached OAuth token with early-refresh
//!   (was a per-channel `Mutex<Option<CachedToken>>` + `get_token`/`refresh`).
//! - [`BotRegistry`] / [`ChannelBot`]: per-channel bot session state manager (was a
//!   per-channel `*State` struct with 7 near-identical methods over a
//!   `DashMap<String, *BotEntry>`).

use dashmap::DashMap;
use serde::{de::DeserializeOwned, Serialize};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use types::error::CarrierResult;

// ---------------------------------------------------------------------------
// Inbound dedup
// ---------------------------------------------------------------------------

/// Idempotency filter for inbound messages, shared across channel crates.
///
/// Each channel previously hand-rolled a `DashMap<String, Instant>` + a TTL
/// constant + an `evict_old_entries()`. This consolidates that, parameterized by
/// TTL and max entries so each channel keeps its own dedup window (e.g. feishu
/// 300s, dingtalk 60s) instead of being forced to a single value.
pub struct InboundDedup {
    seen: DashMap<String, Instant>,
    ttl: Duration,
    max_entries: usize,
}

impl InboundDedup {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        Self {
            seen: DashMap::new(),
            ttl,
            max_entries,
        }
    }

    /// Record `key` and return whether it is new (i.e. not a recently-seen duplicate).
    ///
    /// Expired entries are pruned on each call; if the table exceeds `max_entries`
    /// after insertion, the oldest entries are dropped to bound memory.
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        // Fast path: seen recently and still within TTL → duplicate.
        if let Some(entry) = self.seen.get(key) {
            if *entry + self.ttl > now {
                return false;
            }
        }
        // Prune expired entries.
        let ttl = self.ttl;
        self.seen.retain(|_, t| *t + ttl > now);
        // Record / refresh the timestamp for this key.
        self.seen.insert(key.to_string(), now);
        // Bound growth: drop oldest entries when over capacity.
        if self.seen.len() > self.max_entries {
            let mut entries: Vec<(String, Instant)> = self
                .seen
                .iter()
                .map(|e| (e.key().clone(), *e.value()))
                .collect();
            entries.sort_by_key(|(_, t)| *t);
            let drop_count = entries.len().saturating_sub(self.max_entries);
            for (k, _) in entries.into_iter().take(drop_count) {
                self.seen.remove(&k);
            }
        }
        true
    }

    /// Current number of tracked keys (for diagnostics/tests).
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether no keys are tracked.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Token cache
// ---------------------------------------------------------------------------

/// A cached token plus the instant at which it should be refreshed.
pub struct CachedToken {
    pub access_token: String,
    pub expires_at: Instant,
}

/// Return a valid token from `cache`, fetching a fresh one via `fetch` when the
/// cached value is missing or past its refresh-ahead window.
///
/// `fetch` returns `(token, expire_secs)` (the server-advertised lifetime).
/// `refresh_ahead` is subtracted from `expire_secs` so the token is proactively
/// refreshed before it actually expires. The cache lock is released before
/// `fetch` runs, so a slow refresh doesn't block concurrent readers (they'll
/// also miss the cache and one wins the write).
pub async fn get_cached_token<F, Fut>(
    cache: &Mutex<Option<CachedToken>>,
    refresh_ahead: Duration,
    fetch: F,
) -> CarrierResult<String>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = CarrierResult<(String, u64)>> + Send,
{
    // Fast path: return a still-valid cached token.
    {
        let guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref cached) = *guard {
            if cached.expires_at > Instant::now() {
                return Ok(cached.access_token.clone());
            }
        }
    }

    // Cache miss / expired → fetch a fresh token (lock released during I/O).
    let (token, expire_secs) = fetch().await?;
    let expires_at =
        Instant::now() + Duration::from_secs(expire_secs.saturating_sub(refresh_ahead.as_secs()));

    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedToken {
            access_token: token.clone(),
            expires_at,
        });
    }

    Ok(token)
}

// ---------------------------------------------------------------------------
// Bot registry (per-channel session state manager)
// ---------------------------------------------------------------------------

/// Resolve the effective app secret: try the env var named by `secret_env`
/// first, fall back to the inline value.
///
/// Shared by all channel `build_entry` implementations; not part of the
/// [`ChannelBot`] trait since it operates purely on the two raw fields.
pub fn resolve_secret(secret_env: &Option<String>, inline: &Option<String>) -> String {
    if let Some(ref env_name) = secret_env {
        if let Ok(s) = std::env::var(env_name) {
            if !s.is_empty() {
                return s;
            }
        }
    }
    inline.clone().unwrap_or_default()
}

/// A runtime bot entry held in a [`BotRegistry`].
///
/// Implemented by each channel's concrete `*BotEntry`. Only exposes what the
/// generic registry methods need; channel-specific fields (token cache, config)
/// stay on the concrete type and are reached via `ChannelBot::Entry`.
pub trait BotEntry {
    /// Display name (used in logs and `status_list`).
    fn name(&self) -> &str;
    /// Resolved app secret (used to detect credential changes on refresh).
    fn secret(&self) -> &str;
    /// Active flag (read by `status_list`, written by the spawn path).
    fn active(&self) -> &AtomicBool;
}

/// Parameterizes a [`BotRegistry`] over one channel platform.
///
/// Each channel crate provides one zero-sized marker type implementing this
/// (e.g. `struct FeishuBot;`). Deliberately minimal: two associated types, two
/// string consts, one required constructor, one optional status hook.
pub trait ChannelBot {
    /// Concrete runtime entry stored in the registry.
    type Entry: BotEntry;
    /// Session-file schema (`senders/{key}/session.json`). Must round-trip the
    /// existing on-disk format — production servers already have these files.
    type Session: DeserializeOwned + Serialize;

    /// Lowercase channel id matched against the session file's `channel` field
    /// (e.g. `"feishu"`, `"dingtalk"`); also used in save/parse warning text.
    const CHANNEL: &'static str;
    /// Capitalized label for "Loaded X session" startup logs (e.g. `"Feishu"`),
    /// kept byte-identical to the previous per-channel messages.
    const LABEL: &'static str;

    /// The map key for this session (`&sf.app_id` / `&sf.app_key`).
    fn key(sf: &Self::Session) -> &str;
    /// Build a runtime entry from a parsed session file, or `None` to skip it
    /// (empty/invalid credentials). Channel-specific validation text lives here.
    fn build_entry(sf: &Self::Session) -> Option<Self::Entry>;

    /// Insert channel-specific identity fields into a `status_list` object
    /// (e.g. `app_id`+`brand`, or `app_key`). Default adds nothing.
    fn status_extra(_entry: &Self::Entry, _out: &mut serde_json::Map<String, serde_json::Value>) {}
}

/// Generic per-channel bot session state manager.
///
/// Holds the live `DashMap<String, B::Entry>` and the seven load/save/query
/// methods that were previously hand-duplicated across channel `*State` structs.
/// The WebSocket connection loops never touch this — they hold a token-cache
/// `Arc` cloned at spawn time — so generalizing this container does not affect
/// live connections.
pub struct BotRegistry<B: ChannelBot> {
    pub bots: DashMap<String, B::Entry>,
}

impl<B: ChannelBot> BotRegistry<B> {
    pub fn new() -> Self {
        Self {
            bots: DashMap::new(),
        }
    }

    /// Load all sessions from `senders/*/session.json` (initial load at startup).
    /// Only loads files where `channel == B::CHANNEL`.
    pub fn load_from_dir(&self) {
        let home = types::config::home_dir();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some(B::CHANNEL) {
                continue;
            }
            let sf: B::Session = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(e) => {
                    warn!(sender_id = %sender_id, "Failed to parse {} session: {e}", B::CHANNEL);
                    continue;
                }
            };
            let key = B::key(&sf);
            if key.is_empty() {
                continue;
            }
            if self.bots.contains_key(key) {
                continue;
            }
            let entry = match B::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %entry.name(), key = %key, "Loaded {} session", B::LABEL);
            self.bots.insert(key.to_string(), entry);
        }
    }

    /// Load new sessions from `senders/*/session.json` (skips already-loaded).
    /// Refreshes an existing bot in place if its session file changed.
    /// Only loads files where `channel == B::CHANNEL`.
    pub fn load_new_from_dir(&self) {
        let home = types::config::home_dir();
        for (sender_id, json) in types::config::scan_sender_sessions(&home) {
            if json.get("channel").and_then(|v| v.as_str()) != Some(B::CHANNEL) {
                continue;
            }
            // Refresh existing bot if session file changed. Lookup is by the
            // senders/ directory name (the "dir name == key" invariant).
            if let Some(mut existing) = self.bots.get_mut(&sender_id) {
                let sf: B::Session = match serde_json::from_value(json) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let new_entry = match B::build_entry(&sf) {
                    Some(e) => e,
                    None => continue,
                };
                if existing.secret() != new_entry.secret() {
                    info!(key = %B::key(&sf), "Refreshing {} session from updated file", B::LABEL);
                    *existing = new_entry;
                }
                continue;
            }
            let sf: B::Session = match serde_json::from_value(json) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let key = B::key(&sf);
            if key.is_empty() {
                continue;
            }
            let entry = match B::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %entry.name(), key = %key, "Dynamic watcher loaded new {} session", B::LABEL);
            self.bots.insert(key.to_string(), entry);
        }
    }

    /// Save a session file to `senders/{key}/session.json`.
    pub fn save_session(&self, sf: &B::Session) {
        let sender_id = B::key(sf);
        if sender_id.is_empty() {
            warn!("Cannot save {} session with empty key", B::CHANNEL);
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

    /// Get a bot session by key.
    pub fn get_session(
        &self,
        key: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, B::Entry>> {
        self.bots.get(key)
    }

    /// Get status of all bots for the API.
    pub fn status_list(&self) -> Vec<serde_json::Value> {
        self.bots
            .iter()
            .map(|entry| {
                let e = entry.value();
                let mut map = serde_json::Map::new();
                map.insert(
                    "name".to_string(),
                    serde_json::Value::String(e.name().to_string()),
                );
                map.insert(
                    "active".to_string(),
                    serde_json::Value::Bool(e.active().load(Ordering::Relaxed)),
                );
                B::status_extra(e, &mut map);
                serde_json::Value::Object(map)
            })
            .collect()
    }
}

impl<B: ChannelBot> Default for BotRegistry<B> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    // --- Mock channel for BotRegistry tests ---

    #[derive(Debug, Serialize, serde::Deserialize)]
    struct MockSession {
        #[serde(default)]
        channel: String,
        name: String,
        app_id: String,
        app_secret: Option<String>,
        secret_env: Option<String>,
    }

    struct MockEntry {
        name: String,
        app_id: String,
        app_secret: String,
        active: AtomicBool,
    }

    impl BotEntry for MockEntry {
        fn name(&self) -> &str {
            &self.name
        }
        fn secret(&self) -> &str {
            &self.app_secret
        }
        fn active(&self) -> &AtomicBool {
            &self.active
        }
    }

    struct MockBot;
    impl ChannelBot for MockBot {
        type Entry = MockEntry;
        type Session = MockSession;
        const CHANNEL: &'static str = "mock";
        const LABEL: &'static str = "Mock";

        fn key(sf: &MockSession) -> &str {
            &sf.app_id
        }
        fn build_entry(sf: &MockSession) -> Option<MockEntry> {
            let secret = resolve_secret(&sf.secret_env, &sf.app_secret);
            if sf.app_id.is_empty() || secret.is_empty() {
                return None;
            }
            Some(MockEntry {
                name: sf.name.clone(),
                app_id: sf.app_id.clone(),
                app_secret: secret,
                active: AtomicBool::new(false),
            })
        }
        fn status_extra(entry: &MockEntry, out: &mut serde_json::Map<String, serde_json::Value>) {
            out.insert("app_id".to_string(), entry.app_id.clone().into());
        }
    }

    fn mock_session(app_id: &str, secret: &str) -> MockSession {
        MockSession {
            channel: "mock".to_string(),
            name: format!("bot-{app_id}"),
            app_id: app_id.to_string(),
            app_secret: Some(secret.to_string()),
            secret_env: None,
        }
    }

    /// Serializes tests that mutate the process-global OPENCARRIER_HOME, so the
    /// two registry tests don't clobber each other's env var when run in parallel.
    static HOME_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Unique temp OPENCARRIER_HOME, restored on drop. Holds `HOME_ENV_LOCK` for
    /// its whole lifetime so concurrent env-mutating tests run one at a time.
    struct TempHome {
        path: String,
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl TempHome {
        fn new(tag: &str) -> Self {
            let guard = HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir =
                std::env::temp_dir().join(format!("oc-botreg-{}-{}", std::process::id(), tag));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var("OPENCARRIER_HOME", &dir);
            Self {
                path: dir.to_string_lossy().to_string(),
                _guard: guard,
            }
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            std::env::remove_var("OPENCARRIER_HOME");
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn dedup_first_seen_is_new_second_is_dup() {
        let d = InboundDedup::new(Duration::from_secs(60), 1000);
        assert!(d.check("msg-1"));
        assert!(!d.check("msg-1")); // immediate repeat → duplicate
        assert!(d.check("msg-2"));
    }

    #[tokio::test]
    async fn token_cache_fetches_then_reuses() {
        let cache = Arc::new(Mutex::new(None));
        let fetches = Arc::new(Mutex::new(0u32));
        let cache_for_fn = cache.clone();
        let fetches_for_fn = fetches.clone();
        let t1 = get_cached_token(&cache_for_fn, Duration::from_secs(300), || {
            let fetches_for_fn = fetches_for_fn.clone();
            async move {
                *fetches_for_fn.lock().unwrap() += 1;
                Ok(("tok-1".to_string(), 7200))
            }
        })
        .await
        .unwrap();
        assert_eq!(t1, "tok-1");
        assert_eq!(*fetches.lock().unwrap(), 1);

        // Second call within the refresh-ahead window reuses the cached token
        // (7200 - 300 = 6900s ahead) → no second fetch.
        let cache_for_fn = cache.clone();
        let t2 = get_cached_token(&cache_for_fn, Duration::from_secs(300), || async {
            unreachable!("cached token should be reused");
        })
        .await
        .unwrap();
        assert_eq!(t2, "tok-1");
    }

    #[test]
    fn registry_load_save_refresh_roundtrip() {
        let _home = TempHome::new("roundtrip");
        let reg = BotRegistry::<MockBot>::new();

        // save_session writes senders/{key}/session.json.
        reg.save_session(&mock_session("bot1", "secret-1"));
        let home = types::config::home_dir();
        assert!(home.join("senders/bot1/session.json").exists());

        // load_from_dir discovers it.
        reg.load_from_dir();
        let e = reg.get_session("bot1").expect("bot1 loaded");
        assert_eq!(e.name(), "bot-bot1");
        assert_eq!(e.secret(), "secret-1");
        drop(e);

        // A second save (new bot) is only picked up by load_new_from_dir.
        reg.save_session(&mock_session("bot2", "secret-2"));
        reg.load_new_from_dir();
        assert!(reg.get_session("bot2").is_some());

        // Editing bot1's secret triggers the in-place refresh branch.
        reg.save_session(&mock_session("bot1", "secret-1b"));
        reg.load_new_from_dir();
        assert_eq!(reg.get_session("bot1").unwrap().secret(), "secret-1b");

        // status_list carries base fields + status_extra identity key.
        let list = reg.status_list();
        assert_eq!(list.len(), 2);
        let bot1 = list
            .iter()
            .find(|v| v["app_id"] == "bot1")
            .expect("bot1 in status");
        assert_eq!(bot1["name"], "bot-bot1");
        assert_eq!(bot1["active"], false);
    }

    #[test]
    fn registry_skips_empty_and_wrong_channel() {
        let _home = TempHome::new("skip");
        let reg = BotRegistry::<MockBot>::new();

        // Empty key must not be saved or loaded.
        reg.save_session(&mock_session("", "secret"));
        let home = types::config::home_dir();
        assert!(!home.join("senders").join("session.json").exists());

        // A session for a different channel is ignored on load.
        let other = home.join("senders/other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            other.join("session.json"),
            r#"{"channel":"not-mock","name":"x","app_id":"other","app_secret":"s"}"#,
        )
        .unwrap();
        reg.load_from_dir();
        assert!(reg.get_session("other").is_none());
    }
}
