//! `[NOTIFY:type]content[/NOTIFY]` cross-channel push markers.

use std::collections::HashMap;

use tracing::{error, info, warn};

use super::parse::parse_notify_markers;
use super::types::{ChannelSendFn, NotifyTarget};

/// Parse and dispatch `[NOTIFY:…]` markers. Returns the reply text with markers
/// stripped. Pushes are fire-and-forget (`spawn_blocking`); failures are logged.
///
/// When `routes` or `send_fn` is missing, markers are still stripped so they do
/// not leak to the user.
///
/// `admin_sender_ids` is used when a route has `recipients = "admins"`. Each id
/// is routed by format: `*@im.wechat` → route's channel/bot_id; bare openid →
/// `weixin-oa` with `source_bot_id`.
pub fn process_notify_markers(
    response: &str,
    send_fn: Option<&ChannelSendFn>,
    routes: Option<&HashMap<String, NotifyTarget>>,
    source_sender_id: &str,
    source_bot_id: &str,
    admin_sender_ids: &[String],
) -> String {
    let (notifications, cleaned) = parse_notify_markers(response);
    if notifications.is_empty() {
        return cleaned;
    }

    let Some(send_fn) = send_fn else {
        warn!(
            notify_count = notifications.len(),
            "Notify markers present but no channel_send_fn configured"
        );
        return cleaned;
    };
    let Some(routes) = routes else {
        warn!(
            notify_count = notifications.len(),
            "Notify markers present but no notify_routes configured"
        );
        return cleaned;
    };

    for (ntype, content) in &notifications {
        let Some(target) = routes.get(ntype) else {
            warn!(notify_type = %ntype, "Notify marker has no route in notify_routes.json");
            continue;
        };

        let msg = match &target.prefix {
            Some(p) if !p.is_empty() => {
                format!("{p}\n{content}\n来源用户: {source_sender_id}")
            }
            _ => format!("{content}\n来源用户: {source_sender_id}"),
        };

        // Resolve recipient user_ids with per-recipient channel routing.
        // iLink IDs ("xxx@im.wechat") go through the weixin (iLink)
        // channel; bare openids go through weixin-oa (customer-send API).
        let recipient_ids: Vec<(String, String, String)> =
            if target.recipients.as_deref() == Some("admins") {
                if admin_sender_ids.is_empty() {
                    warn!(
                        notify_type = %ntype,
                        "recipients=admins but no admins resolved"
                    );
                }
                admin_sender_ids
                    .iter()
                    .map(|sender_id| {
                        if sender_id.contains("@im.wechat") {
                            (
                                target.channel.clone(),
                                target.bot_id.clone(),
                                sender_id.clone(),
                            )
                        } else {
                            (
                                "weixin-oa".to_string(),
                                source_bot_id.to_string(),
                                sender_id.clone(),
                            )
                        }
                    })
                    .collect()
            } else if target.user_id.is_empty() {
                warn!(
                    notify_type = %ntype,
                    "Notify route has empty user_id and recipients != admins"
                );
                Vec::new()
            } else {
                vec![(
                    target.channel.clone(),
                    target.bot_id.clone(),
                    target.user_id.clone(),
                )]
            };

        for (channel, bot_id, user_id) in recipient_ids {
            info!(
                notify_type = %ntype,
                target_channel = %channel,
                target_user = %user_id,
                "Notify marker matched, pushing cross-channel"
            );
            let send_fn = send_fn.clone();
            let msg = msg.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(e) = send_fn(&channel, &bot_id, &user_id, &msg) {
                    error!(%channel, %user_id, error = %e, "Notify push failed");
                }
            });
        }
    }

    cleaned
}
