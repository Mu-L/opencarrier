//! WeChat Official Account API client.

use sha1::{Digest, Sha1};

use crate::models::{TokenResponse, WechatApiError};

const WECHAT_API_BASE: &str = "https://api.weixin.qq.com";

/// Verify a WeChat callback signature (checkSign).
///
/// Sorts token + timestamp + nonce lexicographically, concatenates, and
/// SHA-1 hashes the result. Returns true if it matches the provided signature.
pub fn check_sign(token: &str, timestamp: &str, nonce: &str, signature: &str) -> bool {
    let mut parts: [&str; 3] = [token, timestamp, nonce];
    parts.sort_unstable();
    let joined = parts.concat();
    let mut hasher = Sha1::new();
    hasher.update(joined.as_bytes());
    let hash = hasher.finalize();
    let computed = hex::encode(hash);
    computed == signature
}

/// Get an access_token for the given app_id/app_secret.
pub async fn get_access_token(
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
) -> Result<TokenResponse, String> {
    let url = format!(
        "{}/cgi-bin/token?grant_type=client_credential&appid={}&secret={}",
        WECHAT_API_BASE, app_id, app_secret
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("get_access_token request failed: {e}"))?;
    let body = resp
        .json::<TokenResponse>()
        .await
        .map_err(|e| format!("get_access_token parse failed: {e}"))?;
    Ok(body)
}

/// Send a customer service text message via WeChat API.
pub async fn custom_send_text(
    http: &reqwest::Client,
    access_token: &str,
    openid: &str,
    text: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/cgi-bin/message/custom/send?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({
        "touser": openid,
        "msgtype": "text",
        "text": {
            "content": text,
        },
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("custom_send request failed: {e}"))?;
    // Parse response body as text first, then deserialize — avoids reqwest::Error vs serde_json::Error mismatch
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("custom_send read body failed: {e}"))?;
    let err: WechatApiError = serde_json::from_str(&resp_text)
        .unwrap_or(WechatApiError { errcode: -1, errmsg: resp_text });
    if err.errcode != 0 {
        return Err(format!(
            "WeChat API error {}: {}",
            err.errcode, err.errmsg
        ));
    }
    Ok(())
}