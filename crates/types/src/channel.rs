//! Channel trait — interface for platform message adapters.
//!
//! Each channel (feishu, wecom, weixin, dingtalk) implements this trait
//! to receive inbound messages and send outbound replies.

use tokio::sync::mpsc;

use crate::plugin::PluginMessage;

/// Errors that can occur in channel operations.
#[derive(Debug)]
pub enum ChannelError {
    /// The requested bot/session was not found.
    UnknownBot(String),
    /// Token refresh or acquisition failed.
    TokenFailed(String),
    /// Sending a message failed.
    SendFailed(String),
    /// Operation not supported by this channel mode.
    NotSupported(String),
    /// Configuration error.
    Config(String),
    /// Other channel-specific error.
    Other(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChannelError::UnknownBot(id) => write!(f, "Unknown bot: {id}"),
            ChannelError::TokenFailed(e) => write!(f, "Token error: {e}"),
            ChannelError::SendFailed(e) => write!(f, "Send failed: {e}"),
            ChannelError::NotSupported(e) => write!(f, "Not supported: {e}"),
            ChannelError::Config(e) => write!(f, "Config error: {e}"),
            ChannelError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ChannelError {}

impl From<String> for ChannelError {
    fn from(s: String) -> Self {
        ChannelError::Other(s)
    }
}

/// A channel adapter that bridges an external platform to the carrier kernel.
///
/// Implementations handle platform-specific protocols (WebSocket, HTTP callback, etc.)
/// and translate between platform messages and the unified `PluginMessage` format.
pub trait Channel: Send + Sync {
    /// Channel type identifier (e.g. "weixin", "feishu", "wecom", "dingtalk").
    fn channel_type(&self) -> &str;

    /// Human-readable channel name.
    fn name(&self) -> &str;

    /// Bot identifier this channel belongs to.
    fn bot_id(&self) -> &str;

    /// Start receiving messages from the channel.
    ///
    /// Inbound messages are sent through `sender` for routing to agents.
    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError>;

    /// Send a text message through the channel.
    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError>;

    /// Stop the channel and release resources.
    fn stop(&mut self);

    /// Start a specific sender that was added after initial startup.
    ///
    /// Called by the API after writing a new `senders/{sender_id}/session.json`.
    /// The channel should load the session and start its connection immediately,
    /// without waiting for a polling cycle.
    fn start_sender(&self, sender_id: &str, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError> {
        let _ = (sender_id, sender);
        Err(ChannelError::NotSupported(format!("start_sender not implemented for {}", self.channel_type())))
    }

    /// Whether this channel supports proactive push (sending without an inbound
    /// context). Channels that require a context_token / response_url should
    /// return false; cron and other server-initiated notifications must be
    /// buffered for these channels until the user sends an inbound message.
    fn supports_proactive_push(&self) -> bool {
        false
    }
}
