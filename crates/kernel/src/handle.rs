//! KernelHandle trait implementation — the runtime-to-kernel interface.
//!
//! Implements the `KernelHandle` trait for `CarrierKernel`, providing agent
//! spawning, messaging, memory, task, cron, A2A, clone, and plugin operations.

use async_trait::async_trait;
use runtime::kernel_handle::{self, KernelHandle};
use runtime::llm_driver::CompletionRequest;
use runtime::memory_handle::MemoryHandle;
use types::agent::{AgentId, AgentManifest};
use types::error::{CarrierError, CarrierResult};
use types::event::*;
use types::message::{ContentBlock, Message, MessageContent, Role};
use std::sync::Arc;

/// Well-known agent ID for system/kernel-originated events.
pub const SYSTEM_AGENT_ID: AgentId = AgentId(uuid::Uuid::nil());

use crate::capabilities::manifest_to_capabilities;
use crate::kernel::CarrierKernel;
use memory::MemorySubstrate;

// ── Export helper ──────────────────────────────────────────

// ── KernelHandle trait implementation ─────────────────────

#[async_trait]
impl KernelHandle for CarrierKernel {
    async fn spawn_agent(
        &self,
        manifest_toml: &str,
        parent_id: Option<&str>,
    ) -> CarrierResult<(String, String)> {
        let content_hash = types::manifest_signing::hash_manifest(manifest_toml);
        tracing::debug!(hash = %content_hash, "Manifest SHA-256 computed for integrity tracking");

        let manifest: AgentManifest = toml::from_str(manifest_toml)
            .map_err(|e| CarrierError::ManifestParse(format!("Invalid manifest: {e}")))?;
        let name = manifest.name.clone();
        let parent = parent_id.and_then(|pid| pid.parse::<AgentId>().ok());
        let id = self.spawn_agent_with_parent(manifest, parent, None)?;
        Ok((id.to_string(), name))
    }

    async fn send_to_agent(
        &self,
        agent_id: &str,
        message: &str,
        sender_id: Option<&str>,
        sender_name: Option<&str>,
        _caller_agent_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
    ) -> CarrierResult<String> {
        let (id, _target_entry) = self.registry.resolve(agent_id)?;

        let handle: Option<Arc<dyn KernelHandle>> = self
            .coordination
            .self_handle
            .get()
            .and_then(|w| w.upgrade())
            .map(|arc| arc as Arc<dyn KernelHandle>);

        let result = self
            .send_message_with_handle(
                id,
                message,
                handle,
                sender_id.map(|s| s.to_string()),
                sender_name.map(|s| s.to_string()),
                owner_id.map(|s| s.to_string()),
                channel_type.map(|s| s.to_string()),
                None,
                None,
            )
            .await?;

        Ok(result.response)
    }

