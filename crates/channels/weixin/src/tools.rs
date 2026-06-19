//! WeChat iLink plugin tools — built-in, no FFI.

use types::plugin::PluginToolContext;
use types::tool::{PluginToolDef, PluginToolError, ToolProvider};
use serde_json::Value;

use crate::auth;
use crate::token::WEIXIN_STATE;

// ---------------------------------------------------------------------------
// QR Login tool
// ---------------------------------------------------------------------------

pub struct WeixinQrLoginTool;

impl ToolProvider for WeixinQrLoginTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "weixin_qr_login".to_string(),
            description: "Trigger WeChat iLink QR code login. Returns a QR code URL for the user to scan with WeChat. After scanning, the bot token is saved automatically.".to_string(),
            parameters_json: r#"{"type":"object","properties":{"bot_id":{"type":"string","description":"Name for this WeChat account (used as tenant ID)"}},"required":["bot_id"]}"#.to_string(),
        }
    }

    fn execute(&self, args: &Value, _context: &PluginToolContext) -> Result<String, PluginToolError> {
        let bot_id = args["bot_id"]
            .as_str()
            .unwrap_or("default")
            .to_string();

        let http = crate::build_http_client();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("Runtime error: {e}")))?;

        let bot = bot_id.clone();
        let result = rt.block_on(async { auth::qr_login(&http, &bot, None).await });

        result.map_err(PluginToolError::tool)
    }
}

// ---------------------------------------------------------------------------
// Send Message tool
// ---------------------------------------------------------------------------

pub struct WeixinSendMessageTool;

impl ToolProvider for WeixinSendMessageTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "weixin_send_message".to_string(),
            description: "Send a text message to a WeChat user via iLink. Requires an active QR-logged-in session. You can only reply to users who have already sent a message (context_token required).".to_string(),
            parameters_json: r#"{"type":"object","properties":{"bot_id":{"type":"string","description":"Bot ID (WeChat account)"},"user_id":{"type":"string","description":"iLink user ID to send to"},"text":{"type":"string","description":"Message text"}},"required":["bot_id","user_id","text"]}"#.to_string(),
        }
    }

    fn execute(&self, args: &Value, _context: &PluginToolContext) -> Result<String, PluginToolError> {
        let bot_id = args["bot_id"]
            .as_str()
            .ok_or_else(|| PluginToolError::tool("missing bot_id"))?;
        let user_id = args["user_id"]
            .as_str()
            .ok_or_else(|| PluginToolError::tool("missing user_id"))?;
        let text = args["text"]
            .as_str()
            .ok_or_else(|| PluginToolError::tool("missing text"))?;

        let state = WEIXIN_STATE
            .get_session_for_send(bot_id, user_id)
            .ok_or_else(|| PluginToolError::tool(format!("No session for bot {bot_id}, user {user_id}")))?;

        if state.is_expired() {
            return Err(PluginToolError::tool("Token expired, please re-scan QR code"));
        }

        let context_token = state.get_context_token(user_id).ok_or_else(|| {
            PluginToolError::tool(format!(
                "No context_token for user {user_id} — can only reply to received messages"
            ))
        })?;

        let bot_token = state.bot_token.clone();
        let baseurl = state.baseurl.clone();
        let http = state.http.clone();
        let client_id = format!("openclaw-weixin-{}", uuid::Uuid::new_v4().as_simple());

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| PluginToolError::tool(format!("Runtime error: {e}")))?;

        rt.block_on(async {
            crate::api::send_message(
                &http,
                &bot_token,
                &baseurl,
                user_id,
                &context_token,
                &client_id,
                text,
            )
            .await
            .map_err(PluginToolError::tool)
        })?;

        Ok("Message sent".to_string())
    }
}

// ---------------------------------------------------------------------------
// Status tool
// ---------------------------------------------------------------------------

pub struct WeixinStatusTool;

impl ToolProvider for WeixinStatusTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "weixin_status".to_string(),
            description: "Show status of all linked WeChat accounts (bots). Shows which are active, expired, or waiting for QR scan.".to_string(),
            parameters_json: r#"{"type":"object","properties":{}}"#.to_string(),
        }
    }

    fn execute(&self, _args: &Value, _context: &PluginToolContext) -> Result<String, PluginToolError> {
        let statuses = WEIXIN_STATE.status_list();
        if statuses.is_empty() {
            return Ok("No WeChat accounts linked. Use weixin_qr_login to link one.".to_string());
        }
        Ok(serde_json::to_string_pretty(&statuses).unwrap_or_else(|_| "Status error".to_string()))
    }
}
