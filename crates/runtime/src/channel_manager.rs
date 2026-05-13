//! Channel manager — lifecycle management for channel adapters.
//!
//! Replaces the old `PluginManager` for channel operations. Each channel
//! (feishu, wecom, weixin, dingtalk) is registered as a `Box<dyn Channel>`
//! and managed directly — no FFI, no plugin abstraction layer.

use std::collections::HashMap;
use std::sync::Arc;

use types::channel::Channel;
use types::plugin::{PluginMessage, PluginStatus};
use types::tool::ToolDefinition;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::plugin::bridge::{ChannelSendFn, PluginBridgeManager};
use crate::plugin::router::SenderRouter;
use crate::plugin::tool_dispatch::PluginToolDispatcher;
use crate::kernel_handle::KernelHandle;

/// Manages the lifecycle of all registered channel adapters.
pub struct ChannelManager {
    /// Registered channels keyed by a unique name (e.g. "feishu", "wecom_app_kf").
    /// Wrapped in Arc<std::sync::Mutex> so the bridge's sync send_response can access them.
    channels: Arc<std::sync::Mutex<HashMap<String, Box<dyn Channel>>>>,
    /// Bridge message sender (inbound messages from channels).
    message_tx: mpsc::Sender<PluginMessage>,
    /// Bridge message receiver (moved to bridge on start).
    message_rx: Option<mpsc::Receiver<PluginMessage>>,
    /// Kernel handle for bridge routing.
    kernel: Arc<dyn KernelHandle>,
    /// Sender-based router (route_key → agent_id), set before start().
    sender_router: Option<Arc<SenderRouter>>,
    /// Tool dispatcher for plugin-style tools (weixin tools, etc.).
    tool_dispatcher: Arc<PluginToolDispatcher>,
}

impl ChannelManager {
    /// Create a new channel manager.
    pub fn new(kernel: Arc<dyn KernelHandle>) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            channels: Arc::new(std::sync::Mutex::new(HashMap::new())),
            message_tx: tx,
            message_rx: Some(rx),
            kernel,
            sender_router: None,
            tool_dispatcher: Arc::new(PluginToolDispatcher::new()),
        }
    }

    /// Set the sender-based router (must be called before start()).
    pub fn set_sender_router(&mut self, router: Arc<SenderRouter>) {
        self.sender_router = Some(router);
    }

    /// Register a channel adapter under a unique name.
    pub fn register(&mut self, name: &str, channel: Box<dyn Channel>) {
        self.channels
            .lock()
            .unwrap()
            .insert(name.to_string(), channel);
    }

    /// Get a reference to the tool dispatcher (for registering tool providers).
    pub fn tool_dispatcher(&self) -> Arc<PluginToolDispatcher> {
        self.tool_dispatcher.clone()
    }

    /// Start all registered channels and the bridge.
    pub async fn start(&mut self) {
        // Start channel adapters
        {
            let mut channels = self.channels.lock().unwrap();
            for (name, channel) in channels.iter_mut() {
                match channel.start(self.message_tx.clone()) {
                    Ok(()) => {
                        info!(
                            channel = %name,
                            channel_type = %channel.channel_type(),
                            bot_id = %channel.bot_id(),
                            "Channel started"
                        );
                    }
                    Err(e) => {
                        error!(
                            channel = %name,
                            channel_type = %channel.channel_type(),
                            error = %e,
                            "Failed to start channel"
                        );
                    }
                }
            }
        }

        // Build bridge
        let mut bridge = PluginBridgeManager::new(self.kernel.clone());

        if let Some(ref router) = self.sender_router {
            bridge.set_sender_router(router.clone());
        }

        // Set up channel send function for bridge to deliver responses
        let channels_for_send = self.channels.clone();
        let send_fn: ChannelSendFn = Arc::new(move |channel_type, bot_id, user_id, text| {
            let channels = channels_for_send.lock().unwrap();
            for channel in channels.values() {
                if channel.channel_type() == channel_type {
                    return channel.send(bot_id, user_id, text);
                }
            }
            Err(format!(
                "Channel not found for type: {}, bot: {}",
                channel_type, bot_id
            ))
        });
        bridge.set_channel_send_fn(send_fn);

        // Start bridge in a background task
        if let Some(rx) = self.message_rx.take() {
            tokio::spawn(async move {
                bridge.run(rx).await;
            });
        }

        let count = self.channels.lock().unwrap().len();
        info!(channels = count, "Channel manager started");
    }

    /// Send a text message through a channel by channel type and bot ID.
    pub fn channel_send(
        &self,
        channel_type: &str,
        bot_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<(), String> {
        let channels = self.channels.lock().unwrap();
        for channel in channels.values() {
            if channel.channel_type() == channel_type {
                return channel.send(bot_id, user_id, text);
            }
        }
        Err(format!(
            "Channel not found for type: {}, bot: {}",
            channel_type, bot_id
        ))
    }

    /// Send a text message by searching all channels for a matching bot_id.
    /// This matches the old PluginManager behavior where bot_id was the primary key.
    pub fn channel_send_by_bot(
        &self,
        bot_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<(), String> {
        let channels = self.channels.lock().unwrap();
        for channel in channels.values() {
            match channel.send(bot_id, user_id, text) {
                Ok(()) => return Ok(()),
                Err(_) => continue,
            }
        }
        Err(format!("No channel found for bot: {}", bot_id))
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

    /// Start a new sender that was added after initial startup.
    ///
    /// Called by the API after writing a new `senders/{sender_id}/session.json`.
    /// The matching channel loads the session and starts its connection immediately.
    pub fn start_sender(&self, channel_type: &str, sender_id: &str) -> Result<(), String> {
        let mut channels = self.channels.lock().unwrap();
        for channel in channels.values_mut() {
            if channel.channel_type() == channel_type {
                return channel.start_sender(sender_id, self.message_tx.clone());
            }
        }
        Err(format!(
            "Channel not found for type: {}, sender: {}",
            channel_type, sender_id
        ))
    }

    /// Get all plugin tool definitions.
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.tool_dispatcher.definitions()
    }

    /// Get status of all registered channels.
    pub fn status(&self) -> Vec<PluginStatus> {
        let channels = self.channels.lock().unwrap();
        channels
            .iter()
            .map(|(name, channel)| PluginStatus {
                name: name.clone(),
                version: String::new(),
                loaded: true,
                channels: vec![channel.channel_type().to_string()],
                tools: Vec::new(),
                bot_count: 0,
                last_error: None,
            })
            .collect()
    }

    /// Stop all channels and release resources.
    pub fn stop_all(&self) {
        let mut channels = self.channels.lock().unwrap();
        for (name, channel) in channels.iter_mut() {
            info!(channel = %name, "Stopping channel");
            channel.stop();
        }
    }
}

impl Drop for ChannelManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}
