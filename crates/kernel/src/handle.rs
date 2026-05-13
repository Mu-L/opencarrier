//! KernelHandle trait implementation — the runtime-to-kernel interface.
//!
//! Implements the `KernelHandle` trait for `CarrierKernel`, providing agent
//! spawning, messaging, memory, task, cron, A2A, clone, and plugin operations.

use async_trait::async_trait;
use runtime::kernel_handle::{self, KernelHandle};
use types::agent::{AgentId, AgentManifest};
use types::event::*;
use types::memory::Memory;
use std::sync::Arc;

use crate::capabilities::manifest_to_capabilities;
use crate::kernel::CarrierKernel;

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
    ) -> Result<String, String> {
        let (id, _target_entry): (AgentId, types::agent::AgentEntry) = match agent_id.parse() {
            Ok(id) => {
                let entry = self
                    .registry
                    .get(id)
                    .ok_or_else(|| format!("Agent not found: {agent_id}"))?;
                (id, entry)
            }
            Err(_) => {
                let entry = self
                    .registry
                    .find_by_name(agent_id)
                    .ok_or_else(|| format!("Agent '{agent_id}' not found"))?;
                (entry.id, entry)
            }
        };

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
            )
            .await
            .map_err(|e| format!("Send failed: {e}"))?;

        Ok(result.response)
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
        let id: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        CarrierKernel::kill_agent(self, id).map_err(|e| format!("Kill failed: {e}"))
    }

    fn restart_agent(&self, agent_id: &str) -> Result<(), String> {
        let id: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
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

    fn memory_store(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        let aid: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        self.memory
            .structured_set(aid, owner_id, user_id, key, value)
            .map_err(|e| format!("Memory store failed: {e}"))
    }

    fn memory_recall(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let aid: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        self.memory
            .structured_get(aid, owner_id, user_id, key)
            .map_err(|e| format!("Memory recall failed: {e}"))
    }

    fn memory_list(
        &self,
        agent_id: &str,
        owner_id: &str,
        user_id: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, String> {
        let aid: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        self.memory
            .list_kv(aid, owner_id, user_id)
            .map_err(|e| format!("Memory list failed: {e}"))
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
        let system_agent = AgentId::new();
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

    async fn knowledge_add_entity(
        &self,
        entity: types::memory::Entity,
    ) -> Result<String, String> {
        self.memory
            .add_entity(entity)
            .await
            .map_err(|e| format!("Knowledge add entity failed: {e}"))
    }

    async fn knowledge_add_relation(
        &self,
        relation: types::memory::Relation,
    ) -> Result<String, String> {
        self.memory
            .add_relation(relation)
            .await
            .map_err(|e| format!("Knowledge add relation failed: {e}"))
    }

    async fn knowledge_query(
        &self,
        pattern: types::memory::GraphPattern,
    ) -> Result<Vec<types::memory::GraphMatch>, String> {
        self.memory
            .query_graph(pattern)
            .await
            .map_err(|e| format!("Knowledge query failed: {e}"))
    }

    async fn cron_create(
        &self,
        agent_id: &str,
        job_json: serde_json::Value,
    ) -> Result<String, String> {
        use types::scheduler::{
            CronAction, CronDelivery, CronJob, CronJobId, CronSchedule,
        };

        let name = job_json["name"]
            .as_str()
            .ok_or("Missing 'name' field")?
            .to_string();
        let schedule: CronSchedule = serde_json::from_value(job_json["schedule"].clone())
            .map_err(|e| format!("Invalid schedule: {e}"))?;
        let action: CronAction = serde_json::from_value(job_json["action"].clone())
            .map_err(|e| format!("Invalid action: {e}"))?;
        let delivery: CronDelivery = if job_json["delivery"].is_object() {
            serde_json::from_value(job_json["delivery"].clone())
                .map_err(|e| format!("Invalid delivery: {e}"))?
        } else {
            CronDelivery::None
        };
        let one_shot = job_json["one_shot"].as_bool().unwrap_or(false);

        let aid = types::agent::AgentId(
            uuid::Uuid::parse_str(agent_id).map_err(|e| format!("Invalid agent ID: {e}"))?,
        );

        let job = CronJob {
            id: CronJobId::new(),
            agent_id: aid,
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

    async fn cron_list(&self, agent_id: &str) -> Result<Vec<serde_json::Value>, String> {
        let aid = types::agent::AgentId(
            uuid::Uuid::parse_str(agent_id).map_err(|e| format!("Invalid agent ID: {e}"))?,
        );
        let jobs = self.cron_scheduler.list_jobs(aid);
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

    fn refresh_tools(
        &self,
        agent_id_str: &str,
    ) -> Option<Vec<types::tool::ToolDefinition>> {
        let agent_id: types::agent::AgentId = agent_id_str.parse().ok()?;
        let tools = self.available_tools(agent_id);
        if tools.is_empty() {
            None
        } else {
            Some(tools)
        }
    }

}

// Non-trait methods on CarrierKernel (called directly, not via KernelHandle)
impl CarrierKernel {
    pub async fn clone_install(&self, name: &str, agx_data: &[u8]) -> Result<(String, String), String> {
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

        Ok((id.to_string(), agent_name))
    }
}
