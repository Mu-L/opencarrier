//! WeChat Official Account API client (multi-tenant).
//!
//! Each tool call carries its own `app_id` / `app_secret`, allowing a single
//! MCP server process to serve multiple WeChat Official Accounts
//! simultaneously.  Access tokens are cached per `app_id` and auto-refreshed.
//!
//! ## Token refresh deduplication
//!
//! WeChat access_tokens are globally unique — each call to `cgi-bin/token`
//! invalidates the previous token. When multiple agent loops share the same
//! app_id, concurrent refreshes would invalidate each other's tokens (40001).
//!
//! We prevent this by tracking in-flight refresh requests: the first caller
//! performs the actual HTTP request; concurrent callers for the same app_id
//! wait for the in-flight request to complete and reuse its result.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::sync::Mutex;

const WECHAT_API_BASE: &str = "https://api.weixin.qq.com";

/// Refresh the token this many seconds before it actually expires.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Multi-tenant WeChat API client.  Token cache keyed by `app_id`.
#[derive(Clone)]
pub struct WeChatClient {
    http: reqwest::Client,
    /// app_id → cached token
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
    /// app_id → in-flight refresh handle (dedup)
    in_flight: Arc<Mutex<HashMap<String, Arc<tokio::sync::Notify>>>>,
}

struct CachedToken {
    access_token: String,
    secret: String,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// WeChat JSON response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    errcode: Option<i64>,
    errmsg: Option<String>,
}

// ---------------------------------------------------------------------------
// Impl
// ---------------------------------------------------------------------------

