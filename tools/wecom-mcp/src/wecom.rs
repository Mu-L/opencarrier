//! WeCom (WeChat Work) API client (multi-tenant).
//!
//! Supports two API patterns:
//! - **Direct API**: Uses `corp_id + secret` -> access_token -> standard REST API calls
//! - **MCP Proxy**: Uses `bot_id + bot_secret` -> SHA256 signature -> resolve MCP endpoint -> JSON-RPC 2.0 proxy
//!
//! Each tool call carries its own credentials, allowing a single MCP server process
//! to serve multiple WeCom organizations simultaneously. Access tokens are cached
//! per `corp_id + secret` and auto-refreshed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

const WECOM_API_BASE: &str = "https://qyapi.weixin.qq.com";

/// Refresh the token this many seconds before it actually expires.
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(300);

/// MCP config cache duration (24 hours).
const MCP_CONFIG_TTL: Duration = Duration::from_secs(24 * 3600);

// ---------------------------------------------------------------------------
// SHA256 helper
// ---------------------------------------------------------------------------

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// Public types — Direct API client
// ---------------------------------------------------------------------------

/// Multi-tenant WeCom API client. Token cache keyed by `corp_id`.
#[derive(Clone)]
pub struct WecomClient {
    http: reqwest::Client,
    /// corp_id -> (access_token, secret, expires_at)
    tokens: Arc<Mutex<HashMap<String, CachedToken>>>,
}

struct CachedToken {
    access_token: String,
    secret: String,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// WeCom JSON response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    errcode: Option<i64>,
    errmsg: Option<String>,
}

// ---------------------------------------------------------------------------
// Direct API client impl
// ---------------------------------------------------------------------------

impl WecomClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Obtain a valid access_token for the given corp, refreshing when needed.
    pub async fn get_token(&self, corp_id: &str, secret: &str) -> Result<String> {
        // Fast path — cached and not about to expire AND secret unchanged.
        {
            let guard = self.tokens.lock().await;
            if let Some(cached) = guard.get(corp_id) {
                if cached.secret == secret
                    && cached.expires_at > Instant::now() + TOKEN_EXPIRY_MARGIN
                {
                    return Ok(cached.access_token.clone());
                }
            }
        }

        // Slow path — hit the WeCom API.
        let url = format!(
            "{}/cgi-bin/gettoken?corpid={}&corpsecret={}",
            WECOM_API_BASE, corp_id, secret
        );
        let resp: TokenResponse = self.http.get(&url).send().await?.json().await?;

        if let Some(code) = resp.errcode {
            if code != 0 {
                bail!(
                    "WeCom token error {}: {}",
                    code,
                    resp.errmsg.unwrap_or_default()
                );
            }
        }

        let access_token = resp.access_token.context("no access_token in response")?;
        let expires_in = resp.expires_in.unwrap_or(7200);

        {
            let mut guard = self.tokens.lock().await;
            guard.insert(
                corp_id.to_string(),
                CachedToken {
                    access_token: access_token.clone(),
                    secret: secret.to_string(),
                    expires_at: Instant::now() + Duration::from_secs(expires_in),
                },
            );
        }

        Ok(access_token)
    }

    /// POST JSON body with auto-injected access_token in query string.
    pub async fn api_post(
        &self,
        corp_id: &str,
        secret: &str,
        path: &str,
        body: &Value,
    ) -> Result<Value> {
        let token = self.get_token(corp_id, secret).await?;
        let url = format!("{}{}?access_token={}", WECOM_API_BASE, path, token);
        let json: Value = self.http.post(&url).json(body).send().await?.json().await?;
        check_error(&json)?;
        Ok(json)
    }

    /// GET with auto-injected access_token in query string.
    #[allow(dead_code)]
    pub async fn api_get(
        &self,
        corp_id: &str,
        secret: &str,
        path: &str,
    ) -> Result<Value> {
        let token = self.get_token(corp_id, secret).await?;
        let url = format!("{}{}?access_token={}", WECOM_API_BASE, path, token);
        let json: Value = self.http.get(&url).send().await?.json().await?;
        check_error(&json)?;
        Ok(json)
    }

    /// Simple GET (no auth) — returns raw text.
    #[allow(dead_code)]
    pub async fn http_get_text(&self, url: &str) -> Result<String> {
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            bail!("HTTP {} fetching {}", resp.status(), url);
        }
        let text = resp.text().await?;
        Ok(text)
    }

    /// Simple GET (no auth) — returns JSON.
    pub async fn http_get_json(&self, url: &str) -> Result<Value> {
        let resp = self.http.get(url).send().await?;
        if !resp.status().is_success() {
            bail!("HTTP {} fetching {}", resp.status(), url);
        }
        let json: Value = resp.json().await?;
        Ok(json)
    }
}

