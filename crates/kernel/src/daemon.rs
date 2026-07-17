//! Daemon background services — watchers, heartbeat, cron tick loop, hub upgrades.
//!
//! All methods live on `CarrierKernel` but are organized here for clarity.

use types::agent::{AgentId, AgentState, ScheduleMode};
use futures::stream::{FuturesUnordered, StreamExt};

use super::handle::SYSTEM_AGENT_ID;
use types::event::*;
use types::scheduler::CronJob;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::kernel::CarrierKernel;
use runtime::kernel_handle::KernelHandle;

// ── Cron delivery helper ───────────────────────────────────

/// Turn a free-form cron job name into a path/key-safe slug.
///
/// `job.name` may contain CJK, punctuation, spaces, etc. (validate only rejects
/// control chars) so agents can name jobs naturally — e.g. "发布第二篇：OpenAI
/// 硬件". But the name is interpolated into `task_id` (used as a message
/// identity/dedup key AND as the agent's output-path template `output/{tid}/`
/// in prompt_builder.rs) and into the event `type` string `cron.{name}`. Path
/// separators (`/`, `\`), `..`, ASCII `:`, spaces, and other path/identifier-
/// hostile chars would corrupt those, so replace them with `-`. CJK, letters,
/// digits, and emoji are kept (the filesystem sandbox is UTF-8 clean). The
/// original name is still used verbatim for logs/display.
fn slugify(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_control() {
            continue;
        }
        if matches!(c, '/' | '\\' | ':' | ' ' | '.' | '<' | '>' | '"' | '|' | '?' | '*') {
            s.push('-');
        } else {
            s.push(c);
        }
    }
    // Collapse runs of '-' and trim leading/trailing '-'.
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_dash && !out.is_empty() {
                out.push('-');
            }
            prev_dash = true;
        } else {
            out.push(c);
            prev_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "job".to_string()
    } else {
        out
    }
}

