//! Token management and WeCom API helpers.
//!
//! Supports three WeCom integration modes:
//! - **App** (企业应用): corp_id + agent_id + secret → message/send
//! - **Kf** (微信客服): corp_id + open_kfid + secret → kf/send_msg
//! - **SmartBot** (智能对话机器人): WebSocket long connection + response_url reply
//!
//! Session files are stored in `~/.opencarrier/wecom-sessions/<name>.json` and
//! discovered dynamically via `WecomState::load_new_from_dir()`.

use dashmap::DashMap;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const WECOM_API_BASE: &str = "https://qyapi.weixin.qq.com";

/// Refresh token 5 minutes before actual expiry.
const TOKEN_REFRESH_BUFFER_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// WeCom integration mode
// ---------------------------------------------------------------------------

/// Which WeCom API to use for this tenant.
pub enum WecomMode {
    /// Enterprise application — sends via `cgi-bin/message/send`.
    App { agent_id: String },
    /// Customer service — sends via `cgi-bin/kf/send_msg`.
    Kf { open_kfid: String },
    /// Smart dialog bot — WebSocket long connection to `wss://openws.work.weixin.qq.com`.
    SmartBot { bot_id: String, secret: String },
}

// ---------------------------------------------------------------------------
// Bot entry
// ---------------------------------------------------------------------------

/// Per-bot configuration and cached token.
pub struct BotEntry {
    /// Unique tenant name (used as DashMap key).
    pub name: String,
    /// Enterprise corp ID (not used for Bot mode).
    pub corp_id: String,
    /// Application/customer-service secret (not used for Bot mode).
    pub secret: String,
    /// Webhook port for callback server (App and Kf modes).
    pub webhook_port: u16,
    /// AES key for callback encryption (App and Kf modes).
    pub encoding_aes_key: Option<String>,
    /// Callback verification token (App and Kf modes).
    pub callback_token: Option<String>,
    /// Integration mode.
    pub mode: WecomMode,
    /// Shared HTTP client.
    pub http: Client,
    /// Cached access token with expiry.
    cached_token: Mutex<Option<(String, Instant)>>,
    /// MCP bot credentials (for App/Kf modes; SmartBot reuses mode's bot_id/secret).
    pub mcp_bot_id: Option<String>,
    pub mcp_bot_secret: Option<String>,
}

impl BotEntry {
    // -----------------------------------------------------------------------
    // Constructors per mode
    // -----------------------------------------------------------------------

    /// Create an enterprise application bot.
    #[allow(clippy::too_many_arguments)]
    pub fn new_app(
        name: String,
        corp_id: String,
        agent_id: String,
        secret: String,
        webhook_port: u16,
        encoding_aes_key: Option<String>,
        callback_token: Option<String>,
        mcp_bot_id: Option<String>,
        mcp_bot_secret: Option<String>,
    ) -> Self {
        Self {
            name,
            corp_id,
            secret,
            webhook_port,
            encoding_aes_key,
            callback_token,
            mode: WecomMode::App { agent_id },
            http: Client::new(),
            cached_token: Mutex::new(None),
            mcp_bot_id,
            mcp_bot_secret,
        }
    }

    /// Create a customer service bot.
    #[allow(clippy::too_many_arguments)]
    pub fn new_kf(
        name: String,
        corp_id: String,
        open_kfid: String,
        secret: String,
        webhook_port: u16,
        encoding_aes_key: Option<String>,
        callback_token: Option<String>,
        mcp_bot_id: Option<String>,
        mcp_bot_secret: Option<String>,
    ) -> Self {
        Self {
            name,
            corp_id,
            secret,
            webhook_port,
            encoding_aes_key,
            callback_token,
            mode: WecomMode::Kf { open_kfid },
            http: Client::new(),
            cached_token: Mutex::new(None),
            mcp_bot_id,
            mcp_bot_secret,
        }
    }

