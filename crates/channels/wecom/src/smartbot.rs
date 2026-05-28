//! SmartBot channel adapter — WebSocket long connection to WeChat Work AI Bot.
//!
//! Connects to `wss://openws.work.weixin.qq.com`, subscribes with bot_id + secret,
//! handles heartbeat (30s ping), and converts WeChat-specific messages into
//! PluginMessage format.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use types::channel::{Channel, ChannelError};
use types::plugin::{PluginContent, PluginMessage};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::token;

// ---------------------------------------------------------------------------
// Global response_url store (shared across all SmartBot instances)
// ---------------------------------------------------------------------------

/// Global store for pending response_urls keyed by "{bot_id}:{user_id}".
/// Shared across all SmartBotChannel instances so that the kernel dispatch
/// (which picks the first matching channel_type) can find response_urls
/// regardless of which channel stored them.
pub static RESPONSE_URLS: std::sync::OnceLock<Arc<Mutex<HashMap<String, String>>>> =
    std::sync::OnceLock::new();

/// Shared store type alias kept for convenience.
type ResponseUrlStore = Arc<Mutex<HashMap<String, String>>>;

/// WebSocket endpoint for WeChat Work AI Bot.
const WS_URL: &str = "wss://openws.work.weixin.qq.com";
/// Heartbeat interval in seconds.
const PING_INTERVAL_SECS: u64 = 30;
/// Reconnect delay in seconds.
const RECONNECT_DELAY_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// WeChat Work WS protocol types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct WsHeaders {
    req_id: String,
}

#[derive(Debug, Clone, Serialize)]
struct SubscribeBody {
    bot_id: String,
    secret: String,
}

#[derive(Debug, Clone, Serialize)]
struct SubscribeRequest {
    cmd: String,
    headers: WsHeaders,
    body: SubscribeBody,
}

#[derive(Debug, Clone, Deserialize)]
struct MsgCallbackBody {
    msgid: String,
    #[allow(dead_code)]
    aibotid: String,
    #[serde(rename = "chatid")]
    chat_id: Option<String>,
    chattype: String,
    from: MsgFrom,
    msgtype: String,
    response_url: Option<String>,
    #[serde(flatten)]
    content: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct MsgFrom {
    userid: String,
}

#[derive(Debug, Clone, Deserialize)]
struct EventCallbackBody {
    event: EventDetail,
    from: MsgFrom,
    #[allow(dead_code)]
    chattype: String,
}

#[derive(Debug, Clone, Deserialize)]
struct EventDetail {
    eventtype: String,
}

// ---------------------------------------------------------------------------
// SmartBotChannel
// ---------------------------------------------------------------------------

/// WeChat Work SmartBot channel adapter.
///
/// Maintains a single WebSocket long-connection to WeChat Work's bot platform.
/// Automatically reconnects on failure.
///
/// When the host calls `send()`, it looks up the stored `response_url` for
/// the user and sends the reply via HTTP POST (markdown format).
pub struct SmartBotChannel {
    bot_name: String,
    bot_id: String,
    secret: String,
}

impl SmartBotChannel {
    pub fn new(bot_name: String, bot_id: String, secret: String) -> Self {
        Self {
            bot_name,
            bot_id,
            secret,
        }
    }
}

impl Channel for SmartBotChannel {
    fn channel_type(&self) -> &str {
        "wecom"
    }

    fn supports_proactive_push(&self) -> bool {
        // SmartBot replies require a response_url from a recent inbound callback.
        false
    }

    fn name(&self) -> &str {
        "WeChat Work SmartBot"
    }

    fn bot_id(&self) -> &str {
        &self.bot_id
    }

    fn start(&mut self, sender: tokio::sync::mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        let bot_name = self.bot_name.clone();
        let secret = self.secret.clone();
        let bot_id = self.bot_id.clone();
        let response_urls = RESPONSE_URLS
            .get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
            .clone();

        // Spawn the WebSocket connection loop in its own thread with a dedicated
        // tokio runtime.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to create tokio runtime for SmartBot");
            rt.block_on(run_ws_loop(
                bot_name,
                secret,
                bot_id,
                sender,
                response_urls,
            ));
        });

