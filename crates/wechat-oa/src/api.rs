//! WeChat Official Account API client — the single workspace copy of every
//! `api.weixin.qq.com` call (moved from `channels/weixin-oa/src/api.rs`
//! 2026-08-18; the channel crate re-exports it).
//!
//! Error convention: WeChat errcodes are embedded in `CarrierError::Network`
//! strings ("WeChat API error {errcode}: ...", "... errcode={code}") so the
//! existing string predicates (`contains("40001")`) keep working; use
//! [`extract_errcode`] when a typed code is needed.

use serde::Deserialize;
use sha1::{Digest, Sha1};
use types::error::{CarrierError, CarrierResult};

const WECHAT_API_BASE: &str = "https://api.weixin.qq.com";

// --- WeChat API response types (moved with the client) ---

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: Option<String>,
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WechatApiError {
    #[serde(default)]
    pub errcode: i64,
    #[serde(default)]
    pub errmsg: String,
}

impl WechatApiError {
    pub fn from_serde_json_err(e: serde_json::Error) -> Self {
        WechatApiError {
            errcode: -1,
            errmsg: e.to_string(),
        }
    }
}

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
///
/// Uses the `cgi-bin/stable_token` endpoint (not `cgi-bin/token`). The stable
/// token is NOT invalidated when another system (e.g. the chuxing backend)
/// fetches a token with `cgi-bin/token` for the same app_id — this is critical
/// when multiple services share one official account. `force_refresh=false`
/// lets WeChat return a cached stable token.
pub async fn get_access_token(
    http: &reqwest::Client,
    app_id: &str,
    app_secret: &str,
) -> CarrierResult<TokenResponse> {
    let url = format!("{}/cgi-bin/stable_token", WECHAT_API_BASE);
    let body = serde_json::json!({
        "grant_type": "client_credential",
        "appid": app_id,
        "secret": app_secret,
        "force_refresh": false,
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("get_access_token request failed: {e}")))?;
    let tok = resp
        .json::<TokenResponse>()
        .await
        .map_err(|e| CarrierError::Serialization(format!("get_access_token parse failed: {e}")))?;
    if let Some(code) = tok.errcode {
        if code != 0 {
            return Err(CarrierError::Network(format!(
                "get_access_token WeChat error {}: {}",
                code,
                tok.errmsg.as_deref().unwrap_or("?")
            )));
        }
    }
    Ok(tok)
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
) -> CarrierResult<Option<String>> {
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
        .map_err(|e| CarrierError::Network(format!("user/info request failed: {e}")))?;
    let val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CarrierError::Serialization(format!("user/info parse failed: {e}")))?;
    if let Some(code) = val.get("errcode").and_then(|v| v.as_i64()) {
        if code != 0 {
            return Err(CarrierError::Network(format!("user/info errcode={code}")));
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
) -> CarrierResult<()> {
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
        .map_err(|e| CarrierError::Network(format!("custom_send request failed: {e}")))?;
    // Parse response body as text first, then deserialize — avoids reqwest::Error vs serde_json::Error mismatch
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("custom_send read body failed: {e}")))?;
    let err: WechatApiError = serde_json::from_str(&resp_text)
        .unwrap_or(WechatApiError { errcode: -1, errmsg: resp_text });
    if err.errcode != 0 {
        return Err(CarrierError::Network(format!(
            "WeChat API error {}: {}",
            err.errcode, err.errmsg
        )));
    }
    Ok(())
}

