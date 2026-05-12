//! WeCom channel adapter — webhook server for inbound/outbound messages.

use std::collections::HashMap;

use axum::extract::Query;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use carrier_types::channel::Channel;
use carrier_types::plugin::{PluginContent, PluginMessage};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::crypto;
use crate::token;

// ---------------------------------------------------------------------------
// Callback parameters
// ---------------------------------------------------------------------------

#[derive(Deserialize, Clone)]
struct CallbackParams {
    msg_signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    echostr: Option<String>,
}

// ---------------------------------------------------------------------------
// WeCom Channel
// ---------------------------------------------------------------------------

/// A WeCom channel that receives messages via webhook and sends via API.
pub struct WeComChannel {
    bot_id: String,
    corp_id: String,
    webhook_port: u16,
    encoding_aes_key: Option<String>,
    callback_token: Option<String>,
    is_kf: bool,
}

impl WeComChannel {
    pub fn new(
        bot_id: String,
        corp_id: String,
        webhook_port: u16,
        encoding_aes_key: Option<String>,
        callback_token: Option<String>,
        is_kf: bool,
    ) -> Self {
        Self {
            bot_id,
            corp_id,
            webhook_port,
            encoding_aes_key,
            callback_token,
            is_kf,
        }
    }
}

impl Channel for WeComChannel {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    fn name(&self) -> &str {
        "WeChat Work"
    }

    fn bot_id(&self) -> &str {
        &self.bot_id
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        let bot_id = self.bot_id.clone();
        let corp_id = self.corp_id.clone();
        let encoding_aes_key = self.encoding_aes_key.clone();
        let callback_token = self.callback_token.clone();
        let port = self.webhook_port;
        let is_kf = self.is_kf;

        // Spawn in its own thread with dedicated runtime
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for WeCom webhook");
            rt.block_on(async move {
                run_webhook_server(
                    bot_id,
                    corp_id,
                    encoding_aes_key,
                    callback_token,
                    port,
                    is_kf,
                    sender,
                )
                .await;
            });
        });

        info!(
            bot = %self.bot_id,
            port = self.webhook_port,
            kf = self.is_kf,
            "WeCom channel started"
        );

        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        let bot = crate::token::WECOM_STATE
            .get_session_for_send(bot_id)
            .ok_or_else(|| format!("Unknown bot: {bot_id}"))?;

        match &bot.entry.mode {
            token::WecomMode::App { .. } => {
                token::send_app_message(&bot.entry, user_id, text)?;
            }
            token::WecomMode::Kf { .. } => {
                token::send_kf_message(&bot.entry, user_id, text)?;
            }
            token::WecomMode::SmartBot { .. } => {
                return Err(
                    "SmartBot mode does not support send via channel (use response_url)"
                        .to_string(),
                );
            }
        }

        Ok(())
    }

    fn stop(&mut self) {
        // Webhook server runs until process exit; no graceful shutdown needed.
    }
}

// ---------------------------------------------------------------------------
// Webhook server
// ---------------------------------------------------------------------------

async fn run_webhook_server(
    bot_id: String,
    corp_id: String,
    encoding_aes_key: Option<String>,
    callback_token: Option<String>,
    port: u16,
    is_kf: bool,
    tx: mpsc::Sender<PluginMessage>,
) {
    let state = WebhookState {
        bot_id,
        corp_id,
        encoding_aes_key,
        callback_token,
        is_kf,
        tx,
    };

    let app = Router::new()
        .route("/wecom/webhook", get(webhook_get))
        .route("/wecom/webhook", post(webhook_post))
        .with_state(std::sync::Arc::new(state));

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
        Ok(l) => l,
        Err(e) => {
            warn!("Failed to bind webhook port {}: {e}", port);
            return;
        }
    };

    info!("WeCom webhook server listening on port {}", port);
    if let Err(e) = axum::serve(listener, app).await {
        warn!("Webhook server error: {e}");
    }
}

#[derive(Clone)]
struct WebhookState {
    bot_id: String,
    #[allow(dead_code)]
    corp_id: String,
    encoding_aes_key: Option<String>,
    callback_token: Option<String>,
    #[allow(dead_code)]
    is_kf: bool,
    tx: mpsc::Sender<PluginMessage>,
}

// ---------------------------------------------------------------------------
// GET handler — callback URL verification
// ---------------------------------------------------------------------------

