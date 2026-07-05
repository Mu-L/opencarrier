//! KernelHandle trait implementation — the runtime-to-kernel interface.
//!
//! Implements the `KernelHandle` trait for `CarrierKernel`, providing agent
//! spawning, messaging, memory, task, cron, A2A, clone, and plugin operations.

use async_trait::async_trait;
use runtime::kernel_handle::{self, KernelHandle};
use runtime::llm_driver::CompletionRequest;
use runtime::memory_handle::MemoryHandle;
use types::agent::{AgentId, AgentManifest};
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
    ) -> Result<(String, String), String> {
        let content_hash = types::manifest_signing::hash_manifest(manifest_toml);
        tracing::debug!(hash = %content_hash, "Manifest SHA-256 computed for integrity tracking");

        let manifest: AgentManifest =
            toml::from_str(manifest_toml).map_err(|e| format!("Invalid manifest: {e}"))?;
        let name = manifest.name.clone();
        let parent = parent_id.and_then(|pid| pid.parse::<AgentId>().ok());
        let id = self
            .spawn_agent_with_parent(manifest, parent, None)
            .map_err(|e| format!("Spawn failed: {e}"))?;
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
    ) -> Result<String, String> {
        let (id, _target_entry) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;

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
            )
            .await
            .map_err(|e| format!("Send failed: {e}"))?;

        Ok(result.response)
    }

    async fn describe_content(
        &self,
        content_type: &str,
        url: &str,
        _metadata: Option<&str>,
    ) -> Result<String, String> {
        if content_type != "image" {
            return Ok(format!("[用户发送了非文本内容: {content_type}]"));
        }

        // Parse image data — either from data URI or HTTP download
        let (base64_data, mime) = if let Some(rest) = url.strip_prefix("data:") {
            // Data URI: data:{mime};base64,{data}
            let sep = rest.find(";base64,").ok_or("Invalid data URI format")?;
            let mime = rest[..sep].to_string();
            let b64 = rest[sep + ";base64,".len()..].to_string();

            // Size check (base64 is ~33% larger than raw)
            let max_b64 = 5 * 1024 * 1024 * 2;
            if b64.len() > max_b64 {
                return Err(format!("Image too large (data URI): {} chars", b64.len()));
            }

            (b64, mime)
        } else {
            // HTTP download — SSRF protection before fetching
            types::ssrf::check_ssrf(url)?;

            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

            let response = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("Failed to download image: {e}"))?;

            if !response.status().is_success() {
                return Err(format!("Image download failed with status: {}", response.status()));
            }

            let data = response
                .bytes()
                .await
                .map_err(|e| format!("Failed to read image bytes: {e}"))?;

            // Size check (5 MB max)
            let max_bytes = 5 * 1024 * 1024;
            if data.len() > max_bytes {
                return Err(format!("Image too large: {} bytes (max 5 MB)", data.len()));
            }

            // Detect MIME from URL extension
            let mime = {
                let path = std::path::Path::new(url);
                let ext = path
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
            };

            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
            (b64, mime)
        };

        // Build vision request
        let request = CompletionRequest {
            model: String::new(), // brain sets this from the resolved endpoint
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    ContentBlock::Image {
                        media_type: mime,
                        data: base64_data.clone(),
                    },
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
            Arc::clone(&*self.brain.brain.read().map_err(|e| format!("Brain lock: {e}"))?)
                as Arc<dyn runtime::llm_driver::Brain>;

        let result = brain
            .complete("vision", request)
            .await
            .map_err(|e| format!("Vision call failed: {e}"))?;

        let description = result.text();
        if description.is_empty() {
            return Err("Vision model returned empty description".into());
        }

        tracing::info!(content_type, b64_len = base64_data.len(), desc_len = description.len(), "Content described by vision model");
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

    fn kill_agent(&self, agent_id: &str) -> Result<(), String> {
        let (id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        CarrierKernel::kill_agent(self, id).map_err(|e| format!("Kill failed: {e}"))
    }

    fn restart_agent(&self, agent_id: &str) -> Result<(), String> {
        let (id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.stop_agent_run(id)
            .map_err(|e| format!("Stop failed: {e}"))?;

        // Re-read agent.toml from workspace to pick up tool/capability changes
        if let Some(entry) = self.registry.get(id) {
            if let Some(ref ws) = entry.manifest.workspace {
                let toml_path = ws.join("agent.toml");
                if toml_path.exists() {
                    match std::fs::read_to_string(&toml_path) {
                        Ok(toml_str) => {
                            match toml::from_str::<types::agent::AgentManifest>(&toml_str) {
                                Ok(new_manifest) => {
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
                                        .update_manifest(id, new_manifest.clone())
                                        .map_err(|e| format!("Update manifest failed: {e}"))?;
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
            .set_state(id, types::agent::AgentState::Running)
            .map_err(|e| format!("State reset failed: {e}"))?;
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
    ) -> Result<String, String> {
        self.memory
            .task_post(title, description, assigned_to, created_by)
            .await
            .map_err(|e| format!("Task post failed: {e}"))
    }

    async fn task_claim(&self, agent_id: &str) -> Result<Option<serde_json::Value>, String> {
        self.memory
            .task_claim(agent_id)
            .await
            .map_err(|e| format!("Task claim failed: {e}"))
    }

    async fn task_complete(&self, task_id: &str, result: &str) -> Result<(), String> {
        self.memory
            .task_complete(task_id, result)
            .await
            .map_err(|e| format!("Task complete failed: {e}"))
    }

    async fn task_list(&self, status: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
        self.memory
            .task_list(status)
            .await
            .map_err(|e| format!("Task list failed: {e}"))
    }

    async fn publish_event(
        &self,
        event_type: &str,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let system_agent = SYSTEM_AGENT_ID;
        let payload_bytes =
            serde_json::to_vec(&serde_json::json!({"type": event_type, "data": payload}))
                .map_err(|e| format!("Serialize failed: {e}"))?;
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
    ) -> Result<String, String> {
        use types::scheduler::{
            CronAction, CronDelivery, CronJob, CronJobId, CronSchedule,
        };

        let name = job_json["name"]
            .as_str()
            .ok_or("'name' must be a string")?
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
                .map_err(|e| format!("Invalid schedule: {e}"))?
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
                .map_err(|e| format!("Invalid action: {e}"))?
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
                        .map_err(|e| format!("Invalid delivery: {e}"))?
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
        let (aid, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;

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

        let id = self
            .cron_scheduler
            .add_job(job, one_shot)
            .map_err(|e| format!("{e}"))?;

        if let Err(e) = self.cron_scheduler.persist() {
            tracing::warn!("Failed to persist cron jobs: {e}");
        }

        Ok(serde_json::json!({
            "job_id": id.to_string(),
            "status": "created"
        })
        .to_string())
    }

    async fn cron_list(&self, agent_id: &str, owner_id: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
        let (aid, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
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

    async fn cron_cancel(&self, job_id: &str) -> Result<(), String> {
        let id = types::scheduler::CronJobId(
            uuid::Uuid::parse_str(job_id).map_err(|e| format!("Invalid job ID: {e}"))?,
        );
        self.cron_scheduler
            .remove_job(id)
            .map_err(|e| format!("{e}"))?;

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
    ) -> Result<(String, String), String> {
        let child_manifest: AgentManifest =
            toml::from_str(manifest_toml).map_err(|e| format!("Invalid manifest: {e}"))?;
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

    fn resolve_agent_workspace(&self, agent_name: &str) -> Option<String> {
        self.registry
            .find_by_name(agent_name)
            .and_then(|entry| entry.manifest.workspace.clone())
            .map(|p| p.to_string_lossy().to_string())
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

        // Search plugin tool dispatcher — channel tools (e.g. weixin_oa_send_image,
        // weixin_oa_send_miniprogram) registered as ToolProvider instances. These
        // are exact-match candidates: skill-declared tool names must resolve here.
        // Skill tool resolution passes the exact tool name as the query, so prefer
        // a high exact-match score.
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
                    1000 // skill-declared exact match — always wins
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

        // Filter by max_level + always exclude Dangerous
        scored.retain(|(_, _, def)| {
            let level = types::tool::PermissionLevel::for_tool(&def.name);
            level <= max_level && level != types::tool::PermissionLevel::Dangerous
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
    ) -> Result<String, String> {
        use base64::Engine;
        let brain: Arc<dyn runtime::llm_driver::Brain> =
            Arc::clone(&*self.brain.brain.read().map_err(|e| format!("Brain lock: {e}"))?)
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
            .map_err(|e| format!("Image generation failed: {e}"))?;

        let image = match response.media {
            Some(types::media::MediaOutput::Images { items }) => {
                items.into_iter().next().ok_or("image generation returned empty list")?
            }
            Some(types::media::MediaOutput::Image { data, .. }) => types::media::GeneratedImage {
                data_base64: base64::engine::general_purpose::STANDARD.encode(&data),
                url: None,
            },
            _ => return Err("image generation returned no media".into()),
        };

        let bytes = if !image.data_base64.is_empty() {
            base64::engine::general_purpose::STANDARD
                .decode(&image.data_base64)
                .map_err(|e| format!("decode image: {e}"))?
        } else if let Some(url) = image.url {
            reqwest::Client::new()
                .get(&url)
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| format!("download image: {e}"))?
                .bytes()
                .await
                .map_err(|e| format!("read image: {e}"))?
                .to_vec()
        } else {
            return Err("image has neither base64 data nor url".into());
        };

        let out_dir = std::path::PathBuf::from(out_dir);
        tokio::fs::create_dir_all(&out_dir)
            .await
            .map_err(|e| format!("create out_dir: {e}"))?;
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let path = out_dir.join(format!("cover_{timestamp}.png"));
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|e| format!("write image: {e}"))?;

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
    ) -> Result<(), String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.system_kv_set(&agent_id.to_string(), owner_id, user_id, key, value)
            .map_err(|e| e.to_string())
    }

    fn kv_get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.system_kv_get(&agent_id.to_string(), owner_id, user_id, key)
            .map_err(|e| e.to_string())
    }

    fn kv_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.list_kv(&agent_id.to_string(), owner_id, user_id)
            .map_err(|e| e.to_string())
    }

    fn kv_delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<(), String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.system_kv_delete(&agent_id.to_string(), owner_id, user_id, key)
            .map_err(|e| e.to_string())
    }

    async fn tree_ingest(
        &self,
        req: types::memory_tree::IngestRequest,
    ) -> Result<types::memory_tree::IngestResult, String> {
        self.memory.tree_ingest_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_query_source(
        &self,
        req: types::memory_tree::SourceQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.memory.tree_query_source_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_query_global(
        &self,
        req: types::memory_tree::GlobalQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.memory.tree_query_global_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_query_topic(
        &self,
        req: types::memory_tree::TopicQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.memory.tree_query_topic_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_search_entities(
        &self,
        req: types::memory_tree::EntitySearch<'_>,
    ) -> Result<Vec<types::memory_tree::EntityMatch>, String> {
        self.memory.tree_search_entities_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_drill_down(
        &self,
        req: types::memory_tree::DrillDownQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.memory.tree_drill_down_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_fetch_leaves(
        &self,
        req: types::memory_tree::FetchLeavesQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.memory.tree_fetch_leaves_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<types::memory_tree::TreeSummary>, String> {
        self.memory.tree_list_sources_async(owner_id, source_kind, limit).await
            .map_err(|e| e.to_string())
    }

    fn analytics_user_stats(&self, agent_id: &str, active_days: u32) -> Result<serde_json::Value, String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.analytics_user_stats(&agent_id.to_string(), active_days)
            .map_err(|e| e.to_string())
    }

    fn analytics_user_lookup(&self, agent_id: &str, sender_id: &str) -> Result<serde_json::Value, String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.analytics_user_lookup(&agent_id.to_string(), sender_id)
            .map_err(|e| e.to_string())
    }

    fn analytics_usage(&self, agent_id: &str, days: u32) -> Result<serde_json::Value, String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.analytics_usage(&agent_id.to_string(), days)
            .map_err(|e| e.to_string())
    }

    fn analytics_recent_conversations(&self, agent_id: &str, limit: u32) -> Result<serde_json::Value, String> {
        let (agent_id, _) = self.registry.resolve(agent_id)
            .map_err(|e| e.to_string())?;
        self.memory.analytics_recent_conversations(&agent_id.to_string(), limit)
            .map_err(|e| e.to_string())
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

    pub async fn clone_install(&self, name: &str, agx_data: &[u8]) -> Result<(String, String, String), String> {
        use clone::{build_manifest_from_workspace, extract_agx};

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

        // v3: extract .agx directly to workspace
        let security_warnings = extract_agx(agx_data, &workspace_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&workspace_dir);
            format!("Failed to extract .agx: {e}")
        })?;

        // Build manifest from extracted workspace
        let mut manifest = build_manifest_from_workspace(&workspace_dir, &clone_name, Some(clone_name.clone()))
            .map_err(|e| {
                let _ = std::fs::remove_dir_all(&workspace_dir);
                format!("Failed to build manifest: {e}")
            })?;
        manifest.workspace = Some(workspace_dir.clone());

        // Write agent.toml to workspace
        let toml_str = toml::to_string_pretty(&manifest)
            .map_err(|e| format!("Failed to serialize agent.toml: {e}"))?;
        std::fs::write(workspace_dir.join("agent.toml"), toml_str)
            .map_err(|e| format!("Failed to write agent.toml: {e}"))?;

        // Spawn the agent
        let agent_name = manifest.name.clone();
        let display_name = manifest.display_name.clone();
        let id = self
            .spawn_agent(manifest)
            .map_err(|e| format!("Spawn failed: {e}"))?;

        // Resolve plugin dependencies
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
            plugins = ?plugins,
            "Clone installed (v3 extract flow)"
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
    ) -> Result<(), String> {
        self.inner.system_kv_set(agent_id, owner_id, user_id, key, value)
            .map_err(|e| e.to_string())
    }

    fn kv_get(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        self.inner.system_kv_get(agent_id, owner_id, user_id, key)
            .map_err(|e| e.to_string())
    }

    fn kv_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, String> {
        self.inner.list_kv(agent_id, owner_id, user_id)
            .map_err(|e| e.to_string())
    }

    fn kv_delete(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<(), String> {
        self.inner.system_kv_delete(agent_id, owner_id, user_id, key)
            .map_err(|e| e.to_string())
    }

    async fn tree_ingest(
        &self,
        req: types::memory_tree::IngestRequest,
    ) -> Result<types::memory_tree::IngestResult, String> {
        self.inner.tree_ingest_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_query_source(
        &self,
        req: types::memory_tree::SourceQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.inner.tree_query_source_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_query_global(
        &self,
        req: types::memory_tree::GlobalQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.inner.tree_query_global_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_query_topic(
        &self,
        req: types::memory_tree::TopicQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.inner.tree_query_topic_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_search_entities(
        &self,
        req: types::memory_tree::EntitySearch<'_>,
    ) -> Result<Vec<types::memory_tree::EntityMatch>, String> {
        self.inner.tree_search_entities_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_drill_down(
        &self,
        req: types::memory_tree::DrillDownQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.inner.tree_drill_down_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_fetch_leaves(
        &self,
        req: types::memory_tree::FetchLeavesQuery<'_>,
    ) -> Result<types::memory_tree::QueryResponse, String> {
        self.inner.tree_fetch_leaves_async(req).await
            .map_err(|e| e.to_string())
    }

    async fn tree_list_sources(
        &self,
        owner_id: &str,
        source_kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<types::memory_tree::TreeSummary>, String> {
        self.inner.tree_list_sources_async(owner_id, source_kind, limit).await
            .map_err(|e| e.to_string())
    }

    fn analytics_user_stats(&self, agent_id: &str, active_days: u32) -> Result<serde_json::Value, String> {
        self.inner.analytics_user_stats(agent_id, active_days)
            .map_err(|e| e.to_string())
    }

    fn analytics_user_lookup(&self, agent_id: &str, sender_id: &str) -> Result<serde_json::Value, String> {
        self.inner.analytics_user_lookup(agent_id, sender_id)
            .map_err(|e| e.to_string())
    }

    fn analytics_usage(&self, agent_id: &str, days: u32) -> Result<serde_json::Value, String> {
        self.inner.analytics_usage(agent_id, days)
            .map_err(|e| e.to_string())
    }

    fn analytics_recent_conversations(&self, agent_id: &str, limit: u32) -> Result<serde_json::Value, String> {
        self.inner.analytics_recent_conversations(agent_id, limit)
            .map_err(|e| e.to_string())
    }
}
