//! Central stable_token cache — the ONE token authority for the whole process
//! (2026-08-18 three-shell convergence: channel account cache, api-routes
//! `OA_TOKENS` cache, and the publish tool's per-call fetch all delegate here).
//!
//! - Keyed by `app_id`; the cached entry records a hash of the `app_secret`
//!   it was issued for, so a rotated secret is an automatic miss.
//! - Per-app single-flight: concurrent misses funnel into one fetch. WeChat
//!   rate-limits token issuance and concurrent fetches can invalidate each
//!   other's tokens (for accounts without an IP whitelist) — a stampede is a
//!   real outage vector, not a theoretical one.
//! - `std::time::Instant` (never tokio time): the publish tool runs this
//!   inside its own `new_current_thread` runtime via `block_on`, so the cache
//!   must be runtime-agnostic.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use types::error::{CarrierError, CarrierResult};

/// Early-expiry margin (refresh 300s before actual expiry), matching the old
/// channel-side cache behavior.
const TOKEN_MARGIN_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub(crate) struct CachedToken {
    pub secret_hash: u64,
    pub token: String,
    pub expires_at: Instant,
}

static CACHE: std::sync::LazyLock<DashMap<String, CachedToken>> =
    std::sync::LazyLock::new(DashMap::new);

static INFLIGHT: std::sync::LazyLock<DashMap<String, Arc<tokio::sync::Mutex<()>>>> =
    std::sync::LazyLock::new(DashMap::new);

pub(crate) fn hash_secret(secret: &str) -> u64 {
    let mut h = DefaultHasher::new();
    secret.hash(&mut h);
    h.finish()
}

/// Pure cache-hit decision, factored out for unit testing without HTTP:
/// a cached token is valid only for the same secret and before `now`.
pub(crate) fn cached_token(
    cache: &DashMap<String, CachedToken>,
    app_id: &str,
    secret_hash: u64,
    now: Instant,
) -> Option<String> {
    let entry = cache.get(app_id)?;
    if entry.secret_hash == secret_hash && entry.expires_at > now {
        Some(entry.token.clone())
    } else {
        None
    }
}

/// Get a valid access_token for `app_id`/`app_secret`, refreshing only when
/// the cached one is absent, expired, or was issued for a different secret.
pub async fn get_token(
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
) -> CarrierResult<String> {
    let secret_hash = hash_secret(app_secret);
    let now = Instant::now();
    if let Some(tok) = cached_token(&CACHE, app_id, secret_hash, now) {
        return Ok(tok);
    }

    // Single-flight per app_id: everyone funneling here waits on one fetcher;
    // after the lock, re-check (the winner may have refreshed while we waited).
    let lock = INFLIGHT
        .entry(app_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone();
    let _guard = lock.lock().await;

    let now = Instant::now();
    if let Some(tok) = cached_token(&CACHE, app_id, secret_hash, now) {
        return Ok(tok);
    }

    tracing::debug!(app_id, "stable_token cache miss, fetching");
    let resp = crate::api::get_access_token(http, app_id, app_secret).await?;
    let token = resp.access_token.ok_or_else(|| {
        CarrierError::Network(format!(
            "no access_token in stable_token response (errcode={:?}, errmsg={:?})",
            resp.errcode, resp.errmsg
        ))
    })?;
    let expires_in = resp.expires_in.unwrap_or(7200);
    let margin = expires_in.saturating_sub(TOKEN_MARGIN_SECS);
    CACHE.insert(
        app_id.to_string(),
        CachedToken {
            secret_hash,
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(margin),
        },
    );
    Ok(token)
}

/// Drop the cached token for `app_id` (call on 40001 invalid-credential
/// errors so the next [`get_token`] refetches).
pub fn invalidate(app_id: &str) {
    CACHE.remove(app_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_with(
        app_id: &str,
        secret: &str,
        token: &str,
        ttl: Duration,
    ) -> DashMap<String, CachedToken> {
        let m = DashMap::new();
        m.insert(
            app_id.to_string(),
            CachedToken {
                secret_hash: hash_secret(secret),
                token: token.to_string(),
                expires_at: Instant::now() + ttl,
            },
        );
        m
    }

    #[test]
    fn hit_when_same_secret_and_unexpired() {
        let m = cache_with("wxAAA", "s1", "TOK", Duration::from_secs(60));
        assert_eq!(
            cached_token(&m, "wxAAA", hash_secret("s1"), Instant::now()),
            Some("TOK".to_string())
        );
    }

    #[test]
    fn miss_when_expired() {
        let m = cache_with("wxAAA", "s1", "TOK", Duration::from_secs(0));
        assert_eq!(
            cached_token(&m, "wxAAA", hash_secret("s1"), Instant::now()),
            None
        );
    }

    /// A rotated secret is an automatic miss — the cached token was issued
    /// for different credentials and must not be reused.
    #[test]
    fn miss_when_secret_rotated() {
        let m = cache_with("wxAAA", "old-secret", "TOK", Duration::from_secs(60));
        assert_eq!(
            cached_token(&m, "wxAAA", hash_secret("new-secret"), Instant::now()),
            None
        );
    }

    #[test]
    fn miss_when_absent() {
        let m: DashMap<String, CachedToken> = DashMap::new();
        assert_eq!(cached_token(&m, "wxAAA", 1, Instant::now()), None);
    }
}
