//! KernelHandle trait implementation — the runtime-to-kernel interface.
//!
//! Implements the `KernelHandle` trait for `CarrierKernel`, providing agent
//! spawning, messaging, memory, task, cron, A2A, clone, and plugin operations.

use async_trait::async_trait;
use carrier_runtime::kernel_handle::{self, KernelHandle};
use carrier_types::agent::{AgentId, AgentManifest};
use carrier_types::event::*;
use carrier_types::memory::Memory;
use std::sync::Arc;

use crate::capabilities::manifest_to_capabilities;
use crate::kernel::CarrierKernel;

// ── Export helper ──────────────────────────────────────────

/// Recursively collect .md files under `dir`, storing relative paths from `base`.
fn collect_files_recursive(
    dir: &std::path::Path,
    base: &std::path::Path,
    result: &mut std::collections::HashMap<String, String>,
) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files_recursive(&path, base, result);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Ok(relative) = path.strip_prefix(base) {
                    if let Some(rel_str) = relative.to_str() {
                        let content = std::fs::read_to_string(&path).unwrap_or_default();
                        result.insert(rel_str.to_string(), content);
                    }
                }
            }
        }
    }
}

// ── KernelHandle trait implementation ─────────────────────

#[async_trait]
impl KernelHandle for CarrierKernel {
    async fn spawn_agent(
        &self,
        manifest_toml: &str,
        parent_id: Option<&str>,
    ) -> Result<(String, String), String> {
        let content_hash = carrier_types::manifest_signing::hash_manifest(manifest_toml);
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
    ) -> Result<String, String> {
        let (id, _target_entry): (AgentId, carrier_types::agent::AgentEntry) = match agent_id.parse() {
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
        self.registry
            .set_state(id, carrier_types::agent::AgentState::Running)
            .map_err(|e| format!("State reset failed: {e}"))?;
        Ok(())
    }

    fn memory_store(
        &self,
        agent_id: &str,
        sender_id: &str,
        key: &str,
        value: serde_json::Value,
    ) -> Result<(), String> {
        let aid: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        self.memory
            .structured_set(aid, sender_id, key, value)
            .map_err(|e| format!("Memory store failed: {e}"))
    }

    fn memory_recall(
        &self,
        agent_id: &str,
        sender_id: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let aid: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        self.memory
            .structured_get(aid, sender_id, key)
            .map_err(|e| format!("Memory recall failed: {e}"))
    }

    fn memory_list(
        &self,
        agent_id: &str,
        sender_id: &str,
    ) -> Result<Vec<(String, serde_json::Value)>, String> {
        let aid: AgentId = agent_id
            .parse()
            .map_err(|_| "Invalid agent ID".to_string())?;
        self.memory
            .list_kv(aid, sender_id)
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
        entity: carrier_types::memory::Entity,
    ) -> Result<String, String> {
        self.memory
            .add_entity(entity)
            .await
            .map_err(|e| format!("Knowledge add entity failed: {e}"))
    }

    async fn knowledge_add_relation(
        &self,
        relation: carrier_types::memory::Relation,
    ) -> Result<String, String> {
        self.memory
            .add_relation(relation)
            .await
            .map_err(|e| format!("Knowledge add relation failed: {e}"))
    }

    async fn knowledge_query(
        &self,
        pattern: carrier_types::memory::GraphPattern,
    ) -> Result<Vec<carrier_types::memory::GraphMatch>, String> {
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
        use carrier_types::scheduler::{
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

        let aid = carrier_types::agent::AgentId(
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
        let aid = carrier_types::agent::AgentId(
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
        let id = carrier_types::scheduler::CronJobId(
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
        let agents = self
            .a2a
            .a2a_external_agents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        agents
            .iter()
            .map(|(_, card)| (card.name.clone(), card.url.clone()))
            .collect()
    }

    fn get_a2a_agent_url(&self, name: &str) -> Option<String> {
        let agents = self
            .a2a
            .a2a_external_agents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let name_lower = name.to_lowercase();
        agents
            .iter()
            .find(|(_, card)| card.name.to_lowercase() == name_lower)
            .map(|(_, card)| card.url.clone())
    }

    async fn spawn_agent_checked(
        &self,
        manifest_toml: &str,
        parent_id: Option<&str>,
        parent_caps: &[carrier_types::capability::Capability],
    ) -> Result<(String, String), String> {
        let child_manifest: AgentManifest =
            toml::from_str(manifest_toml).map_err(|e| format!("Invalid manifest: {e}"))?;
        let child_caps = manifest_to_capabilities(&child_manifest);

        carrier_types::capability::validate_capability_inheritance(parent_caps, &child_caps)?;

        tracing::info!(
            parent = parent_id.unwrap_or("kernel"),
            child = %child_manifest.name,
            child_caps = child_caps.len(),
            "Capability inheritance validated — spawning child agent"
        );

        KernelHandle::spawn_agent(self, manifest_toml, parent_id).await
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
    ) -> Option<Vec<carrier_types::tool::ToolDefinition>> {
        let agent_id: carrier_types::agent::AgentId = agent_id_str.parse().ok()?;
        let tools = self.available_tools(agent_id);
        if tools.is_empty() {
            None
        } else {
            Some(tools)
        }
    }

    async fn clone_install(&self, name: &str, agx_data: &[u8]) -> Result<(String, String), String> {
        use carrier_clone::{convert_to_manifest, install_clone_to_workspace, load_agx};

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

        let tmp_dir = std::env::temp_dir().join(format!("carrier-clone-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).map_err(|e| format!("Failed to create temp dir: {e}"))?;
        let tmp_path = tmp_dir.join("clone.agx");
        std::fs::write(&tmp_path, agx_data)
            .map_err(|e| format!("Failed to write temp file: {e}"))?;

        let clone_data = load_agx(&tmp_path).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_dir);
            format!("Failed to parse .agx: {e}")
        })?;
        let _ = std::fs::remove_dir_all(&tmp_dir);

        let clone_name = name.to_string();

        if self.registry.find_by_name(&clone_name).is_some() {
            return Err(format!("Agent '{}' already exists", clone_name));
        }

        if let Err(e) = std::fs::create_dir_all(&workspace_dir) {
            return Err(format!(
                "Workspace for '{}' already exists or cannot be created: {e}",
                clone_name
            ));
        }

        install_clone_to_workspace(&clone_data, &workspace_dir).map_err(|e| {
            let _ = std::fs::remove_dir_all(&workspace_dir);
            format!("Failed to install clone: {e}")
        })?;

        let mut manifest = convert_to_manifest(&clone_data, Some(name.to_string()));
        manifest.name = clone_name.clone();
        manifest.workspace = Some(workspace_dir);

        let agent_name = manifest.name.clone();
        let id = self
            .spawn_agent(manifest)
            .map_err(|e| format!("Spawn failed: {e}"))?;

        if !clone_data.plugins.is_empty() {
            self.resolve_plugin_dependencies(&clone_data.plugins).await;
        }

        tracing::info!(
            name = %agent_name,
            id = %id,
            warnings = clone_data.security_warnings.len(),
            plugins = ?clone_data.plugins,
            "Clone installed via clone_install tool"
        );

        Ok((id.to_string(), agent_name))
    }

    fn clone_export(&self, name: &str) -> Result<Vec<u8>, String> {
        use carrier_clone::{pack_agx, AgentData, CloneData, SkillData, SkillScriptData};
        use std::collections::HashMap;

        let workspace_str = self
            .resolve_agent_workspace(name)
            .ok_or_else(|| format!("Agent '{}' not found or has no workspace", name))?;
        let workspace = std::path::Path::new(&workspace_str);

        let read_file = |path: &std::path::Path| -> String {
            std::fs::read_to_string(path).unwrap_or_default()
        };

        let soul = read_file(&workspace.join("SOUL.md"));
        let system_prompt = read_file(&workspace.join("system_prompt.md"));
        let memory_index = read_file(&workspace.join("MEMORY.md"));
        let evolution = read_file(&workspace.join("EVOLUTION.md"));
        let profile = read_file(&workspace.join("profile.md"));

        let description = if let Some(rest) = profile.strip_prefix("---") {
            if let Some(end) = rest.find("---") {
                let fm = &profile[3..3 + end];
                fm.lines()
                    .find_map(|line| {
                        let trimmed = line.trim();
                        trimmed
                            .strip_prefix("description:")
                            .map(|v| v.trim().trim_matches('"').to_string())
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        let manifest = workspace
            .join("template.json")
            .exists()
            .then(|| {
                std::fs::read_to_string(workspace.join("template.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<carrier_clone::TemplateManifest>(&s).ok())
            })
            .flatten()
            .unwrap_or_else(|| carrier_clone::TemplateManifest {
                version: "1".to_string(),
                name: name.to_string(),
                display_name: String::new(),
                description: description.clone(),
                author: String::new(),
                tags: vec![],
                exported_at: String::new(),
                knowledge_version: 0,
                plugins: vec![],
            });

        let mut knowledge = HashMap::new();
        let knowledge_dir = workspace.join("data").join("knowledge");
        if knowledge_dir.exists() {
            collect_files_recursive(&knowledge_dir, &knowledge_dir, &mut knowledge);
        }

        let mut skills = Vec::new();
        let skills_dir = workspace.join("skills");
        if skills_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&skills_dir) {
                for entry in entries.flatten() {
                    let skill_path = entry.path();
                    if skill_path.is_dir() {
                        let skill_md_path = skill_path.join("SKILL.md");
                        if skill_md_path.exists() {
                            let content = read_file(&skill_md_path);
                            let (fm, body) = carrier_clone::parse_frontmatter(&content);
                            let skill_name = fm.get("name").cloned().unwrap_or_else(|| {
                                skill_path
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("unknown")
                                    .to_string()
                            });
                            let when_to_use = fm.get("when_to_use").cloned().unwrap_or_default();
                            let allowed_tools = fm
                                .get("allowed_tools")
                                .map(|s| carrier_clone::parse_string_array(s))
                                .unwrap_or_default();

                            let mut scripts = Vec::new();
                            let scripts_dir = skill_path.join("scripts");
                            if scripts_dir.exists() {
                                if let Ok(script_entries) = std::fs::read_dir(&scripts_dir) {
                                    for se in script_entries.flatten() {
                                        let sp = se.path();
                                        if sp.extension().map(|e| e == "toml").unwrap_or(false) {
                                            let toml_content = read_file(&sp);
                                            let script_name = sp
                                                .file_stem()
                                                .and_then(|n| n.to_str())
                                                .unwrap_or("unknown")
                                                .to_string();
                                            let desc = carrier_clone::parse_toml_description(
                                                &toml_content,
                                            );
                                            scripts.push(SkillScriptData {
                                                name: script_name,
                                                description: desc,
                                                toml_content,
                                            });
                                        }
                                    }
                                }
                            }

                            skills.push(SkillData {
                                name: skill_name,
                                when_to_use,
                                allowed_tools,
                                prompt: body.trim().to_string(),
                                scripts,
                            });
                        }
                    }
                }
            }
        }

        let mut agents = Vec::new();
        let agents_dir = workspace.join("agents");
        if agents_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&agents_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        let content = read_file(&path);
                        let (fm, body) = carrier_clone::parse_frontmatter(&content);
                        let agent_name = fm.get("name").cloned().unwrap_or_else(|| {
                            path.file_stem()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string()
                        });
                        agents.push(AgentData {
                            name: agent_name,
                            description: fm.get("description").cloned().unwrap_or_default(),
                            tools: fm
                                .get("tools")
                                .map(|s| carrier_clone::parse_string_array(s))
                                .unwrap_or_default(),
                            model: fm
                                .get("model")
                                .cloned()
                                .unwrap_or_else(|| "sonnet".to_string()),
                            color: fm.get("color").cloned(),
                            prompt: body.trim().to_string(),
                        });
                    }
                }
            }
        }

        let mut style = HashMap::new();
        let style_dir = workspace.join("style");
        if style_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&style_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                            style.insert(fname.to_string(), read_file(&path));
                        }
                    }
                }
            }
        }

