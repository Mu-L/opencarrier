//! WeChat Official Account plugin tools — built-in, no FFI.

use serde_json::Value;
use tracing::{info, warn};
use types::error::CarrierResult;
use types::plugin::PluginToolContext;
use types::tool::{PluginToolDef, PluginToolError, ToolProvider};

use crate::api;

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
pub(crate) async fn refresh_token(account: &crate::channel::OaAccountState) -> CarrierResult<String> {
    account.invalidate_token().await;
    account.get_token().await
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
            parameters_json: r#"{"type":"object","properties":{"app_id":{"type":"string","description":"Target OA app_id"},"html_path":{"type":"string","description":"Path to the WeChat-ready HTML article (absolute or relative to ~/.opencarrier)"},"title":{"type":"string","description":"Article title"},"author":{"type":"string","description":"Article author (作者). Usually resolved from the article's META_AUTHOR field; if omitted WeChat leaves the author blank."},"cover_path":{"type":"string","description":"Optional path to a generated cover image. If omitted/upload fails, falls back to the first image in the material library."},"publish":{"type":"boolean","default":true,"description":"Submit the draft for publishing immediately after creation."},"digest":{"type":"string","description":"Optional article digest/summary (摘要). Usually resolved from META_DIGEST; if both omitted, WeChat auto-extracts from the article beginning."}},"required":["app_id","html_path","title"]}"#.to_string(),
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
        let author = args["author"].as_str().filter(|s| !s.is_empty()).map(|s| s.to_string());

        // Build a fresh HTTP client; tokens flow through the central
        // `wechat-oa` core cache (keyed by app_id) — no WEIXIN_OA_STATE
        // registration needed and repeat publishes hit the cache.
        let http = reqwest::Client::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("runtime error: {e}")))?;

        rt.block_on(async move {
            let mut token = wechat_oa::token::get_token(&http, &app_id, &app_secret)
                .await
                .map_err(|e| PluginToolError::tool(e.to_string()))?;

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
                &http, &token, &title, &content, Some(&thumb), author.as_deref(), digest.as_deref(),
            )
            .await
            {
                Ok(mid) => mid,
                Err(e) if is_token_expired(&e.to_string()) => {
                    wechat_oa::token::invalidate(&app_id);
                    token = wechat_oa::token::get_token(&http, &app_id, &app_secret)
                        .await
                        .map_err(|e| PluginToolError::tool(e.to_string()))?;
                    api::add_draft(&http, &token, &title, &content, Some(&thumb), author.as_deref(), digest.as_deref())
                        .await
                        .map_err(|e| PluginToolError::tool(e.to_string()))?
                }
                Err(e) => return Err(PluginToolError::tool(e.to_string())),
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
                    Err(e) if is_token_expired(&e.to_string()) => {
                        wechat_oa::token::invalidate(&app_id);
                        match wechat_oa::token::get_token(&http, &app_id, &app_secret).await {
                            Ok(new_tok) => {
                                match api::freepublish_submit(&http, &new_tok, &draft_media_id).await {
                                    Ok(pid) => publish_id = Some(pid),
                                    Err(e2) => publish_error = Some(e2.to_string()),
                                }
                            }
                            Err(e2) => publish_error = Some(e2.to_string()),
                        }
                    }
                    Err(e) => publish_error = Some(e.to_string()),
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

            // Track the submitted publish for the daemon's zero-LLM PublishPoll
            // arm — but only for server-bound accounts (a senders/<app_id>
            // session exists): user-profile accounts have no credentials the
            // poller could use, so tracking them would strand forever-pending
            // ids that never resolve and never let the poller self-delete.
            if let Some(ref pid) = publish_id {
                let home = types::config::home_dir();
                if wechat_oa::session::load_account(&home, &app_id).is_some() {
                    if let Err(e) = wechat_oa::publish_tracker::track(&home, &app_id, pid) {
                        warn!(error = %e, publish_id = %pid, "publish_tracker track failed (poll arm will not see this publish)");
                    }
                } else {
                    info!(app_id = %app_id, publish_id = %pid, "user-profile account: publish status not tracked");
                }
            }

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
