//! Plugin bridge — routes messages between plugin channels and the kernel.

use std::sync::Arc;

use types::channel::{ChannelError, RoutingMode};
use types::plugin::{PluginContent, PluginMessage};
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
    Arc<dyn Fn(&str, &str, &str, &str) -> Result<(), ChannelError> + Send + Sync>;

/// A function that reports a channel type's routing mode.
/// Used by the bridge to decide whether to run the multi-clone pipeline.
pub type RoutingModeFn = Arc<dyn Fn(&str) -> RoutingMode + Send + Sync>;

// ---------------------------------------------------------------------------
// Notify routing — cross-channel push markers
// ---------------------------------------------------------------------------

/// Where to push a notification of a given type.
///
/// Configured in `~/.opencarrier/notify_routes.json`. The agent emits a
/// `[NOTIFY:type]content[/NOTIFY]` marker in its reply; the bridge looks up
/// the type here and pushes via channel_send_fn.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct NotifyTarget {
    pub channel: String,
    #[serde(default)]
    pub bot_id: String,
    pub user_id: String,
    #[serde(default)]
    pub prefix: Option<String>,
}

/// Parse `[NOTIFY:type]content[/NOTIFY]` markers from agent reply text.
///
/// Returns (list of (type, content), text with markers stripped). Reliable
/// side-effect trigger — the agent just outputs text, no tool-calling needed.
fn parse_notify_markers(text: &str) -> (Vec<(String, String)>, String) {
    let open = "[NOTIFY:";
    let close = "[/NOTIFY]";
    let mut notifications = Vec::new();
    let mut cleaned = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        cleaned.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        // type ends at the first ']'
        match after_open.find(']') {
            Some(type_end) => {
                let ntype = after_open[..type_end].trim().to_string();
                let after_type = &after_open[type_end + 1..];
                match after_type.find(close) {
                    Some(content_end) => {
                        let content = after_type[..content_end].trim().to_string();
                        if !ntype.is_empty() {
                            notifications.push((ntype, content));
                        }
                        rest = &after_type[content_end + close.len()..];
                    }
                    None => {
                        // No closing tag — emit as-is and stop
                        cleaned.push_str(open);
                        cleaned.push_str(after_open);
                        rest = "";
                    }
                }
            }
            None => {
                cleaned.push_str(open);
                cleaned.push_str(after_open);
                rest = "";
            }
        }
    }
    cleaned.push_str(rest);
    (notifications, cleaned)
}

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
}

