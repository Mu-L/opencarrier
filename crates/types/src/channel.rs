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

/// How a channel routes inbound messages to agents.
///
/// Declared by each channel via `Channel::routing_mode()`. The bridge branches
/// on this rather than hardcoding channel names, so new one-to-one channels
/// just declare `DirectBind` with zero bridge changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoutingMode {
    /// Per-sender routing with clone/naming/switch support.
    ///
    /// Each sender (user) can have a default agent plus named clones. The bridge
    /// runs the full pipeline: naming flow, rename detection, @-name switching,
    /// `/list`, and the "needs naming" check. Used by weixin iLink, wecom,
    /// feishu, dingtalk (conversational agents that get personalized per user).
    SenderBased,

    /// Direct bind to a single fixed agent — no clones, no naming.
    ///
    /// All inbound messages route straight to the channel's `bind_agent`. The
    /// clone/naming/switch pipeline is skipped entirely. Used by weixin-oa and
    /// future one-to-one channels (OA, SMS, email auto-reply, customer-service
    /// bots) where one channel = one fixed agent serving all users.
    DirectBind,
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

    /// How this channel routes messages to agents.
    ///
    /// Defaults to `SenderBased` (backward compatible). One-to-one channels
    /// (weixin-oa, etc.) override to return `DirectBind`.
    fn routing_mode(&self) -> RoutingMode {
        RoutingMode::SenderBased
    }

    /// Start receiving messages from the channel.
    ///
    /// Inbound messages are sent through `sender` for routing to agents.
    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), ChannelError>;

    /// Send a text message through the channel.
    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), ChannelError>;

    /// Deliver rich content. Each channel picks the highest-fidelity
    /// representation it supports from `content` (miniprogram > video > file >
    /// image > link > text) and sends it to `user_id` on bot `bot_id`.
    ///
    /// The default implementation degrades to plain text (or a formatted link)
    /// via [`send`](Self::send) - so channels that only support text need do
    /// nothing. Channels with richer native forms (weixin-oa miniprogram cards,
    /// wecom kf files/video, weixin iLink images/video) override this to pick
    /// their best form, resolving the sending account by `bot_id` (multi-tenant).
    fn deliver(
        &self,
        content: &crate::content::ContentDescriptor,
        bot_id: &str,
        user_id: &str,
    ) -> Result<(), ChannelError> {
        let text = content.as_text().ok_or_else(|| {
            ChannelError::NotSupported(
                "channel has no representation for this content and no text fallback".into(),
            )
        })?;
        self.send(bot_id, user_id, &text)
    }

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