        info!(
            bot = %self.bot_name,
            bot_id = %self.bot_id,
            "SmartBot channel started"
        );

        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError> {
        // Use the passed bot_id (from the original message's bot_id field)
        // rather than self.bot_id, because the kernel dispatch picks channels
        // by channel_type only and may route to a different SmartBotChannel instance.
        let key = format!("{}:{}", bot_id, user_id);
        let response_url = RESPONSE_URLS
            .get()
            .expect("RESPONSE_URLS not initialized")
            .lock()
            .unwrap()
            .remove(&key)
            .ok_or_else(|| {
                ChannelError::NotSupported("No response_url available for this user. SmartBot can only reply within callback context.".to_string())
            })?;

        let bot = crate::token::WECOM_STATE
            .get_session_for_send(bot_id)
            .ok_or_else(|| ChannelError::UnknownBot(bot_id.to_string()))?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| ChannelError::Other(format!("Runtime creation failed: {e}")))?;
        rt.block_on(token::send_smartbot_response_async(
            &bot.entry.http,
            &response_url,
            text,
        ))
        .map_err(ChannelError::SendFailed)
    }

    fn stop(&mut self) {
        // WebSocket loop runs until process exit.
    }
}

// ---------------------------------------------------------------------------
// WebSocket connection loop
// ---------------------------------------------------------------------------

async fn run_ws_loop(
    bot_name: String,
    secret: String,
    bot_id: String,
    sender: tokio::sync::mpsc::Sender<PluginMessage>,
    response_urls: ResponseUrlStore,
) {
    loop {
        match connect_and_handle(
            &bot_name,
            &secret,
            &bot_id,
            &sender,
            &response_urls,
        )
        .await
        {
            Ok(()) => {
                info!("SmartBot WebSocket disconnected normally, reconnecting...");
            }
            Err(e) => {
                error!(
                    "SmartBot WebSocket error: {}, reconnecting in {}s...",
                    e, RECONNECT_DELAY_SECS
                );
            }
        }
        tokio::time::sleep(Duration::from_secs(RECONNECT_DELAY_SECS)).await;
    }
}

async fn connect_and_handle(
    _bot_name: &str,
    secret: &str,
    bot_id: &str,
    sender: &tokio::sync::mpsc::Sender<PluginMessage>,
    response_urls: &ResponseUrlStore,
) -> Result<(), String> {
    info!("SmartBot connecting to {}...", WS_URL);
    let (ws_stream, _) = tokio_tungstenite::connect_async(WS_URL)
        .await
        .map_err(|e| format!("WebSocket connect failed: {e}"))?;
    info!("SmartBot connected!");
    let (mut write, mut read) = ws_stream.split();

    // Subscribe
    let req_id = uuid::Uuid::new_v4().to_string();
    let subscribe = SubscribeRequest {
        cmd: "aibot_subscribe".to_string(),
        headers: WsHeaders {
            req_id: req_id.clone(),
        },
        body: SubscribeBody {
            bot_id: bot_id.to_string(),
            secret: secret.to_string(),
        },
    };
    write
        .send(Message::Text(serde_json::to_string(&subscribe).unwrap()))
        .await
        .map_err(|e| format!("Send subscribe failed: {e}"))?;

    info!("SmartBot subscribe sent: req_id={}", req_id);

    // Wait for subscribe ack
    let sub_resp: serde_json::Value = read
        .next()
        .await
        .ok_or_else(|| "Connection closed before subscribe response".to_string())?
        .map_err(|e| format!("Read subscribe response failed: {e}"))?
        .into_text()
        .map_err(|e| format!("Subscribe response not text: {e}"))?
        .parse()
        .map_err(|e| format!("Parse subscribe response failed: {e}"))?;

    info!("SmartBot subscribe response: {}", sub_resp);
    if sub_resp["errcode"].as_i64() != Some(0) {
        return Err(format!(
            "Subscribe failed: {}",
            sub_resp["errmsg"].as_str().unwrap_or("unknown")
        ));
    }
    info!("SmartBot subscribed successfully!");

    // Main loop: heartbeat + message handling
    let mut ping_interval = tokio::time::interval(Duration::from_secs(PING_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = ping_interval.tick() => {
                let ping = serde_json::json!({
                    "cmd": "ping",
                    "headers": {"req_id": uuid::Uuid::new_v4().to_string()}
                });
                if let Err(e) = write.send(Message::Text(ping.to_string())).await {
                    warn!("SmartBot ping failed: {:?}", e);
                    return Err("Ping failed".to_string());
                }
            }

            msg = read.next() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_))) => {
                        info!("SmartBot received close frame");
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        error!("SmartBot WebSocket read error: {}", e);
                        return Err(format!("Read error: {e}"));
                    }
                    None => {
                        info!("SmartBot WebSocket closed");
                        return Ok(());
                    }
                    _ => continue,
                };

                if let Err(e) = handle_ws_message(&text, bot_id, sender, response_urls).await {
                    warn!("SmartBot message handling error: {}", e);
                }
            }
        }
    }
}

