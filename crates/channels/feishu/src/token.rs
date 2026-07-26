//! tenant_access_token management for Feishu/Lark.
//!
//! Fetches and caches the tenant_access_token (2h validity, auto-refresh).
//! Uses POST `/open-apis/auth/v3/tenant_access_token/internal`.
//!
//! Caching/refresh mechanics live in `channels_common::get_cached_token`; this
//! struct only holds the Feishu credentials + HTTP client and supplies the
//! platform-specific fetch call.

use crate::api;
use crate::models::*;
use channels_common::{get_cached_token, CachedToken};
use reqwest::Client;
use std::sync::Mutex;
use std::time::Duration;
use tracing::info;
use types::error::{CarrierError, CarrierResult};

/// Thread-safe cache for a single tenant's access token.
pub struct BotTokenCache {
    app_id: String,
    app_secret: String,
    api_base: String,
    http: Client,
    token: Mutex<Option<CachedToken>>,
}

impl BotTokenCache {
    pub fn new(app_id: String, app_secret: String, api_base: &str) -> Self {
        Self {
            app_id,
            app_secret,
            api_base: api_base.to_string(),
            http: Client::new(),
            token: Mutex::new(None),
        }
    }

    /// Get a valid tenant_access_token, refreshing if necessary.
    pub async fn get_token(&self) -> CarrierResult<String> {
        let http = self.http.clone();
        let api_base = self.api_base.clone();
        let app_id = self.app_id.clone();
        let app_secret = self.app_secret.clone();
        get_cached_token(&self.token, Duration::from_secs(TOKEN_REFRESH_AHEAD_SECS), move || async move {
            let resp =
                api::get_tenant_token(&http, &api_base, &app_id, &app_secret).await
                .map_err(|e| e.to_string())?;
            if resp.code != 0 {
                return Err(format!(
                    "Feishu token API error: code={} msg={}",
                    resp.code, resp.msg
                ));
            }
            let token = resp
                .tenant_access_token
                .ok_or("Missing tenant_access_token in response".to_string())?;
            let expire_secs = resp.expire.unwrap_or(7200);
            info!(app_id = %app_id, expire_secs, "Refreshed Feishu tenant_access_token");
            Ok((token, expire_secs))
        })
        .await
        .map_err(CarrierError::Network)
    }

    /// Get the HTTP client (for use by api functions).
    pub fn http(&self) -> &Client {
        &self.http
    }

    /// Get the API base URL.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// Get the app_id.
    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Get the app_secret.
    pub fn app_secret(&self) -> &str {
        &self.app_secret
    }
}