impl PluginBridgeManager {
    /// Create a new bridge manager.
    pub fn new(kernel: Arc<dyn KernelHandle>) -> Self {
        Self {
            kernel,
            channel_send_fn: None,
            routing_mode_fn: None,
            notify_routes: None,
            sender_router: None,
            cron_delivery: None,
            pending_naming: Arc::new(DashMap::new()),
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

    /// Set the routing-mode probe (tells the bridge which channels are DirectBind).
    pub fn set_routing_mode_fn(&mut self, f: RoutingModeFn) {
        self.routing_mode_fn = Some(f);
    }

    /// Set notify routing (enables `[NOTIFY:type]content[/NOTIFY] markers → cross-channel push).
    pub fn set_notify_routes(&mut self, routes: Arc<std::collections::HashMap<String, NotifyTarget>>) {
        self.notify_routes = Some(routes);
    }

    /// Backward-compatible: add a loaded plugin to the bridge.
    /// Builds a channel_send_fn that routes through the plugin's channels.
    pub fn add_plugin(&mut self, plugin: Arc<dyn super::instance::PluginInstance>) {
        let f: ChannelSendFn = Arc::new(move |channel_type, bot_id, user_id, text| {
            // Try exact match first
            for channel in plugin.channels() {
                if channel.channel_type == channel_type && channel.bot_id == bot_id {
                    return plugin.channel_send(channel, bot_id, user_id, text).map_err(ChannelError::Other);
                }
            }
            // Fallback: any channel of the same type
            for channel in plugin.channels() {
                if channel.channel_type == channel_type {
                    return plugin.channel_send(channel, bot_id, user_id, text).map_err(ChannelError::Other);
                }
            }
            Err(ChannelError::UnknownBot(format!("No plugin channel found for type: {}", channel_type)))
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
            None => self.resolve_non_text_content(&msg).await,
        };

        let rk = self.route_key(&msg);
        info!(
            channel = %msg.channel_type,
            bot = %msg.bot_id,
            route_key = %rk,
            text_len = text.len(),
            "Bridge handling inbound message"
        );

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

        // Determine this channel's routing mode. DirectBind channels (weixin-oa,
        // future one-to-one channels) skip the entire multi-clone pipeline and
        // route straight to their fixed bind_agent.
        let direct_bind = self
            .routing_mode_fn
            .as_ref()
            .map(|f| f(&msg.channel_type) == RoutingMode::DirectBind)
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
                Ok(response) => self.send_response(&msg, &response).await,
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
                    Err(e) if e.contains("already_admin") => {
                        self.send_response(&msg, "您已经是管理员了。").await;
                    }
                    Err(e) if e.contains("already_pending") => {
                        self.send_response(&msg, "您已提交过申请，请耐心等待审批。").await;
                    }
                    Err(_) => {
                        self.send_response(&msg, "申请提交失败，请稍后再试。").await;
                    }
                }
            }
            return;
        }

        // Save media data (files and images) to the agent's workspace input/ directory
        let saved_filename = self.save_media_to_input(&msg, &agent_id, &rk).await;

        // Append file_read hint if media was saved
        let final_text = match saved_filename {
            Some(rel_path) => format!("{}\n[文件已保存至 {}，请用 file_read 读取]", text, rel_path),
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

    /// Resolve non-text content into a text description.
    /// Images go through the vision model; other types use hardcoded fallback.
    async fn resolve_non_text_content(&self, msg: &PluginMessage) -> String {
        if let PluginContent::Image { url, caption, .. } = &msg.content {
            match self
                .kernel
                .describe_content("image", url, caption.as_deref())
                .await
            {
                Ok(desc) => {
                    info!(url = %url, desc_len = desc.len(), "Vision model described image");
                    return desc;
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "describe_content failed, using fallback");
                }
            }
        }
        self.describe_non_text_content(msg)
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
        // Parse cross-channel notify markers before sending to the user.
        // Each [NOTIFY:type]content[/NOTIFY] triggers a push via channel_send_fn;
        // the marker is stripped from the user-facing reply.
        let (notifications, cleaned) = parse_notify_markers(response);
        if !notifications.is_empty() {
            if let (Some(send_fn), Some(routes)) =
                (self.channel_send_fn.clone(), self.notify_routes.clone())
            {
                for (ntype, content) in &notifications {
                    if let Some(target) = routes.get(ntype) {
                        let msg = match &target.prefix {
                            Some(p) if !p.is_empty() => {
                                format!("{p}\n{content}\n来源用户: {}", original.sender_id)
                            }
                            _ => format!("{content}\n来源用户: {}", original.sender_id),
                        };
                        let send_fn = send_fn.clone();
                        let channel = target.channel.clone();
                        let bot_id = target.bot_id.clone();
                        let user_id = target.user_id.clone();
                        info!(
                            notify_type = %ntype,
                            target_channel = %channel,
                            target_user = %user_id,
                            "Notify marker matched, pushing cross-channel"
                        );
                        tokio::task::spawn_blocking(move || {
                            if let Err(e) = send_fn(&channel, &bot_id, &user_id, &msg) {
                                error!(%channel, %user_id, error = %e, "Notify push failed");
                            }
                        });
                    } else {
                        warn!(notify_type = %ntype, "Notify marker has no route in notify_routes.json");
                    }
                }
            } else {
                warn!(
                    notify_count = notifications.len(),
                    "Notify markers present but no notify_routes/send_fn configured"
                );
            }
        }

        let response = cleaned.as_str();
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
