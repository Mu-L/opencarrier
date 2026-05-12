//! Channel trait — interface for platform message adapters.
//!
//! Each channel (feishu, wecom, weixin, dingtalk) implements this trait
//! to receive inbound messages and send outbound replies.

use tokio::sync::mpsc;

use crate::plugin::PluginMessage;

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
    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String>;

    /// Send a text message through the channel.
    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String>;

    /// Stop the channel and release resources.
    fn stop(&mut self);
}