/// Fire a single cron job (system event or agent turn), recording success/failure.
pub(super) async fn cron_fire_job(kernel: &Arc<CarrierKernel>, job: CronJob) {
    let job_id = job.id;
    let agent_id = job.agent_id;
    let job_name = job.name.clone();

    match &job.action {
        types::scheduler::CronAction::SystemEvent { text } => {
            tracing::debug!(job = %job_name, "Cron: firing system event");
            let payload_bytes = serde_json::to_vec(&serde_json::json!({
                "type": format!("cron.{}", slugify(&job_name)),
                "text": text,
                "job_id": job_id.to_string(),
            }))
            .unwrap_or_default();
            let event = Event::new(
                SYSTEM_AGENT_ID,
                EventTarget::Broadcast,
                EventPayload::Custom(payload_bytes),
            );
            kernel.publish_event(event).await;
            kernel.cron_scheduler.record_success(job_id);
        }
        types::scheduler::CronAction::AgentTurn {
            message,
            timeout_secs,
            ..
        } => {
            tracing::debug!(job = %job_name, agent = %agent_id, "Cron: firing agent turn");
            let timeout_s = timeout_secs.unwrap_or(120);
            let timeout = std::time::Duration::from_secs(timeout_s);
            let delivery = job.delivery.clone();
            let owner_id = job.owner_id.clone();
            // Generate task_id: {job_name slug}-{YYYYMMDD}. The name is slugified
            // because task_id is used as an identity/dedup key and interpolated
            // into the agent's output-path template (`output/{tid}/`); raw name
            // chars like `/` or `:` would corrupt paths and keys.
            let task_id = format!(
                "{}-{}",
                slugify(&job_name),
                chrono::Local::now().format("%Y%m%d")
            );
            tracing::info!(job = %job_name, task_id = %task_id, "Cron: generated task_id");
            let kh: std::sync::Arc<dyn runtime::kernel_handle::KernelHandle> =
                kernel.clone();
            match tokio::time::timeout(
                timeout,
                kernel.send_message_with_handle(
                    agent_id,
                    message,
                    Some(kh),
                    job.sender_id.clone(),
                    None,
                    job.owner_id.clone(),
                    None,
                    Some(task_id),
                ),
            )
            .await
            {
                Ok(Ok(result)) => {
                    match cron_deliver_response(
                        kernel,
                        agent_id,
                        owner_id.as_deref(),
                        &result.response,
                        &delivery,
                    )
                    .await
                    {
                        Ok(()) => {
                            tracing::info!(job = %job_name, "Cron job completed successfully");
                            kernel.cron_scheduler.record_success(job_id);
                        }
                        Err(e) => {
                            tracing::warn!(job = %job_name, error = %e, "Cron job delivery failed");
                            kernel.cron_scheduler.record_failure(job_id, &e);
                        }
                    }
                }
                Ok(Err(e)) => {
                    let err_msg = format!("{e}");
                    tracing::warn!(job = %job_name, error = %err_msg, "Cron job failed");
                    kernel.cron_scheduler.record_failure(job_id, &err_msg);
                    let notice = format!(
                        "⚠️ 定时任务「{}」执行失败：{}",
                        job_name, err_msg
                    );
                    if let Err(de) = cron_deliver_response(
                        kernel,
                        agent_id,
                        owner_id.as_deref(),
                        &notice,
                        &delivery,
                    )
                    .await
                    {
                        tracing::warn!(job = %job_name, error = %de, "Failure-notice delivery failed");
                    }
                }
                Err(_) => {
                    tracing::warn!(job = %job_name, timeout_s, "Cron job timed out");
                    kernel.cron_scheduler.record_failure(
                        job_id,
                        &format!("timed out after {timeout_s}s"),
                    );
                    let notice = format!(
                        "⚠️ 定时任务「{}」执行超时（{}秒未完成）",
                        job_name, timeout_s
                    );
                    if let Err(de) = cron_deliver_response(
                        kernel,
                        agent_id,
                        owner_id.as_deref(),
                        &notice,
                        &delivery,
                    )
                    .await
                    {
                        tracing::warn!(job = %job_name, error = %de, "Timeout-notice delivery failed");
                    }
                }
            }
        }
    }
}

