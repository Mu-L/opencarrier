//! Feishu/Lark REST API client.
//!
//! Stateless async functions for: token acquisition, message send/reply, WS endpoint, media download.

use crate::models::*;
use reqwest::{header::HeaderMap, Client};
use std::time::Duration;

/// Build standard Feishu API headers with Bearer token.
fn feishu_headers(token: &str) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert("Content-Type", "application/json".parse().unwrap());
    h.insert("Authorization", format!("Bearer {token}").parse().unwrap());
    h
}

/// POST `/open-apis/auth/v3/tenant_access_token/internal`
///
/// Exchange app_id/app_secret for a tenant_access_token (2h validity).
pub async fn get_tenant_token(
    http: &Client,
    base: &str,
    app_id: &str,
    app_secret: &str,
) -> Result<TenantTokenResponse, String> {
    let url = format!("{base}/open-apis/auth/v3/tenant_access_token/internal");
    let body = TenantTokenRequest {
        app_id: app_id.to_string(),
        app_secret: app_secret.to_string(),
    };

    let resp = http
        .post(&url)
        .json(&body)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("Feishu token request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Feishu token HTTP {status}: {body}"));
    }

    resp.json::<TenantTokenResponse>()
        .await
        .map_err(|e| format!("Feishu token parse error: {e}"))
}

/// POST `/open-apis/im/v1/messages`
///
/// Send a message to a chat or user.
pub async fn send_message(
    http: &Client,
    token: &str,
    base: &str,
    receive_id: &str,
    receive_id_type: &str,
    msg_type: &str,
    content: &str,
) -> Result<SendMessageResponse, String> {
    let url = format!("{base}/open-apis/im/v1/messages?receive_id_type={receive_id_type}");
    let body = SendMessageRequest {
        receive_id: receive_id.to_string(),
        msg_type: msg_type.to_string(),
        content: content.to_string(),
    };

    let resp = http
        .post(&url)
        .headers(feishu_headers(token))
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Feishu send_message request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Feishu send_message HTTP {status}: {body}"));
    }

    resp.json::<SendMessageResponse>()
        .await
        .map_err(|e| format!("Feishu send_message parse error: {e}"))
}

/// POST `/callback/ws/endpoint`
///
/// Get the WebSocket URL for long-connection event subscription.
/// Uses AppID + AppSecret in body (NOT Bearer token auth).
pub async fn get_ws_endpoint(
    http: &Client,
    app_id: &str,
    app_secret: &str,
    base: &str,
) -> Result<WsEndpointResponse, String> {
    let url = format!("{base}/callback/ws/endpoint");
    let body = serde_json::json!({
        "AppID": app_id,
        "AppSecret": app_secret,
    });

    tracing::info!(
        url = %url,
        app_id = %app_id,
        "Calling Feishu ws/endpoint"
    );

    let resp = http
        .post(&url)
        .header("Content-Type", "application/json")
        .header("locale", "zh")
        .json(&body)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("Feishu ws/endpoint request failed: {e}"))?;

    let status = resp.status();
    let body_text = resp.text().await.unwrap_or_default();

    tracing::info!(
        status = %status,
        body = %body_text,
        "Feishu ws/endpoint response"
    );

    if !status.is_success() {
        return Err(format!("Feishu ws/endpoint HTTP {status}: {body_text}"));
    }

    serde_json::from_str(&body_text).map_err(|e| format!("Feishu ws/endpoint parse error: {e}"))
}

/// GET `/open-apis/im/v1/images/{image_key}`
///
/// Download an image by its key. Returns raw bytes.
pub async fn download_image(
    http: &Client,
    token: &str,
    base: &str,
    image_key: &str,
) -> Result<Vec<u8>, String> {
    let url = format!("{base}/open-apis/im/v1/images/{image_key}");
    let resp = http
        .get(&url)
        .headers(feishu_headers(token))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Feishu download_image request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Feishu download_image HTTP {status}: {body}"));
    }

    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("Feishu download_image read error: {e}"))
}

/// GET `/open-apis/im/v1/files/{file_key}`
///
/// Download a file by its key. Returns raw bytes.
pub async fn download_file(
    http: &Client,
    token: &str,
    base: &str,
    file_key: &str,
) -> Result<Vec<u8>, String> {
    let url = format!("{base}/open-apis/im/v1/files/{file_key}");
    let resp = http
        .get(&url)
        .headers(feishu_headers(token))
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("Feishu download_file request failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Feishu download_file HTTP {status}: {body}"));
    }

    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("Feishu download_file read error: {e}"))
}
