//! Shared function types and notify routing config for outbound delivery.

use std::sync::Arc;

use types::channel::{ChannelError, RoutingMode};

/// A function that can send a response through a channel.
/// Used by the bridge to deliver agent replies back to users.
pub type ChannelSendFn =
    Arc<dyn Fn(&str, &str, &str, &str) -> Result<(), ChannelError> + Send + Sync>;

/// A function that delivers rich content through a channel.
/// `(channel_type, bot_id, user_id, content)` -> result. Used by the bridge to
/// deliver `[DELIVER:key]` marker content in the highest-fidelity form the
/// channel supports (falls back to text via `Channel::deliver`'s default).
pub type ChannelDeliverFn = Arc<
    dyn Fn(&str, &str, &str, &types::content::ContentDescriptor) -> Result<(), ChannelError>
        + Send
        + Sync,
>;

/// A function that reports a channel type's routing mode.
/// Used by the bridge to decide whether to run the multi-clone pipeline.
pub type RoutingModeFn = Arc<dyn Fn(&str) -> RoutingMode + Send + Sync>;

/// Where to push a notification of a given type.
///
/// Configured in `~/.opencarrier/notify_routes.json`. The agent emits a
/// `[NOTIFY:type]content[/NOTIFY]` marker in its reply; the bridge looks up
/// the type here and pushes via channel_send_fn.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct NotifyTarget {
    pub channel: String,
    #[serde(default)]
    pub bot_id: String,
    /// Explicit recipient. Ignored when `recipients == Some("admins")`.
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub prefix: Option<String>,
    /// `"admins"` → fan out to every admin (creator + approved) of the agent
    /// that produced the reply, using `channel`/`bot_id`/`prefix` as the
    /// template. Resolved from the agent's `admins.json` at push time, so
    /// adding/removing admins changes the recipient set automatically.
    /// Absent → single push to `user_id` (legacy behavior).
    #[serde(default)]
    pub recipients: Option<String>,
}
