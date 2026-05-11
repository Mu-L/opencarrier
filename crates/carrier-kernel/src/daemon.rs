//! Daemon background services — watchers, heartbeat, cron tick loop, hub upgrades.
//!
//! All methods live on `CarrierKernel` but are organized here for clarity.

use carrier_types::agent::{AgentId, AgentState, ScheduleMode};
use carrier_types::event::*;
use carrier_types::memory::Memory;
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::kernel::CarrierKernel;

// ── Cron delivery helper ───────────────────────────────────

/// Deliver a cron job's agent response to the configured delivery target.
pub(super) async fn cron_deliver_response(
    _kernel: &CarrierKernel,
    agent_id: AgentId,
    response: &str,
    delivery: &carrier_types::scheduler::CronDelivery,
) -> Result<(), String> {
    use carrier_types::scheduler::CronDelivery;

    if response.is_empty() {
        return Ok(());
    }

    match delivery {
        CronDelivery::None => Ok(()),
        CronDelivery::LastChannel => Ok(()),
        CronDelivery::Webhook { url } => {
            tracing::debug!(url = %url, "Cron: delivering via webhook");
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
                carrier_lifecycle::evolution_config::read_evolution_config(workspace.as_path());

            if matches!(
                config.evolution_mode,
                carrier_lifecycle::evolution_config::EvolutionMode::Disabled
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

            let llm_call: Arc<carrier_lifecycle::watcher::LlmCallback> = Arc::new(
                move |sys: &str, user: &str, max_tokens: u32| -> anyhow::Result<String> {
                    let request = carrier_runtime::llm_driver::CompletionRequest {
                        model: String::new(),
                        messages: vec![carrier_types::message::Message {
                            role: carrier_types::message::Role::User,
                            content: carrier_types::message::MessageContent::Text(user.to_string()),
                        }],
                        tools: vec![],
                        max_tokens,
                        temperature: 0.3,
                        system: Some(sys.to_string()),
                        thinking: None,
                        extra: Default::default(),
                    };
                    rt_handle
                        .block_on(async { driver.complete(request).await })
                        .map(|r: carrier_runtime::llm_driver::CompletionResponse| r.text())
                        .map_err(|e| anyhow::anyhow!("{e}"))
                },
            );

            match carrier_lifecycle::watcher::spawn_watcher(
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
        let mut bg_agents: Vec<(carrier_types::agent::AgentId, String, ScheduleMode)> = Vec::new();

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

        // Probe local providers for reachability and model discovery
        {
            let kernel = Arc::clone(self);
            tokio::spawn(async move {
                let local_providers: Vec<(String, String)> = {
                    let catalog = kernel
                        .brain
                        .model_catalog
                        .read()
                        .unwrap_or_else(|e| e.into_inner());
                    catalog
                        .list_providers()
                        .iter()
                        .filter(|p| !p.key_required)
                        .map(|p| (p.id.clone(), p.base_url.clone()))
                        .collect()
                };

                for (provider_id, base_url) in &local_providers {
                    let result =
                        carrier_runtime::provider_health::probe_provider(provider_id, base_url)
                            .await;
                    if result.reachable {
                        info!(
                            provider = %provider_id,
                            models = result.discovered_models.len(),
                            latency_ms = result.latency_ms,
                            "Local provider online"
                        );
                        if !result.discovered_models.is_empty() {
                            if let Ok(mut catalog) = kernel.brain.model_catalog.write() {
                                catalog.merge_discovered_models(
                                    provider_id,
                                    &result.discovered_models,
                                );
                            }
                        }
                    } else {
                        warn!(
                            provider = %provider_id,
                            error = result.error.as_deref().unwrap_or("unknown"),
                            "Local provider offline"
                        );
                    }
                }
            });
        }

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

        // Periodic memory consolidation
        {
            let interval_hours = self.config.memory.consolidation_interval_hours;
            if interval_hours > 0 {
                let kernel = Arc::clone(self);
                tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                        interval_hours * 3600,
                    ));
                    interval.tick().await;
                    loop {
                        interval.tick().await;
                        if kernel.runtime.supervisor.is_shutting_down() {
                            break;
                        }
                        match kernel.memory.consolidate().await {
                            Ok(report) => {
                                if report.memories_decayed > 0 || report.memories_merged > 0 {
                                    info!(
                                        merged = report.memories_merged,
                                        decayed = report.memories_decayed,
                                        duration_ms = report.duration_ms,
                                        "Memory consolidation completed"
                                    );
                                }
                            }
                            Err(e) => {
                                warn!("Memory consolidation failed: {e}");
                            }
                        }
                    }
                });
                info!("Memory consolidation scheduled every {interval_hours} hour(s)");
            }
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
                    for job in due {
                        let job_id = job.id;
                        let agent_id = job.agent_id;
                        let job_name = job.name.clone();

                        match &job.action {
                            carrier_types::scheduler::CronAction::SystemEvent { text } => {
                                tracing::debug!(job = %job_name, "Cron: firing system event");
                                let payload_bytes = serde_json::to_vec(&serde_json::json!({
                                    "type": format!("cron.{}", job_name),
                                    "text": text,
                                    "job_id": job_id.to_string(),
                                }))
                                .unwrap_or_default();
                                let event = Event::new(
                                    AgentId::new(),
                                    EventTarget::Broadcast,
                                    EventPayload::Custom(payload_bytes),
                                );
                                kernel.publish_event(event).await;
                                kernel.cron_scheduler.record_success(job_id);
                            }
                            carrier_types::scheduler::CronAction::AgentTurn {
                                message,
                                timeout_secs,
                                ..
                            } => {
                                tracing::debug!(job = %job_name, agent = %agent_id, "Cron: firing agent turn");
                                let timeout_s = timeout_secs.unwrap_or(120);
                                let timeout = std::time::Duration::from_secs(timeout_s);
                                let delivery = job.delivery.clone();
                                let kh: std::sync::Arc<
                                    dyn carrier_runtime::kernel_handle::KernelHandle,
                                > = kernel.clone();
                                match tokio::time::timeout(
                                    timeout,
                                    kernel.send_message_with_handle(
                                        agent_id,
                                        message,
                                        Some(kh),
                                        None,
                                        None,
                                        None,
                                    ),
                                )
                                .await
                                {
                                    Ok(Ok(result)) => {
                                        match cron_deliver_response(
                                            &kernel,
                                            agent_id,
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
                                    }
                                    Err(_) => {
                                        tracing::warn!(job = %job_name, timeout_s, "Cron job timed out");
                                        kernel.cron_scheduler.record_failure(
                                            job_id,
                                            &format!("timed out after {timeout_s}s"),
                                        );
                                    }
                                }
                            }
                        }
                    }

                    persist_counter += 1;
                    if persist_counter >= 20 {
                        persist_counter = 0;
                        if let Err(e) = kernel.cron_scheduler.persist() {
                            tracing::warn!("Cron persist failed: {e}");
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

        // Discover configured external A2A agents
        if let Some(ref a2a_config) = self.config.a2a {
            if a2a_config.enabled && !a2a_config.external_agents.is_empty() {
                let kernel = Arc::clone(self);
                let agents = a2a_config.external_agents.clone();
                tokio::spawn(async move {
                    let discovered = carrier_runtime::a2a::discover_external_agents(&agents).await;
                    if let Ok(mut store) = kernel.a2a.a2a_external_agents.lock() {
                        *store = discovered;
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
                    match k.send_message(aid, &msg).await {
                        Ok(_) => {}
                        Err(e) => {
                            warn!(agent_id = %aid, error = %e, "Background tick failed");
                        }
                    }
                })
            });
    }
}