/// Check a WeChat API JSON response for an error errcode.
/// Returns Ok(()) if errcode==0, else Err with the message.
fn check_wechat_error(resp_text: String, label: &str) -> CarrierResult<()> {
    let err: WechatApiError = serde_json::from_str(&resp_text)
        .unwrap_or(WechatApiError { errcode: -1, errmsg: resp_text });
    if err.errcode != 0 {
        return Err(CarrierError::Network(format!("WeChat API error {} ({})", err.errcode, err.errmsg)));
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
) -> CarrierResult<()> {
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
        .map_err(|e| CarrierError::Network(format!("custom_send_image request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("custom_send_image read body failed: {e}")))?;
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
) -> CarrierResult<()> {
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
        .map_err(|e| CarrierError::Network(format!("custom_send_miniprogrampage request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("custom_send_miniprogrampage read body failed: {e}")))?;
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
) -> CarrierResult<(String, Option<String>)> {
    let url = format!(
        "{}/cgi-bin/material/add_material?access_token={}&type=image",
        WECHAT_API_BASE, access_token
    );
    let part = reqwest::multipart::Part::bytes(image_bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")
        .map_err(|e| CarrierError::InvalidInput(format!("invalid mime: {e}")))?;
    let form = reqwest::multipart::Form::new().part("media", part);
    let resp = http
        .post(&url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("upload_media request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("upload_media read body failed: {e}")))?;
    let parsed: UploadMaterialResponse = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("upload_media parse failed: {e} (body: {resp_text})")))?;
    let media_id = parsed.media_id.ok_or_else(|| {
        CarrierError::Serialization(format!(
            "upload_media: no media_id (errcode={}, errmsg={})",
            parsed.errcode, parsed.errmsg
        ))
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
) -> CarrierResult<String> {
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
        .map_err(|e| CarrierError::Network(format!("add_draft request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("add_draft read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("add_draft parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "add_draft WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    v["media_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| CarrierError::Serialization(format!("add_draft: no media_id (body: {resp_text})")))
}

/// Submit a draft for publishing (`/cgi-bin/freepublish/submit`).
///
/// The draft MUST have a cover (thumb_media_id) or WeChat rejects the publish.
/// Returns the `publish_id` for status tracking.
pub async fn freepublish_submit(
    http: &reqwest::Client,
    access_token: &str,
    media_id: &str,
) -> CarrierResult<String> {
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
        .map_err(|e| CarrierError::Network(format!("freepublish request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("freepublish read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("freepublish parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "freepublish WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    v["publish_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| CarrierError::Serialization(format!("freepublish: no publish_id (body: {resp_text})")))
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
) -> CarrierResult<Vec<(String, Option<String>)>> {
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
        .map_err(|e| CarrierError::Network(format!("list_materials request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("list_materials read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("list_materials parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "list_materials WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
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

// ---------------------------------------------------------------------------
// Data-plane APIs (2026-08-18 P0 batch: follower total / publish status /
// draft inventory / template messages — all zero-LLM consumers)
// ---------------------------------------------------------------------------

/// Paged follower list (`/cgi-bin/user/get`). The FIRST page (next_openid=None)
/// already carries the official `total` — that is what the follower report and
/// the admin endpoint use; full paging (10000/page) is for future backfill.
#[derive(Debug, Clone)]
pub struct UserGetResult {
    /// Official total follower count of the account.
    pub total: i64,
    /// OpenIDs in this page.
    pub count: i64,
    /// Cursor for the next page; empty string when the list is exhausted.
    pub next_openid: String,
    /// Raw WeChat response (includes `data.openid`).
    pub raw: serde_json::Value,
}

pub async fn user_get(
    http: &reqwest::Client,
    access_token: &str,
    next_openid: Option<&str>,
) -> CarrierResult<UserGetResult> {
    let mut url = format!(
        "{}/cgi-bin/user/get?access_token={}",
        WECHAT_API_BASE, access_token
    );
    if let Some(cursor) = next_openid.filter(|s| !s.is_empty()) {
        url.push_str("&next_openid=");
        url.push_str(cursor);
    }
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("user_get request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("user_get read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("user_get parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "user_get WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(UserGetResult {
        total: v["total"].as_i64().unwrap_or(0),
        count: v["count"].as_i64().unwrap_or(0),
        next_openid: v["next_openid"].as_str().unwrap_or("").to_string(),
        raw: v,
    })
}

/// Publish status (`/cgi-bin/freepublish/get`). Raw passthrough — the caller
/// interprets `publish_status` (0/3/4/5 success, 1/2 fail, others in-flight
/// per the freepublish docs).
pub async fn freepublish_get(
    http: &reqwest::Client,
    access_token: &str,
    publish_id: &str,
) -> CarrierResult<serde_json::Value> {
    let url = format!(
        "{}/cgi-bin/freepublish/get?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({ "publish_id": publish_id });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("freepublish_get request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("freepublish_get read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("freepublish_get parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "freepublish_get WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v)
}

/// Draft box inventory (`/cgi-bin/draft/batchget`). Raw passthrough.
/// `no_content=true` skips article bodies (cheap count/list shape).
pub async fn draft_batchget(
    http: &reqwest::Client,
    access_token: &str,
    offset: u32,
    count: u32,
    no_content: bool,
) -> CarrierResult<serde_json::Value> {
    let url = format!(
        "{}/cgi-bin/draft/batchget?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({
        "offset": offset,
        "count": count,
        "no_content": if no_content { 1 } else { 0 },
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("draft_batchget request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("draft_batchget read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("draft_batchget parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "draft_batchget WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v)
}

/// Draft box count (`/cgi-bin/draft/count`).
pub async fn draft_count(http: &reqwest::Client, access_token: &str) -> CarrierResult<i64> {
    let url = format!(
        "{}/cgi-bin/draft/count?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let resp = http
        .post(&url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("draft_count request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("draft_count read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("draft_count parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "draft_count WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v["total_count"].as_i64().unwrap_or(0))
}

/// Send a template message (`/cgi-bin/message/template/send`).
///
/// Template messages have NO 48h-window limit — this is the escape hatch for
/// the customer-service-message 45015 failure path, and the carrier for
/// scheduled zero-LLM pushes to followers outside the window.
/// `data` is the per-template field map, e.g. `{"thing1": {"value": "..."}}`.
pub async fn template_send(
    http: &reqwest::Client,
    access_token: &str,
    touser: &str,
    template_id: &str,
    url: Option<&str>,
    miniprogram: Option<&serde_json::Value>,
    data: &serde_json::Value,
) -> CarrierResult<serde_json::Value> {
    let api_url = format!(
        "{}/cgi-bin/message/template/send?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let mut body = serde_json::json!({
        "touser": touser,
        "template_id": template_id,
        "data": data,
    });
    if let Some(u) = url.filter(|s| !s.is_empty()) {
        body["url"] = serde_json::Value::String(u.to_string());
    }
    if let Some(mp) = miniprogram {
        body["miniprogram"] = mp.clone();
    }
    let resp = http
        .post(&api_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("template_send request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("template_send read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("template_send parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "template_send WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v)
}

/// Template inventory (`/cgi-bin/template/get_all_private_template`).
/// Raw passthrough (`template_count` + `template_list`).
pub async fn get_all_private_template(
    http: &reqwest::Client,
    access_token: &str,
) -> CarrierResult<serde_json::Value> {
    let url = format!(
        "{}/cgi-bin/template/get_all_private_template?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("get_all_private_template request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("get_all_private_template read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("get_all_private_template parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "get_all_private_template WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v)
}

// ── Comment (留言) management ──────────────────────────────
//
// Ported from the retired wechat-oa-mcp server (2026-08-18 API-over-MCP
// convergence): the comment family was that server's only real increment
// over the core crate — the reader-comment knowledge-base plan depends on
// it. Same wire shape for all eight endpoints, so they share one POST
// spine and live behind the single token authority.

/// Shared POST + errcode-check spine (comment family, datacube, …) —
async fn wx_json_post(
    http: &reqwest::Client,
    access_token: &str,
    path: &str,
    body: serde_json::Value,
) -> CarrierResult<serde_json::Value> {
    let url = format!("{}/{path}?access_token={access_token}", WECHAT_API_BASE);
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("{path} request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("{path} read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("{path} parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "{path} WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v)
}

/// Open the comment section of a published article (`/cgi-bin/comment/open`).
/// `msg_data_id` comes from the article's publish status; `index` is the
/// article position in a multi-article post (0 = first).
pub async fn comment_open(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/open",
        serde_json::json!({ "msg_data_id": msg_data_id, "index": index }),
    )
    .await
}

/// Close the comment section (`/cgi-bin/comment/close`).
pub async fn comment_close(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/close",
        serde_json::json!({ "msg_data_id": msg_data_id, "index": index }),
    )
    .await
}

/// List reader comments (`/cgi-bin/comment/list`). `comment_type`: 0=all,
/// 1=normal, 2=featured (精选). Raw passthrough (`comment_list` + `total`).
pub async fn comment_list(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
    comment_type: u32,
    begin: u32,
    count: u32,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/list",
        serde_json::json!({
            "msg_data_id": msg_data_id,
            "index": index,
            "type": comment_type,
            "begin": begin,
            "count": count,
        }),
    )
    .await
}

/// Mark a comment as featured/精选 (`/cgi-bin/comment/markelect`).
pub async fn comment_mark_elect(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
    comment_id: i64,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/markelect",
        serde_json::json!({ "msg_data_id": msg_data_id, "index": index, "comment_id": comment_id }),
    )
    .await
}

/// Remove the featured mark (`/cgi-bin/comment/unmarkelect`).
pub async fn comment_unmark_elect(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
    comment_id: i64,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/unmarkelect",
        serde_json::json!({ "msg_data_id": msg_data_id, "index": index, "comment_id": comment_id }),
    )
    .await
}

/// Delete a comment (`/cgi-bin/comment/delete`). Irreversible.
pub async fn comment_delete(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
    comment_id: i64,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/delete",
        serde_json::json!({ "msg_data_id": msg_data_id, "index": index, "comment_id": comment_id }),
    )
    .await
}

/// Reply to a comment (`/cgi-bin/comment/reply/add`). The reply appears as
/// the account's official reply under the comment.
pub async fn comment_reply_add(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
    comment_id: i64,
    content: &str,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/reply/add",
        serde_json::json!({
            "msg_data_id": msg_data_id,
            "index": index,
            "comment_id": comment_id,
            "content": content,
        }),
    )
    .await
}

/// Delete an official reply (`/cgi-bin/comment/reply/delete`).
pub async fn comment_reply_delete(
    http: &reqwest::Client,
    access_token: &str,
    msg_data_id: i64,
    index: u32,
    comment_id: i64,
    reply_id: i64,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "cgi-bin/comment/reply/delete",
        serde_json::json!({
            "msg_data_id": msg_data_id,
            "index": index,
            "comment_id": comment_id,
            "reply_id": reply_id,
        }),
    )
    .await
}

/// Published-article inventory (`/cgi-bin/freepublish/batchget`). Raw
/// passthrough: `{total_count, item_count, item: [{article_id, content:
/// {news_item: [{title, url, ...}]}}]}`. NOTE: items do NOT carry
/// `msg_data_id` — the comment APIs need the `mid=` from each article's
/// `url` (see [`extract_mid_from_url`]).
pub async fn freepublish_batchget(
    http: &reqwest::Client,
    access_token: &str,
    offset: u32,
    count: u32,
    no_content: bool,
) -> CarrierResult<serde_json::Value> {
    let url = format!(
        "{}/cgi-bin/freepublish/batchget?access_token={}",
        WECHAT_API_BASE, access_token
    );
    let body = serde_json::json!({
        "offset": offset,
        "count": count,
        "no_content": if no_content { 1 } else { 0 },
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("freepublish_batchget request failed: {e}")))?;
    let resp_text = resp
        .text()
        .await
        .map_err(|e| CarrierError::Network(format!("freepublish_batchget read body failed: {e}")))?;
    let v: serde_json::Value = serde_json::from_str(&resp_text)
        .map_err(|e| CarrierError::Serialization(format!("freepublish_batchget parse failed: {e} (body: {resp_text})")))?;
    let errcode = v["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "freepublish_batchget WeChat error {}: {}",
            errcode,
            v["errmsg"].as_str().unwrap_or("?")
        )));
    }
    Ok(v)
}

/// Extract the comment-API `msg_data_id` from a published article's URL —
/// freepublish/batchget does not return it directly, but the article `url`
/// embeds it as the `mid=` query parameter (e.g. `...&mid=2247499040&idx=1`
/// → 2247499040). Empirically verified against the live account 2026-08-18.
pub fn extract_mid_from_url(url: &str) -> Option<i64> {
    let start = url.find("mid=")? + "mid=".len();
    let rest = &url[start..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

// ── Datacube (数据统计) ────────────────────────────────────
//
// Read/share statistics for mass-sent articles. WeChat's datacube is T+1
// (yesterday or older) and single-day per call for getarticletotal; live
// per-article read counts do not exist as a server API — only the article
// page's embedded counters show real-time numbers.

/// Fetch per-article read statistics (`POST /datacube/getarticletotal`).
///
/// `date` = `YYYY-MM-DD`, a single day (begin==end), yesterday or older.
/// Raw passthrough: `{"list": [{msg_data_id, title, details: [{stat_date,
/// int_page_read_count, int_page_read_user, ori_page_read_count,
/// share_count, ...}]}]}` — join against article URLs via
/// [`extract_mid_from_url`] (the `mid` and datacube `msg_data_id` share one
/// id space).
pub async fn article_total(
    http: &reqwest::Client,
    access_token: &str,
    date: &str,
) -> CarrierResult<serde_json::Value> {
    wx_json_post(
        http,
        access_token,
        "datacube/getarticletotal",
        serde_json::json!({ "begin_date": date, "end_date": date }),
    )
    .await
}

/// Extract a WeChat errcode from one of this module's error strings —
/// "WeChat API error 45015 (…)", "user/info errcode=40001",
/// "get_access_token WeChat error 40001: invalid credential". Returns the
/// first non-zero code found, else None. Prefer a typed check over
/// `contains("45015")`-style predicates in new code.
pub fn extract_errcode(err: &str) -> Option<i64> {
    for marker in ["errcode=", "errcode:", "error "] {
        let mut search = 0usize;
        while let Some(rel) = err[search..].find(marker) {
            let start = search + rel + marker.len();
            // Skip whitespace between the marker and the code ("errcode: 40013").
            let digits: String = err[start..]
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if let Ok(code) = digits.parse::<i64>() {
                if code != 0 {
                    return Some(code);
                }
            }
            search = start;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{extract_errcode, extract_mid_from_url};

    #[test]
    fn extract_errcode_covers_all_module_formats() {
        // custom_send / template paths: "WeChat API error {code} (...)"
        assert_eq!(
            extract_errcode("WeChat API error 45015 (response out of time limit...)"),
            Some(45015)
        );
        // labeled paths: "add_draft WeChat error {code}: {msg}"
        assert_eq!(
            extract_errcode("add_draft WeChat error 40005: invalid type"),
            Some(40005)
        );
        // get_user_unionid: "user/info errcode={code}"
        assert_eq!(extract_errcode("user/info errcode=40003"), Some(40003));
        // "errcode: 40013" form
        assert_eq!(extract_errcode("stable token errcode: 40013 bad appid"), Some(40013));
    }

    #[test]
    fn extract_errcode_handles_zero_and_absent() {
        // errcode=0 is success — keep scanning, then give up.
        assert_eq!(extract_errcode("user/info errcode=0"), None);
        assert_eq!(extract_errcode("plain transport failure"), None);
    }

    /// The comment APIs' msg_data_id hides in the article URL's mid= param —
    /// freepublish/batchget items don't carry it (verified live 2026-08-18).
    #[test]
    fn extract_mid_from_article_url() {
        let url = "http://mp.weixin.qq.com/s?__biz=MzIwOTU1Njc5Mg==&mid=2247499040&idx=1&sn=ace";
        assert_eq!(extract_mid_from_url(url), Some(2247499040));
        // mid at end of string, other query params before it.
        assert_eq!(extract_mid_from_url("https://x/s?idx=1&mid=99"), Some(99));
        // No mid param / non-numeric -> None.
        assert_eq!(extract_mid_from_url("https://x/s?idx=1&sn=ab"), None);
        assert_eq!(extract_mid_from_url("https://x/s?mid=abc"), None);
    }
}
