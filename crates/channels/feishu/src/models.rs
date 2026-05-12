//! Feishu/Lark API type definitions.
//!
//! Covers: tenant_access_token, WebSocket event frames, message send/reply.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Feishu (China) API base URL.
pub const FEISHU_API_BASE: &str = "https://open.feishu.cn";
/// Lark (International) API base URL.
pub const LARK_API_BASE: &str = "https://open.larksuite.com";

/// Token refresh safety margin (refresh 5 minutes before expiry).
pub const TOKEN_REFRESH_AHEAD_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Tenant Access Token
// ---------------------------------------------------------------------------

/// Request body for `POST /open-apis/auth/v3/tenant_access_token/internal`.
#[derive(Debug, Clone, Serialize)]
pub struct TenantTokenRequest {
    pub app_id: String,
    pub app_secret: String,
}

/// Response from tenant_access_token endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct TenantTokenResponse {
    pub code: i64,
    pub msg: String,
    pub tenant_access_token: Option<String>,
    pub expire: Option<u64>,
}

// ---------------------------------------------------------------------------
// Send Message
// ---------------------------------------------------------------------------

/// Request body for `POST /open-apis/im/v1/messages`.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    pub receive_id: String,
    pub msg_type: String,
    pub content: String,
}

/// Response from send message endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SendMessageResponse {
    pub code: i64,
    pub msg: String,
    pub data: Option<SendMessageData>,
}

/// Data payload in a send/reply message response.
#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageData {
    pub message_id: Option<String>,
}

// ---------------------------------------------------------------------------
// WebSocket endpoint
// ---------------------------------------------------------------------------

/// Response from `POST /open-apis/callback/ws/endpoint`.
#[derive(Debug, Clone, Deserialize)]
pub struct WsEndpointResponse {
    pub code: i64,
    pub msg: String,
    pub data: Option<WsEndpointData>,
}

/// WebSocket connection URL payload.
#[derive(Debug, Clone, Deserialize)]
pub struct WsEndpointData {
    /// WebSocket URL (returned as "URL" in the API response).
    #[serde(alias = "URL", alias = "endpoint")]
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// Text message content (parsed from JSON string in MessageContent.content)
// ---------------------------------------------------------------------------

/// Parsed text message body: `{"text": "hello"}`.
#[derive(Debug, Clone, Deserialize)]
pub struct TextContent {
    pub text: Option<String>,
}

// ---------------------------------------------------------------------------
// Tenant configuration (read from bot.toml)
// ---------------------------------------------------------------------------

/// Per-tenant configuration parsed from bot.toml.
#[derive(Debug, Clone)]
pub struct FeishuBotConfig {
    pub name: String,
    pub app_id: String,
    pub app_secret: String,
    /// "feishu" (China) or "lark" (International).
    pub brand: String,
}

impl FeishuBotConfig {
    /// Get the API base URL for this tenant's brand.
    pub fn api_base(&self) -> &'static str {
        if self.brand == "lark" {
            LARK_API_BASE
        } else {
            FEISHU_API_BASE
        }
    }
}

// ---------------------------------------------------------------------------
// Session file format
// ---------------------------------------------------------------------------

/// Session file format (written to `feishu-sessions/<app_id>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuSessionFile {
    pub name: String,
    pub app_id: String,
    pub app_secret: Option<String>,
    pub secret_env: Option<String>,
    pub brand: String,
    pub bind_agent: Option<String>,
}
