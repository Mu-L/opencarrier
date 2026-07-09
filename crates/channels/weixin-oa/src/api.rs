//! WeChat Official Account API client.

use serde::Deserialize;
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

/// Fetch a follower's unionid via `cgi-bin/user/info`.
///
/// Returns `Ok(None)` when the user is not a follower or has no resolvable
/// unionid (errcode 0 but no unionid field). Returns `Err` on a transport or
/// API error so the caller can decide whether to retry or fall back.
pub async fn get_user_unionid(
    http: &reqwest::Client,
    access_token: &str,
    openid: &str,
) -> Result<Option<String>, String> {
    let url = format!(
        "{}/cgi-bin/user/info?access_token={}&openid={}&lang=zh_CN",
        WECHAT_API_BASE,
        access_token,
        openid
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("user/info request failed: {e}"))?;
    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("user/info parse failed: {e}"))?;
    if let Some(code) = val.get("errcode").and_then(|v| v.as_i64()) {
        if code != 0 {
            return Err(format!("user/info errcode={code}"));
        }
    }
    Ok(val
        .get("unionid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string()))
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

/// Check a WeChat API JSON response for an error errcode.
/// Returns Ok(()) if errcode==0, else Err with the message.
fn check_wechat_error(resp_text: String, label: &str) -> Result<(), String> {
    let err: WechatApiError = serde_json::from_str(&resp_text)
        .unwrap_or(WechatApiError { errcode: -1, errmsg: resp_text });
    if err.errcode != 0 {
        return Err(format!("WeChat API error {} ({})", err.errcode, err.errmsg));
    }
    let _ = label;
    Ok(())
}

/// Send a customer service image message via WeChat API.
pub async fn custom_send_image(
    http: &reqwest::Client,
    access_token: &str,
    openid: &str,
    media_id: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/cgi-bin/message/custom/send?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({
        "touser": openid,
        "msgtype": "image",
        "image": {
            "media_id": media_id,
        },
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("custom_send_image request failed: {e}"))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("custom_send_image read body failed: {e}"))?;
    check_wechat_error(resp_text, "custom_send_image")
}

/// Send a customer service mini-program card message via WeChat API.
///
/// Requires the mini-program to be linked to the same WeChat Open Platform account
/// as the official account. `mini_appid` is the mini-program's appid (not the OA's
/// appid), which is required when the OA has multiple linked mini-programs.
/// Ref: https://developers.weixin.qq.com/doc/offiaccount/Message_Management/Service_Center_messages.html#%E5%B0%8F%E7%A8%8B%E5%BA%8F%E9%A1%B5%E9%9D%A2
pub async fn custom_send_miniprogrampage(
    http: &reqwest::Client,
    access_token: &str,
    openid: &str,
    title: &str,
    pagepath: &str,
    thumb_media_id: &str,
    mini_appid: &str,
) -> Result<(), String> {
    let url = format!(
        "{}/cgi-bin/message/custom/send?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({
        "touser": openid,
        "msgtype": "miniprogrampage",
        "miniprogrampage": {
            "title": title,
            "pagepath": pagepath,
            "thumb_media_id": thumb_media_id,
            "appid": mini_appid,
        },
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("custom_send_miniprogrampage request failed: {e}"))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("custom_send_miniprogrampage read body failed: {e}"))?;
    check_wechat_error(resp_text, "custom_send_miniprogrampage")
}

/// Response from uploading permanent material.
#[derive(Debug, Deserialize)]
pub struct UploadMaterialResponse {
    #[serde(default)]
    pub media_id: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub errcode: i64,
    #[serde(default)]
    pub errmsg: String,
}

/// Upload an image to the WeChat permanent material library (`add_material`).
///
/// Permanent material does NOT expire (unlike temp media which lasts 3 days).
/// Used for fixed, reused assets like the 月卡 image.
/// Returns (media_id, optional url).
pub async fn upload_media_permanent(
    http: &reqwest::Client,
    access_token: &str,
    image_bytes: Vec<u8>,
    filename: &str,
) -> Result<(String, Option<String>), String> {
    let url = format!(
        "{}/cgi-bin/material/add_material?access_token={}&type=image",
        WECHAT_API_BASE, access_token
    );
    let part = reqwest::multipart::Part::bytes(image_bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")
        .map_err(|e| format!("invalid mime: {e}"))?;
    let form = reqwest::multipart::Form::new().part("media", part);
    let resp = http
        .post(&url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("upload_media request failed: {e}"))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("upload_media read body failed: {e}"))?;
    let parsed: UploadMaterialResponse = serde_json::from_str(&resp_text)
        .map_err(|e| format!("upload_media parse failed: {e} (body: {resp_text})"))?;
    let media_id = parsed.media_id.ok_or_else(|| {
        format!(
            "upload_media: no media_id (errcode={}, errmsg={})",
            parsed.errcode, parsed.errmsg
        )
    })?;
    Ok((media_id, parsed.url))
}

/// Create a draft article in the OA draft box (`/cgi-bin/draft/add`).
///
/// `thumb_media_id` is the cover image's permanent media_id (required for
/// publishing later — WeChat freepublish rejects drafts without a cover).
/// Returns the draft's `media_id`.
pub async fn add_draft(
    http: &reqwest::Client,
    access_token: &str,
    title: &str,
    content: &str,
    thumb_media_id: Option<&str>,
    author: Option<&str>,
    digest: Option<&str>,
) -> Result<String, String> {
    let url = format!(
        "{}/cgi-bin/draft/add?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let mut article = serde_json::json!({
        "article_type": "news",
        "title": title,
        "content": content,
        "author": author.unwrap_or(""),
        "content_source_url": "",
        "digest": digest.unwrap_or(""),
        "need_open_comment": 0,
        "only_fans_can_comment": 0,
    });
    if let Some(tid) = thumb_media_id {
        if !tid.is_empty() {
            article["thumb_media_id"] = serde_json::Value::String(tid.to_string());
        }
    }
    let body = serde_json::json!({ "articles": [article] });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("add_draft request failed: {e}"))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("add_draft read body failed: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("add_draft parse failed: {e} (body: {resp_text})"))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(format!(
            "add_draft WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        ));
    }
    v["media_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("add_draft: no media_id (body: {resp_text})"))
}