    async fn describe_content(
        &self,
        content_type: &str,
        url: &str,
        _metadata: Option<&str>,
    ) -> CarrierResult<String> {
        if content_type != "image" {
            return Ok(format!("[用户发送了非文本内容: {content_type}]"));
        }

        // Prefer HTTP(S) URL → vision provider fetches the image itself.
        // Avoids embedding large base64 payloads (token bloat / timeouts).
        let image_block = if url.starts_with("https://") || url.starts_with("http://") {
            // Soft SSRF guard: block obvious private-network targets even when
            // the provider does the fetch (we still don't want to pass them).
            types::ssrf::check_ssrf(url)?;
            let mime = mime_from_image_url(url);
            tracing::info!(%url, %mime, "Vision describe via public URL (no base64)");
            ContentBlock::Image {
                media_type: mime,
                data: String::new(),
                url: Some(url.to_string()),
            }
        } else if let Some(rest) = url.strip_prefix("data:") {
            // Legacy data-URI path (fallback only).
            let sep = rest
                .find(";base64,")
                .ok_or_else(|| CarrierError::InvalidInput("Invalid data URI format".into()))?;
            let mime = rest[..sep].to_string();
            let b64 = rest[sep + ";base64,".len()..].to_string();
            let max_b64 = 5 * 1024 * 1024 * 2;
            if b64.len() > max_b64 {
                return Err(CarrierError::InvalidInput(format!(
                    "Image too large (data URI): {} chars",
                    b64.len()
                )));
            }
            tracing::warn!(
                b64_len = b64.len(),
                "Vision describe falling back to data URI base64"
            );
            ContentBlock::Image {
                media_type: mime,
                data: b64,
                url: None,
            }
        } else {
            let preview: String = url.chars().take(80).collect();
            return Err(CarrierError::InvalidInput(format!(
                "Unsupported image reference (need https:// URL or data URI): {preview}"
            )));
        };

        let request = CompletionRequest {
            model: String::new(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    image_block,
                    ContentBlock::Text {
                        text: "请详细描述这张图片的内容。".to_string(),
                        provider_metadata: None,
                    },
                ]),
            }],
            tools: vec![],
            max_tokens: 1024,
            temperature: 0.3,
            system: None,
            thinking: None,
            extra: Default::default(),
        };

        let brain: Arc<dyn runtime::llm_driver::Brain> =
            Arc::clone(&*self.brain.brain.read().map_err(|e| CarrierError::Internal(format!("Brain lock: {e}")))?)
                as Arc<dyn runtime::llm_driver::Brain>;

        let result = brain
            .complete("vision", request)
            .await
            .map_err(|e| CarrierError::LlmDriver(format!("Vision call failed: {e}")))?;

        let description = result.text();
        if description.is_empty() {
            return Err(CarrierError::LlmDriver("Vision model returned empty description".into()));
        }

        tracing::info!(
            content_type,
            desc_len = description.len(),
            via_url = url.starts_with("http"),
            "Content described by vision model"
        );
        Ok(description)
    }

    fn list_agents(&self) -> Vec<kernel_handle::AgentInfo> {
        let agents = self.registry.list();
        agents
            .into_iter()
            .map(|e| {
                let (modality, model) = self.resolve_model_label(&e.manifest.model.modality);
                kernel_handle::AgentInfo {
                    id: e.id.to_string(),
                    name: e.name.clone(),
                    display_name: e.manifest.display_name.clone(),
                    state: format!("{:?}", e.state),
                    modality,
                    model,
                    description: e.manifest.description.clone(),
                    tags: e.tags.clone(),
                    tools: e.manifest.capabilities.tools.clone(),
                }
            })
            .collect()
    }

    fn kill_agent(&self, agent_id: &str) -> CarrierResult<()> {
        let (id, _) = self.registry.resolve(agent_id)?;
        CarrierKernel::kill_agent(self, id).map_err(CarrierError::from)
    }

    fn restart_agent(&self, agent_id: &str) -> CarrierResult<()> {
        let (id, _) = self.registry.resolve(agent_id)?;
        self.stop_agent_run(id)?;

        // Re-read agent.toml from workspace to pick up tool/capability changes
        if let Some(entry) = self.registry.get(id) {
            if let Some(ref ws) = entry.manifest.workspace {
                let toml_path = ws.join("agent.toml");
                if toml_path.exists() {
                    match std::fs::read_to_string(&toml_path) {
                        Ok(toml_str) => {
                            match toml::from_str::<types::agent::AgentManifest>(&toml_str) {
                                Ok(new_manifest) => {
                                    // Surface type drift in agent.toml that would
                                    // otherwise silently empty security fields
                                    // (tool_blocklist/tool_allowlist). Runtime
                                    // reload is the most likely place an operator
                                    // typo lands, so drain + warn here too.
                                    let _drift = types::serde_compat::take_lenient_diagnostics();
                                    if !_drift.is_empty() {
                                        tracing::warn!(agent = %entry.name, count = _drift.len(), details = ?_drift, "agent.toml fields fell back to empty defaults due to type drift — check tool_blocklist/tool_allowlist");
                                    }
                                    let name = entry.name.clone();
                                    let mut new_manifest = new_manifest;
                                    // Preserve workspace path (not in agent.toml)
                                    new_manifest.workspace = Some(ws.clone());
                                    // Preserve exec_policy inheritance
                                    if new_manifest.exec_policy.is_none() {
                                        new_manifest.exec_policy =
                                            Some(self.config.exec_policy.clone());
                                    }
                                    // Update in-memory registry
                                    self.registry
                                        .update_manifest(id, new_manifest.clone())?;
                                    // Re-grant capabilities
                                    let caps = manifest_to_capabilities(&new_manifest);
                                    self.coordination.capabilities.grant(id, caps);
                                    // Persist updated manifest to SQLite
                                    if let Some(updated_entry) = self.registry.get(id) {
                                        if let Err(e) = self.memory.save_agent(&updated_entry) {
                                            tracing::warn!(
                                                agent = %name,
                                                "Failed to persist reloaded manifest: {e}"
                                            );
                                        }
                                    }
                                    tracing::info!(
                                        agent = %name,
                                        "Reloaded manifest from agent.toml on restart"
                                    );
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        agent = %entry.name,
                                        "Failed to parse agent.toml on restart: {e}"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                agent = %entry.name,
                                "Failed to read agent.toml on restart: {e}"
                            );
                        }
                    }
                }
            }
        }

        self.registry
            .set_state(id, types::agent::AgentState::Running)?;
        Ok(())
    }

    fn find_agents(&self, query: &str) -> Vec<kernel_handle::AgentInfo> {
        let q = query.to_lowercase();
        let agents = self.registry.list();
        agents
            .into_iter()
            .filter(|e| {
                let name_match = e.name.to_lowercase().contains(&q);
                let tag_match = e.tags.iter().any(|t| t.to_lowercase().contains(&q));
                let tool_match = e
                    .manifest
                    .capabilities
                    .tools
                    .iter()
                    .any(|t| t.to_lowercase().contains(&q));
                let desc_match = e.manifest.description.to_lowercase().contains(&q);
                name_match || tag_match || tool_match || desc_match
            })
            .map(|e| {
                let (modality, model) = self.resolve_model_label(&e.manifest.model.modality);
                kernel_handle::AgentInfo {
                    id: e.id.to_string(),
                    name: e.name.clone(),
                    display_name: e.manifest.display_name.clone(),
                    state: format!("{:?}", e.state),
                    modality,
                    model,
                    description: e.manifest.description.clone(),
                    tags: e.tags.clone(),
                    tools: e.manifest.capabilities.tools.clone(),
                }
            })
            .collect()
    }

    async fn task_post(
        &self,
        title: &str,
        description: &str,
        assigned_to: Option<&str>,
        created_by: Option<&str>,
    ) -> CarrierResult<String> {
        self.memory
            .task_post(title, description, assigned_to, created_by)
            .await
    }

    async fn task_claim(&self, agent_id: &str) -> CarrierResult<Option<serde_json::Value>> {
        self.memory.task_claim(agent_id).await
    }

    async fn task_complete(&self, task_id: &str, result: &str) -> CarrierResult<()> {
        self.memory.task_complete(task_id, result).await
    }

    async fn task_list(&self, status: Option<&str>) -> CarrierResult<Vec<serde_json::Value>> {
        self.memory.task_list(status).await
    }

    async fn publish_event(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) -> CarrierResult<()> {
        let system_agent = SYSTEM_AGENT_ID;
        let payload_bytes =
            serde_json::to_vec(&serde_json::json!({"type": event_type, "data": payload}))
                .map_err(|e| CarrierError::Serialization(e.to_string()))?;
        let event = Event::new(
            system_agent,
            EventTarget::Broadcast,
            EventPayload::Custom(payload_bytes),
        );
        CarrierKernel::publish_event(self, event).await;
        Ok(())
    }

    async fn cron_create(
        &self,
        agent_id: &str,
        owner_id: Option<&str>,
        sender_id: Option<&str>,
        job_json: serde_json::Value,
    ) -> CarrierResult<String> {
        use types::scheduler::{
            CronAction, CronDelivery, CronJob, CronJobId, CronSchedule,
        };

        let name = job_json["name"]
            .as_str()
            .ok_or_else(|| CarrierError::InvalidInput("'name' must be a string".into()))?
            .to_string();
        let schedule: CronSchedule = {
            let schedule_val = job_json.get("schedule").cloned().unwrap_or(serde_json::Value::Null);
            // LLMs sometimes wrap the schedule in a string; unwrap it.
            let resolved = match &schedule_val {
                serde_json::Value::String(s) => {
                    serde_json::from_str::<serde_json::Value>(s).unwrap_or(schedule_val)
                }
                other => other.clone(),
            };
            serde_json::from_value(resolved)
                .map_err(|e| CarrierError::Serialization(format!("Invalid schedule: {e}")))?
        };
        let action: CronAction = {
            let action_val = job_json.get("action").cloned().unwrap_or(serde_json::Value::Null);
            let resolved = match &action_val {
                serde_json::Value::String(s) => {
                    serde_json::from_str::<serde_json::Value>(s).unwrap_or(action_val)
                }
                other => other.clone(),
            };
            serde_json::from_value(resolved)
                .map_err(|e| CarrierError::Serialization(format!("Invalid action: {e}")))?
        };
        let delivery: CronDelivery = {
            let val = job_json.get("delivery").cloned().unwrap_or(serde_json::Value::Null);
            if val.is_null() {
                // Default to LastChannel when owner_id is set so cron results
                // are pushed to the user automatically.
                if owner_id.is_some() {
                    CronDelivery::LastChannel
                } else {
                    CronDelivery::None
                }
            } else {
                let resolved = match &val {
                    serde_json::Value::String(s) => {
                        serde_json::from_str::<serde_json::Value>(s).unwrap_or_else(|_| val.clone())
                    }
                    other => other.clone(),
                };
                if resolved.is_object() {
                    serde_json::from_value(resolved)
                        .map_err(|e| CarrierError::Serialization(format!("Invalid delivery: {e}")))?
                } else {
                    tracing::warn!("delivery is not an object, defaulting to None: {val}");
                    CronDelivery::None
                }
            }
        };
        let one_shot = match job_json.get("one_shot") {
            Some(v) => match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => matches!(s.to_lowercase().as_str(), "true" | "1" | "yes"),
                _ => false,
            },
            None => false,
        };

        tracing::debug!(agent_id, "cron_create resolving agent_id");
        let (aid, _) = self.registry.resolve(agent_id)?;

        let job = CronJob {
            id: CronJobId::new(),
            agent_id: aid,
            owner_id: owner_id.map(|s| s.to_string()),
            sender_id: sender_id.map(|s| s.to_string()),
            name,
            schedule,
            action,
            delivery,
            enabled: true,
            created_at: chrono::Utc::now(),
            next_run: None,
            last_run: None,
        };

        let id = self.cron_scheduler.add_job(job, one_shot)?;

        if let Err(e) = self.cron_scheduler.persist() {
            tracing::warn!("Failed to persist cron jobs: {e}");
        }

        Ok(serde_json::json!({
            "job_id": id.to_string(),
            "status": "created"
        })
        .to_string())
    }

    async fn cron_list(&self, agent_id: &str, owner_id: Option<&str>) -> CarrierResult<Vec<serde_json::Value>> {
        let (aid, _) = self.registry.resolve(agent_id)?;
        let mut jobs = self.cron_scheduler.list_jobs(aid);
        if let Some(oid) = owner_id {
            jobs.retain(|j| j.owner_id.as_deref() == Some(oid));
        }
        let json_jobs: Vec<serde_json::Value> = jobs
            .into_iter()
            .map(|j| serde_json::to_value(&j).unwrap_or_default())
            .collect();
        Ok(json_jobs)
    }

    async fn cron_cancel(&self, job_id: &str) -> CarrierResult<()> {
        let id = types::scheduler::CronJobId(
            uuid::Uuid::parse_str(job_id)
                .map_err(|e| CarrierError::InvalidInput(format!("Invalid job ID: {e}")))?,
        );
        self.cron_scheduler.remove_job(id)?;

        if let Err(e) = self.cron_scheduler.persist() {
            tracing::warn!("Failed to persist cron jobs: {e}");
        }

        Ok(())
    }

    fn list_a2a_agents(&self) -> Vec<(String, String)> {
        self.a2a.cleanup_stale_agents();
        let agents = self
            .a2a
            .a2a_external_agents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        agents
            .iter()
            .map(|(_, card, _)| (card.name.clone(), card.url.clone()))
            .collect()
    }

    fn get_a2a_agent_url(&self, name: &str) -> Option<String> {
        self.a2a.cleanup_stale_agents();
        let agents = self
            .a2a
            .a2a_external_agents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let name_lower = name.to_lowercase();
        agents
            .iter()
            .find(|(_, card, _)| card.name.to_lowercase() == name_lower)
            .map(|(_, card, _)| card.url.clone())
    }

    async fn spawn_agent_checked(
        &self,
        manifest_toml: &str,
        parent_id: Option<&str>,
        parent_caps: &[types::capability::Capability],
    ) -> CarrierResult<(String, String)> {
        let child_manifest: AgentManifest = toml::from_str(manifest_toml)
            .map_err(|e| CarrierError::ManifestParse(format!("Invalid manifest: {e}")))?;
        let child_caps = manifest_to_capabilities(&child_manifest);

        types::capability::validate_capability_inheritance(parent_caps, &child_caps)?;

        tracing::info!(
            parent = parent_id.unwrap_or("kernel"),
            child = %child_manifest.name,
            child_caps = child_caps.len(),
            "Capability inheritance validated — spawning child agent"
        );

        KernelHandle::spawn_agent(self, manifest_toml, parent_id).await
    }

    fn home_dir(&self) -> Option<std::path::PathBuf> {
        Some(self.config.home_dir.clone())
    }

    fn external_url(&self) -> Option<String> {
        self.config.external_url.clone()
    }

    fn resolve_agent_workspace(&self, agent_name: &str) -> Option<String> {
        // Accept either agent name or UUID string — callers (esp. cron) may pass
        // either form. Workspace path still comes from the manifest (name-based dir).
        self.registry
            .resolve(agent_name)
            .ok()
            .and_then(|(_, entry)| entry.manifest.workspace.clone())
            .map(|p| p.to_string_lossy().to_string())
    }

    fn deliver_content(
        &self,
        agent: &str,
        content_key: &str,
        channel_type: &str,
        bot_id: &str,
        user_id: &str,
    ) -> CarrierResult<()> {
        let ws = self.resolve_agent_workspace(agent).ok_or_else(|| {
            CarrierError::AgentNotFound(format!(
                "deliver_content: agent {agent} not found or has no workspace"
            ))
        })?;
        let ws_path = std::path::Path::new(&ws);
        let config = runtime::outbound::ContentRegistry::global()
            .load(agent, ws_path)
            .ok_or_else(|| {
                CarrierError::Internal(format!(
                    "deliver_content: failed to load content.toml for agent {agent} under {}",
                    ws_path.display()
                ))
            })?;
        let desc = config.get(content_key).cloned().ok_or_else(|| {
            CarrierError::Internal(format!(
                "deliver_content: key '{content_key}' not found in {}/content.toml",
                ws_path.display()
            ))
        })?;

        let guard = self
            .channel_deliver_fn
            .read()
            .map_err(|e| CarrierError::Internal(e.to_string()))?;
        let deliver_fn = guard.as_ref().ok_or_else(|| {
            CarrierError::Config("deliver_content: channel_deliver_fn not wired".into())
        })?;
        deliver_fn(channel_type, bot_id, user_id, &desc)
            .map_err(|e| CarrierError::Network(format!("deliver_content: {e}")))
    }

    fn get_toolset_tools(
        &self,
        toolset_name: &str,
    ) -> Option<Vec<types::tool::ToolDefinition>> {
        let registry = self.plugins.toolset_registry.read().ok()?;

        // Resolve the registry key — try direct match first, then normalize-matching
        let resolved_key = if registry.contains_key(toolset_name) {
            toolset_name.to_string()
        } else {
            let normalized = runtime::mcp::normalize_name(toolset_name);
            registry
                .keys()
                .find(|k| runtime::mcp::normalize_name(k) == normalized)
                .cloned()?
        };

        let tools = registry.get(&resolved_key).cloned()?;
        if tools.is_empty() {
            None
        } else {
            Some(tools)
        }
    }

    fn search_tools(
        &self,
        query: &str,
        limit: usize,
        max_level: types::tool::PermissionLevel,
    ) -> Vec<(String, types::tool::ToolDefinition)> {
        let registry = match self.plugins.toolset_registry.read() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("toolset_registry read poisoned: {e}");
                return Vec::new();
            }
        };
        let query_lower = query.to_lowercase();
        let keywords: Vec<&str> = query_lower
            .split_whitespace()
            .filter(|w| w.len() >= 2)
            .collect();
        let mut scored: Vec<(usize, String, types::tool::ToolDefinition)> = Vec::new();

        // Search builtin toolsets
        for (ts_name, tools) in registry.iter() {
            let ts_lower = ts_name.to_lowercase();
            for tool in tools {
                let name_lower = tool.name.to_lowercase();
                let desc_lower = tool.description.to_lowercase();
                let score = CarrierKernel::score_tool(
                    &query_lower, &keywords,
                    &name_lower, &desc_lower, &ts_lower,
                );
                if score > 0 {
                    scored.push((score, ts_name.clone(), tool.clone()));
                }
            }
        }

        // Search MCP servers — return individual tools so the agent can call them directly.
        for entry in self.plugins.mcp_connections.iter() {
            let conn = entry.value();
            let config = conn.config();
            let server_name = config.name.to_lowercase();
            let server_desc = config.description.to_lowercase();
            let server_score = CarrierKernel::score_tool(
                &query_lower, &keywords,
                &server_name, &server_desc, &server_name,
            );
            let ts = format!("mcp_{}", runtime::mcp::normalize_name(&config.name));
            for tool in conn.tools() {
                let name_lower = tool.name.to_lowercase();
                let desc_lower = tool.description.to_lowercase();
                let tool_score = CarrierKernel::score_tool(
                    &query_lower, &keywords,
                    &name_lower, &desc_lower, &server_name,
                );
                let score = if tool_score > 0 { tool_score } else { server_score };
                if score > 0 {
                    // conn.tools() already returns namespaced names (e.g. mcp_wechat_oa_create_draft)
                    scored.push((score + 50, ts.clone(), types::tool::ToolDefinition {
                        name: tool.name.clone(),
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                    }));
                }
            }
        }

        // Search plugin tool dispatcher — remaining channel tools (e.g.
        // charter_create_order, weixin_oa_publish_article) registered as
        // ToolProvider instances. Rich content delivery now uses the unified
        // Channel::deliver path and [DELIVER:key] markers instead of channel-
        // specific send tools. These are exact-match candidates: flow-declared
        // tool names must resolve here. Flow tool resolution passes the exact
        // tool name as the query, so prefer a high exact-match score.
        if let Some(dispatcher) = self
            .plugins
            .plugin_tool_dispatcher
            .lock()
            .ok()
            .and_then(|g| g.clone())
        {
            for tool in dispatcher.definitions() {
                let name_lower = tool.name.to_lowercase();
                let exact = name_lower == query_lower;
                let score = if exact {
                    1000 // flow-declared exact match — always wins
                } else {
                    CarrierKernel::score_tool(
                        &query_lower, &keywords,
                        &name_lower,
                        &tool.description.to_lowercase(),
                        "plugin",
                    )
                };
                if score > 0 {
                    scored.push((score, "plugin".to_string(), tool));
                }
            }
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0));

        // Filter by max_level. Dangerous tools (e.g. shell_exec) are only
        // visible when max_level is Dangerous — typically via system-flow
        // turn elevation, not a permanent agent grant.
        scored.retain(|(_, _, def)| {
            let level = types::tool::PermissionLevel::for_tool(&def.name);
            level <= max_level
        });

        let count = scored.len();
        scored.truncate(limit);
        tracing::info!(
            query = query,
            results = scored.len(),
            total_candidates = count,
            "tool_search executed"
        );
        scored.into_iter().map(|(_, ts, def)| (ts, def)).collect()
    }

    fn execute_plugin_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        context: &types::plugin::PluginToolContext,
    ) -> Option<Result<String, String>> {
        let dispatcher = self
            .plugins
            .plugin_tool_dispatcher
            .lock()
            .ok()
            .and_then(|g| g.clone())?;
        if !dispatcher.has_tool(tool_name) {
            return None;
        }
        Some(dispatcher.execute(tool_name, args, context))
    }

    async fn generate_image_to_file(
        &self,
        prompt: &str,
        out_dir: &str,
    ) -> CarrierResult<String> {
        use base64::Engine;
        let brain: Arc<dyn runtime::llm_driver::Brain> =
            Arc::clone(&*self.brain.brain.read().map_err(|e| CarrierError::Internal(format!("Brain lock: {e}")))?)
                as Arc<dyn runtime::llm_driver::Brain>;

        // Build an image-gen request (mirrors runtime/src/tools/media.rs).
        let mut extra = serde_json::Map::new();
        extra.insert("model".to_string(), serde_json::json!("dall-e-3"));
        extra.insert("size".to_string(), serde_json::json!("1024x1024"));
        extra.insert("quality".to_string(), serde_json::json!("hd"));
        extra.insert("n".to_string(), serde_json::json!(1));
        let request = CompletionRequest {
            model: String::new(),
            messages: vec![types::message::Message {
                role: types::message::Role::User,
                content: types::message::MessageContent::Text(prompt.to_string()),
            }],
            tools: vec![],
            max_tokens: 0,
            temperature: 0.0,
            system: None,
            thinking: None,
            extra: serde_json::Value::Object(extra),
        };

        let response = brain
            .complete("image", request)
            .await
            .map_err(|e| CarrierError::LlmDriver(format!("Image generation failed: {e}")))?;

        let image = match response.media {
            Some(types::media::MediaOutput::Images { items }) => items.into_iter().next().ok_or_else(|| {
                CarrierError::LlmDriver("image generation returned empty list".into())
            })?,
            Some(types::media::MediaOutput::Image { data, .. }) => types::media::GeneratedImage {
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                url: None,
            },
            _ => return Err(CarrierError::LlmDriver("image generation returned no media".into())),
        };

        let bytes = if !image.data_base64.is_empty() {
            base64::engine::general_purpose::STANDARD
                .decode(&image.data_base64)
                .map_err(|e| CarrierError::Internal(format!("decode image: {e}")))?
        } else if let Some(url) = image.url {
            reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| CarrierError::Network(format!("download image: {e}")))?
                .bytes()
                .await
                .map_err(|e| CarrierError::Network(format!("read image: {e}")))?
                .to_vec()
        } else {
            return Err(CarrierError::Internal("image has neither base64 data nor url".into()));
        };

        let out_dir = std::path::PathBuf::from(out_dir);
        tokio::fs::create_dir_all(&out_dir).await?;
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let path = out_dir.join(format!("cover_{timestamp}.png"));
        tokio::fs::write(&path, &bytes).await?;

        let path_str = path.to_string_lossy().to_string();
        tracing::info!(path = %path_str, bytes = bytes.len(), "Cover image generated");
        Ok(path_str)
    }

}

