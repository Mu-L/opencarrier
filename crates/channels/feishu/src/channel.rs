//! FeishuChannel — WebSocket-based Channel for Feishu/Lark.
//!
//! Spawns a dedicated OS thread with its own tokio runtime.
//! Connects to the Feishu WebSocket long-connection endpoint and dispatches
//! inbound messages through the host's native `mpsc::Sender<PluginMessage>`.

use crate::token::BotTokenCache;
use crate::ws::FeishuWsClient;
use carrier_types::channel::Channel;
use carrier_types::plugin::PluginMessage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::info;

/// Channel adapter for a single Feishu tenant (one app_id).
pub struct FeishuChannel {
    bot_id: String,
    bot_uuid: String,
    token_cache: Arc<BotTokenCache>,
    shutdown: Arc<AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl FeishuChannel {
    pub fn new(bot_id: String, bot_uuid: String, token_cache: Arc<BotTokenCache>) -> Self {
        Self {
            bot_id,
            bot_uuid,
            token_cache,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
        }
    }
}

impl Channel for FeishuChannel {
    fn channel_type(&self) -> &str {
        "feishu"
    }

    fn name(&self) -> &str {
        &self.bot_id
    }

    #[allow(clippy::misnamed_getters)]
    fn bot_id(&self) -> &str {
        &self.bot_uuid
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        let bot_id = self.bot_id.clone();
        let bot_uuid = self.bot_uuid.clone();
        let token_cache = self.token_cache.clone();
        let shutdown = self.shutdown.clone();
        let thread_tenant = bot_id.clone();

        let handle = std::thread::Builder::new()
            .name(format!("feishu-ws-{bot_id}"))
            .spawn(move || {
                run_ws_loop(&thread_tenant, bot_uuid, token_cache, shutdown, sender);
            })
            .map_err(|e| format!("Failed to spawn Feishu WS thread: {e}"))?;

        self.thread_handle = Some(handle);
        info!(tenant = %bot_id, "FeishuChannel started");
        Ok(())
    }

    fn send(&self, bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        // Verify tenant matches (by bot_uuid)
        if bot_id != self.bot_uuid {
            return Err(format!(
                "Tenant mismatch: expected {}, got {}",
                self.bot_uuid, bot_id
            ));
        }

        let content = serde_json::json!({ "text": text }).to_string();
        let token_cache = self.token_cache.clone();
        let user_id = user_id.to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("Failed to create send runtime: {e}")));
                    return;
                }
            };
            let result = rt.block_on(async {
                let token = token_cache
                    .get_token()
                    .await
                    .map_err(|e| format!("Token error: {e}"))?;
                let http = token_cache.http().clone();
                let base = token_cache.api_base().to_string();
                let resp = crate::api::send_message(
                    &http, &token, &base, &user_id, "open_id", "text", &content,
                )
                .await?;

                if resp.code != 0 {
                    return Err(format!(
                        "Feishu send error: code={} msg={}",
                        resp.code, resp.msg
                    ));
                }
                Ok(())
            });
            let _ = tx.send(result);
        });

        rx.recv()
            .map_err(|e| format!("Send thread disconnected: {e}"))?
    }

    fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);

        if let Some(handle) = self.thread_handle.take() {
            match handle.join() {
                Ok(()) => info!(tenant = %self.bot_id, "WS thread joined cleanly"),
                Err(e) => {
                    if let Some(s) = e.downcast_ref::<&str>() {
                        tracing::error!(tenant = %self.bot_id, "WS thread panicked: {s}");
                    }
                }
            }
        }

        info!(tenant = %self.bot_id, "FeishuChannel stopped");
    }
}

/// Main WebSocket loop (runs in a dedicated thread with its own runtime).
fn run_ws_loop(
    bot_id: &str,
    bot_uuid: String,
    token_cache: Arc<BotTokenCache>,
    shutdown: Arc<AtomicBool>,
    sender: mpsc::Sender<PluginMessage>,
) {
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(tenant = bot_id, "Failed to create tokio runtime: {e}");
            return;
        }
    };

    let ws_client = FeishuWsClient::new(bot_id.to_string(), bot_uuid, token_cache, shutdown);

    rt.block_on(async move {
        ws_client.run(&sender).await;
    });
}