/// Deliver a cron job's agent response to the configured delivery target.
///
/// - `None`: silent — no notification sent
/// - `LastChannel`: route to the channel the sender (owner_id) most recently
///   used. Buffered for later delivery if the channel doesn't support
///   proactive push or if the send attempt fails.
/// - `Webhook`: HTTP POST to the configured URL.
pub(super) async fn cron_deliver_response(
    kernel: &Arc<CarrierKernel>,
    agent_id: AgentId,
    owner_id: Option<&str>,
    response: &str,
    delivery: &types::scheduler::CronDelivery,
) -> Result<(), String> {
    use types::scheduler::CronDelivery;

    if response.is_empty() {
        return Ok(());
    }

    // Process [PUBLISH] markers the same way interactive replies do, so
    // cron-triggered publishes actually create drafts (previously this path
    // shipped the raw marker text to the channel and never published). The
    // follow-up uses the sender's last known channel if any; the draft itself
    // is created via the kernel regardless of delivery target.
    let sender_id = owner_id.unwrap_or("");
    let (pchannel, pbot, psend_fn) = cron_publish_followup_target(kernel, sender_id);
    let kh: std::sync::Arc<dyn runtime::kernel_handle::KernelHandle> = kernel.clone();
    let cleaned = runtime::plugin::bridge::process_publish_markers(
        kh,
        psend_fn,
        &pchannel,
        &pbot,
        sender_id,
        &agent_id.to_string(),
        response,
    );
    // Process [DELIVER:key] markers the same way interactive replies do, so
    // cron-triggered rich content (e.g. a charter confirm card) is delivered
    // instead of shipping the raw marker to the channel. Targets the sender's
    // last known channel; if none is known the markers are still stripped.
    let deliver_fn = kernel
        .channel_deliver_fn
        .read()
        .ok()
        .and_then(|g| g.clone());
    let config = kernel
        .resolve_agent_workspace(&agent_id.to_string())
        .and_then(|ws| {
            std::fs::read_to_string(std::path::Path::new(&ws).join("content.toml")).ok()
        })
        .and_then(|text| toml::from_str::<types::content::ContentConfig>(&text).ok());
    let cleaned = runtime::plugin::bridge::process_deliver_markers_pub(
        deliver_fn,
        config.as_ref(),
        &pchannel,
        &pbot,
        sender_id,
        &cleaned,
    )
    .await;
    // If the response was only publish/deliver markers, there's nothing left
    // to deliver to the user.
    if cleaned.trim().is_empty() {
        return Ok(());
    }
    let response = cleaned.as_str();

    // Suppress no-reply sentinels on the cron path too — same convention as the
    // interactive send_response path (bridge.rs). A scheduled turn that resolves
    // to "no reply" (e.g. a flow's final step emitting `[no reply needed]`, or a
    // non-LLM agent printing it) would otherwise ship the literal marker to
    // 客服消息 (45015) or the webhook. PUBLISH markers above already fired, so
    // only the final text send is skipped.
    if runtime::plugin::bridge::is_no_reply_sentinel(response) {
        tracing::info!(
            agent = %agent_id,
            "Cron suppressing no-reply sentinel — not sending to channel"
        );
        return Ok(());
    }

    match delivery {
        CronDelivery::None => Ok(()),
        CronDelivery::LastChannel => {
            let sender_id = owner_id.ok_or_else(|| {
                "LastChannel delivery requires owner_id on the cron job".to_string()
            })?;
            deliver_via_last_channel(kernel, agent_id, sender_id, response).await
        }
        CronDelivery::Webhook { url } => {
            tracing::debug!(url = %url, "Cron: delivering via webhook");
            types::ssrf::check_ssrf(url)?;
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| format!("webhook client init failed: {e}"))?;
            let payload = serde_json::json!({
                "agent_id": agent_id.to_string(),
                "response": response,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            });
            let resp = client.post(url).json(&payload).send().await.map_err(|e| {
                tracing::warn!(error = %e, "Cron webhook delivery failed");
                format!("webhook delivery failed: {e}")
            })?;
            tracing::debug!(status = %resp.status(), "Cron webhook delivered");
            Ok(())
        }
    }
}

/// Best-effort channel target for a cron publish *follow-up* notification.
///
/// The publish draft itself is created via the kernel/WeChat API and needs no
/// channel; this only routes the post-publish success/failure message. Returns
/// the sender's last known `(channel_type, bot_id)` if we have one, plus the
/// channel send fn. When there's no known channel the follow-up is skipped
/// (empty strings) but publishing still proceeds.
fn cron_publish_followup_target(
    kernel: &Arc<CarrierKernel>,
    sender_id: &str,
) -> (String, String, Option<runtime::plugin::bridge::ChannelSendFn>) {
    let last = kernel
        .memory
        .cron_delivery()
        .get_last_channel(sender_id)
        .ok()
        .flatten();
    let send_fn = kernel
        .channel_send_fn
        .read()
        .ok()
        .and_then(|g| g.clone());
    match last {
        Some(c) => (c.channel_type, c.bot_id, send_fn),
        None => (String::new(), String::new(), send_fn),
    }
}

