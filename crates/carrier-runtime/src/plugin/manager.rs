//! Plugin manager — lifecycle management for loaded plugins.

use std::path::Path;
use std::sync::Arc;

use carrier_types::plugin::PluginMessage;
use carrier_types::tool::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::bridge::PluginBridgeManager;
use super::builtin_registry::BuiltinPluginRegistry;
use super::loader::PluginLoader;
use super::router::SenderRouter;
use super::tool_dispatch::PluginToolDispatcher;
use crate::kernel_handle::KernelHandle;

// ---------------------------------------------------------------------------
// Plugin manager
// ---------------------------------------------------------------------------

/// Manages the lifecycle of all loaded plugins: loading, starting, stopping.
pub struct PluginManager {
    /// Tool dispatcher for routing tool calls.
    tool_dispatcher: Arc<PluginToolDispatcher>,
    /// Bridge message sender (inbound messages from plugins).
    message_tx: mpsc::Sender<PluginMessage>,
    /// Bridge message receiver (moved to bridge on start).
    message_rx: Option<mpsc::Receiver<PluginMessage>>,
    /// Successfully loaded plugins.
    loaded_plugins: Vec<Arc<dyn super::instance::PluginInstance>>,
    /// Kernel handle for bridge routing.
    kernel: Arc<dyn KernelHandle>,
    /// Sender-based router (route_key → agent_id), set before start().
    sender_router: Option<Arc<SenderRouter>>,
}