    /// Create a smart dialog bot.
    pub fn new_smartbot(name: String, corp_id: String, bot_id: String, secret: String) -> Self {
        Self {
            name,
            corp_id,
            secret: secret.clone(),
            webhook_port: 0,
            encoding_aes_key: None,
            callback_token: None,
            mode: WecomMode::SmartBot { bot_id, secret },
            http: Client::new(),
            cached_token: Mutex::new(None),
            mcp_bot_id: None, // SmartBot uses mode's bot_id directly
            mcp_bot_secret: None,
        }
    }

    // -----------------------------------------------------------------------
    // Access helpers
    // -----------------------------------------------------------------------

    /// Get agent_id if this is an App-mode bot.
    pub fn agent_id(&self) -> Option<&str> {
        match &self.mode {
            WecomMode::App { agent_id } => Some(agent_id),
            _ => None,
        }
    }

    /// Get open_kfid if this is a Kf-mode bot.
    pub fn open_kfid(&self) -> Option<&str> {
        match &self.mode {
            WecomMode::Kf { open_kfid } => Some(open_kfid),
            _ => None,
        }
    }

    /// Get bot_id if this is a SmartBot-mode bot.
    pub fn bot_id(&self) -> Option<&str> {
        match &self.mode {
            WecomMode::SmartBot { bot_id, .. } => Some(bot_id),
            _ => None,
        }
    }

    /// Get bot secret if this is a SmartBot-mode bot.
    pub fn bot_secret(&self) -> Option<&str> {
        match &self.mode {
            WecomMode::SmartBot { secret, .. } => Some(secret),
            _ => None,
        }
    }

    /// Get MCP bot credentials (bot_id, bot_secret).
    /// SmartBot mode reuses its mode's bot_id and secret.
    /// App/Kf modes use the dedicated mcp_bot_id/mcp_bot_secret fields.
    pub fn mcp_credentials(&self) -> Option<(&str, &str)> {
        match &self.mode {
            WecomMode::SmartBot { bot_id, secret } => Some((bot_id, secret)),
            _ => self
                .mcp_bot_id
                .as_deref()
                .zip(self.mcp_bot_secret.as_deref()),
        }
    }

    /// Get a valid access token, refreshing if needed.
    /// Returns error for SmartBot mode (no token needed).
    pub fn get_access_token(&self) -> Result<String, String> {
        match &self.mode {
            WecomMode::SmartBot { .. } => Err("SmartBot mode does not use access tokens".into()),
            _ => self.get_or_refresh_token(),
        }
    }

    fn get_or_refresh_token(&self) -> Result<String, String> {
        // Check cache
        if let Some((token, expires_at)) = self.cached_token.lock().unwrap().as_ref() {
            if Instant::now() < *expires_at {
                return Ok(token.clone());
            }
        }

        // Fetch new token
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("Runtime error: {e}"))?;
        let token = rt.block_on(self.fetch_token())?;

        Ok(token)
    }

    async fn fetch_token(&self) -> Result<String, String> {
        let url = format!(
            "{}/cgi-bin/gettoken?corpid={}&corpsecret={}",
            WECOM_API_BASE, self.corp_id, self.secret
        );

        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("token request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("token response parse error: {e}"))?;

        let errcode = resp["errcode"].as_i64().unwrap_or(-1);
        if errcode != 0 {
            let errmsg = resp["errmsg"].as_str().unwrap_or("unknown");
            return Err(format!("token error: {errcode} {errmsg}"));
        }

        let token = resp["access_token"]
            .as_str()
            .ok_or("missing access_token")?
            .to_string();
        let expires_in = resp["expires_in"].as_u64().unwrap_or(7200);

        let expires_at = Instant::now()
            + Duration::from_secs(expires_in.saturating_sub(TOKEN_REFRESH_BUFFER_SECS));

        info!(bot = %self.name, "Refreshed WeCom access token");

        *self.cached_token.lock().unwrap() = Some((token.clone(), expires_at));
        Ok(token)
    }
}