/// Check for `errcode != 0` in a WeCom JSON response.
fn check_error(json: &Value) -> Result<()> {
    if let Some(code) = json.get("errcode").and_then(|v| v.as_i64()) {
        if code != 0 {
            let msg = json
                .get("errmsg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("WeCom API error {}: {}", code, msg);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public types — MCP Proxy
// ---------------------------------------------------------------------------

/// WeCom MCP Proxy — routes tool calls through WeCom's MCP gateway.
#[derive(Clone)]
pub struct WecomMcpProxy {
    client: WecomClient,
    /// bot_id -> (category -> url, expires_at)
    config_cache: Arc<Mutex<HashMap<String, CachedMcpConfig>>>,
}

struct CachedMcpConfig {
    category_urls: HashMap<String, String>,
    expires_at: Instant,
}

// ---------------------------------------------------------------------------
// MCP Proxy impl
// ---------------------------------------------------------------------------

impl WecomMcpProxy {
    pub fn new() -> Self {
        Self {
            client: WecomClient::new(),
            config_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get a reference to the underlying direct API client.
    pub fn client(&self) -> &WecomClient {
        &self.client
    }

    /// Build the authentication request body for MCP config resolution.
    fn build_config_request(bot_id: &str, bot_secret: &str) -> Value {
        let time = chrono::Utc::now().timestamp();
        let nonce = format!(
            "mcp_{}_{}",
            chrono::Utc::now().timestamp_millis(),
            hex::encode(rand::random::<[u8; 4]>())
        );
        let time_str = time.to_string();
        let sig_input = format!("{}{}{}{}", bot_secret, bot_id, time_str, nonce);
        let signature = sha256_hex(&sig_input);

        serde_json::json!({
            "bot_id": bot_id,
            "time": time,
            "nonce": nonce,
            "signature": signature,
            "bind_source": 2,
            "cli_version": "OpenCarrier/0.3.0"
        })
    }

    /// Resolve the MCP endpoint URL for a given category.
    #[allow(unused_variables)]
    async fn get_category_url(
        &self,
        corp_id: &str,
        bot_id: &str,
        bot_secret: &str,
        category: &str,
    ) -> Result<String> {
        // Check cache first.
        {
            let guard = self.config_cache.lock().await;
            if let Some(cached) = guard.get(bot_id) {
                if cached.expires_at > Instant::now() {
                    if let Some(url) = cached.category_urls.get(category) {
                        return Ok(url.clone());
                    }
                    bail!("MCP category '{}' not found or not authorized for bot {}", category, bot_id);
                }
            }
        }

        // Fetch fresh config.
        let auth_body = Self::build_config_request(bot_id, bot_secret);
        let url = format!("{}/cgi-bin/aibot/cli/get_mcp_config", WECOM_API_BASE);
        let resp = self
            .client
            .http
            .post(&url)
            .json(&auth_body)
            .send()
            .await?;

        if !resp.status().is_success() {
            bail!("MCP config HTTP {} for bot {}", resp.status(), bot_id);
        }

        let json: Value = resp.json().await?;
        check_error(&json)?;

        let list = json
            .get("list")
            .and_then(|v| v.as_array())
            .context("no 'list' in MCP config response")?;

        let mut category_urls: HashMap<String, String> = HashMap::new();
        for item in list {
            let biz_type = item
                .get("biz_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let item_url = item.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let is_authed = item
                .get("is_authed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_authed && !item_url.is_empty() {
                category_urls.insert(biz_type.to_string(), item_url.to_string());
            }
        }

        // Cache for 24 hours.
        {
            let mut guard = self.config_cache.lock().await;
            guard.insert(
                bot_id.to_string(),
                CachedMcpConfig {
                    category_urls: category_urls.clone(),
                    expires_at: Instant::now() + MCP_CONFIG_TTL,
                },
            );
        }

        if let Some(url) = category_urls.get(category) {
            Ok(url.clone())
        } else {
            bail!(
                "MCP category '{}' not found or not authorized for bot {}",
                category,
                bot_id
            );
        }
    }

    /// Invalidate the config cache for a given bot (e.g. on 401/403).
    async fn invalidate_config(&self, bot_id: &str) {
        let mut guard = self.config_cache.lock().await;
        guard.remove(bot_id);
    }

    /// Derive the MCP category from a tool name.
    ///
    /// Tool names follow the pattern `wecom_{category}_{action}`.
    /// Categories: contact, doc, msg, todo, meeting, schedule.
    fn category_for_tool(tool_name: &str) -> &'static str {
        // Strip "wecom_" prefix if present.
        let stripped = tool_name.strip_prefix("wecom_").unwrap_or(tool_name);
        // Hard-coded mapping based on known tool names.
        match stripped {
            // Contact
            "get_userlist" => "contact",
            // Doc
            "get_doc_content" | "create_doc" | "edit_doc_content"
            | "smartpage_export_task" | "smartpage_get_export_result"
            | "smartsheet_get_sheet" | "smartsheet_add_sheet"
            | "smartsheet_update_sheet" | "smartsheet_delete_sheet"
            | "smartsheet_get_fields" | "smartsheet_add_fields"
            | "smartsheet_update_fields" | "smartsheet_delete_fields"
            | "smartsheet_get_records" | "smartsheet_add_records"
            | "smartsheet_update_records" | "smartsheet_delete_records" => "doc",
            // Msg
            "get_msg_chat_list" | "get_message" | "send_message" => "msg",
            // Todo
            "get_todo_list" | "get_todo_detail" | "create_todo"
            | "update_todo" | "delete_todo" | "change_todo_user_status" => "todo",
            // Meeting
            "create_meeting" | "list_user_meetings" | "get_meeting_info"
            | "cancel_meeting" | "set_invite_meeting_members" => "meeting",
            // Schedule
            "get_schedule_list_by_range" | "get_schedule_detail" | "create_schedule"
            | "update_schedule" | "cancel_schedule" | "add_schedule_attendees"
            | "del_schedule_attendees" | "check_availability" => "schedule",
            _ => "contact", // fallback
        }
    }

    /// Main MCP proxy method — call a tool through the WeCom MCP gateway.
    pub async fn call_mcp_tool(
        &self,
        corp_id: &str,
        secret: &str,
        bot_id: &str,
        bot_secret: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<String> {
        let result = self
            .call_mcp_tool_inner(corp_id, secret, bot_id, bot_secret, tool_name, arguments)
            .await;

        // On failure that looks like an auth issue, invalidate config cache and retry once.
        match &result {
            Ok(_) => result,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("401") || err_str.contains("403") {
                    tracing::warn!("MCP proxy auth error for bot {}, invalidating config cache and retrying", bot_id);
                    self.invalidate_config(bot_id).await;
                    self.call_mcp_tool_inner(corp_id, secret, bot_id, bot_secret, tool_name, arguments)
                        .await
                } else {
                    result
                }
            }
        }
    }

    /// Inner implementation of the MCP proxy call.
    async fn call_mcp_tool_inner(
        &self,
        corp_id: &str,
        _secret: &str,
        bot_id: &str,
        bot_secret: &str,
        tool_name: &str,
        arguments: &Value,
    ) -> Result<String> {
        let category = Self::category_for_tool(tool_name);
        let endpoint_url = self
            .get_category_url(corp_id, bot_id, bot_secret, category)
            .await?;

        // Build JSON-RPC 2.0 request.
        let timestamp = chrono::Utc::now().timestamp_millis();
        let random_hex = hex::encode(rand::random::<[u8; 4]>());
        let rpc_id = format!("mcp_{}_{}", timestamp, random_hex);

        let rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": rpc_id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments
            }
        });

        let resp = self
            .client
            .http
            .post(&endpoint_url)
            .json(&rpc_request)
            .send()
            .await?;

        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            bail!("MCP proxy HTTP {} — auth error for tool {}", status, tool_name);
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "MCP proxy HTTP {} for tool {}: {}",
                status,
                tool_name,
                &text[..text.len().min(500)]
            );
        }

        let json: Value = resp.json().await?;

        // Check for JSON-RPC error.
        if let Some(error) = json.get("error") {
            let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            let message = error
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown error");
            bail!("MCP JSON-RPC error {}: {}", code, message);
        }

        // Extract text from result.content[].text array.
        let result = json.get("result").cloned().unwrap_or(Value::Null);

        // Check isError flag.
        if let Some(is_error) = result.get("isError").and_then(|v| v.as_bool()) {
            if is_error {
                // Still return the content so the caller can see the error message.
                let texts = extract_content_texts(&result);
                bail!("MCP tool error: {}", texts.join("; "));
            }
        }

        let texts = extract_content_texts(&result);
        if texts.is_empty() {
            Ok(result.to_string())
        } else {
            Ok(texts.join("\n"))
        }
    }
}

/// Extract text strings from a JSON-RPC result's `content` array.
fn extract_content_texts(result: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(content) = result.get("content").and_then(|v| v.as_array()) {
        for item in content {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                texts.push(text.to_string());
            }
        }
    }
    texts
}
