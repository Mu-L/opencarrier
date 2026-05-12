//! App-ID/Secret auth helpers for multi-tenant MCP servers.
//!
//! Each tool call carries `app_id` and `app_secret` fields, allowing one MCP
//! server to serve multiple app accounts simultaneously. Tokens are cached
//! per `app_id` and auto-refreshed before expiry.
//!
//! Used by feishu-mcp, wecom-mcp, wechat-oa-mcp, etc.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Mutex;

/// Refresh token this many seconds before actual expiry.
const EXPIRY_MARGIN: Duration = Duration::from_secs(300);

/// Trait for parameter structs that hold app_id + app_secret.
pub trait AppCredentials {
    fn app_id(&self) -> &str;
    fn app_secret(&self) -> &str;
}

type TokenFetcher = dyn Fn(&reqwest::Client, &str, &str) -> tokio::task::JoinHandle<Result<TokenResult, String>> + Send + Sync;

/// Generic multi-tenant token cache keyed by `app_id`.
///
/// Each entry stores: `(access_token, app_secret, expires_at)`.
/// The `app_secret` is stored so the cache can detect credential rotation
/// (e.g. if a different secret is passed for the same app_id, the token
/// is re-fetched instead of returning a stale one).
#[derive(Clone)]
pub struct TokenCache {
    http: reqwest::Client,
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
    token_fetcher: Arc<TokenFetcher>,
}

struct CachedToken {
    access_token: String,
    secret: String,
    expires_at: Instant,
}

/// Result of a token fetch operation.
pub struct TokenResult {
    pub access_token: String,
    pub expires_in_secs: u64,
}

impl TokenCache {
    /// Create a new token cache with a custom token fetcher.
    ///
    /// The fetcher receives `(&Client, app_id, app_secret)` and should return
    /// a `JoinHandle<Result<TokenResult, String>>`. Use `tokio::spawn` inside
    /// the closure so the fetch can be async.
    pub fn new<F>(fetcher: F) -> Self
    where
        F: Fn(&reqwest::Client, &str, &str) -> tokio::task::JoinHandle<Result<TokenResult, String>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            http: reqwest::Client::new(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            token_fetcher: Arc::new(fetcher),
        }
    }

    /// Get a valid access token for the given credentials, refreshing if needed.
    pub async fn get_token(&self, app_id: &str, app_secret: &str) -> Result<String, String> {
        // Fast path — cached, not expired, and secret unchanged.
        {
            let guard = self.tokens.lock().await;
            if let Some(cached) = guard.get(app_id) {
                if cached.secret == app_secret
                    && cached.expires_at > Instant::now() + EXPIRY_MARGIN
                {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Slow path — fetch new token.
        let handle = (self.token_fetcher)(&self.http, app_id, app_secret);
        let result = handle
            .await
            .map_err(|e| format!("Token fetch task failed: {e}"))??;

        {
            let mut guard = self.tokens.lock().await;
            guard.insert(
                app_id.to_string(),
                CachedToken {
                    access_token: result.access_token.clone(),
                    secret: app_secret.to_string(),
                    expires_at: Instant::now() + Duration::from_secs(result.expires_in_secs),
                },
            );
        }

        Ok(result.access_token)
    }

    /// Get a reference to the HTTP client (for making API calls).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}

/// Define a params struct that includes `app_id: String` and `app_secret: String`.
///
/// ```ignore
/// define_app_params!(SendMessageParams {
///     #[schemars(description = "接收者ID")]
///     receive_id: String,
/// });
/// ```
#[macro_export]
macro_rules! define_app_params {
    ($name:ident { $($field:tt)* }) => {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
        struct $name {
            #[schemars(description = "应用 App ID")]
            app_id: String,
            #[schemars(description = "应用 App Secret")]
            app_secret: String,
            $($field)*
        }

        impl $crate::app_auth::AppCredentials for $name {
            fn app_id(&self) -> &str { &self.app_id }
            fn app_secret(&self) -> &str { &self.app_secret }
        }
    };
}

/// Generic authenticated API GET request.
pub async fn api_get(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    query: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{base_url}/{path}");
    let mut req = http
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(30));

    if let Some(q) = query {
        if let Some(obj) = q.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    req = req.query(&[(k, s)]);
                } else if !v.is_null() {
                    req = req.query(&[(k, v.to_string())]);
                }
            }
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("API request failed: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("API read body failed: {e}"))?;

    if !status.is_success() {
        return Err(format!("API HTTP {status}: {}", &text[..text.len().min(500)]));
    }

    serde_json::from_str(&text).map_err(|e| format!("API JSON parse error: {e}"))
}

/// Generic authenticated API POST request with JSON body.
pub async fn api_post(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    path: &str,
    body: &Value,
) -> Result<Value, String> {
    let url = format!("{base_url}/{path}");
    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .json(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("API request failed: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("API read body failed: {e}"))?;

    if !status.is_success() {
        return Err(format!("API HTTP {status}: {}", &text[..text.len().min(500)]));
    }

    serde_json::from_str(&text).map_err(|e| format!("API JSON parse error: {e}"))
}

/// Generic authenticated API request with any method.
pub async fn api_request(
    http: &reqwest::Client,
    base_url: &str,
    token: &str,
    method: reqwest::Method,
    path: &str,
    query: Option<&Value>,
    body: Option<&Value>,
) -> Result<Value, String> {
    let url = format!("{base_url}/{path}");
    let mut req = http
        .request(method, &url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(30));

    if let Some(q) = query {
        if let Some(obj) = q.as_object() {
            for (k, v) in obj {
                if let Some(s) = v.as_str() {
                    req = req.query(&[(k, s)]);
                } else if !v.is_null() {
                    req = req.query(&[(k, v.to_string())]);
                }
            }
        }
    }

    if let Some(b) = body {
        req = req.json(b);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("API request failed: {e}"))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("API read body failed: {e}"))?;

    if !status.is_success() {
        return Err(format!("API HTTP {status}: {}", &text[..text.len().min(500)]));
    }

    serde_json::from_str(&text).map_err(|e| format!("API JSON parse error: {e}"))
}
