//! Shared components for OpenCarrier channel adapters.
//!
//! Consolidates logic that was copy-pasted across channel crates:
//! - [`InboundDedup`]: idempotency filter for inbound messages (was a per-channel
//!   `DashMap<String, Instant>` + TTL constant + `evict_old_entries()`).
//! - [`get_cached_token`] / [`CachedToken`]: cached OAuth token with early-refresh
//!   (was a per-channel `Mutex<Option<CachedToken>>` + `get_token`/`refresh`).

use dashmap::DashMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
            let mut entries: Vec<(String, Instant)> =
                self.seen.iter().map(|e| (e.key().clone(), *e.value())).collect();
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
) -> Result<String, String>
where
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<(String, u64), String>> + Send,
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
    let expires_at = Instant::now()
        + Duration::from_secs(expire_secs.saturating_sub(refresh_ahead.as_secs()));

    {
        let mut guard = cache.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(CachedToken {
            access_token: token.clone(),
            expires_at,
        });
    }

    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

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
}