// ---------------------------------------------------------------------------
// API helpers
// ---------------------------------------------------------------------------

/// Make a POST request to a WeCom API endpoint (with access_token).
pub async fn wedoc_post(
    http: &Client,
    path: &str,
    token: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let url = format!("{}/{}?access_token={}", WECOM_API_BASE, path, token);
    let resp: serde_json::Value = http
        .post(&url)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("API request failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("API response parse error: {e}"))?;

    let errcode = resp["errcode"].as_i64().unwrap_or(-1);
    if errcode != 0 {
        let errmsg = resp["errmsg"].as_str().unwrap_or("unknown");
        return Err(format!("WeCom API error {errcode}: {errmsg}"));
    }

    Ok(resp)
}

/// Send an application message to a WeCom user (App mode).
pub fn send_app_message(bot: &BotEntry, user_id: &str, content: &str) -> Result<(), String> {
    let agent_id = bot
        .agent_id()
        .ok_or("send_app_message requires App mode")?
        .to_string();
    let token = bot.get_access_token()?;

    let http = bot.http.clone();
    let user_id = user_id.to_string();
    let content = content.to_string();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(format!("Runtime error: {e}")));
                return;
            }
        };
        let result = rt.block_on(async {
            let body = serde_json::json!({
                "touser": &user_id,
                "msgtype": "text",
                "agentid": &agent_id,
                "text": { "content": &content }
            });
            wedoc_post(&http, "cgi-bin/message/send", &token, &body).await
        });
        let _ = tx.send(result);
    });

    let _ = rx
        .recv()
        .map_err(|e| format!("Send thread disconnected: {e}"))??;
    Ok(())
}

/// Send a customer service message (Kf mode).
pub fn send_kf_message(bot: &BotEntry, user_id: &str, content: &str) -> Result<(), String> {
    let open_kfid = bot
        .open_kfid()
        .ok_or("send_kf_message requires Kf mode")?
        .to_string();
    let token = bot.get_access_token()?;

    let http = bot.http.clone();
    let user_id = user_id.to_string();
    let content = content.to_string();

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = tx.send(Err(format!("Runtime error: {e}")));
                return;
            }
        };
        let result = rt.block_on(async {
            let body = serde_json::json!({
                "touser": &user_id,
                "open_kfid": &open_kfid,
                "msgtype": "text",
                "text": { "content": &content }
            });
            wedoc_post(&http, "cgi-bin/kf/send_msg", &token, &body).await
        });
        let _ = tx.send(result);
    });

    let _ = rx
        .recv()
        .map_err(|e| format!("Send thread disconnected: {e}"))??;
    Ok(())
}

/// Send a reply via the SmartBot response_url (HTTP POST with markdown).
pub async fn send_smartbot_response_async(
    http: &Client,
    response_url: &str,
    content: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "msgtype": "markdown",
        "markdown": {
            "content": content
        }
    });
    let resp: serde_json::Value = http
        .post(response_url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("smartbot response failed: {e}"))?
        .json()
        .await
        .map_err(|e| format!("smartbot response parse error: {e}"))?;

    let errcode = resp["errcode"].as_i64().unwrap_or(-1);
    if errcode != 0 {
        let errmsg = resp["errmsg"].as_str().unwrap_or("unknown");
        return Err(format!("smartbot response error {errcode}: {errmsg}"));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Session file format & WecomState
// ---------------------------------------------------------------------------

/// Session file format (written to `wecom-sessions/<name>.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WecomSessionFile {
    pub name: String,
    pub mode: String, // "app" | "kf" | "smartbot"
    // smartbot fields
    pub bot_id: Option<String>,
    // app fields
    pub agent_id: Option<String>,
    // app/kf shared
    pub corp_id: Option<String>,
    pub open_kfid: Option<String>,
    pub secret: Option<String>,
    pub secret_env: Option<String>,
    pub webhook_port: Option<u16>,
    pub encoding_aes_key: Option<String>,
    pub callback_token: Option<String>,
    pub mcp_bot_id: Option<String>,
    pub mcp_bot_secret: Option<String>,
    pub bind_agent: Option<String>,
}

