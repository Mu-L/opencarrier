//! WeChat Official Account plugin tools — built-in, no FFI.

use serde_json::Value;
use types::plugin::PluginToolContext;
use types::tool::{PluginToolDef, PluginToolError, ToolProvider};

use crate::api;
use crate::channel::WEIXIN_OA_STATE;

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
            let token = account.get_token().await.map_err(PluginToolError::tool)?;

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

            api::custom_send_image(&account.http, &token, &openid, &final_media_id)
                .await
                .map_err(PluginToolError::tool)?;

            Ok(format!(
                "Image sent to user {openid} (media_id={final_media_id})"
            ))
        })
    }
}
