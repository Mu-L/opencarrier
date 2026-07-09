//! Data models for WeChat Official Account channel.

use serde::{Deserialize, Serialize};

// --- Session file (persisted to senders/{app_id}/session.json) ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinOaSessionFile {
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default = "default_sender_key")]
    pub sender_key: String,
    #[serde(default)]
    pub name: String,
    pub app_id: String,
    pub app_secret: String,
    /// WeChat OA Token — shared secret used for checkSign signature verification.
    /// Configured alongside the server URL in the 公众号后台.
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub wechat_id: String,
    #[serde(default)]
    pub bind_agent: Option<String>,
    /// Optional 86bus `bind-openid` endpoint. When set, the weixin-oa webhook
    /// POSTs `{ "openid_sa": <from_user> }` on each inbound message so the 86bus
    /// backend can associate the service-account openid with a business identity.
    /// The returned `matched` role is cached and surfaced to the agent.
    #[serde(default)]
    pub bind_openid_url: Option<String>,
}

fn default_channel() -> String {
    "weixin-oa".to_string()
}

fn default_sender_key() -> String {
    "app_id".to_string()
}

// --- WeChat API types ---

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: Option<String>,
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WechatApiError {
    #[serde(default)]
    pub errcode: i64,
    #[serde(default)]
    pub errmsg: String,
}

impl WechatApiError {
    pub fn from_serde_json_err(e: serde_json::Error) -> Self {
        WechatApiError {
            errcode: -1,
            errmsg: e.to_string(),
        }
    }
}

// --- WeChat XML message types ---

/// Parsed representation of an inbound WeChat OA message.
#[derive(Debug, Clone, Default)]
pub struct OaMessage {
    pub msg_type: String,
    pub content: String,
    pub from_user: String,
    pub to_user: String,
    pub msg_id: String,
    pub create_time: u64,
    /// Event type (subscribe, unsubscribe, CLICK, SCAN, etc.)
    pub event: String,
    /// Event key (menu key, QR code key, etc.)
    pub event_key: String,
    /// Voice recognition result (if the OA has voice-to-text enabled)
    pub recognition: String,
    /// Media ID for voice/image/video messages
    pub media_id: String,
}

/// Parse WeChat XML into OaMessage.
pub fn parse_xml_message(xml: &str) -> Option<OaMessage> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let root = doc.root_element();
    let mut msg = OaMessage::default();

    for node in root.children() {
        let tag = node.tag_name().name();
        let text = node.text().unwrap_or("").to_string();
        match tag {
            "MsgType" => msg.msg_type = text,
            "Content" => msg.content = text,
            "FromUserName" => msg.from_user = text,
            "ToUserName" => msg.to_user = text,
            "MsgId" => msg.msg_id = text,
            "CreateTime" => msg.create_time = text.parse().unwrap_or(0),
            "Event" => msg.event = text,
            "EventKey" => msg.event_key = text,
            "Recognition" => msg.recognition = text,
            "MediaId" => msg.media_id = text,
            _ => {}
        }
    }

    if msg.msg_type.is_empty() && msg.from_user.is_empty() {
        return None;
    }

    Some(msg)
}

// --- JSON-wrapped message (from intermediate proxy layer) ---

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyKeyPayload {
    #[serde(alias = "type")]
    pub msg_type: Option<String>,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub openid: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub eventkey: String,
    #[serde(default)]
    pub raw: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProxyMessage {
    pub key: Option<String>, // JSON string of ProxyKeyPayload
    pub data: Option<String>, // raw XML
    pub sign: Option<String>,
    pub openid: Option<String>,
}

impl ProxyMessage {
    /// Parse a proxy-layer JSON message into an OaMessage.
    pub fn to_oa_message(&self) -> Option<OaMessage> {
        // Try the "key" field first (JSON-wrapped payload)
        if let Some(key_str) = &self.key {
            if let Ok(payload) = serde_json::from_str::<ProxyKeyPayload>(key_str) {
                let msg_type = payload.msg_type.unwrap_or_default();
                let content = payload.key.clone();
                let from_user = payload.openid.clone();
                let event = payload.event.clone();
                let event_key = payload.eventkey.clone();
                if from_user.is_empty() {
                    return None;
                }
                return Some(OaMessage {
                    msg_type,
                    content,
                    from_user,
                    event,
                    event_key,
                    ..Default::default()
                });
            }
        }

        // Try raw XML in "data" field
        if let Some(data) = &self.data {
            if let Some(msg) = parse_xml_message(data) {
                return Some(msg);
            }
        }

        // Fallback: try parsing entire POST body as XML
        None
    }
}