async fn webhook_get(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<WebhookState>>,
    Query(params): Query<CallbackParams>,
) -> axum::response::Response {
    let msg_signature = match params.msg_signature.as_deref() {
        Some(s) => s,
        None => {
            return (axum::http::StatusCode::BAD_REQUEST, "missing msg_signature").into_response()
        }
    };
    let timestamp = match params.timestamp.as_deref() {
        Some(s) => s,
        None => return (axum::http::StatusCode::BAD_REQUEST, "missing timestamp").into_response(),
    };
    let nonce = match params.nonce.as_deref() {
        Some(s) => s,
        None => return (axum::http::StatusCode::BAD_REQUEST, "missing nonce").into_response(),
    };
    let echostr = match params.echostr.as_deref() {
        Some(s) => s,
        None => return (axum::http::StatusCode::BAD_REQUEST, "missing echostr").into_response(),
    };

    // Verify signature if callback_token is configured
    if let Some(ref token) = state.callback_token {
        if !crypto::is_valid_wecom_signature(token, timestamp, nonce, echostr, msg_signature) {
            return (axum::http::StatusCode::FORBIDDEN, "invalid signature").into_response();
        }
    }

    // Decrypt echostr if encoding_aes_key is configured
    let response = if let Some(ref aes_key) = state.encoding_aes_key {
        match crypto::decode_wecom_payload(aes_key, echostr) {
            Ok(decrypted) => decrypted,
            Err(e) => {
                warn!("Failed to decrypt echostr: {e}");
                return (
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                    "decrypt error",
                )
                    .into_response();
            }
        }
    } else {
        echostr.to_string()
    };

    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        response,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// POST handler — incoming messages
// ---------------------------------------------------------------------------

async fn webhook_post(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<WebhookState>>,
    Query(_params): Query<CallbackParams>,
    body: String,
) -> &'static str {
    let fields = if let Some(ref aes_key) = state.encoding_aes_key {
        // Encrypted payload — need signature verification
        let xml_fields = match crypto::parse_wecom_xml_fields(&body) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to parse XML: {e}");
                return "success";
            }
        };

        let encrypted = match xml_fields.get("Encrypt") {
            Some(e) => e.clone(),
            None => {
                warn!("No Encrypt field in XML");
                return "success";
            }
        };

        // Decrypt
        match crypto::decode_wecom_payload(aes_key, &encrypted) {
            Ok(decrypted_xml) => match crypto::parse_wecom_xml_fields(&decrypted_xml) {
                Ok(f) => f,
                Err(e) => {
                    warn!("Failed to parse decrypted XML: {e}");
                    return "success";
                }
            },
            Err(e) => {
                warn!("Failed to decrypt payload: {e}");
                return "success";
            }
        }
    } else {
        // Unencrypted payload
        match crypto::parse_wecom_xml_fields(&body) {
            Ok(f) => f,
            Err(e) => {
                warn!("Failed to parse XML: {e}");
                return "success";
            }
        }
    };

    let msg_type = fields.get("MsgType").map(|s| s.as_str()).unwrap_or("");
    let from_user = fields.get("FromUserName").cloned().unwrap_or_default();
    let msg_id = fields.get("MsgId").cloned().unwrap_or_default();
    let event = fields.get("Event").map(|s| s.as_str()).unwrap_or("");

    let timestamp_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    // Build bot_id for routing
    let bot_id = state.bot_id.clone();

    // Handle text messages
    if msg_type == "text" {
        let content = fields.get("Content").cloned().unwrap_or_default();

        let message = PluginMessage {
            channel_type: "wecom".to_string(),
            platform_message_id: msg_id,
            sender_id: from_user.clone(),
            sender_name: from_user.clone(),
            bot_id,
            content: PluginContent::Text(content),
            timestamp_ms,
            is_group: false,
            thread_id: None,
            metadata: HashMap::new(),
        };

        let _ = state.tx.send(message).await;
    } else if msg_type == "event" && (event == "subscribe" || event == "enter_agent") {
        let message = PluginMessage {
            channel_type: "wecom".to_string(),
            platform_message_id: msg_id,
            sender_id: from_user.clone(),
            sender_name: from_user.clone(),
            bot_id,
            content: PluginContent::Command {
                name: event.to_string(),
                args: vec![],
            },
            timestamp_ms,
            is_group: false,
            thread_id: None,
            metadata: HashMap::new(),
        };

        let _ = state.tx.send(message).await;
    }

    "success"
}
