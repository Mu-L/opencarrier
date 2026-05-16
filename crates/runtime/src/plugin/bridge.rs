//! Plugin bridge — routes messages between plugin channels and the kernel.

use std::sync::Arc;

use types::plugin::PluginMessage;
use dashmap::DashMap;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::router::SenderRouter;
use crate::kernel_handle::KernelHandle;

// ---------------------------------------------------------------------------
// Channel response sender
// ---------------------------------------------------------------------------

/// A function that can send a response through a channel.
/// Used by the bridge to deliver agent replies back to users.
pub type ChannelSendFn =
    Arc<dyn Fn(&str, &str, &str, &str) -> Result<(), String> + Send + Sync>;

// ---------------------------------------------------------------------------
// Bridge manager
// ---------------------------------------------------------------------------

/// Routes inbound plugin messages to agents and delivers responses back
/// through the originating channel.
#[derive(Clone)]
pub struct PluginBridgeManager {
    /// Kernel handle for sending messages to agents.
    kernel: Arc<dyn KernelHandle>,
    /// Function to send responses through channels (channel_type, bot_id, user_id, text).
    channel_send_fn: Option<ChannelSendFn>,
    /// Sender-based routing (route_key → agent_id).
    sender_router: Option<Arc<SenderRouter>>,
    /// Cron delivery: last-channel tracking + buffered notifications.
    cron_delivery: Option<Arc<memory::CronDeliveryStore>>,
    /// route_key of users currently in the "naming" flow (waiting for agent name).
    pending_naming: DashMap<String, String>,
}

impl PluginBridgeManager {
    /// Create a new bridge manager.
    pub fn new(kernel: Arc<dyn KernelHandle>) -> Self {
        Self {
            kernel,
            channel_send_fn: None,
            sender_router: None,
            cron_delivery: None,
            pending_naming: DashMap::new(),
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
            Err(format!("No plugin channel found for type: {}", channel_type))
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
    /// Each message is handled in its own tokio task, allowing concurrent
    /// processing of messages from different users. Same-owner messages are
    /// still serialized via the per-owner lock in messaging.rs.
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
        let text = match msg.content.as_text() {
            Some(t) => t.to_string(),
            None => self.describe_non_text_content(&msg),
        };

        let rk = self.route_key(&msg);

        // Record the channel this sender last used (for cron delivery routing).
        if let Some(ref cron_delivery) = self.cron_delivery {
            if let Err(e) = cron_delivery.touch_sender_channel(&rk, &msg.channel_type, &msg.bot_id) {
                tracing::warn!(error = %e, "Failed to touch sender channel");
            }
        }

        // Deliver any buffered cron notifications for this sender before
        // processing the actual message. We use msg's context so the reply
        // can use the active context_token / response_url.
        if let Some(ref cron_delivery) = self.cron_delivery {
            match cron_delivery.drain_pending(&rk) {
                Ok(notifications) if !notifications.is_empty() => {
                    for n in notifications {
                        self.send_response(&msg, &n.message).await;
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "Failed to drain pending notifications"),
            }
        }

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

        // 2. Try name-based routing (message starts with an alias)
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
                Ok(response) => self.send_response(&msg, &response).await,
                Err(e) => {
                    error!(agent = %agent_id, error = %e, "Failed to send message to agent");
                    self.send_response(&msg, "抱歉，处理消息时遇到了问题，请稍后再试。").await;
                }
            }
            return;
        }

        // 3. /list command
        if text.trim().eq_ignore_ascii_case("/list") {
            let response = self.format_agent_list(&rk);
            self.send_response(&msg, &response).await;
            return;
        }

        // 4. Default routing via route_key
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

        // 5. Check if this agent needs a name
        if let Some(ref router) = self.sender_router {
            if router.needs_naming(&rk) {
                self.pending_naming.insert(rk.clone(), agent_id.clone());
                self.send_response(&msg, "请给我取个名字吧！以后叫这个名字我就会出来。").await;
                return;
            }
        }

        info!(
            channel = %msg.channel_type,
            bot = %msg.bot_id,
            agent = %agent_id,
            route_key = %rk,
            "Routing plugin message to agent"
        );

        match self
            .kernel
            .send_to_agent(
                &agent_id,
                &text,
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
    // Name-based routing
    // -----------------------------------------------------------------------

    /// Try to route by matching the start of the text against aliases for the route_key.
    /// Supports two formats:
    ///   `@名字` or `@名字 你好` — @ prefix triggers agent switch
    ///   `名字 你好` — name at start of text (legacy format)
    /// Returns (agent_id, remaining_text) if matched, None otherwise.
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
            let aliases = router.list_aliases(route_key);
            let current_agent = router.get_route(route_key);

            if !aliases.is_empty() {
                lines.push("你的助手：".to_string());
                for (name, agent_id) in &aliases {
                    let agent_name = agents
                        .iter()
                        .find(|a| &a.id == agent_id)
                        .map(|a| a.name.as_str())
                        .unwrap_or("?");
                    let is_current = current_agent.as_ref() == Some(agent_id);
                    let marker = if is_current { " ★" } else { "" };
                    lines.push(format!("  {name}（{agent_name}）{marker}"));
                }
            }

            // Show agents without aliases
            let aliased_agents: Vec<String> = aliases.iter().map(|(_, aid)| aid.clone()).collect();
            let unnamed: Vec<_> = agents
                .iter()
                .filter(|a| !aliased_agents.contains(&a.id))
                .collect();
            if !unnamed.is_empty() {
                lines.push(String::new());
                lines.push("可用但未命名的助手：".to_string());
                for agent in unnamed {
                    let is_current = current_agent.as_ref() == Some(&agent.id);
                    let marker = if is_current { " ★" } else { "" };
                    let desc = if agent.description.is_empty() {
                        String::new()
                    } else {
                        format!(" — {}", agent.description)
                    };
                    lines.push(format!("  {}（{}）{}{marker}", agent.id, agent.name, desc));
                }
            }
        } else {
            lines.push("助手列表：".to_string());
            for agent in &agents {
                lines.push(format!("  {} — {}", agent.name, agent.description));
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

    fn describe_non_text_content(&self, msg: &PluginMessage) -> String {
        use types::plugin::PluginContent;
        match &msg.content {
            PluginContent::Image { url, caption } => {
                let cap = caption
                    .as_deref()
                    .map(|c| format!(" ({})", c))
                    .unwrap_or_default();
                format!("[用户发送了一张图片{}]: {}", cap, url)
            }
            PluginContent::File { url, filename } => {
                format!("[用户发送了一个文件]: {} ({})", filename, url)
            }
            PluginContent::Voice {
                url,
                duration_seconds,
            } => {
                format!("[用户发送了一段{}秒的语音]: {}", duration_seconds, url)
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

    // -----------------------------------------------------------------------
    // Outbound response
    // -----------------------------------------------------------------------

    async fn send_response(&self, original: &PluginMessage, response: &str) {
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