// ── MemoryHandle trait implementation ─────────────────────

#[async_trait]
impl MemoryHandle for CarrierKernel {
    fn kv_set(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> CarrierResult<()> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.system_kv_set(&agent_id.to_string(), owner_id, user_id, key, value)
    }

    fn kv_get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<Option<serde_json::Value>> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.system_kv_get(&agent_id.to_string(), owner_id, user_id, key)
    }

    fn kv_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> CarrierResult<Vec<(String, serde_json::Value)>> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.list_kv(&agent_id.to_string(), owner_id, user_id)
    }

    fn kv_delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<()> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.system_kv_delete(&agent_id.to_string(), owner_id, user_id, key)
    }

    async fn tree_ingest(
        &self,
        req: types::memory_tree::IngestRequest,
    ) -> CarrierResult<types::memory_tree::IngestResult> {
        self.memory.tree_ingest_async(req).await
    }

    async fn tree_query_source(
        &self,
        req: types::memory_tree::SourceQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.memory.tree_query_source_async(req).await
    }

    async fn tree_query_global(
        &self,
        req: types::memory_tree::GlobalQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.memory.tree_query_global_async(req).await
    }

    async fn tree_query_topic(
        &self,
        req: types::memory_tree::TopicQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.memory.tree_query_topic_async(req).await
    }

    async fn tree_search_entities(
        &self,
        req: types::memory_tree::EntitySearch<'_>,
    ) -> CarrierResult<Vec<types::memory_tree::EntityMatch>> {
        self.memory.tree_search_entities_async(req).await
    }

    async fn tree_drill_down(
        &self,
        req: types::memory_tree::DrillDownQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.memory.tree_drill_down_async(req).await
    }

    async fn tree_fetch_leaves(
        &self,
        req: types::memory_tree::FetchLeavesQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.memory.tree_fetch_leaves_async(req).await
    }

    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> CarrierResult<Vec<types::memory_tree::TreeSummary>> {
        self.memory.tree_list_sources_async(owner_id, source_kind, limit).await
    }

    fn analytics_user_stats(&self, agent_id: &str, active_days: u32) -> CarrierResult<serde_json::Value> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.analytics_user_stats(&agent_id.to_string(), active_days)
    }

    fn analytics_user_lookup(&self, agent_id: &str, sender_id: &str) -> CarrierResult<serde_json::Value> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.analytics_user_lookup(&agent_id.to_string(), sender_id)
    }

    fn analytics_usage(&self, agent_id: &str, days: u32) -> CarrierResult<serde_json::Value> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.analytics_usage(&agent_id.to_string(), days)
    }

    fn analytics_recent_conversations(&self, agent_id: &str, limit: u32) -> CarrierResult<serde_json::Value> {
        let (agent_id, _) = self.registry.resolve(agent_id)?;
        self.memory.analytics_recent_conversations(&agent_id.to_string(), limit)
    }
}

