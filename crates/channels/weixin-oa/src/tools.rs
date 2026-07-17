//! WeChat Official Account plugin tools — built-in, no FFI.

use serde_json::Value;
use tracing::{info, warn};
use types::plugin::PluginToolContext;
use types::tool::{PluginToolDef, PluginToolError, ToolProvider};

use crate::api;
use crate::channel::WEIXIN_OA_STATE;

/// Resolve a path that may be absolute or relative to `~/.opencarrier`.
fn resolve_path(p: &str) -> std::path::PathBuf {
    if p.starts_with('/') {
        std::path::PathBuf::from(p)
    } else {
        types::config::home_dir().join(p)
    }
}

/// Returns true if the error indicates an expired/invalid access_token (WeChat errcode 40001).
pub(crate) fn is_token_expired(err: &str) -> bool {
    err.contains("40001")
}

/// Get a fresh access_token. If a prior call failed with 40001, call this to
/// invalidate the cache and get a new token for one retry.
pub(crate) async fn refresh_token(account: &crate::channel::OaAccountState) -> Result<String, String> {
    account.invalidate_token().await;
    account.get_token().await
}

/// Reject if the recipient is a WeCom kf customer (external_userid starts with
/// "wm") — weixin_oa_* tools only work for OA followers (openid starts "o").
/// Tells the agent to switch to the corresponding wecom_send_* tool.
fn reject_if_wecom_kf(context: &PluginToolContext) -> Result<(), PluginToolError> {
    if context.sender_id.starts_with("wm") {
        return Err(PluginToolError::tool(
            "这是企业微信客服用户(external_userid 以 wm 开头)，weixin_oa_* 只适用于公众号粉丝(openid 以 o 开头)，会 40003 invalid openid。请改用对应的 wecom_send_* 工具（wecom_send_image / wecom_send_miniprogram 等）",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Send image tool
// ---------------------------------------------------------------------------

pub struct WeixinOaSendImageTool;

impl ToolProvider for WeixinOaSendImageTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "weixin_oa_send_image".to_string(),
            description: "Send an image to the current WeChat Official Account user (the person you are replying to) via the customer service message API. The user_id/app_id are resolved automatically from the conversation context. Provide either a pre-uploaded permanent media_id, OR a file path (absolute, or relative to the home dir) to upload on the fly. NOTE: can only reply to users who sent a message within the last 48 hours.".to_string(),
            parameters_json: r#"{"type":"object","properties":{"media_id":{"type":"string","description":"A pre-uploaded permanent material media_id (preferred for fixed assets like the 月卡 image)"},"file_path":{"type":"string","description":"Path to an image file to upload then send. Absolute path or relative to ~/.opencarrier. Used when no media_id is available."}},"required":[]}"#.to_string(),
        }
    }

    fn execute(
        &self,
        args: &Value,
        context: &PluginToolContext,
    ) -> Result<String, PluginToolError> {
        reject_if_wecom_kf(context)?;
        // bot_id = app_id, sender_id = user's openid — both come from context
        let app_id = if context.bot_id.is_empty() {
            return Err(PluginToolError::tool(
                "no bot_id (app_id) in context — tool can only be used inside a weixin-oa conversation",
            ));
        } else {
            &context.bot_id
        };
        let openid = if context.sender_id.is_empty() {
            return Err(PluginToolError::tool(
                "no sender_id (openid) in context — cannot determine recipient",
            ));
        } else {
            &context.sender_id
        };

        let media_id = args["media_id"].as_str();
        let file_path = args["file_path"].as_str();
        if media_id.is_none() && file_path.is_none() {
            return Err(PluginToolError::tool(
                "must provide either media_id or file_path",
            ));
        }

        // Resolve the OA account state for this app_id
        let account = WEIXIN_OA_STATE
            .accounts
            .get(app_id)
            .map(|a| a.clone())
            .ok_or_else(|| {
                PluginToolError::tool(format!(
                    "no weixin-oa account registered for app_id {app_id}"
                ))
            })?;

        let openid = openid.to_string();
        let media_id = media_id.map(|s| s.to_string());
        let file_path = file_path.map(|s| s.to_string());

        // Tool::execute is sync — run the async send on a dedicated runtime
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("runtime error: {e}")))?;

        rt.block_on(async move {
            let mut token = account.get_token().await.map_err(PluginToolError::tool)?;

            let final_media_id = if let Some(mid) = media_id {
                mid
            } else {
                // Upload the file as permanent material
                let path = file_path.unwrap();
                let resolved = if path.starts_with('/') {
                    std::path::PathBuf::from(path)
                } else {
                    // relative to ~/.opencarrier
                    let home = types::config::home_dir();
                    home.join(&path)
                };
                let bytes = std::fs::read(&resolved).map_err(|e| {
                    PluginToolError::tool(format!("failed to read image {resolved:?}: {e}"))
                })?;
                let filename = resolved
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("image.png")
                    .to_string();
                let (mid, _url) = api::upload_media_permanent(
                    &account.http,
                    &token,
                    bytes,
                    &filename,
                )
                .await
                .map_err(PluginToolError::tool)?;
                mid
            };

            let result = api::custom_send_image(&account.http, &token, &openid, &final_media_id).await;
            if let Err(e) = result {
                if is_token_expired(&e) {
                    token = refresh_token(&account).await.map_err(PluginToolError::tool)?;
                    api::custom_send_image(&account.http, &token, &openid, &final_media_id).await
                        .map_err(PluginToolError::tool)?;
                } else {
                    return Err(PluginToolError::tool(e));
                }
            }

            Ok(format!(
                "Image sent to user {openid} (media_id={final_media_id})"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Send mini-program card tool
// ---------------------------------------------------------------------------

pub struct WeixinOaSendMiniprogramTool;

impl ToolProvider for WeixinOaSendMiniprogramTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "weixin_oa_send_miniprogram".to_string(),
            description: "Send a mini-program card to the current WeChat Official Account user (the person you are replying to) via the customer service message API. The mini-program must be linked to the same WeChat Open Platform account. user_id/app_id are resolved automatically from context.".to_string(),
            parameters_json: r#"{"type":"object","properties":{"title":{"type":"string","description":"Card title text displayed in the mini-program card"},"pagepath":{"type":"string","description":"Mini-program page path with params, e.g. pages/index/type/type?id=883"},"mini_appid":{"type":"string","description":"The mini-program appid (e.g. wx7c62aa603ab603f4)"},"thumb_media_id":{"type":"string","description":"A pre-uploaded permanent material media_id for the card cover image. Use this OR thumb_url, not both."},"thumb_url":{"type":"string","description":"URL of the cover image to download and upload. Use this OR thumb_media_id, not both."}},"required":["title","pagepath","mini_appid"]}"#.to_string(),
        }
    }

    fn execute(
        &self,
        args: &Value,
        context: &PluginToolContext,
    ) -> Result<String, PluginToolError> {
        reject_if_wecom_kf(context)?;
        let openid = if context.sender_id.is_empty() {
            return Err(PluginToolError::tool(
                "no sender_id (openid) in context — cannot determine recipient",
            ));
        } else {
            &context.sender_id
        };

        let title = args["title"].as_str().ok_or_else(|| {
            PluginToolError::tool("missing required parameter: title")
        })?;
        let pagepath = args["pagepath"].as_str().ok_or_else(|| {
            PluginToolError::tool("missing required parameter: pagepath")
        })?;
        let mini_appid = args["mini_appid"].as_str().ok_or_else(|| {
            PluginToolError::tool("missing required parameter: mini_appid")
        })?;
        let thumb_media_id = args["thumb_media_id"].as_str();
        let thumb_url = args["thumb_url"].as_str();
        if thumb_media_id.is_none() && thumb_url.is_none() {
            return Err(PluginToolError::tool(
                "must provide either thumb_media_id or thumb_url",
            ));
        }

        // Resolve the OA account: use context bot_id (app_id), or fall back to
        // the only registered account when invoked without an inbound message
        // context (single-OA deployments).
        let account = if !context.bot_id.is_empty() {
            WEIXIN_OA_STATE
                .accounts
                .get(&context.bot_id)
                .map(|a| a.clone())
                .ok_or_else(|| {
                    PluginToolError::tool(format!(
                        "no weixin-oa account registered for app_id {}",
                        context.bot_id
                    ))
                })?
        } else {
            WEIXIN_OA_STATE
                .accounts
                .iter()
                .next()
                .map(|e| e.value().clone())
                .ok_or_else(|| {
                    PluginToolError::tool(
                        "no bot_id (app_id) in context and no weixin-oa account registered",
                    )
                })?
        };

        let openid = openid.to_string();
        let title = title.to_string();
        let pagepath = pagepath.to_string();
        let mini_appid = mini_appid.to_string();
        let thumb_media_id = thumb_media_id.map(|s| s.to_string());
        let thumb_url = thumb_url.map(|s| s.to_string());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("runtime error: {e}")))?;

        rt.block_on(async move {
            let mut token = account.get_token().await.map_err(PluginToolError::tool)?;

            let final_thumb_media_id = if let Some(mid) = thumb_media_id {
                mid
            } else {
                // Download image from thumb_url and upload as permanent material
                let url = thumb_url.unwrap();
                let resp = account.http.get(&url).send().await.map_err(|e| {
                    PluginToolError::tool(format!("failed to download thumb_url: {e}"))
                })?;
                let bytes = resp.bytes().await.map_err(|e| {
                    PluginToolError::tool(format!("failed to read thumb_url body: {e}"))
                })?;
                let filename = url
                    .rsplit('/')
                    .next()
                    .unwrap_or("image.png")
                    .to_string();
                let (mid, _url) = api::upload_media_permanent(
                    &account.http, &token, bytes.to_vec(), &filename,
                )
                .await
                .map_err(PluginToolError::tool)?;
                mid
            };

            let result = api::custom_send_miniprogrampage(
                &account.http, &token, &openid, &title, &pagepath, &final_thumb_media_id, &mini_appid,
            ).await;
            if let Err(e) = result {
                if is_token_expired(&e) {
                    token = refresh_token(&account).await.map_err(PluginToolError::tool)?;
                    api::custom_send_miniprogrampage(
                        &account.http, &token, &openid, &title, &pagepath, &final_thumb_media_id, &mini_appid,
                    ).await.map_err(PluginToolError::tool)?;
                } else {
                    return Err(PluginToolError::tool(e));
                }
            }

            Ok(format!(
                "Mini-program card sent to user {openid} (pagepath={pagepath})"
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// Publish article tool (AI + API pattern — no MCP)
// ---------------------------------------------------------------------------

/// Publish a formatted HTML article to a WeChat OA: resolve a cover, create a
/// draft, and optionally submit it for publishing. Driven by the bridge's
/// `[PUBLISH:app_id]` marker handler, so no agent tool-chain is involved.
pub struct WeixinOaPublishArticleTool;

impl ToolProvider for WeixinOaPublishArticleTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "weixin_oa_publish_article".to_string(),
            description: "Publish a formatted HTML article to a WeChat Official Account: resolve a cover (upload the given cover_path, else fall back to the first image in the material library), create a draft, and optionally submit it for publishing. Credentials are resolved from the registered OA account for app_id.".to_string(),
            parameters_json: r#"{"type":"object","properties":{"app_id":{"type":"string","description":"Target OA app_id"},"html_path":{"type":"string","description":"Path to the WeChat-ready HTML article (absolute or relative to ~/.opencarrier)"},"title":{"type":"string","description":"Article title"},"cover_path":{"type":"string","description":"Optional path to a generated cover image. If omitted/upload fails, falls back to the first image in the material library."},"publish":{"type":"boolean","default":true,"description":"Submit the draft for publishing immediately after creation."},"digest":{"type":"string","description":"Optional article digest/summary (摘要). If omitted, WeChat auto-extracts from the article beginning."}},"required":["app_id","html_path","title"]}"#.to_string(),
        }
    }

    fn execute(&self, args: &Value, _context: &PluginToolContext) -> Result<String, PluginToolError> {
        let app_id = args["app_id"]
            .as_str()
            .ok_or_else(|| PluginToolError::tool("missing app_id"))?
            .to_string();
        // app_secret comes from the user's own profile (multi-user: each user's
        // OA credentials live in their own directory). Required — without it we
        // can't get an access_token.
        let app_secret = args["app_secret"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                PluginToolError::tool(
                    "用户资料里没有这个公众号的凭证(app_secret 缺失),请先把公众号 app_id+app_secret 告诉我,我存到你资料里后再发".to_string(),
                )
            })?
            .to_string();
        let html_path = args["html_path"]
            .as_str()
            .ok_or_else(|| PluginToolError::tool("missing html_path"))?
            .to_string();
        let title = args["title"]
            .as_str()
            .ok_or_else(|| PluginToolError::tool("missing title"))?
            .to_string();
        let cover_path = args["cover_path"].as_str().map(|s| s.to_string());
        let publish = args["publish"].as_bool().unwrap_or(true);
        let digest = args["digest"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());

        // Build a fresh HTTP client and fetch an access_token directly from the
        // user-supplied app_id/app_secret — no server-level WEIXIN_OA_STATE
        // registration needed (publish is outbound, per-user).
        let http = reqwest::Client::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("runtime error: {e}")))?;

        rt.block_on(async move {
            let mut token = api::get_access_token(&http, &app_id, &app_secret)
                .await
                .map_err(PluginToolError::tool)?
                .access_token
                .ok_or_else(|| PluginToolError::tool("get_access_token returned no access_token"))?;

            // --- Resolve cover (mandatory — WeChat publish requires one) ---
            // Tier a: upload the generated cover_path. Tier b: first image in
            // the material library. Both fail → abort (no coverless publish).
            let mut thumb_media_id: Option<String> = None;
            let mut cover_source = "none";

            if let Some(cp) = &cover_path {
                let resolved = resolve_path(cp);
                match std::fs::read(&resolved) {
                    Ok(bytes) => {
                        let filename = resolved
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("cover.png")
                            .to_string();
                        match api::upload_media_permanent(&http, &token, bytes, &filename).await {
                            Ok((mid, _)) => {
                                thumb_media_id = Some(mid);
                                cover_source = "generated";
                            }
                            Err(e) => warn!(error = %e, cover = %resolved.display(), "cover upload failed, falling back to material library"),
                        }
                    }
                    Err(e) => warn!(error = %e, cover = %resolved.display(), "cover file unreadable, falling back to material library"),
                }
            }

            if thumb_media_id.is_none() {
                match api::list_materials(&http, &token, "image", 0, 1).await {
                    Ok(items) => {
                        if let Some((mid, _url)) = items.first() {
                            thumb_media_id = Some(mid.clone());
                            cover_source = "library";
                            info!(media_id = %mid, "Using material-library image as cover");
                        }
                    }
                    Err(e) => warn!(error = %e, "list_materials cover fallback failed"),
                }
            }

            let thumb = thumb_media_id.ok_or_else(|| {
                PluginToolError::tool(
                    "封面生成失败且素材库无可用图片,无法发布(WeChat 发布必须有封面)".to_string(),
                )
            })?;

            // --- Read article HTML ---
            let resolved_html = resolve_path(&html_path);
            let content = std::fs::read_to_string(&resolved_html)
                .map_err(|e| PluginToolError::tool(format!("failed to read article {resolved_html:?}: {e}")))?;

            // --- Create draft (token retry on 40001) ---
            let draft_media_id = match api::add_draft(
                &http, &token, &title, &content, Some(&thumb), None, digest.as_deref(),
            )
            .await
            {
                Ok(mid) => mid,
                Err(e) if is_token_expired(&e) => {
                    token = api::get_access_token(&http, &app_id, &app_secret)
                        .await
                        .map_err(PluginToolError::tool)?
                        .access_token
                        .ok_or_else(|| PluginToolError::tool("get_access_token returned no access_token"))?;
                    api::add_draft(&http, &token, &title, &content, Some(&thumb), None, digest.as_deref())
                        .await
                        .map_err(PluginToolError::tool)?
                }
                Err(e) => return Err(PluginToolError::tool(e)),
            };
            info!(draft_media_id = %draft_media_id, "Draft created");

            // --- Publish (token retry on 40001) ---
            // Soft-fail: if the draft was created but freepublish fails (e.g.
            // 48001 "api unauthorized" — account isn't a verified service
            // account), return the draft media_id + the error so the caller
            // can tell the user to publish manually from the OA backend. Don't
            // discard the successfully-created draft by hard-erroring.
            let mut publish_id = None;
            let mut publish_error = None;
            if publish {
                match api::freepublish_submit(&http, &token, &draft_media_id).await {
                    Ok(pid) => publish_id = Some(pid),
                    Err(e) if is_token_expired(&e) => {
                        match api::get_access_token(&http, &app_id, &app_secret).await {
                            Ok(resp) => match resp.access_token {
                                Some(new_tok) => {
                                    match api::freepublish_submit(&http, &new_tok, &draft_media_id).await {
                                        Ok(pid) => publish_id = Some(pid),
                                        Err(e2) => publish_error = Some(e2),
                                    }
                                }
                                None => publish_error = Some("get_access_token returned no access_token".to_string()),
                            },
                            Err(e2) => publish_error = Some(e2.to_string()),
                        }
                    }
                    Err(e) => publish_error = Some(e),
                }
            }

            let status = if publish_id.is_some() {
                "published"
            } else if publish_error.is_some() {
                "draft_created_publish_failed"
            } else {
                "draft"
            };
            if let Some(ref err) = publish_error {
                warn!(draft_media_id = %draft_media_id, error = %err, "Draft created but freepublish failed (account may lack publish permission, e.g. 48001)");
            }
            info!(draft_media_id = %draft_media_id, publish_id = ?publish_id, cover_source, status, "Article publish completed");

            Ok(serde_json::json!({
                "media_id": draft_media_id,
                "publish_id": publish_id,
                "publish_error": publish_error,
                "cover_source": cover_source,
                "status": status,
            })
            .to_string())
        })
    }
}

