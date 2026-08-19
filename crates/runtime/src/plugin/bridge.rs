//! Plugin bridge — routes messages between plugin channels and the kernel.
//!
//! Marker processing (PUBLISH / DELIVER / silence / WeChat sanitize) lives in
//! [`crate::outbound`]. This module owns inbound routing and the interactive
//! `send_response` orchestrator; public types and marker helpers are re-exported
//! here for backward-compatible import paths.

use std::sync::Arc;

use types::channel::RoutingMode;
use types::error::CarrierError;
use types::plugin::{PluginContent, PluginMessage};
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::router::SenderRouter;
use crate::kernel_handle::KernelHandle;
// Re-export outbound types and marker APIs so existing
// `runtime::plugin::bridge::…` imports keep compiling.
pub use crate::outbound::{
    ChannelDeliverFn, ChannelSendFn, ContentRegistry, NotifyTarget, OutboundCtx, OutboundResult,
    RoutingModeFn, is_no_reply_sentinel, prepare_outbound, process_deliver_markers_pub,
    process_publish_markers,
};

// ---------------------------------------------------------------------------
// Plugin bridge manager
// ---------------------------------------------------------------------------

/// Routes inbound plugin messages to agents and delivers responses back
/// through the originating channel.
#[derive(Clone)]
pub struct PluginBridgeManager {
    /// Kernel handle for sending messages to agents.
    kernel: Arc<dyn KernelHandle>,
    /// Function to send responses through channels (channel_type, bot_id, user_id, text).
    channel_send_fn: Option<ChannelSendFn>,
    /// Function to deliver rich content through channels
    /// (channel_type, bot_id, user_id, content). Backs `[DELIVER:key]` markers.
    channel_deliver_fn: Option<ChannelDeliverFn>,
    /// Function to look up a channel's routing mode by channel_type.
    routing_mode_fn: Option<RoutingModeFn>,
    /// Notify routing: notify_type → push target. Loaded from notify_routes.json.
    notify_routes: Option<Arc<std::collections::HashMap<String, NotifyTarget>>>,
    /// Sender-based routing (route_key → agent_id).
    sender_router: Option<Arc<SenderRouter>>,
    /// Cron delivery: last-channel tracking + buffered notifications.
    cron_delivery: Option<Arc<memory::CronDeliveryStore>>,
    /// route_key of users currently in the "naming" flow (waiting for agent name).
    pending_naming: Arc<DashMap<String, String>>,
    /// Per route_key mutex so same-user messages (esp. WeChat) run serially.
    /// Prevents concurrent agent loops racing the same session when multiple
    /// inbounds land close together. Cross-user traffic still concurrent.
    route_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl PluginBridgeManager {
    /// Create a new bridge manager.
    pub fn new(kernel: Arc<dyn KernelHandle>) -> Self {
        Self {
            kernel,
            channel_send_fn: None,
            channel_deliver_fn: None,
            routing_mode_fn: None,
            notify_routes: None,
            sender_router: None,
            cron_delivery: None,
            pending_naming: Arc::new(DashMap::new()),
            route_locks: Arc::new(DashMap::new()),
        }
    }

    /// Set the sender-based router (enables route_key routing).
    pub fn set_sender_router(&mut self, router: Arc<SenderRouter>) {
        self.sender_router = Some(router);
    }

    /// Set the cron delivery store (enables last-channel tracking + buffer drain).
    pub fn set_cron_delivery(&mut self, store: Arc<memory::CronDeliveryStore>) {
        self.cron_delivery = Some(store);
    }

    /// Set the channel send function for delivering responses.
    pub fn set_channel_send_fn(&mut self, f: ChannelSendFn) {
        self.channel_send_fn = Some(f);
    }

    /// Set the channel deliver function for delivering rich content (`[DELIVER]`).
    pub fn set_channel_deliver_fn(&mut self, f: ChannelDeliverFn) {
        self.channel_deliver_fn = Some(f);
    }

    /// Set the routing-mode probe (tells the bridge which channels are DirectBind).
    pub fn set_routing_mode_fn(&mut self, f: RoutingModeFn) {
        self.routing_mode_fn = Some(f);
    }

    /// Set notify routing (enables `[NOTIFY:type]content[/NOTIFY] markers → cross-channel push).
    pub fn set_notify_routes(&mut self, routes: Arc<std::collections::HashMap<String, NotifyTarget>>) {
        self.notify_routes = Some(routes);
    }

    /// Load an agent's content config via the shared [`ContentRegistry`].
    fn load_content_config(&self, agent_id: &str) -> Option<Arc<types::content::ContentConfig>> {
        let ws = self.kernel.resolve_agent_workspace(agent_id)?;
        ContentRegistry::global().load(agent_id, std::path::Path::new(&ws))
    }

    /// Resolve admin sender_ids for `[NOTIFY]` `recipients=admins` fan-out.
    fn resolve_admin_sender_ids(&self, agent_id: &str) -> Vec<String> {
        if agent_id.is_empty() {
            return Vec::new();
        }
        self.kernel
            .resolve_agent_workspace(agent_id)
            .map(|ws| {
                crate::plugin::admin_store::read_admins(std::path::Path::new(&ws))
                    .admins
                    .into_iter()
                    .map(|a| a.sender_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Backward-compatible: add a loaded plugin to the bridge.
    /// Builds a channel_send_fn that routes through the plugin's channels.
    pub fn add_plugin(&mut self, plugin: Arc<dyn super::instance::PluginInstance>) {
        let f: ChannelSendFn = Arc::new(move |channel_type, bot_id, user_id, text| {
            // Try exact match first
            for channel in plugin.channels() {
                if channel.channel_type == channel_type && channel.bot_id == bot_id {
                    return plugin.channel_send(channel, bot_id, user_id, text);
                }
            }
            // Fallback: any channel of the same type
            for channel in plugin.channels() {
                if channel.channel_type == channel_type {
                    return plugin.channel_send(channel, bot_id, user_id, text);
                }
            }
            Err(CarrierError::InvalidInput(format!("No plugin channel found for type: {}", channel_type)))
        });

        // If no send fn set yet, use this one. Otherwise, chain them.
        if self.channel_send_fn.is_none() {
            self.channel_send_fn = Some(f);
        } else {
            // Already have a send fn — keep the first one (or we could chain,
            // but in practice the ChannelManager sets one fn that covers all channels)
        }
    }

    /// Run the message processing loop (consumes self).
    ///
    /// Each message is handled in its own tokio task so different users stay
    /// concurrent. Same `route_key` is serialized inside `handle_inbound`
    /// via `route_locks` (WeChat iLink redelivery / rapid multi-send).
    pub async fn run(self, mut rx: mpsc::Receiver<PluginMessage>) {
        info!("Plugin bridge started");

        while let Some(msg) = rx.recv().await {
            let bridge = self.clone();
            tokio::spawn(async move {
                bridge.handle_inbound(msg).await;
            });
        }

        info!("Plugin bridge stopped (channel closed)");
    }

    // -----------------------------------------------------------------------
    // Route key — platform-dependent routing key
    // -----------------------------------------------------------------------

    /// Return the routing key for a message:
    /// - WeChat iLink: sender_id (one user = one assistant)
    /// - WeCom/Feishu/DingTalk: bot_id (one bot = one assistant)
    fn route_key(&self, msg: &PluginMessage) -> String {
        match msg.channel_type.as_str() {
            "weixin" => msg.sender_id.clone(),
            _ => msg.bot_id.clone(),
        }
    }

    // -----------------------------------------------------------------------
    // Inbound message handling
    // -----------------------------------------------------------------------

    async fn handle_inbound(&self, msg: PluginMessage) {
        // Serialize same route_key early (before heavy work / agent loop) so
        // concurrent WeChat redeliveries queue instead of multi-replying.
        let rk = self.route_key(&msg);
        let lock = self
            .route_locks
            .entry(rk.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _route_guard = lock.lock().await;

        // Text is resolved immediately. Images/files are described *after*
        // agent routing + save, so we can hand vision a public view_url
        // instead of a huge base64 data URI.
        let mut deferred_media = false;
        let text = match msg.content.as_text() {
            Some(t) => t.to_string(),
            None if matches!(
                msg.content,
                PluginContent::Image { .. } | PluginContent::File { .. }
            ) =>
            {
                deferred_media = true;
                String::new()
            }
            None => self.resolve_non_text_content(&msg).await,
        };

        info!(
            channel = %msg.channel_type,
            bot = %msg.bot_id,
            route_key = %rk,
            text_len = text.len(),
            platform_message_id = %msg.platform_message_id,
            "Bridge handling inbound message"
        );

        // Record the channel this sender last used (for cron delivery routing).
        // Keyed by the USER identity (msg.sender_id), not the route_key: the
        // push path (do_push_message gate / deliver_via_last_channel) addresses
        // users, and for weixin-oa/wecom the route_key is the app/bot id —
        // keying by rk left those users without a sender_channels row and
        // stranded their buffered notifications (buffer side already keys by
        // user id, so drain must match).
        if let Some(ref cron_delivery) = self.cron_delivery {
            if let Err(e) = cron_delivery.touch_sender_channel(&msg.sender_id, &msg.channel_type, &msg.bot_id) {
                tracing::warn!(error = %e, "Failed to touch sender channel");
            }
        }

        // Deliver any buffered cron notifications for this sender before
        // processing the actual message. We use msg's context so the reply
        // can use the active context_token / response_url.
        if let Some(ref cron_delivery) = self.cron_delivery {
            match cron_delivery.drain_pending(&msg.sender_id) {
                Ok(notifications) if !notifications.is_empty() => {
                    for n in notifications {
                        self.send_response(&msg, &n.message).await;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "Failed to drain pending notifications"),
            }
        }

        // Determine this channel's routing mode. DirectBind channels (weixin-oa,
        // future one-to-one channels) skip the entire multi-clone pipeline and
        // route straight to their fixed bind_agent.
        let direct_bind = self
            .routing_mode_fn
            .as_ref()
            .map(|f| f(&msg.channel_type, &msg.bot_id) == RoutingMode::DirectBind)
            .unwrap_or(false);

        // Multi-clone pipeline: naming flow, rename detection, @-name switching,
        // and /list — only relevant for SenderBased channels.
        if !direct_bind {
            // 1. Check if route is in naming flow
            if let Some((_, agent_id)) = self.pending_naming.remove(&rk) {
                let name = text.trim().to_string();
            if !name.is_empty() {
                if let Some(ref router) = self.sender_router {
                    router.set_alias(&rk, &name, &agent_id);
                }
                let confirm = format!("好的，我现在叫{name}。以后叫我{name}我就出来啦！");
                self.send_response(&msg, &confirm).await;
            } else {
                // Empty name, keep in pending
                self.pending_naming.insert(rk.clone(), agent_id);
                self.send_response(&msg, "名字不能为空哦，请再告诉我你想叫我什么？").await;
            }
            return;
        }

        // 2. Detect rename requests (e.g. "以后叫我小趣", "改名叫小趣")
        if let Some(ref router) = self.sender_router {
            if let Some(new_name) = Self::parse_rename(&text) {
                if let Some(agent_id) = router.get_route(&rk) {
                    router.set_alias(&rk, &new_name, &agent_id);
                    let confirm = format!("好的，我以后叫{new_name}啦！叫我{new_name}我就出来。");
                    self.send_response(&msg, &confirm).await;
                    return;
                }
            }
        }

        // 3. Try name-based routing (message starts with an alias)
        if let Some((agent_id, remaining)) = self.try_route_by_name(&text, &rk) {
            info!(
                channel = %msg.channel_type,
                bot = %msg.bot_id,
                agent = %agent_id,
                route_key = %rk,
                "Routing by name to agent"
            );

            // Update default route to this agent
            if let Some(ref router) = self.sender_router {
                router.set_route(&rk, &agent_id);
            }

            let msg_text = if remaining.is_empty() { "你好".to_string() } else { remaining };
            match self
                .kernel
                .send_to_agent(
                    &agent_id,
                    &msg_text,
                    Some(&msg.sender_id),
                    Some(&msg.sender_name),
                    None,
                    Some(&rk),
                    Some(&msg.channel_type),
                )
                .await
            {
                Ok(response) => {
                    if response.trim().is_empty() {
                        // Sender-gate Merged outcome: this message was
                        // coalesced into an earlier combined turn whose reply
                        // already went to this recipient — sending here would
                        // duplicate it.
                        debug!(agent = %agent_id, "empty response (merged into a prior turn) — no send");
                    } else {
                        self.send_response(&msg, &response).await;
                    }
                }
                Err(e) => {
                    error!(agent = %agent_id, error = %e, "Failed to send message to agent");
                    self.send_response(&msg, "抱歉，处理消息时遇到了问题，请稍后再试。").await;
                }
            }
            return;
        }

        // 4. /list command
        if text.trim().eq_ignore_ascii_case("/list") {
            let response = self.format_agent_list(&rk);
            self.send_response(&msg, &response).await;
            return;
        }
        } // end multi-clone pipeline (!direct_bind)

        // 5. Default routing via route_key
        let agent_id = self.resolve_agent(&msg);
        if agent_id.is_empty() {
            warn!(
                channel = %msg.channel_type,
                bot = %msg.bot_id,
                route_key = %rk,
                "No agent resolved, dropping message"
            );
            return;
        }

        // 6. Check if this agent needs a name (SenderBased only — DirectBind
        // channels have a fixed agent that never needs naming)
        if !direct_bind {
        if let Some(ref router) = self.sender_router {
            if router.needs_naming(&rk) {
                info!(route_key = %rk, agent = %agent_id, "Agent needs naming, entering naming flow");
                self.pending_naming.insert(rk.clone(), agent_id.clone());
                self.send_response(&msg, "请给我取个名字吧！以后叫这个名字我就会出来。").await;
                return;
            }
        }
        } // end needs-naming check (!direct_bind)

        info!(
            channel = %msg.channel_type,
            bot = %msg.bot_id,
            agent = %agent_id,
            route_key = %rk,
            "Routing plugin message to agent"
        );

        // Auto-assign first sender as creator admin (if admins.json is empty)
        if !msg.sender_id.is_empty() {
            if let Some(ws) = self.kernel.resolve_agent_workspace(&agent_id) {
                let ws_path = std::path::Path::new(&ws);
                match crate::plugin::admin_store::auto_assign_creator(ws_path, &msg.sender_id, &msg.sender_name) {
                    Ok(true) => info!(agent = %agent_id, sender = %msg.sender_id, "Auto-assigned creator admin"),
                    Ok(false) => {}
                    Err(e) => warn!(agent = %agent_id, error = %e, "Failed to auto-assign creator admin"),
                }
            }
        }

        // Intercept admin permission request
        let trimmed = text.trim();
        if trimmed == "申请管理权限" || trimmed == "申请管理员" || trimmed == "申请管理员权限" {
            if let Some(ws) = self.kernel.resolve_agent_workspace(&agent_id) {
                let ws_path = std::path::Path::new(&ws);
                match crate::plugin::admin_store::add_pending(ws_path, &msg.sender_id, &msg.sender_name) {
                    Ok(()) => {
                        self.send_response(&msg, "已收到您的管理权限申请，请等待管理员审批。").await;
                    }
                    Err(crate::plugin::admin_store::AdminError::AlreadyAdmin) => {
                        self.send_response(&msg, "您已经是管理员了。").await;
                    }
                    Err(crate::plugin::admin_store::AdminError::AlreadyPending) => {
                        self.send_response(&msg, "您已提交过申请，请耐心等待审批。").await;
                    }
                    Err(_) => {
                        self.send_response(&msg, "申请提交失败，请稍后再试。").await;
                    }
                }
            }
            return;
        }

        // Automation rules (weixin iLink): fixed keyword replies, zero LLM.
        // Mirrors the weixin-oa webhook gate — rules scoped (channel "weixin",
        // app_id = resolved agent) matched on text deliver a fixed reply and
        // SKIP the agent turn. iLink bot_id is "default" for every session, so
        // the agent is the only meaningful scope (one business = one rule set).
        // Runs after the system interactions (naming / rename / @name / /list
        // / admin request) so those keep their meaning even when a keyword
        // rule would also match the same text.
        if msg.channel_type == "weixin"
            && self.weixin_automation_gate(&msg, &agent_id, &text).await
        {
            return;
        }

        // Save media data (files and images) to the agent's workspace input/ directory
        let saved_filename = self.save_media_to_input(&msg, &agent_id, &rk).await;

        // Describe images via public view_url after save (vision fetches URL).
        let text = if deferred_media {
            self.resolve_media_after_save(&msg, &agent_id, &rk, saved_filename.as_deref())
                .await
        } else {
            text
        };

        // Append file_read / path hint if media was saved
        let final_text = match saved_filename {
            Some(ref rel_path) => {
                // Prefer a short relative path under the sender dir (input/xxx)
                let short = rel_path
                    .rsplit_once("/input/")
                    .map(|(_, f)| format!("input/{f}"))
                    .unwrap_or_else(|| rel_path.clone());
                format!(
                    "{text}\n[文件已保存至 {short}，可用 file_list(\"input/\") 或 image_analyze 路径读取]"
                )
            }
            None => text,
        };

        match self
            .kernel
            .send_to_agent(
                &agent_id,
                &final_text,
                Some(&msg.sender_id),
                Some(&msg.sender_name),
                None,
                Some(&rk),
                Some(&msg.channel_type),
            )
            .await
        {
            Ok(response) => {
                self.send_response(&msg, &response).await;
            }
            Err(e) => {
                error!(
                    agent = %agent_id,
                    error = %e,
                    "Failed to send message to agent"
                );
                self.send_response(&msg, "抱歉，处理消息时遇到了问题，请稍后再试。").await;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Automation rules (weixin iLink) — fixed keyword replies, zero LLM
    // -----------------------------------------------------------------------

    /// iLink automation gate: deliver fixed replies for matching keyword
    /// rules WITHOUT the agent LLM. Rules are scoped (channel "weixin",
    /// app_id = the agent that would have answered) — iLink bot_id is
    /// "default" for every session, so the resolved agent is the scope.
    /// Returns true when the inbound was fully handled (at least one reply
    /// delivered, none failed) and the agent turn should be skipped. Bypass
    /// tasks (`Push` to a target other than `"current"`) are delivered but
    /// do NOT skip the agent — same semantics as the weixin-oa webhook gate.
    async fn weixin_automation_gate(
        &self,
        msg: &PluginMessage,
        agent_id: &str,
        text: &str,
    ) -> bool {
        let rules = match self.kernel.automation_rule_list("weixin", agent_id).await {
            Ok(r) => r,
            Err(e) => {
                warn!(bot = %msg.bot_id, error = %e, "weixin: automation_rule_list failed");
                return false;
            }
        };
        if rules.is_empty() || text.trim().is_empty() {
            return false;
        }

        let mut delivered = 0usize;
        let mut failures = 0usize;
        for rule in &rules {
            if !weixin_keyword_rule_hit(rule, text) {
                continue;
            }
            match rule.task_kind {
                // Bypass push (target ≠ current): deliver, agent still runs.
                types::automation::TaskKind::Push
                    if rule.target != "current" && !rule.target.is_empty() =>
                {
                    match serde_json::from_value::<types::content::ContentDescriptor>(
                        rule.task_payload.clone(),
                    ) {
                        Ok(content) => {
                            if let Err(e) = self
                                .kernel
                                .push_message(
                                    rule.target.clone(),
                                    content,
                                    agent_id.to_string(),
                                    msg.bot_id.clone(),
                                )
                                .await
                            {
                                warn!(
                                    bot = %msg.bot_id, rule_id = %rule.id,
                                    target = %rule.target, error = %e,
                                    "weixin: automation bypass push failed"
                                );
                            }
                        }
                        Err(e) => warn!(
                            rule_id = %rule.id, error = %e,
                            "weixin: Push rule has bad task_payload"
                        ),
                    }
                }
                // Reply to the triggering user; counts toward the agent skip.
                types::automation::TaskKind::PushText
                | types::automation::TaskKind::Push
                | types::automation::TaskKind::PushMiniprogram => {
                    match self.weixin_deliver_rule_reply(msg, agent_id, rule).await {
                        Ok(()) => delivered += 1,
                        Err(e) => {
                            warn!(
                                bot = %msg.bot_id, rule_id = %rule.id, error = %e,
                                "weixin: automation reply failed (agent will run)"
                            );
                            failures += 1;
                        }
                    }
                }
                // Needs kernel.notify_admins (not on KernelHandle); the upsert
                // tool rejects notify_admin for channel "weixin". Never fires.
                types::automation::TaskKind::NotifyAdmin => {
                    warn!(
                        rule_id = %rule.id,
                        "weixin: notify_admin rule matched but is not supported on iLink"
                    );
                }
            }
        }
        if delivered > 0 && failures == 0 {
            info!(
                bot = %msg.bot_id,
                sender = %msg.sender_id,
                delivered,
                "weixin: automation rule matched, fixed reply delivered (agent skipped)"
            );
            true
        } else {
            false
        }
    }

    /// Deliver one rule's reply to the triggering user. Text payloads go
    /// through `send_response` (same path as agent replies: WeChat sanitize
    /// and notify markers); rich payloads (image/video URL) go through
    /// `push_message`, which routes via the sender's recorded channel.
    async fn weixin_deliver_rule_reply(
        &self,
        msg: &PluginMessage,
        agent_id: &str,
        rule: &types::automation::AutomationRule,
    ) -> types::error::CarrierResult<()> {
        // PushText and plain-text Push both carry {"text": "..."}.
        if let Some(text) = rule.task_payload.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                self.send_response(msg, text).await;
                return Ok(());
            }
        }
        let content: types::content::ContentDescriptor =
            serde_json::from_value(rule.task_payload.clone()).map_err(|e| {
                CarrierError::Serialization(format!(
                    "rule task_payload is neither {{text}} nor a ContentDescriptor: {e}"
                ))
            })?;
        if content.image.is_none() && content.video.is_none() {
            // Text-only descriptors were handled above; a miniprogram-only
            // payload has no iLink representation — fail so the agent runs.
            return Err(CarrierError::InvalidInput(
                "iLink cannot deliver this payload (miniprogram cards unsupported)".into(),
            ));
        }
        self.kernel
            .push_message(
                msg.sender_id.clone(),
                content,
                agent_id.to_string(),
                msg.bot_id.clone(),
            )
            .await
    }

    // -----------------------------------------------------------------------
    // Name-based routing
    // -----------------------------------------------------------------------

    /// Try to route by matching the start of the text against aliases for the route_key.
    /// Supports two formats:
    ///   `@名字` or `@名字 你好` — @ prefix triggers agent switch
    ///   `名字 你好` — name at start of text (legacy format)
    /// Returns (agent_id, remaining_text) if matched, None otherwise.
    /// Parse a rename request from the user's message.
    /// Matches patterns like "以后叫我小趣", "改名叫小趣", "叫我小趣",
    /// "以后叫小趣", "换个名字叫小趣", "重新叫小趣".
    /// Returns the new name if matched, None otherwise.
    fn parse_rename(text: &str) -> Option<String> {
        let t = text.trim();
        let patterns = [
            "以后叫我",
            "以后叫",
            "改名叫",
            "改名叫我",
            "叫我",
            "换个名字叫",
            "换个名叫",
            "重新叫",
            "换个叫法叫",
            "改个名叫",
        ];
        for pat in &patterns {
            if let Some(rest) = t.strip_prefix(pat) {
                let name = rest
                    .trim()
                    .trim_end_matches(['吧', '！', '!', '。', '~'])
                    .trim();
                if !name.is_empty() && name.len() <= 20 {
                    return Some(name.to_string());
                }
            }
        }
        None
    }

    fn try_route_by_name(&self, text: &str, route_key: &str) -> Option<(String, String)> {
        let router = self.sender_router.as_ref()?;
        if route_key.is_empty() {
            return None;
        }

        let aliases = router.list_aliases(route_key);
        if aliases.is_empty() {
            return None;
        }

        // Strip leading @ if present, then match against aliases
        let text_stripped = text.strip_prefix('@').unwrap_or(text);
        let text_lower = text_stripped.to_lowercase();

        // Find longest matching alias at the start of text
        let mut best_name: Option<&str> = None;
        let mut best_agent_id: Option<String> = None;
        let mut best_len = 0;

        for (name, agent_id) in &aliases {
            if text_lower.starts_with(name.as_str()) && name.len() > best_len {
                // Name must be followed by a separator or end of text
                let rest = &text_lower[name.len()..];
                if rest.is_empty() || rest.starts_with('，') || rest.starts_with(',') || rest.starts_with(' ') || rest.starts_with('！') || rest.starts_with('!') || rest.starts_with('？') || rest.starts_with('?') {
                    best_name = Some(name);
                    best_agent_id = Some(agent_id.clone());
                    best_len = name.len();
                }
            }
        }

        match (best_name, best_agent_id) {
            (Some(_), Some(agent_id)) => {
                // Strip the name and separator from the text
                let remaining = text_stripped[best_len..]
                    .trim_start_matches(['，', ',', ' ', '！', '!', '？', '?'])
                    .to_string();
                info!(
                    route_key = %route_key,
                    agent = %agent_id,
                    "Name-based route matched"
                );
                Some((agent_id, remaining))
            }
            _ => None,
        }
    }

    /// Format the agent list for a route_key, showing aliases and available agents.
    fn format_agent_list(&self, route_key: &str) -> String {
        let agents = self.kernel.list_agents();
        let mut lines = Vec::new();

        if let Some(ref router) = self.sender_router {
            // Only the clones THIS sender installed — not the global agent registry.
            let clones = router.list_clones(route_key);
            let current_agent = router.get_route(route_key);

            lines.push("你的助手：".to_string());
            for (agent_id, entry) in &clones {
                // Router keys clones by English name; AgentInfo.name is the same
                // English name (AgentInfo.id is a UUID and must NOT be used here).
                let display_name = agents
                    .iter()
                    .find(|a| &a.name == agent_id)
                    .map(|a| a.display_name.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| agent_id.clone());
                // alias = the personal name the sender gave this clone ("起的名字")
                let alias = if entry.alias.is_empty() {
                    "-".to_string()
                } else {
                    entry.alias.clone()
                };
                let is_current = current_agent.as_ref() == Some(agent_id);
                let marker = if is_current { " ★" } else { "" };
                // 起的名字 | 显示的中文名 | 分身id(英文)
                lines.push(format!("  {alias} | {display_name} | {agent_id}{marker}"));
            }
        } else {
            lines.push("助手列表：".to_string());
            for agent in &agents {
                let display = if agent.display_name.is_empty() {
                    agent.name.clone()
                } else {
                    agent.display_name.clone()
                };
                lines.push(format!("  - | {} | {}", display, agent.name));
            }
        }

        lines.push(String::new());
        lines.push("提示：直接叫助手名字就能对话，比如\"小明，帮我查一下\"".to_string());

        lines.join("\n")
    }

    /// Resolve which agent handles a message via route_key routing.
    fn resolve_agent(&self, msg: &PluginMessage) -> String {
        if let Some(ref router) = self.sender_router {
            let rk = self.route_key(msg);
            if !rk.is_empty() {
                if let Some(agent_id) = router.resolve(&rk) {
                    return agent_id;
                }
            }
        }

        String::new()
    }

    /// Resolve non-text content into a text description (non-image media, or
    /// legacy path when save-before-describe is unavailable).
    async fn resolve_non_text_content(&self, msg: &PluginMessage) -> String {
        if let PluginContent::Image { url, caption, .. } = &msg.content {
            // Only try vision here when we already have an HTTP(S) URL.
            if url.starts_with("https://") || url.starts_with("http://") {
                match self
                    .kernel
                    .describe_content("image", url, caption.as_deref())
                    .await
                {
                    Ok(desc) => {
                        info!(%url, desc_len = desc.len(), "Vision model described image (direct URL)");
                        return desc;
                    }
                    Err(e) => {
                        warn!(%url, error = %e, "describe_content failed, using fallback");
                    }
                }
            }
        }
        self.describe_non_text_content(msg)
    }

    /// After media is saved under senders/…/input/, build a public view_url and
    /// call vision with that URL (no base64 in the LLM request).
    async fn resolve_media_after_save(
        &self,
        msg: &PluginMessage,
        agent_name: &str,
        route_key: &str,
        saved_rel: Option<&str>,
    ) -> String {
        if let PluginContent::Image { url, caption, .. } = &msg.content {
            // 1) Prefer already-public HTTP(S) URL from the channel.
            let mut candidates: Vec<String> = Vec::new();
            if url.starts_with("https://") || url.starts_with("http://") {
                candidates.push(url.clone());
            }
            // 2) Public view URL for the saved file (requires external_url).
            if let Some(rel) = saved_rel {
                // rel is home-relative: workspaces/{agent}/senders/{owner}/input/file
                // view path is relative to sender data dir: input/file
                if let Some(under_sender) = rel
                    .rsplit_once("/input/")
                    .map(|(_, f)| format!("input/{f}"))
                    .or_else(|| {
                        if rel.contains("input/") {
                            Some(rel[rel.find("input/").unwrap()..].to_string())
                        } else {
                            None
                        }
                    })
                {
                    let sid = if msg.sender_id.is_empty() {
                        route_key
                    } else {
                        msg.sender_id.as_str()
                    };
                    if let Some(view) = crate::file_view::build_file_view_url(
                        self.kernel.external_url().as_deref(),
                        agent_name,
                        &under_sender,
                        sid,
                    ) {
                        candidates.push(view);
                    }
                }
            }

            for candidate in &candidates {
                match self
                    .kernel
                    .describe_content("image", candidate, caption.as_deref())
                    .await
                {
                    Ok(desc) => {
                        info!(
                            url = %candidate,
                            desc_len = desc.len(),
                            "Vision model described image via URL"
                        );
                        let path_hint = saved_rel
                            .and_then(|r| r.rsplit_once("/input/").map(|(_, f)| format!("input/{f}")))
                            .unwrap_or_default();
                        if path_hint.is_empty() {
                            return format!("[用户发送了一张图片]\n视觉描述：{desc}");
                        }
                        return format!(
                            "[用户发送了一张图片，已保存至 {path_hint}]\n视觉描述：{desc}\nview_url: {candidate}"
                        );
                    }
                    Err(e) => {
                        warn!(url = %candidate, error = %e, "describe_content via URL failed");
                    }
                }
            }
        }

        // Fallback: text placeholder (no base64 vision attempt).
        let mut fallback = self.describe_non_text_content(msg);
        if let Some(rel) = saved_rel {
            let short = rel
                .rsplit_once("/input/")
                .map(|(_, f)| format!("input/{f}"))
                .unwrap_or_else(|| rel.to_string());
            fallback.push_str(&format!(
                "\n[文件已保存至 {short}，请用 image_analyze 或 media_describe 分析]"
            ));
        }
        fallback
    }

    fn describe_non_text_content(&self, msg: &PluginMessage) -> String {
        match &msg.content {
            PluginContent::Image { url, caption, .. } => {
                let cap = caption
                    .as_deref()
                    .map(|c| format!(" ({})", c))
                    .unwrap_or_default();
                format!("[用户发送了一张图片{}]: {}", cap, url)
            }
            PluginContent::File { url, filename, data } => {
                if data.is_some() {
                    format!("[用户发送了一个文件]: {}", filename)
                } else if !url.is_empty() {
                    format!("[用户发送了一个文件]: {} ({})", filename, url)
                } else {
                    format!("[用户发送了一个文件]: {} (文件未能下载)", filename)
                }
            }
            PluginContent::Voice {
                url,
                duration_seconds,
            } => {
                format!("[用户发送了一段{}秒的语音]: {}", duration_seconds, url)
            }
            PluginContent::Video { url, duration_seconds, caption } => {
                let dur = duration_seconds
                    .map(|d| format!("{}秒", d))
                    .unwrap_or_default();
                let cap = caption
                    .as_deref()
                    .map(|c| format!(" ({})", c))
                    .unwrap_or_default();
                format!("[用户发送了一段{}视频{}]: {}", dur, cap, url)
            }
            PluginContent::Location { lat, lon } => {
                format!("[用户发送了位置]: 经度 {}, 纬度 {}", lon, lat)
            }
            PluginContent::Command { name, args } => {
                format!("[用户发送了命令]: {} {:?}", name, args)
            }
            PluginContent::Text(_) => unreachable!(),
        }
    }

    /// Save media data (files and images) to the agent's workspace input/ directory.
    /// Returns the workspace-relative path if saved, None otherwise.
    async fn save_media_to_input(&self, msg: &PluginMessage, agent_id: &str, rk: &str) -> Option<String> {
        let (data, filename) = match &msg.content {
            PluginContent::File { data: Some(d), filename, .. } => (d.clone(), filename.clone()),
            PluginContent::Image { data: Some(d), .. } => {
                let ext = types::media::detect_image_mime(d)
                    .strip_prefix("image/")
                    .unwrap_or("png")
                    .to_string();
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis();
                (d.clone(), format!("image_{ts}.{ext}"))
            }
            _ => return None,
        };

        // sender_relative_path returns a home_dir-relative path (e.g. "workspaces/{agent}/senders/{sender}/input"),
        // so we must use home_dir as the base, NOT the workspace root, to avoid double-nesting.
        let base = match self.kernel.home_dir() {
            Some(b) => b.to_path_buf(),
            None => return None,
        };

        let safe = std::path::Path::new(&filename)
            .file_name()
            .unwrap_or(std::ffi::OsStr::new("file"))
            .to_string_lossy();

        let rel_path = format!(
            "{}/{}",
            types::config::sender_relative_path(rk, agent_id, Some(&msg.sender_id), "input"),
            safe
        );

        // Create parent directory
        if let Some(parent) = std::path::Path::new(&rel_path).parent() {
            if tokio::fs::create_dir_all(base.join(parent)).await.is_err() {
                return None;
            }
        }

        let dest = base.join(&rel_path);

        if let Err(e) = tokio::fs::write(&dest, &data).await {
            warn!(filename, error = %e, "Failed to save uploaded media");
            None
        } else {
            info!(filename, path = %dest.display(), size = data.len(), "Media saved to input directory");
            Some(rel_path)
        }
    }

    // -----------------------------------------------------------------------
    // Outbound response
    // -----------------------------------------------------------------------

    async fn send_response(&self, original: &PluginMessage, response: &str) {
        let agent_id = self.resolve_agent(original);
        let content = self.load_content_config(&agent_id);
        let admin_sender_ids = self.resolve_admin_sender_ids(&agent_id);
        let sanitize_wechat = matches!(original.channel_type.as_str(), "weixin" | "weixin-oa");

        let out = prepare_outbound(
            response,
            OutboundCtx {
                kernel: Some(self.kernel.clone()),
                send_fn: self.channel_send_fn.clone(),
                deliver_fn: self.channel_deliver_fn.clone(),
                content: content.as_deref(),
                channel_type: &original.channel_type,
                bot_id: &original.bot_id,
                sender_id: &original.sender_id,
                agent_id: &agent_id,
                process_notify: true,
                notify_routes: self.notify_routes.as_deref(),
                admin_sender_ids: &admin_sender_ids,
                sanitize_wechat,
            },
        )
        .await;

        if out.suppress_text_send {
            return;
        }

        let response = out.cleaned_text.as_str();
        info!(
            channel = %original.channel_type,
            bot = %original.bot_id,
            sender = %original.sender_id,
            text_len = response.len(),
            text_preview = %response.chars().take(50).collect::<String>(),
            "Bridge sending response"
        );
        if let Some(ref send_fn) = self.channel_send_fn {
            let send_fn = send_fn.clone();
            let channel_type = original.channel_type.clone();
            let bot_id = original.bot_id.clone();
            let sender_id = original.sender_id.clone();
            let text = response.to_string();
            let _ = tokio::task::spawn_blocking(move || {
                if let Err(e) = send_fn(&channel_type, &bot_id, &sender_id, &text) {
                    error!(
                        channel = %channel_type,
                        bot = %bot_id,
                        error = %e,
                        "Failed to send response through channel"
                    );
                }
            })
            .await;
        } else {
            warn!(
                channel = %original.channel_type,
                bot = %original.bot_id,
                "No channel send function set, cannot send response"
            );
        }
    }
}

/// Does this rule fire on an inbound iLink text message? iLink has no
/// subscribe/menu/scan event surface — keyword (substring on text) is the
/// only trigger that can ever match here.
fn weixin_keyword_rule_hit(rule: &types::automation::AutomationRule, text: &str) -> bool {
    rule.enabled
        && rule.trigger_kind == types::automation::TriggerKind::Keyword
        && !rule.trigger_data.is_empty()
        && text.trim().contains(rule.trigger_data.trim())
}

#[cfg(test)]
mod automation_gate_tests {
    use super::*;
    use types::automation::{AutomationRule, TaskKind, TriggerKind};

    fn rule(trigger: TriggerKind, data: &str, enabled: bool) -> AutomationRule {
        AutomationRule {
            id: "r1".into(),
            app_id: "bot".into(),
            channel: "weixin".into(),
            name: "t".into(),
            enabled,
            priority: 0,
            trigger_kind: trigger,
            trigger_data: data.into(),
            task_kind: TaskKind::PushText,
            task_payload: serde_json::json!({ "text": "hi" }),
            target: "current".into(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn weixin_keyword_hit_rules() {
        let text = "请问月卡多少钱";
        assert!(weixin_keyword_rule_hit(
            &rule(TriggerKind::Keyword, "月卡", true),
            text
        ));
        // trigger_data trimmed on both sides
        assert!(weixin_keyword_rule_hit(
            &rule(TriggerKind::Keyword, " 月卡 ", true),
            text
        ));
        // not contained
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::Keyword, "季卡", true),
            text
        ));
        // empty trigger never matches (would otherwise match everything)
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::Keyword, "", true),
            text
        ));
        // disabled never matches
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::Keyword, "月卡", false),
            text
        ));
        // iLink has no event surface — these triggers can never match
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::Subscribe, "", true),
            "subscribe"
        ));
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::MenuClick, "x", true),
            "x"
        ));
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::Scan, "x", true),
            "x"
        ));
        // blank text never matches (media placeholders fall through to the agent)
        assert!(!weixin_keyword_rule_hit(
            &rule(TriggerKind::Keyword, "月卡", true),
            "  "
        ));
    }
}

