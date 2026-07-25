//! DingTalk OAuth access token management.
//!
//! Fetches and caches the accessToken (with early refresh).
//! Uses POST `/v1.0/oauth2/accessToken`.
//!
//! Caching/refresh mechanics live in `channels_common::get_cached_token`; this
//! struct only holds the DingTalk credentials + HTTP client and supplies the
//! platform-specific fetch call.

use crate::api;
use crate::models::*;
use channels_common::{get_cached_token, CachedToken};
use reqwest::Client;
use std::sync::Mutex;
use std::time::Duration;
use tracing::info;

/// Thread-safe cache for a DingTalk app's access token.
pub struct AccessTokenCache {
    app_key: String,
    app_secret: String,
    http: Client,
    token: Mutex<Option<CachedToken>>,
}

impl AccessTokenCache {
    pub fn new(app_key: String, app_secret: String) -> Self {
        Self {
            app_key,
            app_secret,
            http: Client::new(),
            token: Mutex::new(None),
        }
    }

    /// Get a valid access token, refreshing if necessary.
    pub async fn get_token(&self) -> Result<String, String> {
        let http = self.http.clone();
        let app_key = self.app_key.clone();
        let app_secret = self.app_secret.clone();
        get_cached_token(&self.token, Duration::from_secs(TOKEN_REFRESH_AHEAD_SECS), move || async move {
            let resp = api::get_access_token(&http, &app_key, &app_secret).await?;
            let token = resp
                .access_token
                .ok_or("Missing accessToken in DingTalk OAuth response")?;
            let expire_secs = resp.expire_in.unwrap_or(7200);
            info!(app_key = %app_key, expire_secs, "Refreshed DingTalk access token");
            Ok((token, expire_secs))
        })
        .await
    }

    pub fn http(&self) -> &Client {
        &self.http
    }

    pub fn app_key(&self) -> &str {
        &self.app_key
    }

    pub fn app_secret(&self) -> &str {
        &self.app_secret
    }
}