/// Deliver a notification to the sender's most recent channel. Attempts a
/// proactive push first; on failure (or for channels that don't support push)
/// the notification is buffered for delivery on the next inbound message.
async fn deliver_via_last_channel(
    kernel: &Arc<CarrierKernel>,
    agent_id: AgentId,
    sender_id: &str,
    response: &str,
) -> Result<(), String> {
    let store = kernel.memory.cron_delivery();
    let last = match store
        .get_last_channel(sender_id)
        .map_err(|e| format!("get_last_channel failed: {e}"))?
    {
        Some(c) => c,
        None => {
            // We've never seen this sender — buffer the notification so it
            // delivers when they first send an inbound message.
            store
                .buffer_notification(
                    sender_id,
                    &agent_id.to_string(),
                    response,
                    "cron",
                    memory::cron_delivery::DEFAULT_TTL_SECS,
                )
                .map_err(|e| format!("buffer notification failed: {e}"))?;
            tracing::info!(sender = %sender_id, "Cron: buffered (no last channel)");
            return Ok(());
        }
    };

    // Check if the channel supports proactive push; if not, buffer directly.
    let supports = kernel
        .channel_supports_proactive_fn
        .read()
        .ok()
        .and_then(|guard| guard.as_ref().map(|f| f(&last.channel_type)))
        .unwrap_or(false);

    if !supports {
        store
            .buffer_notification(
                sender_id,
                &agent_id.to_string(),
                response,
                "cron",
                memory::cron_delivery::DEFAULT_TTL_SECS,
            )
            .map_err(|e| format!("buffer notification failed: {e}"))?;
        tracing::info!(
            sender = %sender_id,
            channel = %last.channel_type,
            "Cron: buffered (channel does not support proactive push)"
        );
        return Ok(());
    }

    // Try proactive push. If it fails, fall back to buffering.
    let send_fn = kernel
        .channel_send_fn
        .read()
        .ok()
        .and_then(|guard| guard.clone());
    let send_fn = match send_fn {
        Some(f) => f,
        None => {
            return Err("channel_send_fn not configured".to_string());
        }
    };

    match send_fn(&last.channel_type, &last.bot_id, sender_id, response) {
        Ok(()) => {
            tracing::info!(
                sender = %sender_id,
                channel = %last.channel_type,
                "Cron: delivered via last channel"
            );
            Ok(())
        }
        Err(e) => {
            tracing::warn!(
                sender = %sender_id,
                channel = %last.channel_type,
                error = %e,
                "Cron: proactive send failed, buffering"
            );
            store
                .buffer_notification(
                    sender_id,
                    &agent_id.to_string(),
                    response,
                    "cron",
                    memory::cron_delivery::DEFAULT_TTL_SECS,
                )
                .map_err(|e| format!("buffer notification failed: {e}"))?;
            Ok(())
        }
    }
}

// ── Background daemon methods ──────────────────────────────

impl CarrierKernel {
    /// Start file watchers for clone agents to auto-compile on knowledge changes.
    fn start_clone_watchers(self: &Arc<Self>) {
        if !self.config.clone_lifecycle.evolution_enabled {
            return;
        }

        let agents = self.registry.list();
        let kernel = Arc::clone(self);

        for entry in &agents {
            let Some(ref _cs) = entry.manifest.clone_source else {
                continue;
            };
            let Some(ref workspace) = entry.manifest.workspace else {
                continue;
            };

            let config =
                lifecycle::evolution_config::read_evolution_config(workspace.as_path());

            if matches!(
                config.evolution_mode,
                lifecycle::evolution_config::EvolutionMode::Disabled
            ) {
                continue;
            }

            let driver = match kernel.resolve_driver(&entry.manifest) {
                Ok(d) => d,
                Err(e) => {
                    warn!(agent = %entry.name, error = %e, "No LLM driver for watcher");
                    continue;
                }
            };
            let rt_handle = tokio::runtime::Handle::current();

            let llm_call: Arc<lifecycle::watcher::LlmCallback> = Arc::new(
                move |sys: &str, user: &str, max_tokens: u32| -> anyhow::Result<String> {
                    let request = runtime::llm_driver::CompletionRequest {
                        model: String::new(),
                        messages: vec![types::message::Message {
                            role: types::message::Role::User,
                            content: types::message::MessageContent::Text(user.to_string()),
                        }],
                        tools: vec![],
                        max_tokens,
                        temperature: 0.3,
                        system: Some(sys.to_string()),
                        thinking: None,
                        extra: Default::default(),
                    };
                    // IMPORTANT: Do NOT use `rt_handle.block_on()` here.
                    // The watcher callback runs on a notify crate thread, and
                    // block_on() can deadlock if all tokio worker threads are busy.
                    // Instead, spawn the async work and wait via oneshot channel.
                    let (tx, rx) = std::sync::mpsc::channel();
                    let driver = driver.clone();
                    rt_handle.spawn(async move {
                        let result = tokio::time::timeout(
                            std::time::Duration::from_secs(60),
                            driver.complete(request),
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("knowledge watcher LLM call timed out after 60s"))
                        .and_then(|r| r.map(|r| r.text()).map_err(|e| anyhow::anyhow!("{e}")));
                        let _ = tx.send(result);
                    });
                    rx.recv_timeout(std::time::Duration::from_secs(65))
                        .map_err(|_| anyhow::anyhow!("knowledge watcher LLM call channel closed or timed out"))?
                },
            );

            match lifecycle::watcher::spawn_watcher(
                workspace.clone(),
                config,
                llm_call,
                None,
            ) {
                Ok(handle) => {
                    info!(agent = %entry.name, "Started knowledge file watcher");
                    if let Ok(mut handles) = kernel.runtime.watcher_handles.lock() {
                        handles.push(handle);
                    }
                }
                Err(e) => {
                    warn!(agent = %entry.name, error = %e, "Failed to start file watcher");
                }
            }
        }
    }