        let clone_data = CloneData {
            manifest: Some(manifest),
            name: name.to_string(),
            description,
            soul,
            system_prompt,
            memory_index,
            knowledge,
            skills,
            profile,
            security_warnings: Vec::new(),
            agents,
            evolution,
            style,
            plugins: vec![],
        };

        pack_agx(&clone_data).map_err(|e| format!("Failed to pack .agx: {e}"))
    }

    async fn clone_publish(&self, name: &str, agx_bytes: &[u8]) -> Result<String, String> {
        let hub_url = self.config.hub.url.clone();
        let api_key = carrier_clone::hub::read_api_key(&self.config.hub.api_key_env)
            .map_err(|e| format!("Hub API Key 未配置: {e}"))?;

        let result = carrier_clone::hub::publish_template(
            &hub_url, &api_key, agx_bytes, None, None,
        )
        .await
        .map_err(|e| format!("Hub publish failed: {e}"))?;

        tracing::info!(
            name = %name,
            result = %result,
            "Clone published to Hub"
        );

        Ok(result)
    }

    async fn execute_plugin_tool(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        sender_id: &str,
        agent_id: &str,
    ) -> Result<String, String> {
        let guard = self.plugins.plugin_tool_dispatcher.lock().unwrap();
        if let Some(ref dispatcher) = *guard {
            let context = carrier_types::plugin::PluginToolContext {
                bot_id: String::new(),
                sender_id: sender_id.to_string(),
                agent_id: agent_id.to_string(),
                channel_type: String::new(),
            };
            dispatcher.execute(tool_name, args, &context)
        } else {
            Err(format!("Unknown tool: {tool_name}"))
        }
    }
}