async fn handle_ws_message(
    raw: &str,
    bot_id: &str,
    sender: &tokio::sync::mpsc::Sender<PluginMessage>,
    response_urls: &ResponseUrlStore,
) -> Result<(), String> {
    let json: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("Parse WS message failed: {e}"))?;
    let cmd = json["cmd"].as_str().unwrap_or("");

    match cmd {
        "aibot_msg_callback" => {
            let body: MsgCallbackBody = serde_json::from_value(json["body"].clone())
                .map_err(|e| format!("Parse msg_callback body failed: {e}"))?;

            let user_id = &body.from.userid;
            let chattype = &body.chattype;
            let msg_type = &body.msgtype;

            info!(
                "SmartBot message: chattype={}, from={}, msgtype={}",
                chattype, user_id, msg_type
            );

            // Parse content based on message type
            let content = match msg_type.as_str() {
                "text" => {
                    let text = body
                        .content
                        .as_ref()
                        .and_then(|c| {
                            c.get("text")
                                .and_then(|t| t.get("content"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("")
                        .to_string();

                    if text.is_empty() {
                        return Ok(());
                    }

                    // Strip @mention prefix in group chats
                    let text = if text.starts_with('@') {
                        if let Some(pos) = text.find(' ') {
                            text[pos + 1..].trim().to_string()
                        } else {
                            text
                        }
                    } else {
                        text
                    };

                    PluginContent::Text(text)
                }
                "image" => {
                    let image_url = body
                        .content
                        .as_ref()
                        .and_then(|c| c.get("image").and_then(|i| i.get("image_url")).and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string();
                    let image_data = if !image_url.is_empty() {
                        match reqwest::Client::new().get(&image_url).send().await {
                            Ok(resp) => match resp.bytes().await {
                                Ok(b) => Some(b.to_vec()),
                                Err(_) => None,
                            },
                            Err(_) => None,
                        }
                    } else {
                        None
                    };
                    PluginContent::Image { url: image_url, caption: None, data: image_data }
                }
                _ => {
                    info!("SmartBot ignoring msgtype: {}", msg_type);
                    return Ok(());
                }
            };

            let timestamp_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;

            // Store response_url for later reply via send()
            if let Some(ref url) = body.response_url {
                let key = format!("{}:{}", bot_id, user_id);
                response_urls.lock().unwrap_or_else(|e| e.into_inner()).insert(key, url.clone());
            }

            let mut metadata = HashMap::new();
            metadata.insert(
                "bot_id".to_string(),
                serde_json::Value::String(bot_id.to_string()),
            );
            if let Some(ref chat_id) = body.chat_id {
                metadata.insert(
                    "chat_id".to_string(),
                    serde_json::Value::String(chat_id.clone()),
                );
            }

            let message = PluginMessage {
                channel_type: "wecom".to_string(),
                platform_message_id: body.msgid.clone(),
                sender_id: user_id.clone(),
                sender_name: user_id.clone(),
                bot_id: bot_id.to_string(),
                content,
                timestamp_ms,
                is_group: chattype == "group",
                thread_id: body.chat_id.clone(),
                metadata,
            };

            let _ = sender.send(message).await;
            info!("SmartBot forwarded message from {}", user_id);
        }

        "aibot_event_callback" => {
            let body: EventCallbackBody = serde_json::from_value(json["body"].clone())
                .map_err(|e| format!("Parse event_callback body failed: {e}"))?;

            info!(
                "SmartBot event: eventtype={}, from={}",
                body.event.eventtype, body.from.userid
            );
        }

        "pong" => {
            // Heartbeat response
        }

        "" => {
            // Empty command, ignore
        }

        _ => {
            info!("SmartBot unknown cmd: {}", cmd);
        }
    }

    Ok(())
}