    /// Check hub for clone template upgrades.
    fn check_hub_upgrades(self: &Arc<Self>) {
        let hub_url = match self.config.hub.url.as_str() {
            "" | "none" => return,
            url => url.to_string(),
        };

        let agents = self.registry.list();
        let kernel = Arc::clone(self);
        tokio::spawn(async move {
            for entry in &agents {
                let Some(ref cs) = entry.manifest.clone_source else {
                    continue;
                };
                let Some(ref tid) = cs.hub_template_id else {
                    continue;
                };

                let local_ver: i64 = match cs.agx_version.parse() {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let url = format!("{}/api/templates/{}", hub_url.trim_end_matches('/'), tid);
                let resp = match reqwest::get(&url).await {
                    Ok(r) if r.status().is_success() => r,
                    _ => continue,
                };
                let json: serde_json::Value = match resp.json().await {
                    Ok(j) => j,
                    Err(_) => continue,
                };
                let remote_ver: i64 = match json
                    .get("latest_version")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse().ok())
                {
                    Some(v) => v,
                    None => continue,
                };

                if remote_ver <= local_ver {
                    continue;
                }

                info!(
                    agent = %entry.name,
                    hub_template = %tid,
                    local = local_ver,
                    remote = remote_ver,
                    auto_upgrade = cs.auto_upgrade,
                    "Hub template update available"
                );

                if !cs.auto_upgrade {
                    continue;
                }

                let agent_name = entry.name.clone();
                match kernel.clone_upgrade(&agent_name).await {
                    Ok(ver) => info!(
                        agent = %agent_name,
                        new_version = %ver,
                        "Auto-upgrade completed"
                    ),
                    Err(e) => warn!(
                        agent = %agent_name,
                        error = %e,
                        "Auto-upgrade failed"
                    ),
                }
            }
        });
    }

