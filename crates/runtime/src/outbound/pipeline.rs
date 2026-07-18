//! Unified outbound reply pipeline shared by the interactive bridge and cron.

use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use crate::kernel_handle::KernelHandle;

use super::deliver::process_deliver_markers_pub;
use super::notify::process_notify_markers;
use super::publish::process_publish_markers;
use super::silence::{is_no_reply_sentinel, sanitize_wechat_text};
use super::types::{ChannelDeliverFn, ChannelSendFn, NotifyTarget};

/// Inputs for [`prepare_outbound`]. Callers choose interactive vs cron behaviour
/// via `process_notify` and `sanitize_wechat` (cron keeps both false).
pub struct OutboundCtx<'a> {
    /// Required for `[PUBLISH]` side effects. When `None`, PUBLISH markers are
    /// still stripped (via an empty-kernel skip) — callers that need publish
    /// must pass a handle.
    pub kernel: Option<Arc<dyn KernelHandle>>,
    pub send_fn: Option<ChannelSendFn>,
    pub deliver_fn: Option<ChannelDeliverFn>,
    pub content: Option<&'a types::content::ContentConfig>,
    pub channel_type: &'a str,
    pub bot_id: &'a str,
    pub sender_id: &'a str,
    pub agent_id: &'a str,
    /// Interactive only: process `[NOTIFY:…]` markers.
    pub process_notify: bool,
    pub notify_routes: Option<&'a HashMap<String, NotifyTarget>>,
    /// Pre-resolved admin `sender_id`s for `recipients = "admins"` routes.
    pub admin_sender_ids: &'a [String],
    /// Interactive WeChat channels only.
    pub sanitize_wechat: bool,
}

/// Result of preparing an agent reply for channel delivery.
pub struct OutboundResult {
    /// Reply text with markers stripped (and optionally WeChat-sanitized).
    pub cleaned_text: String,
    /// When true, the caller must not send `cleaned_text` to the user channel
    /// (empty after markers, or a no-reply sentinel). Side-effect markers have
    /// already been processed.
    pub suppress_text_send: bool,
}

/// Process outbound markers in fixed order, then decide whether final text
/// should be sent:
///
/// 1. NOTIFY (if `process_notify`)
/// 2. PUBLISH
/// 3. DELIVER
/// 4. silence / empty → `suppress_text_send`
/// 5. optional WeChat sanitize on remaining text (does not affect suppress)
pub async fn prepare_outbound(response: &str, ctx: OutboundCtx<'_>) -> OutboundResult {
    let mut text = response.to_string();

    // 1. NOTIFY
    if ctx.process_notify {
        text = process_notify_markers(
            &text,
            ctx.send_fn.as_ref(),
            ctx.notify_routes,
            ctx.sender_id,
            ctx.bot_id,
            ctx.admin_sender_ids,
        );
    }

    // 2. PUBLISH
    if let Some(kernel) = ctx.kernel {
        text = process_publish_markers(
            kernel,
            ctx.send_fn.clone(),
            ctx.channel_type,
            ctx.bot_id,
            ctx.sender_id,
            ctx.agent_id,
            &text,
        );
    }

    // 3. DELIVER
    text = process_deliver_markers_pub(
        ctx.deliver_fn,
        ctx.content,
        ctx.channel_type,
        ctx.bot_id,
        ctx.sender_id,
        &text,
    )
    .await;

    // 4. Suppress final text send for empty replies or no-reply sentinels.
    //    Marker side effects above have already run.
    let suppress_text_send = text.trim().is_empty() || is_no_reply_sentinel(&text);
    if suppress_text_send {
        if is_no_reply_sentinel(&text) {
            info!(
                channel = %ctx.channel_type,
                bot = %ctx.bot_id,
                sender = %ctx.sender_id,
                "Outbound suppressing no-reply sentinel — not sending to channel"
            );
        }
        return OutboundResult {
            cleaned_text: text,
            suppress_text_send: true,
        };
    }

    // 5. Optional WeChat sanitize (only for text that will be sent).
    if ctx.sanitize_wechat {
        text = sanitize_wechat_text(&text);
    }

    OutboundResult {
        cleaned_text: text,
        suppress_text_send: false,
    }
}