/// Runtime state for a single WeCom bot session.
pub struct WecomBotSession {
    pub entry: BotEntry,
    pub active: AtomicBool,
}

/// Global state manager for all WeCom bots.
///
/// Discovers bots by scanning `~/.opencarrier/wecom-sessions/*.json`.
pub struct WecomState {
    pub bots: DashMap<String, WecomBotSession>, // key: name
    pub session_dir: PathBuf,
    pub http: Client,
}

impl WecomState {
    fn new() -> Self {
        let home = types::config::home_dir();
        let session_dir = home.join("wecom-sessions");
        Self {
            bots: DashMap::new(),
            session_dir,
            http: Client::new(),
        }
    }

    /// Resolve the effective secret: try env var first, fall back to inline value.
    fn resolve_secret(sf: &WecomSessionFile) -> String {
        if let Some(ref env_name) = sf.secret_env {
            if let Ok(s) = std::env::var(env_name) {
                if !s.is_empty() {
                    return s;
                }
            }
        }
        sf.secret.clone().unwrap_or_default()
    }

    /// Build a `BotEntry` from a session file.
    fn build_entry(sf: &WecomSessionFile) -> Option<BotEntry> {
        let secret = Self::resolve_secret(sf);
        let mcp_bot_id = sf.mcp_bot_id.clone();
        let mcp_bot_secret = sf.mcp_bot_secret.clone();

        match sf.mode.as_str() {
            "smartbot" => {
                let bot_id = sf.bot_id.as_deref().unwrap_or("").to_string();
                if bot_id.is_empty() {
                    warn!(name = %sf.name, "Skipping smartbot session with empty bot_id");
                    return None;
                }
                let corp_id = sf.corp_id.as_deref().unwrap_or("").to_string();
                Some(BotEntry::new_smartbot(sf.name.clone(), corp_id, bot_id, secret))
            }
            "kf" => {
                let corp_id = sf.corp_id.as_deref().unwrap_or("").to_string();
                let open_kfid = sf.open_kfid.as_deref().unwrap_or("").to_string();
                if corp_id.is_empty() || open_kfid.is_empty() {
                    warn!(name = %sf.name, "Skipping kf session: missing corp_id or open_kfid");
                    return None;
                }
                let webhook_port = sf.webhook_port.unwrap_or(8454);
                Some(BotEntry::new_kf(
                    sf.name.clone(),
                    corp_id,
                    open_kfid,
                    secret,
                    webhook_port,
                    sf.encoding_aes_key.clone(),
                    sf.callback_token.clone(),
                    mcp_bot_id,
                    mcp_bot_secret,
                ))
            }
            _ => {
                // "app" mode (default)
                let corp_id = sf.corp_id.as_deref().unwrap_or("").to_string();
                let agent_id = sf.agent_id.as_deref().unwrap_or("").to_string();
                if corp_id.is_empty() {
                    warn!(name = %sf.name, "Skipping app session with empty corp_id");
                    return None;
                }
                let webhook_port = sf.webhook_port.unwrap_or(8454);
                Some(BotEntry::new_app(
                    sf.name.clone(),
                    corp_id,
                    agent_id,
                    secret,
                    webhook_port,
                    sf.encoding_aes_key.clone(),
                    sf.callback_token.clone(),
                    mcp_bot_id,
                    mcp_bot_secret,
                ))
            }
        }
    }

