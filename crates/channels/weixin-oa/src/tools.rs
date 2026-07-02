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
fn is_token_expired(err: &str) -> bool {
    err.contains("40001")
}

/// Get a fresh access_token. If a prior call failed with 40001, call this to
/// invalidate the cache and get a new token for one retry.
async fn refresh_token(account: &crate::channel::OaAccountState) -> Result<String, String> {
    account.invalidate_token().await;
    account.get_token().await
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
            parameters_json: r#"{"type":"object","properties":{"app_id":{"type":"string","description":"Target OA app_id"},"html_path":{"type":"string","description":"Path to the WeChat-ready HTML article (absolute or relative to ~/.opencarrier)"},"title":{"type":"string","description":"Article title"},"cover_path":{"type":"string","description":"Optional path to a generated cover image. If omitted/upload fails, falls back to the first image in the material library."},"publish":{"type":"boolean","default":true,"description":"Submit the draft for publishing immediately after creation."}},"required":["app_id","html_path","title"]}"#.to_string(),
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
                &http, &token, &title, &content, Some(&thumb), None, None,
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
                    api::add_draft(&http, &token, &title, &content, Some(&thumb), None, None)
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