impl WeChatClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
            in_flight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Obtain a valid access_token for the given account, refreshing when needed.
    ///
    /// Uses deduplication: if a refresh is already in-flight for this app_id,
    /// subsequent callers wait for it instead of firing a second request
    /// (which would invalidate the first token — WeChat's global uniqueness).
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

        // Dedup: check if a refresh is already in-flight for this app_id.
        let notify = {
            let mut guard = self.in_flight.lock().await;
            if let Some(existing) = guard.get(app_id) {
                // Another caller is refreshing — wait for it.
                existing.clone()
            } else {
                // We are the first — register ourselves.
                let notify = Arc::new(tokio::sync::Notify::new());
                guard.insert(app_id.to_string(), notify.clone());
                notify
            }
        };

        // If we're NOT the first caller, wait for the in-flight refresh.
        // Then check the cache again — the first caller should have populated it.
        let is_first = {
            let guard = self.in_flight.lock().await;
            guard.get(app_id).map(|n| Arc::ptr_eq(&notify, n)).unwrap_or(false)
        };

        if !is_first {
            // Wait for the in-flight refresh to complete.
            notify.notified().await;

            // Check cache — the first caller should have populated it.
            let guard = self.tokens.lock().await;
            if let Some(cached) = guard.get(app_id) {
                if cached.secret == app_secret
                    && cached.expires_at > Instant::now() + TOKEN_EXPIRY_MARGIN
                {
                    return Ok(cached.access_token.clone());
                }
            }
            // Cache still not valid (unlikely) — fall through to refresh ourselves.
            // Re-register as the first caller.
            let mut guard = self.in_flight.lock().await;
            let notify = Arc::new(tokio::sync::Notify::new());
            guard.insert(app_id.to_string(), notify.clone());
            // Continue to the slow path below.
        }

        // Slow path — hit the WeChat API (only one caller reaches here per app_id).
        let result = self.refresh_token(app_id, app_secret).await;

        // Remove in-flight marker and notify waiters.
        {
            let mut guard = self.in_flight.lock().await;
            guard.remove(app_id);
        }
        notify.notify_waiters();

        result
    }

    /// Actually call the WeChat API to get a fresh token.
    async fn refresh_token(&self, app_id: &str, app_secret: &str) -> Result<String> {
        let url = format!(
            "{}/cgi-bin/token?grant_type=client_credential&appid={}&secret={}",
            WECHAT_API_BASE, app_id, app_secret
        );
        let resp: TokenResponse = self.http.get(&url).send().await?.json().await?;

        if let Some(code) = resp.errcode {
            if code != 0 {
                bail!(
                    "WeChat token error {}: {}",
                    code,
                    resp.errmsg.unwrap_or_default()
                );
            }
        }

        let access_token = resp.access_token.context("no access_token in response")?;
        let expires_in = resp.expires_in.unwrap_or(7200);

        {
            let mut guard = self.tokens.lock().await;
            guard.insert(
                app_id.to_string(),
                CachedToken {
                    access_token: access_token.clone(),
                    secret: app_secret.to_string(),
                    expires_at: Instant::now() + Duration::from_secs(expires_in),
                },
            );
        }

        Ok(access_token)
    }

    /// GET request with auto-injected access_token.
    pub async fn api_get(
        &self,
        app_id: &str,
        app_secret: &str,
        path: &str,
        query_params: &str,
    ) -> Result<serde_json::Value> {
        let token = self.get_token(app_id, app_secret).await?;
        let url = if query_params.is_empty() {
            format!("{}{}?access_token={}", WECHAT_API_BASE, path, token)
        } else {
            format!(
                "{}{}?access_token={}&{}",
                WECHAT_API_BASE, path, token, query_params
            )
        };
        let json: serde_json::Value = self.http.get(&url).send().await?.json().await?;
        check_error(&json)?;
        Ok(json)
    }

    /// POST JSON body with auto-injected access_token.
    pub async fn api_post(
        &self,
        app_id: &str,
        app_secret: &str,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let token = self.get_token(app_id, app_secret).await?;
        let url = format!("{}{}?access_token={}", WECHAT_API_BASE, path, token);
        let json: serde_json::Value = self.http.post(&url).json(body).send().await?.json().await?;
        check_error(&json)?;
        Ok(json)
    }

    /// Download bytes from a URL. Size capped at 10 MB.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Vec<u8>> {
        // SECURITY: SSRF check before fetching external URL
        mcp_common::ssrf::check_ssrf(url)
            .map_err(|e| anyhow::anyhow!("SSRF check failed: {e}"))?;
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            bail!("HTTP {} fetching {}", resp.status(), url);
        }
        if let Some(len) = resp.content_length() {
            if len > mcp_common::path::MAX_FILE_SIZE as u64 {
                bail!("Response too large: {len} bytes (max 10 MB)");
            }
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > mcp_common::path::MAX_FILE_SIZE {
            bail!(
                "Response too large: {} bytes (max 10 MB)",
                bytes.len()
            );
        }
        Ok(bytes.to_vec())
    }

    /// Upload binary media via multipart/form-data.
    pub async fn upload_media(
        &self,
        app_id: &str,
        app_secret: &str,
        media_type: &str,
        filename: &str,
        data: &[u8],
    ) -> Result<serde_json::Value> {
        let token = self.get_token(app_id, app_secret).await?;
        let url = format!(
            "{}{}?access_token={}&type={}",
            WECHAT_API_BASE, "/cgi-bin/material/add_material", token, media_type
        );

        let mime = match media_type {
            "image" => "image/jpeg",
            "voice" => "audio/mpeg",
            "video" => "video/mp4",
            _ => "application/octet-stream",
        };
        let part = reqwest::multipart::Part::bytes(data.to_vec())
            .file_name(filename.to_string())
            .mime_str(mime)?;
        let form = reqwest::multipart::Form::new().part("media", part);

        let json: serde_json::Value = self
            .http
            .post(&url)
            .multipart(form)
            .send()
            .await?
            .json()
            .await?;
        check_error(&json)?;
        Ok(json)
    }
}

/// Check for `errcode != 0` in a WeChat JSON response.
fn check_error(json: &serde_json::Value) -> Result<()> {
    if let Some(code) = json.get("errcode").and_then(|v| v.as_i64()) {
        if code != 0 {
            let msg = json
                .get("errmsg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("WeChat API error {}: {}", code, msg);
        }
    }
    Ok(())
}
