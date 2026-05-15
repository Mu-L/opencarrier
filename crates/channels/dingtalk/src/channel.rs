//! Per-tenant DingTalk channel adapter.
//!
//! Spawns an OS thread with a tokio runtime running the DingTalk WS client.

use crate::api;
use crate::token::AccessTokenCache;
use crate::ws::DingTalkWsClient;
use types::channel::Channel;
use types::plugin::PluginMessage;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub struct DingTalkChannel {
    bot_name: String,
    app_key: String,
    token_cache: Arc<AccessTokenCache>,
    shutdown: Arc<AtomicBool>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl DingTalkChannel {
    pub fn new(bot_name: String, app_key: String, token_cache: Arc<AccessTokenCache>) -> Self {
        Self {
            bot_name,
            app_key,
            token_cache,
            shutdown: Arc::new(AtomicBool::new(false)),
            thread_handle: None,
        }
    }
}

impl Channel for DingTalkChannel {
    fn channel_type(&self) -> &str {
        "dingtalk"
    }

    fn supports_proactive_push(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.bot_name
    }

    fn bot_id(&self) -> &str {
        &self.app_key
    }

    fn start(&mut self, sender: mpsc::Sender<PluginMessage>) -> Result<(), String> {
        let bot_name = self.bot_name.clone();
        let app_key = self.app_key.clone();
        let token_cache = self.token_cache.clone();
        let shutdown = self.shutdown.clone();
        let log_name = bot_name.clone();

        let handle = std::thread::Builder::new()
            .name(format!("dingtalk-ws-{app_key}"))
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        warn!(tenant = %bot_name, "Failed to create tokio runtime: {e}");
                        return;
                    }
                };

                let client = DingTalkWsClient::new(app_key, token_cache, shutdown);
                rt.block_on(client.run(&sender));
                info!(tenant = %bot_name, "DingTalk WS client exited");
            })
            .map_err(|e| format!("Failed to spawn DingTalk channel thread: {e}"))?;

        self.thread_handle = Some(handle);
        info!(tenant = %log_name, "DingTalkChannel started");
        Ok(())
    }

    fn send(&self, _bot_id: &str, user_id: &str, text: &str) -> Result<(), String> {
        let token_cache = self.token_cache.clone();
        let user_id = user_id.to_string();
        let text = text.to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(Err(format!("Runtime creation failed: {e}")));
                    return;
                }
            };

            let result = rt.block_on(async {
                let token = token_cache
                    .get_token()
                    .await
                    .map_err(|e| format!("Token error: {e}"))?;
                let http = token_cache.http().clone();
                let robot_code = token_cache.app_key().to_string();

                api::send_direct_message(&http, &token, &robot_code, &user_id, &text).await
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
                Ok(()) => info!(tenant = %self.bot_name, "DingTalk channel thread joined"),
                Err(e) => {
                    if let Some(s) = e.downcast_ref::<&str>() {
                        warn!(tenant = %self.bot_name, "DingTalk channel thread panicked: {s}");
                    }
                }
            }
        }
    }
}