impl PluginManager {
    /// Create a new plugin manager.
    pub fn new(kernel: Arc<dyn KernelHandle>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            tool_dispatcher: Arc::new(PluginToolDispatcher::new()),
            message_tx: tx,
            message_rx: Some(rx),
            loaded_plugins: Vec::new(),
            kernel,
            sender_router: None,
        }
    }

    /// Set the sender-based router (must be called before start()).
    pub fn set_sender_router(&mut self, router: Arc<SenderRouter>) {
        self.sender_router = Some(router);
    }

    /// Load all plugins from the given directory.
    pub fn load_all(&mut self, plugins_dir: &Path, registry: &BuiltinPluginRegistry) {
        // 1. Load external (.so) plugins
        let results = PluginLoader::load_all(plugins_dir, self.message_tx.clone());

        for result in results {
            match result {
                Ok(plugin) => {
                    let plugin_arc: Arc<dyn super::instance::PluginInstance> = Arc::new(plugin);
                    self.tool_dispatcher.register(plugin_arc.clone());
                    self.loaded_plugins.push(plugin_arc);
                }
                Err(e) => {
                    error!(error = %e, "Failed to load plugin");
                }
            }
        }

        // 2. Load built-in plugins
        let builtins =
            PluginLoader::load_builtin_plugins(plugins_dir, self.message_tx.clone(), registry);
        for builtin in builtins {
            let plugin_arc: Arc<dyn super::instance::PluginInstance> = Arc::new(builtin);
            self.tool_dispatcher.register(plugin_arc.clone());
            self.loaded_plugins.push(plugin_arc);
        }

        info!(
            loaded = self.loaded_plugins.len(),
            tools = self.tool_dispatcher.definitions().len(),
            "Plugin loading complete"
        );
    }

    /// Start all channel adapters and the bridge.
    pub async fn start(&mut self, _plugins_dir: &Path) {
        // Start channel adapters
        for plugin in &self.loaded_plugins {
            for channel in plugin.channels() {
                if let Err(e) = plugin.start_channel(channel) {
                    error!(
                        plugin = %plugin.name(),
                        channel = %channel.channel_type,
                        error = %e,
                        "Failed to start channel"
                    );
                } else {
                    info!(
                        plugin = %plugin.name(),
                        channel = %channel.channel_type,
                        "Channel started"
                    );
                }
            }
        }

        // Build bridge
        let mut bridge = PluginBridgeManager::new(self.kernel.clone());

        // Set sender router if configured
        if let Some(ref router) = self.sender_router {
            bridge.set_sender_router(router.clone());
        }

        // Discover bots and register sender routes
        let mut first_agent: Option<String> = None;
        for plugin in &self.loaded_plugins {
            bridge.add_plugin(plugin.clone());

            let plugin_dir = plugin.path();
            let bots = super::loader::PluginLoader::discover_bots(plugin_dir);
            let channels: Vec<String> = plugin
                .channels()
                .iter()
                .map(|c| c.channel_type.clone())
                .collect();

            for (bot_uuid, bot_config) in &bots {
                if let Some(ref agent_uuid) = bot_config.bind_agent {
                    if uuid::Uuid::parse_str(agent_uuid).is_err() {
                        error!(
                            bot = %bot_config.name,
                            bind_agent = %agent_uuid,
                            "bind_agent is not a valid UUID, skipping binding"
                        );
                        continue;
                    }

                    // Register sender route: route_key depends on channel type
                    for ch in &channels {
                        if ch == "weixin" {
                            // WeChat uses user_id as route key — skip here,
                            // WeChat routes are registered from token files
                        } else {
                            // WeCom/Feishu/DingTalk use bot_uuid as route key
                            self.set_sender_route(bot_uuid, agent_uuid);
                        }
                        info!(
                            channel = %ch,
                            bot = %bot_config.name,
                            bot_id = %bot_uuid,
                            agent_id = %agent_uuid,
                            "Bound bot to agent"
                        );
                    }

                    if first_agent.is_none() {
                        first_agent = Some(agent_uuid.clone());
                    }
                } else {
                    info!(
                        bot = %bot_config.name,
                        bot_id = %bot_uuid,
                        "Bot has no bind_agent, skipping"
                    );
                }
            }
        }

        // Set first agent on sender router
        if let Some(ref router) = self.sender_router {
            if let Some(agent_id) = first_agent {
                router.set_first_agent(agent_id);
            }
        }

        // Start bridge in a background task
        if let Some(rx) = self.message_rx.take() {
            tokio::spawn(async move {
                bridge.run(rx).await;
            });
        }
    }

    /// Set a sender route (route_key → agent_id).
    pub fn set_sender_route(&self, route_key: &str, agent_id: &str) {
        if let Some(ref router) = self.sender_router {
            router.set_route(route_key, agent_id);
        }
    }

    /// Get a sender's current route (no auto-assign).
    pub fn get_sender_route(&self, sender_id: &str) -> Option<String> {
        self.sender_router.as_ref()?.get_route(sender_id)
    }

    /// Remove a sender's route.
    pub fn remove_sender_route(&self, sender_id: &str) -> Option<String> {
        self.sender_router.as_ref()?.remove_route(sender_id)
    }

    /// List all sender routes.
    pub fn list_sender_routes(&self) -> Vec<(String, String)> {
        match &self.sender_router {
            Some(router) => router.list_routes(),
            None => Vec::new(),
        }
    }

    /// Get all plugin tool definitions (for the LLM tool list).
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_dispatcher.definitions()
    }

    /// Get a reference to the tool dispatcher (for execute_tool integration).
    pub fn tool_dispatcher(&self) -> Arc<PluginToolDispatcher> {
        self.tool_dispatcher.clone()
    }

    /// Send a text message through a channel by bot ID.
    pub fn channel_send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        for plugin in &self.loaded_plugins {
            for channel in plugin.channels() {
                if channel.bot_id == bot_id {
                    return plugin.channel_send(channel, bot_id, user_id, text);
                }
            }
        }
        Err(format!("Channel not found for bot: {bot_id}"))
    }

    /// Dynamically start a channel for a newly created bot (no restart needed).
    pub fn start_dynamic_channel(
        &self,
        platform: &str,
        bot_name: &str,
        bot_id: &str,
        secret: &str,
    ) {
        let sender = self.message_tx.clone();
        match platform {
            "wecom" => {
                crate::plugin::channels::wecom::register_and_start_smartbot(
                    sender,
                    bot_name.to_string(),
                    bot_id.to_string(),
                    secret.to_string(),
                );
            }
            other => {
                info!(platform = %other, "Dynamic channel start not yet implemented for this platform");
            }
        }
    }

    /// Get status of all loaded plugins.
    pub fn status(&self) -> Vec<carrier_types::plugin::PluginStatus> {
        self.loaded_plugins
            .iter()
            .map(|p| carrier_types::plugin::PluginStatus {
                name: p.name().to_string(),
                version: p.version().to_string(),
                loaded: true,
                channels: p
                    .channels()
                    .iter()
                    .map(|c| c.channel_type.clone())
                    .collect(),
                tools: p.tools().iter().map(|t| t.name.clone()).collect(),
                bot_count: 0,
                last_error: None,
            })
            .collect()
    }

    /// Stop all plugins and release resources.
    pub fn stop_all(&self) {
        for plugin in &self.loaded_plugins {
            info!(plugin = %plugin.name(), "Stopping plugin");
            plugin.stop();
        }
    }
}

impl Drop for PluginManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}
