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

/// Strip characters that WeChat iLink/OA cannot render (shows as ???).
/// Keeps: CJK, ASCII, common punctuation, newlines. Removes: emoji,
/// variation selectors, miscellaneous symbols, ornamental characters.
fn sanitize_wechat_text(text: &str) -> String {
    text.chars()
        .filter(|c| {
            // Basic ASCII (printable + newline/tab)
            if c.is_ascii() {
                return !c.is_control() || *c == '\n' || *c == '\t';
            }
            // CJK Unified Ideographs
            if matches!(c, '\u{4E00}'..='\u{9FFF}') {
                return true;
            }
            // CJK Extension A & B
            if matches!(c, '\u{3400}'..='\u{4DBF}' | '\u{20000}'..='\u{2A6DF}') {
                return true;
            }
            // Fullwidth forms (fullwidth ASCII, punctuation)
            if matches!(c, '\u{FF01}'..='\u{FF5E}' | '\u{3000}'..='\u{303F}') {
                return true;
            }
            // CJK compatibility, Kangxi radical, Bopomofo, Hiragana, Katakana
            if matches!(c, '\u{F900}'..='\u{FAFF}' | '\u{2F00}'..='\u{2FDF}'
                        | '\u{3100}'..='\u{318F}' | '\u{3040}'..='\u{309F}'
                        | '\u{30A0}'..='\u{30FF}') {
                return true;
            }
            // Common punctuation (general + CJK-specific)
            if matches!(c, '—' | '–' | '…' | '·' | '×' | '÷' | '°' | '℃'
                        | '←' | '→' | '↑' | '↓' | '■' | '□' | '▪' | '▶'
                        | '《' | '》' | '〈' | '〉' | '【' | '】' | '〖' | '〗'
                        | '「' | '」' | '『' | '』' | '﹏' | '￥' | '＄' | '€') {
                return true;
            }
            // Latin-1 Supplement (accented chars, copyright, registered, etc.)
            if matches!(c, '\u{00A0}'..='\u{00FF}') {
                return true;
            }
            // Common letter/number ranges (Latin Extended, Greek, Cyrillic)
            if c.is_alphanumeric() {
                return true;
            }
            // General punctuation (quotes, dashes, brackets)
            if matches!(c, '\u{2010}'..='\u{205F}') {
                return true;
            }
            false
        })
        .collect::<String>()
}
fn parse_markers(text: &str, open: &str, close: &str) -> (Vec<(String, String)>, String) {
    let mut out = Vec::new();
    let mut cleaned = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        cleaned.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        // key ends at the first ']'
        match after_open.find(']') {
            Some(type_end) => {
                let key = after_open[..type_end].trim().to_string();
                let after_type = &after_open[type_end + 1..];
                match after_type.find(close) {
                    Some(content_end) => {
                        let content = after_type[..content_end].trim().to_string();
                        if !key.is_empty() {
                            out.push((key, content));
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
    (out, cleaned)
}

/// Parse `[NOTIFY:type]content[/NOTIFY]` markers from agent reply text.
fn parse_notify_markers(text: &str) -> (Vec<(String, String)>, String) {
    parse_markers(text, "[NOTIFY:", "[/NOTIFY]")
}

/// Parse `[PUBLISH:app_id]html_path[/PUBLISH]` markers from agent reply text.
/// Triggers the reliable publish handler (cover → draft → publish) for each.
fn parse_publish_markers(text: &str) -> (Vec<(String, String)>, String) {
    parse_markers(text, "[PUBLISH:", "[/PUBLISH]")
}

/// Parse PUBLISH content: "html_path|title|digest" where title and digest are optional.
/// Returns (html_path, optional_title, optional_digest).
fn parse_publish_content(content: &str) -> (String, Option<String>, Option<String>) {
    let parts: Vec<&str> = content.splitn(3, '|').collect();
    let html_path = parts.first().unwrap_or(&"").trim().to_string();
    let title = parts.get(1).filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string());
    let digest = parts.get(2).filter(|s| !s.trim().is_empty()).map(|s| s.trim().to_string());
    (html_path, title, digest)
}

/// Read the app_secret for `app_id` from the sender's own profile.json
/// (preferences.wechat_accounts array). Multi-user: each user's OA credentials
/// live in their own directory; find the matching entry by app_id. Returns
/// None if the profile or that account isn't configured.
fn read_wechat_app_secret(
    home: &std::path::Path,
    sender_id: &str,
    agent_id: &str,
    app_id: &str,
) -> Option<String> {
    let profile_path =
        types::config::sender_data_dir(home, sender_id, agent_id, Some(sender_id))
            .join("profile.json");
    let content = std::fs::read_to_string(&profile_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&content).ok()?;
    let accounts = v["preferences"]["wechat_accounts"].as_array()?;
    for acct in accounts {
        if acct["app_id"].as_str() == Some(app_id) {
            return acct["app_secret"].as_str().map(|s| s.to_string());
        }
    }
    None
}

/// Resolve the article title: first non-empty line of the sibling `.md` file
/// (with leading `#` stripped), else the html filename stem.
fn resolve_article_title(html_path: &str) -> String {
    let p = std::path::Path::new(html_path);
    let md = p.with_extension("md");
    if let Ok(content) = std::fs::read_to_string(&md) {
        if let Some(line) = content.lines().find(|l| !l.trim().is_empty()) {
            let t = line.trim().trim_start_matches('#').trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("未命名文章")
        .to_string()
}

/// Handle a `[PUBLISH:app_id]html_path|digest[/PUBLISH]` marker: generate a
/// cover, create a WeChat OA draft, and publish it — all via in-process API
/// (no MCP, no agent tool-chain; the "AI + API" pattern). The `|digest` part
/// is optional; if omitted, WeChat auto-extracts a digest from the article.
/// Replies to the user with the result once it completes.
#[allow(clippy::too_many_arguments)]
async fn handle_publish_marker(
    kernel: std::sync::Arc<dyn KernelHandle>,
    send_fn: Option<ChannelSendFn>,
    channel_type: &str,
    bot_id: &str,
    sender_id: &str,
    app_id: &str,
    html_path: &str,
    explicit_title: Option<&str>,
    digest: Option<&str>,
    agent_id: &str,
) {
    // Resolve html_path to absolute, mirroring how the agent's file_read
    // resolves relative paths: under the per-sender workspace
    // (workspaces/<agent>/senders/<sender>/), NOT ~/.opencarrier. Absolute
    // paths are used as-is.
    let home = kernel.home_dir().unwrap_or_default();
    let abs_html = if std::path::Path::new(html_path).is_absolute() {
        html_path.to_string()
    } else {
        let base = types::config::sender_data_dir(&home, sender_id, agent_id, Some(sender_id));
        base.join(html_path).to_string_lossy().to_string()
    };

    let title = match explicit_title.filter(|t| !t.is_empty()) {
        Some(t) => t.to_string(),
        None => resolve_article_title(&abs_html),
    };
    let cover_prompt = format!(
        "WeChat official account article cover image, theme: {title}, flat illustration style, vibrant, clean, no text"
    );

    // Generate cover into the article's directory. On failure, omit cover_path
    // and let the publish tool fall back to the material library.
    let out_dir = std::path::Path::new(&abs_html)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let cover_path = match kernel
        .generate_image_to_file(&cover_prompt, &out_dir.to_string_lossy())
        .await
    {
        Ok(p) => {
            info!(cover = %p, "Cover generated for publish");
            Some(p)
        }
        Err(e) => {
            warn!(error = %e, "Cover generation failed; publish tool will try material-library fallback");
            None
        }
    };

    // Read app_secret from the user's OWN profile (multi-user: each user's OA
    // credentials live in their own directory). Find by app_id in the
    // wechat_accounts array. Empty if not configured — the tool reports it.
    let app_secret = read_wechat_app_secret(&home, sender_id, agent_id, app_id);

    // Drive the publish tool deterministically.
    let ctx = types::plugin::PluginToolContext {
        bot_id: bot_id.to_string(),
        sender_id: sender_id.to_string(),
        agent_id: agent_id.to_string(),
        channel_type: channel_type.to_string(),
    };
    // Draft-only by design: AI-generated content must be human-reviewed before
    // going public, so we never auto-publish (freepublish). The tool creates the
    // draft (cover + content); a human publishes from the OA backend after
    // review. This also avoids the 48001 "api unauthorized" gate that
    // freepublish requires a verified service account for.
    let mut args = serde_json::json!({
        "app_id": app_id,
        "app_secret": app_secret.unwrap_or_default(),
        "html_path": abs_html,
        "title": title,
        "publish": false,
    });
    if let Some(d) = digest {
        if !d.is_empty() {
            args["digest"] = serde_json::Value::String(d.to_string());
        }
    }
    if let Some(cp) = cover_path {
        args["cover_path"] = serde_json::Value::String(cp);
    }

    // The publish tool internally block_on's its own runtime (like the other OA
    // tools), so it MUST run on a spawn_blocking thread — calling it directly on
    // an async runtime worker panics ("cannot start a runtime from within a runtime").
    let tool_result = tokio::task::spawn_blocking(move || {
        kernel.execute_plugin_tool("weixin_oa_publish_article", &args, &ctx)
    })
    .await;

    let result_msg = match tool_result {
        Ok(Some(Ok(body))) => {
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let media_id = v["media_id"].as_str().unwrap_or("?");
            let cover_src = v["cover_source"].as_str().unwrap_or("?");
            if let Some(pid) = v["publish_id"].as_str() {
                info!(%app_id, %media_id, %pid, cover_source = %cover_src, "Article published via PUBLISH marker");
                format!(
                    "✅ 文章已发布\n《{title}》\n封面来源:{cover_src}\nmedia_id:{media_id}\npublish_id:{pid}"
                )
            } else if let Some(err) = v["publish_error"].as_str() {
                warn!(%app_id, %media_id, error = %err, "Draft created but freepublish failed");
                format!(
                    "⚠️ 草稿已建,但自动发布失败\n《{title}》\n草稿 media_id:{media_id}\n失败原因:{err}\n→ 请到公众号后台草稿箱手动发布(此账号可能无 freepublish 权限,需认证服务号)"
                )
            } else {
                info!(%app_id, %media_id, cover_source = %cover_src, "Draft created (awaiting human review)");
                format!("✅ 草稿已建,待审核\n《{title}》\n封面来源:{cover_src}\n草稿 media_id:{media_id}\n→ 请到公众号后台草稿箱审核后发布")
            }
        }
        Ok(Some(Err(e))) => {
            error!(%app_id, error = %e, "Publish tool failed");
            format!("❌ 发布失败:{e}")
        }
        Ok(None) => {
            error!(%app_id, "weixin_oa_publish_article tool not registered in dispatcher");
            "❌ 发布失败:publish 工具未注册".to_string()
        }
        Err(join_err) => {
            error!(%app_id, error = %join_err, "Publish task panicked");
            "❌ 发布失败:内部任务异常".to_string()
        }
    };

    // Push the result back to the user as a follow-up message.
    if let Some(send_fn) = send_fn {
        let channel_type = channel_type.to_string();
        let bot_id = bot_id.to_string();
        let sender_id = sender_id.to_string();
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(e) = send_fn(&channel_type, &bot_id, &sender_id, &result_msg) {
                error!(%channel_type, %sender_id, error = %e, "Publish result reply failed");
            }
        })
        .await;
    }
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

                        // Resolve recipient user_ids with per-recipient channel routing.
                        // iLink IDs ("xxx@im.wechat") go through the weixin (iLink)
                        // channel; bare openids go through weixin-oa (customer-send API).
                        // This ensures every admin receives the push on their actual
                        // channel regardless of the route's default.
                        let recipient_ids: Vec<(String, String, String)> =
                            if target.recipients.as_deref() == Some("admins") {
                                let agent_id = self.resolve_agent(original);
                                let admins = if !agent_id.is_empty() {
                                    self.kernel
                                        .resolve_agent_workspace(&agent_id)
                                        .map(|ws| {
                                            crate::plugin::admin_store::read_admins(
                                                std::path::Path::new(&ws),
                                            )
                                            .admins
                                        })
                                        .unwrap_or_default()
                                } else {
                                    Vec::new()
                                };
                                if admins.is_empty() {
                                    warn!(
                                        notify_type = %ntype,
                                        agent = %agent_id,
                                        "recipients=admins but no admins resolved"
                                    );
                                }
                                admins.into_iter().map(|a| {
                                    // Route by sender_id format:
                                    //   @im.wechat → iLink (target channel/bot_id)
                                    //   bare openid → weixin-oa (app_id from original msg)
                                    if a.sender_id.contains("@im.wechat") {
                                        (target.channel.clone(), target.bot_id.clone(), a.sender_id)
                                    } else {
                                        ("weixin-oa".to_string(), original.bot_id.clone(), a.sender_id)
                                    }
                                }).collect()
                            } else if target.user_id.is_empty() {
                                warn!(
                                    notify_type = %ntype,
                                    "Notify route has empty user_id and recipients != admins"
                                );
                                Vec::new()
                            } else {
                                vec![(target.channel.clone(), target.bot_id.clone(), target.user_id.clone())]
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

        // Parse [PUBLISH:app_id]html_path[/PUBLISH] markers. Each fires a
        // reliable publish handler (cover → draft → publish) in the background;
        // the marker is stripped from the user-facing reply and the result is
        // pushed as a follow-up message when the publish completes.
        let (publishes, final_cleaned) = parse_publish_markers(&cleaned);
        if !publishes.is_empty() {
            for (app_id, content) in &publishes {
                let kernel = self.kernel.clone();
                let send_fn = self.channel_send_fn.clone();
                let channel_type = original.channel_type.clone();
                let bot_id = original.bot_id.clone();
                let sender_id = original.sender_id.clone();
                let agent_id = self.resolve_agent(original);
                let app_id = app_id.clone();
                // Parse "html_path|title|digest" — title and digest are optional.
                // Examples: "article.html", "article.html|My Title", "article.html|My Title|Summary text"
                let (html_path, explicit_title, digest) = parse_publish_content(content);
                let digest = digest.filter(|d| !d.is_empty());
                info!(
                    %app_id, %html_path, title_provided = explicit_title.is_some(),
                    digest_provided = digest.is_some(), %agent_id,
                    "PUBLISH marker matched, spawning publish handler"
                );
                tokio::spawn(async move {
                    handle_publish_marker(
                        kernel, send_fn, &channel_type, &bot_id, &sender_id,
                        &app_id, &html_path, explicit_title.as_deref(), digest.as_deref(), &agent_id,
                    )
                    .await;
                });
            }
        }

        let response = final_cleaned.as_str();
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
            // WeChat iLink and OA don't support some Unicode characters (emoji,
            // special symbols, variation selectors) — they show as ???. Strip them.
            let text = match original.channel_type.as_str() {
                "weixin" | "weixin-oa" => sanitize_wechat_text(&text),
                _ => text,
            };
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