/// Submit a draft for publishing (`/cgi-bin/freepublish/submit`).
///
/// The draft MUST have a cover (thumb_media_id) or WeChat rejects the publish.
/// Returns the `publish_id` for status tracking.
pub async fn freepublish_submit(
    http: &reqwest::Client,
    access_token: &str,
    media_id: &str,
) -> Result<String, String> {
    let url = format!(
        "{}/cgi-bin/freepublish/submit?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({ "media_id": media_id });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("freepublish request failed: {e}"))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("freepublish read body failed: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("freepublish parse failed: {e} (body: {resp_text})"))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(format!(
            "freepublish WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        ));
    }
    v["publish_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("freepublish: no publish_id (body: {resp_text})"))
}

/// List permanent materials (`/cgi-bin/material/batchget_material`).
///
/// Returns `(media_id, url)` pairs. Used to pick a fallback cover from the
/// existing image library when generated-cover upload fails.
pub async fn list_materials(
    http: &reqwest::Client,
    access_token: &str,
    material_type: &str,
    offset: i64,
    count: i64,
) -> Result<Vec<(String, Option<String>)>, String> {
    let url = format!(
        "{}/cgi-bin/material/batchget_material?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({
        "type": material_type,
        "offset": offset,
        "count": count,
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("list_materials request failed: {e}"))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| format!("list_materials read body failed: {e}"))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| format!("list_materials parse failed: {e} (body: {resp_text})"))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(format!(
            "list_materials WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        ));
    }
    let items = v["item"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|it| {
                    let mid = it["media_id"].as_str()?.to_string();
                    let url = it["url"].as_str().map(|s| s.to_string());
                    Some((mid, url))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(items)
}