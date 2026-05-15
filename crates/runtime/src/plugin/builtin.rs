//! Built-in plugin — directly compiled channel adapters and tools (no FFI).
//!
//! Used for core channels (weixin, wecom, feishu) that ship with the binary.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use types::plugin::{PluginMessage, PluginToolContext, PluginToolDef};
use types::tool::ToolProvider;
use tokio::sync::mpsc;

use super::instance::PluginInstance;
use super::loader::LoadedChannel;

/// Trait for built-in channel adapters.
///
/// Similar to `types::channel::Channel` but uses the host's
/// native `mpsc::Sender<PluginMessage>` instead of an FFI callback.
/// Prefer using `Channel` from `types::channel` for new code.
pub trait BuiltinChannel: Send + Sync {
    fn channel_type(&self) -> &str;
    fn name(&self) -> &str;
    fn bot_id(&self) -> &str;
    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String>;
    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String>;
    fn stop(&mut self);

    /// Whether this channel supports proactive push (sending without an inbound
    /// context). Channels that require a context_token / response_url should
    /// return false; cron and other server-initiated notifications must be
    /// buffered for these channels until the user sends an inbound message.
    fn supports_proactive_push(&self) -> bool {
        false
    }
}

/// A built-in plugin that directly holds Rust trait objects.
pub struct BuiltinPlugin {
    name: String,
    version: String,
    path: PathBuf,
    channels: Vec<LoadedChannel>,
    tools: Vec<PluginToolDef>,
    channel_adapters: Mutex<HashMap<String, Box<dyn BuiltinChannel>>>,
    tool_providers: HashMap<String, Box<dyn ToolProvider>>,
}

impl BuiltinPlugin {
    pub fn new(name: String, version: String, path: PathBuf) -> Self {
        Self {
            name,
            version,
            path,
            channels: Vec::new(),
            tools: Vec::new(),
            channel_adapters: Mutex::new(HashMap::new()),
            tool_providers: HashMap::new(),
        }
    }

    pub fn register_channel(
        &mut self,
        mut adapter: Box<dyn BuiltinChannel>,
        sender: mpsc::Sender<PluginMessage>,
    ) -> Result<(), String> {
        let channel_type = adapter.channel_type().to_string();
        let name = adapter.name().to_string();
        let bot_id = adapter.bot_id().to_string();

        adapter.start(sender)?;

        self.channels.push(LoadedChannel {
            channel_type: channel_type.clone(),
            name,
            bot_id,
            handle: std::ptr::null_mut(),
        });

        self.channel_adapters
            .lock()
            .unwrap()
            .insert(channel_type, adapter);
        Ok(())
    }

    pub fn register_tool(&mut self, provider: Box<dyn ToolProvider>) {
        let def = provider.definition();
        self.tools.push(PluginToolDef {
            name: def.name.clone(),
            description: def.description.clone(),
            parameters_json: def.parameters_json.clone(),
        });
        self.tool_providers.insert(def.name, provider);
    }
}

unsafe impl Send for BuiltinPlugin {}
unsafe impl Sync for BuiltinPlugin {}

impl PluginInstance for BuiltinPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn channels(&self) -> &[LoadedChannel] {
        &self.channels
    }

    fn tools(&self) -> &[PluginToolDef] {
        &self.tools
    }

    fn start_channel(&self, _channel: &LoadedChannel) -> Result<(), String> {
        Ok(())
    }

    fn channel_send(
        &self,
        channel: &LoadedChannel,
        bot_id: &str,
        user_id: &str,
        text: &str,
    ) -> Result<(), String> {
        let adapters = self.channel_adapters.lock().unwrap();
        if let Some(adapter) = adapters.get(&channel.channel_type) {
            adapter.send(bot_id, user_id, text)
        } else {
            Err(format!(
                "Built-in channel adapter '{}' not found",
                channel.channel_type
            ))
        }
    }

    fn tool_execute(
        &self,
        tool_name: &str,
        args_json: &str,
        context_json: &str,
    ) -> Result<String, String> {
        let provider = self
            .tool_providers
            .get(tool_name)
            .ok_or_else(|| format!("Built-in tool '{}' not found", tool_name))?;

        let args: serde_json::Value =
            serde_json::from_str(args_json).map_err(|e| format!("Args deserialization: {}", e))?;
        let ctx: PluginToolContext = serde_json::from_str(context_json)
            .map_err(|e| format!("Context deserialization: {}", e))?;

        provider
            .execute(&args, &ctx)
            .map_err(|e| e.to_string())
    }

    fn stop(&self) {
        let mut adapters = self.channel_adapters.lock().unwrap();
        for (name, adapter) in adapters.iter_mut() {
            adapter.stop();
            tracing::info!(channel = %name, "Built-in channel stopped");
        }
    }

    fn is_stopped(&self) -> bool {
        self.channel_adapters.lock().unwrap().is_empty()
    }
}