type ToolsetAlias = (fn(&str) -> bool, &'static str);

// Non-trait methods on CarrierKernel (called directly, not via KernelHandle)
impl CarrierKernel {
    /// Score a tool against a search query using multi-signal matching.
    fn score_tool(
        query: &str,
        keywords: &[&str],
        tool_name: &str,
        tool_desc: &str,
        toolset_name: &str,
    ) -> usize {
        let mut score: usize = 0;

        if tool_name == query {
            return 20;
        }
        if tool_name.contains(query) {
            score += 10;
        }
        for kw in keywords {
            if tool_name.contains(kw) {
                score += 5;
            }
        }
        if tool_desc.contains(query) {
            score += 5;
        }
        for kw in keywords {
            if tool_desc.contains(kw) {
                score += 2;
            }
        }
        if toolset_name.contains(query) {
            score += 3;
        }
        for kw in keywords {
            if toolset_name.contains(kw) {
                score += 2;
            }
        }

        let aliases: &[ToolsetAlias] = &[
            (|q: &str| q.contains("file") || q.contains("save") || q.contains("read") || q.contains("write"), "filesystem"),
            (|q: &str| q.contains("browser") || q.contains("browse") || q.contains("网页") || q.contains("打开"), "browser"),
            (|q: &str| q.contains("wechat") || q.contains("微信") || q.contains("公众号") || q.contains("draft"), "wechat-oa"),
            (|q: &str| q.contains("feishu") || q.contains("飞书") || q.contains("lark"), "feishu"),
            (|q: &str| q.contains("wecom") || q.contains("企微") || q.contains("企业微信"), "wecom"),
            (|q: &str| q.contains("shell") || q.contains("command") || q.contains("exec") || q.contains("终端"), "shell"),
            (|q: &str| q.contains("image") || q.contains("图片") || q.contains("media") || q.contains("photo"), "media"),
            (|q: &str| q.contains("search") || q.contains("fetch") || q.contains("web"), "web"),
        ];
        for (matches, ts) in aliases {
            if matches(query) && toolset_name == *ts {
                score += 4;
            }
        }

        score
    }

    /// Install a clone from a file-level manifest + fetched files (dup file-level
    /// path). Writes files via `clone::write_files_to_workspace`, then
    /// build_manifest_from_workspace -> agent.toml -> spawn -> plugins.
    pub async fn clone_install_files(
        &self,
        name: &str,
        files: std::collections::BTreeMap<String, Vec<u8>>,
    ) -> Result<(String, String, String), String> {
        use clone::{build_manifest_from_workspace, write_files_to_workspace};

        if name.is_empty()
            || name.len() > 64
            || name.starts_with('-')
            || name.ends_with('-')
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!(
                "Invalid clone name '{}': must be 1-64 lowercase alphanumeric/hyphen characters",
                name
            ));
        }

        let workspace_dir = self.config.effective_workspaces_dir().join(name);
        if !workspace_dir.starts_with(self.config.effective_workspaces_dir()) {
            return Err("Path traversal denied".to_string());
        }

        let clone_name = name.to_string();

        if self.registry.find_by_name(&clone_name).is_some() {
            return Err(format!("Agent '{}' already exists", clone_name));
        }
        if workspace_dir.exists() {
            return Err(format!(
                "Workspace for '{}' already exists",
                clone_name
            ));
        }

        // File-level write of the fetched definition files.
        let security_warnings =
            write_files_to_workspace(&files, &workspace_dir).map_err(|e| {
                let _ = std::fs::remove_dir_all(&workspace_dir);
                format!("Failed to write files: {e}")
            })?;

        let mut manifest = build_manifest_from_workspace(&workspace_dir, &clone_name, Some(clone_name.clone()))
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&workspace_dir);
                format!("Failed to build manifest: {e}")
            })?;
        manifest.workspace = Some(workspace_dir.clone());

        let toml_str = toml::to_string_pretty(&manifest)
            .map_err(|e| format!("Failed to serialize agent.toml: {e}"))?;
        std::fs::write(workspace_dir.join("agent.toml"), toml_str)
            .map_err(|e| format!("Failed to write agent.toml: {e}"))?;

        let agent_name = manifest.name.clone();
        let display_name = manifest.display_name.clone();
        let id = self
            .spawn_agent(manifest)
            .map_err(|e| format!("Spawn failed: {e}"))?;

        let plugins = std::fs::read_to_string(workspace_dir.join("template.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<clone::TemplateManifest>(&s).ok())
            .map(|t| t.plugins)
            .unwrap_or_default();

        if !plugins.is_empty() {
            self.resolve_plugin_dependencies(&plugins).await;
        }

        tracing::info!(
            name = %agent_name,
            id = %id,
            warnings = security_warnings.len(),
            file_count = files.len(),
            plugins = ?plugins,
            "Clone installed (dup file-level flow)"
        );

        Ok((id.to_string(), agent_name, display_name))
    }
}