    /// Iterates the agent registry and starts background tasks for agents with
    /// `Continuous`, `Periodic`, or `Proactive` schedules.
    pub fn start_background_agents(self: &Arc<Self>) {
        let agents = self.registry.list();
        let mut bg_agents: Vec<(types::agent::AgentId, String, ScheduleMode)> = Vec::new();

        for entry in &agents {
            if matches!(entry.manifest.schedule, ScheduleMode::Reactive) {
                continue;
            }
            bg_agents.push((
                entry.id,
                entry.name.clone(),
                entry.manifest.schedule.clone(),
            ));
        }

        if !bg_agents.is_empty() {
            let count = bg_agents.len();
            let kernel = Arc::clone(self);
            tokio::spawn(async move {
                for (i, (id, name, schedule)) in bg_agents.into_iter().enumerate() {
                    kernel.start_background_for_agent(id, &name, &schedule);
                    if i > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                }
                info!("Started {count} background agent loop(s) (staggered)");
            });
        }

        self.start_heartbeat_monitor();

        // Periodic usage data cleanup (every 24 hours, retain 90 days)
        {
            let kernel = Arc::clone(self);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    if kernel.runtime.supervisor.is_shutting_down() {
                        break;
                    }
                    match kernel.metering.cleanup(90) {
                        Ok(removed) if removed > 0 => {
                            info!("Metering cleanup: removed {removed} old usage records");
                        }
                        Err(e) => {
                            warn!("Metering cleanup failed: {e}");
                        }
                        _ => {}
                    }
                }
            });
        }

        // Connect to configured + extension MCP servers
        let has_mcp = self
            .plugins
            .effective_mcp_servers
            .read()
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if has_mcp {
            let kernel = Arc::clone(self);
            tokio::spawn(async move {
                kernel.connect_mcp_servers().await;
                kernel.build_toolset_registry();
            });
        }

        self.check_hub_upgrades();
        self.start_clone_watchers();

        // Cron scheduler tick loop — fires due jobs every 15 seconds
        {
            let kernel = Arc::clone(self);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut persist_counter = 0u32;
                interval.tick().await;
                loop {
                    interval.tick().await;
                    if kernel.runtime.supervisor.is_shutting_down() {
                        let _ = kernel.cron_scheduler.persist();
                        break;
                    }

                    let due = kernel.cron_scheduler.due_jobs();
                    let mut handles = FuturesUnordered::new();
                    for job in due {
                        let job_id = job.id;
                        let job_name = job.name.clone();
                        let k = Arc::clone(&kernel);
                        handles.push(async move {
                            let handle = tokio::spawn(async move {
                                cron_fire_job(&k, job).await;
                            });
                            (job_id, job_name, handle.await)
                        });
                    }
                    while let Some((job_id, job_name, result)) = handles.next().await {
                        if let Err(join_error) = result {
                            if join_error.is_panic() {
                                tracing::error!(job = %job_name, "Cron job task panicked");
                                kernel.cron_scheduler.record_failure(job_id, "cron task panicked");
                            } else {
                                tracing::error!(job = %job_name, "Cron job task cancelled");
                                kernel.cron_scheduler.record_failure(job_id, "cron task cancelled");
                            }
                        }
                    }

                    persist_counter += 1;
                    if persist_counter >= 20 {
                        persist_counter = 0;
                        if let Err(e) = kernel.cron_scheduler.persist() {
                            tracing::warn!("Cron persist failed: {e}");
                        }
                        // Periodically purge expired pending notifications.
                        match kernel.memory.cron_delivery().purge_expired() {
                            Ok(0) => {}
                            Ok(n) => tracing::debug!(deleted = n, "Purged expired pending notifications"),
                            Err(e) => tracing::warn!("Purge expired notifications failed: {e}"),
                        }
                    }
                }
            });
            if self.cron_scheduler.total_jobs() > 0 {
                info!(
                    "Cron scheduler active with {} job(s)",
                    self.cron_scheduler.total_jobs()
                );
            }
        }

        // Flow run expiry tick - reaps `waiting` flow runs whose `user_input`
        // deadline has passed, marking them `timed_out`. Mirrors the cron loop.
        {
            let kernel = Arc::clone(self);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval.tick().await; // discard immediate first tick
                loop {
                    interval.tick().await;
                    if kernel.runtime.supervisor.is_shutting_down() {
                        break;
                    }
                    let now = chrono::Utc::now().to_rfc3339();
                    match kernel.memory.flow_runs().list_expired(&now) {
                        Ok(rows) => {
                            for r in rows {
                                let completed = r.completed_steps.clone();
                                match kernel
                                    .memory
                                    .flow_runs()
                                    .update_status(&r.run_id, "timed_out", &completed)
                                {
                                    Ok(()) => info!(
                                        run_id = %r.run_id,
                                        flow = %r.flow_name,
                                        "flow_run timed out (user_input deadline passed)"
                                    ),
                                    Err(e) => warn!(
                                        run_id = %r.run_id,
                                        error = %e,
                                        "flow_run timeout mark failed"
                                    ),
                                }
                            }
                        }
                        Err(e) => warn!(error = %e, "list_expired flow_runs failed"),
                    }
                }
            });
        }

        // Discover configured external A2A agents
        if let Some(ref a2a_config) = self.config.a2a {
            if a2a_config.enabled && !a2a_config.external_agents.is_empty() {
                let kernel = Arc::clone(self);
                let agents = a2a_config.external_agents.clone();
                tokio::spawn(async move {
                    let discovered = runtime::a2a::discover_external_agents(&agents).await;
                    if let Ok(mut store) = kernel.a2a.a2a_external_agents.lock() {
                        *store = discovered.into_iter().map(|(url, card)| (url, card, std::time::Instant::now())).collect();
                    }
                });
            }
        }
    }

    /// Periodically checks running agents and publishes events for unresponsive ones.
    fn start_heartbeat_monitor(self: &Arc<Self>) {
        use crate::heartbeat::{check_agents, is_quiet_hours, HeartbeatConfig, RecoveryTracker};

        let kernel = Arc::clone(self);
        let config = HeartbeatConfig::default();
        let interval_secs = config.check_interval_secs;
        let recovery_tracker = RecoveryTracker::new();

        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(config.check_interval_secs));

            loop {
                interval.tick().await;

                if kernel.runtime.supervisor.is_shutting_down() {
                    info!("Heartbeat monitor stopping (shutdown)");
                    break;
                }

                let statuses = check_agents(&kernel.registry, &config);
                for status in &statuses {
                    if let Some(entry) = kernel.registry.get(status.agent_id) {
                        if let Some(ref auto_cfg) = entry.manifest.autonomous {
                            if let Some(ref qh) = auto_cfg.quiet_hours {
                                if is_quiet_hours(qh) {
                                    continue;
                                }
                            }
                        }
                    }

                    if status.state == AgentState::Crashed {
                        let failures = recovery_tracker.failure_count(status.agent_id);

                        if failures >= config.max_recovery_attempts {
                            if let Some(entry) = kernel.registry.get(status.agent_id) {
                                if entry.state == AgentState::Crashed {
                                    let _ = kernel
                                        .registry
                                        .set_state(status.agent_id, AgentState::Terminated);
                                    warn!(
                                        agent = %status.name,
                                        attempts = failures,
                                        "Agent exhausted all recovery attempts — marked Terminated. Manual restart required."
                                    );
                                    let event = Event::new(
                                        status.agent_id,
                                        EventTarget::System,
                                        EventPayload::System(SystemEvent::HealthCheckFailed {
                                            agent_id: status.agent_id,
                                            unresponsive_secs: status.inactive_secs as u64,
                                        }),
                                    );
                                    kernel.coordination.event_bus.publish(event).await;
                                }
                            }
                            continue;
                        }

                        if !recovery_tracker
                            .can_attempt(status.agent_id, config.recovery_cooldown_secs)
                        {
                            debug!(
                                agent = %status.name,
                                "Recovery cooldown active, skipping"
                            );
                            continue;
                        }

                        let attempt = recovery_tracker.record_attempt(status.agent_id);
                        info!(
                            agent = %status.name,
                            attempt = attempt,
                            max = config.max_recovery_attempts,
                            "Auto-recovering crashed agent (attempt {}/{})",
                            attempt,
                            config.max_recovery_attempts
                        );
                        let _ = kernel
                            .registry
                            .set_state(status.agent_id, AgentState::Running);

                        let event = Event::new(
                            status.agent_id,
                            EventTarget::System,
                            EventPayload::System(SystemEvent::HealthCheckFailed {
                                agent_id: status.agent_id,
                                unresponsive_secs: 0,
                            }),
                        );
                        kernel.coordination.event_bus.publish(event).await;
                        continue;
                    }

                    if status.state == AgentState::Running
                        && !status.unresponsive
                        && recovery_tracker.failure_count(status.agent_id) > 0
                    {
                        info!(
                            agent = %status.name,
                            "Agent recovered successfully — resetting recovery tracker"
                        );
                        recovery_tracker.reset(status.agent_id);
                    }

                    if status.unresponsive && status.state == AgentState::Running {
                        let _ = kernel
                            .registry
                            .set_state(status.agent_id, AgentState::Crashed);
                        warn!(
                            agent = %status.name,
                            inactive_secs = status.inactive_secs,
                            "Unresponsive Running agent marked as Crashed for recovery"
                        );

                        let event = Event::new(
                            status.agent_id,
                            EventTarget::System,
                            EventPayload::System(SystemEvent::HealthCheckFailed {
                                agent_id: status.agent_id,
                                unresponsive_secs: status.inactive_secs as u64,
                            }),
                        );
                        kernel.coordination.event_bus.publish(event).await;
                    }
                }
            }
        });

        info!("Heartbeat monitor started (interval: {}s)", interval_secs);
    }

    /// Start the background loop for a single agent.
    pub fn start_background_for_agent(
        self: &Arc<Self>,
        agent_id: AgentId,
        name: &str,
        schedule: &ScheduleMode,
    ) {
        let kernel = Arc::clone(self);
        self.runtime
            .background
            .start_agent(agent_id, name, schedule, move |aid, msg| {
                let k = Arc::clone(&kernel);
                tokio::spawn(async move {
                    // Background ticks are agent-autonomous (no user/sender); give
                    // them an explicit `task:autonomous` label so the session is
                    // traceable instead of falling back to an unlabeled orphan.
                    let handle: Option<std::sync::Arc<dyn runtime::kernel_handle::KernelHandle>> = k
                        .coordination
                        .self_handle
                        .get()
                        .and_then(|w| w.upgrade())
                        .map(|a| a as std::sync::Arc<dyn runtime::kernel_handle::KernelHandle>);
                    match k
                        .send_message_with_handle(
                            aid,
                            &msg,
                            handle,
                            None,
                            None,
                            None,
                            None,
                            Some("autonomous".to_string()),
                        )
                        .await
                    {
                        Ok(_) => {}
                        Err(e) => {
                            warn!(agent_id = %aid, error = %e, "Background tick failed");
                        }
                    }
                })
            });
    }
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugify_keeps_cjk_and_strips_path_chars() {
        // The motivating case: a naturally-named Chinese job must become a safe
        // task_id / event-type segment. ASCII path-hostile chars (here the
        // space) collapse to `-`; CJK and full-width punctuation (`：`, `（）`)
        // are kept — they're UTF-8-safe in paths and harmless in event types.
        assert_eq!(
            slugify("发布第二篇：OpenAI 硬件（2026）"),
            "发布第二篇：OpenAI-硬件（2026）"
        );
    }

    #[test]
    fn slugify_neutralizes_traversal_and_separators() {
        // `/`, `\`, `..`, ASCII `:` — none may survive into a path template or
        // an event-type string.
        assert_eq!(slugify("a/../../etc"), "a-etc");
        assert_eq!(slugify("x\\y"), "x-y");
        assert_eq!(slugify("a:b"), "a-b");
        assert_eq!(slugify("v1.2"), "v1-2");
    }

    #[test]
    fn slugify_ascii_passthrough() {
        // An already-safe ASCII name (the historical whitelist form) is a no-op.
        assert_eq!(slugify("daily-report"), "daily-report");
        assert_eq!(slugify("job_42"), "job_42");
    }

    #[test]
    fn slugify_collapses_and_trims_dashes() {
        assert_eq!(slugify("a   b"), "a-b"); // spaces collapse
        assert_eq!(slugify("--weird--"), "weird"); // leading/trailing trimmed
        assert_eq!(slugify("   "), "job"); // all-hostile -> fallback
    }
}