    /// Load all sessions from the session directory (initial load at startup).
    pub fn load_from_dir(&self) {
        if !self.session_dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&self.session_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %self.session_dir.display(), "Failed to read session directory: {e}");
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    warn!(path = %path.display(), "Failed to read session file: {e}");
                    continue;
                }
            };
            let sf = match serde_json::from_str::<WecomSessionFile>(&content) {
                Ok(s) => s,
                Err(e) => {
                    warn!(path = %path.display(), "Failed to parse session file: {e}");
                    continue;
                }
            };
            if sf.name.is_empty() {
                continue;
            }
            if self.bots.contains_key(&sf.name) {
                continue;
            }
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, mode = %sf.mode, "Loaded WeCom session");
            self.bots.insert(
                sf.name.clone(),
                WecomBotSession {
                    entry,
                    active: AtomicBool::new(false),
                },
            );
        }
    }

    /// Load new sessions (skips already-loaded bots). Called by watcher loop.
    pub fn load_new_from_dir(&self) {
        if !self.session_dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&self.session_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let sf = match serde_json::from_str::<WecomSessionFile>(&content) {
                Ok(s) => s,
                Err(_) => continue,
            };
            if sf.name.is_empty() {
                continue;
            }
            // Refresh existing bot if session file changed (e.g. new secret)
            if let Some(mut existing) = self.bots.get_mut(&sf.name) {
                let new_entry = match Self::build_entry(&sf) {
                    Some(e) => e,
                    None => continue,
                };
                if existing.entry.secret != new_entry.secret
                    || existing.entry.corp_id != new_entry.corp_id
                {
                    info!(name = %sf.name, "Refreshing WeCom session from updated file");
                    existing.entry = new_entry;
                    existing.active.store(true, Ordering::Relaxed);
                }
                continue;
            }
            let entry = match Self::build_entry(&sf) {
                Some(e) => e,
                None => continue,
            };
            info!(name = %sf.name, mode = %sf.mode, "Dynamic watcher loaded new WeCom session");
            self.bots.insert(
                sf.name.clone(),
                WecomBotSession {
                    entry,
                    active: AtomicBool::new(false),
                },
            );
        }
    }

    /// Save a session file to disk.
    pub fn save_session(&self, sf: &WecomSessionFile) {
        if let Err(e) = std::fs::create_dir_all(&self.session_dir) {
            warn!(dir = %self.session_dir.display(), "Failed to create session directory: {e}");
            return;
        }
        let path = self.session_dir.join(format!("{}.json", sf.name));
        match serde_json::to_string_pretty(sf) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(path = %path.display(), "Failed to write session file: {e}");
                }
            }
            Err(e) => {
                warn!("Failed to serialize session file: {e}");
            }
        }
    }

    /// Find a bot for sending by name. Falls back to scanning by bot_id.
    pub fn get_session_for_send(
        &self,
        name: &str,
    ) -> Option<dashmap::mapref::one::Ref<'_, String, WecomBotSession>> {
        if let Some(s) = self.bots.get(name) {
            return Some(s);
        }
        // Fallback: find by smartbot bot_id
        let found_key = self
            .bots
            .iter()
            .find(|e| e.value().entry.bot_id() == Some(name))
            .map(|e| e.key().clone())?;
        self.bots.get(&found_key)
    }

    /// Get status of all bots for the API.
    pub fn status_list(&self) -> Vec<serde_json::Value> {
        self.bots
            .iter()
            .map(|entry| {
                let s = entry.value();
                serde_json::json!({
                    "name": s.entry.name,
                    "mode": match &s.entry.mode {
                        WecomMode::App { .. } => "app",
                        WecomMode::Kf { .. } => "kf",
                        WecomMode::SmartBot { .. } => "smartbot",
                    },
                    "bot_id": s.entry.bot_id(),
                    "active": s.active.load(Ordering::Relaxed),
                })
            })
            .collect()
    }
}

/// Global singleton for WeCom state management.
pub static WECOM_STATE: std::sync::LazyLock<WecomState> =
    std::sync::LazyLock::new(WecomState::new);