// ── MemorySubstrateHandle — wraps MemorySubstrate to implement MemoryHandle ──

/// Thin wrapper that implements `MemoryHandle` by delegating to `MemorySubstrate`.
/// Needed because MemorySubstrate can't depend on the runtime crate's trait.
pub struct MemorySubstrateHandle {
    inner: Arc<MemorySubstrate>,
}

impl MemorySubstrateHandle {
    pub fn new(inner: Arc<MemorySubstrate>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl MemoryHandle for MemorySubstrateHandle {
    fn kv_set(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> CarrierResult<()> {
        self.inner.system_kv_set(agent_id, owner_id, user_id, key, value)
    }

    fn kv_get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<Option<serde_json::Value>> {
        self.inner.system_kv_get(agent_id, owner_id, user_id, key)
    }

    fn kv_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> CarrierResult<Vec<(String, serde_json::Value)>> {
        self.inner.list_kv(agent_id, owner_id, user_id)
    }

    fn kv_delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> CarrierResult<()> {
        self.inner.system_kv_delete(agent_id, owner_id, user_id, key)
    }

    async fn tree_ingest(
        &self,
        req: types::memory_tree::IngestRequest,
    ) -> CarrierResult<types::memory_tree::IngestResult> {
        self.inner.tree_ingest_async(req).await
    }

    async fn tree_query_source(
        &self,
        req: types::memory_tree::SourceQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.inner.tree_query_source_async(req).await
    }

    async fn tree_query_global(
        &self,
        req: types::memory_tree::GlobalQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.inner.tree_query_global_async(req).await
    }

    async fn tree_query_topic(
        &self,
        req: types::memory_tree::TopicQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.inner.tree_query_topic_async(req).await
    }

    async fn tree_search_entities(
        &self,
        req: types::memory_tree::EntitySearch<'_>,
    ) -> CarrierResult<Vec<types::memory_tree::EntityMatch>> {
        self.inner.tree_search_entities_async(req).await
    }

    async fn tree_drill_down(
        &self,
        req: types::memory_tree::DrillDownQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.inner.tree_drill_down_async(req).await
    }

    async fn tree_fetch_leaves(
        &self,
        req: types::memory_tree::FetchLeavesQuery<'_>,
    ) -> CarrierResult<types::memory_tree::QueryResponse> {
        self.inner.tree_fetch_leaves_async(req).await
    }

    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> CarrierResult<Vec<types::memory_tree::TreeSummary>> {
        self.inner.tree_list_sources_async(owner_id, source_kind, limit).await
    }

    fn analytics_user_stats(&self, agent_id: &str, active_days: u32) -> CarrierResult<serde_json::Value> {
        self.inner.analytics_user_stats(agent_id, active_days)
    }

    fn analytics_user_lookup(&self, agent_id: &str, sender_id: &str) -> CarrierResult<serde_json::Value> {
        self.inner.analytics_user_lookup(agent_id, sender_id)
    }

    fn analytics_usage(&self, agent_id: &str, days: u32) -> CarrierResult<serde_json::Value> {
        self.inner.analytics_usage(agent_id, days)
    }

    fn analytics_recent_conversations(&self, agent_id: &str, limit: u32) -> CarrierResult<serde_json::Value> {
        self.inner.analytics_recent_conversations(agent_id, limit)
    }
}

fn mime_from_image_url(url: &str) -> String {
    // Strip query string for extension detection.
    let path = url.split('?').next().unwrap_or(url);
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    }
    .to_string()
}