// ---------------------------------------------------------------------------
// 86bus charter order creation tool
// ---------------------------------------------------------------------------

/// 86bus charter order endpoint (HMAC-signed).
const CHARTER_ORDERS_URL: &str = "https://chuxing.86bus.com/api/ai/orders";

/// Request body for `POST /api/ai/orders`. Field order is fixed so the
/// serialized string is stable across builds (the signature covers these exact
/// bytes — the same string is both signed AND sent, never re-serialized).
#[derive(serde::Serialize)]
struct CharterOrderRequest {
    username: String,
    phone: String,
    person_num: i64,
    start_point: String,
    end_point: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_city: Option<String>,
    go_time: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    back_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remark: Option<String>,
}

/// Compute the HMAC-SHA256 signature for an 86bus request.
/// sign_string = METHOD + "\n" + PATH + "\n" + TIMESTAMP + "\n" + BODY
fn charter_sign(secret: &str, method: &str, path: &str, timestamp: &str, body: &str) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let sign_str = format!("{method}\n{path}\n{timestamp}\n{body}");
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(sign_str.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub struct CharterCreateOrderTool;

impl ToolProvider for CharterCreateOrderTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "charter_create_order".to_string(),
            description: "Create a charter-bus (包车) order via the 86bus backend. The backend auto-resolves addresses to coordinates, computes distance, prices the trip, selects the car type by person count, creates the order, and notifies admins. Returns the quoted price, car type, distance, and a mini-program confirm card (confirm_url/mini_appid/card_title/card_thumb_id) to send the user. After calling this, report money/car_type/distance to the user and send them the confirm card via weixin_oa_send_miniprogram. go_time/back_time are Beijing time 'YYYY-MM-DD HH:MM'. Fill back_time for round-trip, omit for one-way.".to_string(),
            parameters_json: r#"{"type":"object","properties":{"username":{"type":"string","description":"联系人姓名"},"phone":{"type":"string","description":"联系电话（手机号）"},"person_num":{"type":"integer","description":"乘车人数（后端据此自动选车型）"},"start_point":{"type":"string","description":"起点（文字地址，如 南京南站）"},"end_point":{"type":"string","description":"终点（文字地址，如 禄口国际机场）"},"start_city":{"type":"string","description":"起点城市（可选）"},"end_city":{"type":"string","description":"终点城市（可选）"},"go_time":{"type":"string","description":"出发时间，北京时间 YYYY-MM-DD HH:MM"},"back_time":{"type":"string","description":"返程时间，北京时间 YYYY-MM-DD HH:MM。填了=往返，不填=单程"},"remark":{"type":"string","description":"备注（可选）"}},"required":["username","phone","person_num","start_point","end_point","go_time"]}"#.to_string(),
        }
    }

    fn execute(
        &self,
        args: &Value,
        _context: &PluginToolContext,
    ) -> Result<String, PluginToolError> {
        let req = CharterOrderRequest {
            username: args["username"].as_str().ok_or_else(|| {
                PluginToolError::tool("missing required parameter: username")
            })?
            .to_string(),
            phone: args["phone"].as_str().ok_or_else(|| {
                PluginToolError::tool("missing required parameter: phone")
            })?
            .to_string(),
            person_num: args["person_num"].as_i64().ok_or_else(|| {
                PluginToolError::tool("missing required parameter: person_num (integer)")
            })?,
            start_point: args["start_point"].as_str().ok_or_else(|| {
                PluginToolError::tool("missing required parameter: start_point")
            })?
            .to_string(),
            end_point: args["end_point"].as_str().ok_or_else(|| {
                PluginToolError::tool("missing required parameter: end_point")
            })?
            .to_string(),
            start_city: args["start_city"].as_str().filter(|s| !s.is_empty()).map(String::from),
            end_city: args["end_city"].as_str().filter(|s| !s.is_empty()).map(String::from),
            go_time: args["go_time"].as_str().ok_or_else(|| {
                PluginToolError::tool("missing required parameter: go_time")
            })?
            .to_string(),
            back_time: args["back_time"].as_str().filter(|s| !s.is_empty()).map(String::from),
            remark: args["remark"].as_str().filter(|s| !s.is_empty()).map(String::from),
        };

        let ak = types::env::get_env("CHARTER_AK").ok_or_else(|| {
            PluginToolError::tool("CHARTER_AK not configured (set in ~/.opencarrier/.env)")
        })?;
        let sk = types::env::get_env("CHARTER_SK").ok_or_else(|| {
            PluginToolError::tool("CHARTER_SK not configured (set in ~/.opencarrier/.env)")
        })?;

        // Serialize ONCE — this exact string is both signed and sent as the body.
        let body_str = serde_json::to_string(&req)
            .map_err(|e| PluginToolError::tool(format!("serialize request failed: {e}")))?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();
        let signature = charter_sign(&sk, "POST", "/api/ai/orders", &timestamp, &body_str);

        // Tool::execute is sync — run the async HTTP on a dedicated runtime.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("runtime error: {e}")))?;

        rt.block_on(async move {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(8))
                .build()
                .map_err(|e| PluginToolError::tool(format!("http client build failed: {e}")))?;

            // Send the EXACT body_str we signed (do NOT use .json() — it would
            // re-serialize and break the signature).
            let resp = client
                .post(CHARTER_ORDERS_URL)
                .header("X-Api-Key", &ak)
                .header("X-Timestamp", &timestamp)
                .header("X-Signature", &signature)
                .header("Content-Type", "application/json")
                .body(body_str)
                .send()
                .await
                .map_err(|e| PluginToolError::tool(format!("charter order request failed: {e}")))?;

            let status = resp.status();
            let text = resp
                .text()
                .await
                .map_err(|e| PluginToolError::tool(format!("read charter response failed: {e}")))?;

            // Parse to extract errcode + data, regardless of HTTP status.
            let val: Value = serde_json::from_str(&text).map_err(|_| {
                let preview = &text[..text.len().min(300)];
                PluginToolError::tool(format!("charter response not JSON (HTTP {status}): {preview}"))
            })?;

            let errcode = val.get("errcode").and_then(|v| v.as_i64()).unwrap_or(-1);
            if errcode != 0 {
                let errmsg = val
                    .get("errmsg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown error");
                return Err(PluginToolError::tool(format!(
                    "charter order failed (errcode={errcode}): {errmsg}"
                )));
            }

            // Project the fields the agent needs out of data.
            let data = val.get("data").cloned().unwrap_or(Value::Null);
            let order_no = data.get("order_no").and_then(|v| v.as_str()).unwrap_or("");
            let money = data.get("money").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let distance = data.get("distance").and_then(|v| v.as_f64()).unwrap_or(0.0);
            let car_type = data.get("car_type").and_then(|v| v.as_str()).unwrap_or("");
            let confirm_url = data.get("confirm_url").and_then(|v| v.as_str()).unwrap_or("");
            let mini_appid = data.get("mini_appid").and_then(|v| v.as_str()).unwrap_or("");
            let card_title = data.get("card_title").and_then(|v| v.as_str()).unwrap_or("您的包车订单待确认");
            let card_thumb_id = data.get("card_thumb_id").and_then(|v| v.as_str()).unwrap_or("");
            let status_text = data.get("status_text").and_then(|v| v.as_str()).unwrap_or("");

            info!(order_no = %order_no, money, car_type = %car_type, "86bus charter order created");

            Ok(serde_json::json!({
                "order_no": order_no,
                "money": money,
                "distance": distance,
                "car_type": car_type,
                "confirm_url": confirm_url,
                "mini_appid": mini_appid,
                "card_title": card_title,
                "card_thumb_id": card_thumb_id,
                "status_text": status_text,
            })
            .to_string())
        })
    }
}
