//! Feishu/Lark API client (multi-tenant).
//!
//! Each tool call carries its own `app_id` / `app_secret`, allowing a single
//! MCP server process to serve multiple Feishu apps simultaneously.
//! Tenant access tokens are cached per `app_id` and auto-refreshed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

const FEISHU_API_BASE: &str = "https://open.feishu.cn";

/// Refresh the token this many seconds before it actually expires.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Multi-tenant Feishu API client.  Token cache keyed by `app_id`.
#[derive(Clone)]
pub struct FeishuClient {
    http: reqwest::Client,
    /// app_id → CachedToken
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
}

struct CachedToken {
    access_token: String,
    secret: String,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// Feishu JSON response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    code: Option<i64>,
    msg: Option<String>,
    tenant_access_token: Option<String>,
    expire: Option<u64>,
}

// ---------------------------------------------------------------------------
// Impl
// ---------------------------------------------------------------------------

impl FeishuClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Obtain a valid tenant_access_token for the given app, refreshing when needed.
    pub async fn get_token(&self, app_id: &str, app_secret: &str) -> Result<String> {
        // Fast path — cached and not about to expire AND secret unchanged.
        {
            let guard = self.tokens.lock().await;
            if let Some(cached) = guard.get(app_id) {
                if cached.secret == app_secret
                    && cached.expires_at > Instant::now() + TOKEN_EXPIRY_MARGIN
                {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Slow path — hit the Feishu API.
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            FEISHU_API_BASE
        );
        let body = serde_json::json!({
            "app_id": app_id,
            "app_secret": app_secret,
        });
        let resp: TokenResponse = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .json()
            .await?;

        if let Some(code) = resp.code {
            if code != 0 {
                bail!(
                    "Feishu token error {}: {}",
                    code,
                    resp.msg.unwrap_or_default()
                );
            }
        }

        let access_token = resp
            .tenant_access_token
            .context("no tenant_access_token in response")?;
        let expire = resp.expire.unwrap_or(7200);

        {
            let mut guard = self.tokens.lock().await;
            guard.insert(
                app_id.to_string(),
                CachedToken {
                    access_token: access_token.clone(),
                    secret: app_secret.to_string(),
                    expires_at: Instant::now() + Duration::from_secs(expire),
                },
            );
        }

        Ok(access_token)
    }

    /// GET request with Bearer token.
    pub async fn api_get(
        &self,
        app_id: &str,
        app_secret: &str,
        path: &str,
        query: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.api_request(app_id, app_secret, reqwest::Method::GET, path, query, None)
            .await
    }

    /// POST request with Bearer token and JSON body.
    pub async fn api_post(
        &self,
        app_id: &str,
        app_secret: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.api_request(
            app_id,
            app_secret,
            reqwest::Method::POST,
            path,
            None,
            Some(body),
        )
        .await
    }

    /// Generic API request that returns parsed JSON.
    pub async fn api_request(
        &self,
        app_id: &str,
        app_secret: &str,
        method: reqwest::Method,
        path: &str,
        query: Option<&serde_json::Value>,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let raw = self
            .api_request_raw(app_id, app_secret, method, path, query, body)
            .await?;
        let json: serde_json::Value =
            serde_json::from_str(&raw).context("Feishu API response is not valid JSON")?;
        check_feishu_error(&json)?;
        Ok(json)
    }

    /// Generic API request that returns raw text.
    pub async fn api_request_raw(
        &self,
        app_id: &str,
        app_secret: &str,
        method: reqwest::Method,
        path: &str,
        query: Option<&serde_json::Value>,
        body: Option<&serde_json::Value>,
    ) -> Result<String> {
        let token = self.get_token(app_id, app_secret).await?;
        let url = format!("{}/{}", FEISHU_API_BASE, path);

        let mut req = self
            .http
            .request(method, &url)
            .header("Authorization", format!("Bearer {}", token))
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

        let resp = req.send().await?;
        let status = resp.status();
        let text = resp.text().await?;

        if !status.is_success() {
            bail!(
                "Feishu API HTTP {}: {}",
                status,
                &text[..text.len().min(500)]
            );
        }

        Ok(text)
    }
}

/// Check for `code != 0` in a Feishu JSON response.
fn check_feishu_error(json: &serde_json::Value) -> Result<()> {
    if let Some(code) = json.get("code").and_then(|v| v.as_i64()) {
        if code != 0 {
            let msg = json
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("Feishu API error {}: {}", code, msg);
        }
    }
    Ok(())
}
