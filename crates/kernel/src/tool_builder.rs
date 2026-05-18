//! Tool resolution and prompt building — available_tools, toolset registry, system prompt.
//!
//! Assembles the tool set for each agent request using the toolset model:
//! - Core tools (always visible): session_summarize, tool_search, etc.
//! - Toolsets (on-demand): filesystem, shell, knowledge, media, misc, web, agent, + MCP servers
//! - Toolset activation: skill-declared toolsets + tool_search on-demand (both → session.active_toolsets)

use crate::kernel::CarrierKernel;
use crate::prompt_sources::{
    read_agents_directory, read_evolution_rules, read_identity_file, read_knowledge_content,
    read_skills_catalog, read_style_samples, read_user_profile_summary,
    read_workspace_skills_prompts,
};
use types::agent::*;
use types::tool::ToolDefinition;

/// Tool names that are always visible (core tools). These bootstrap the agent:
/// - session_summarize: explicit summarization
/// - tool_search: discover and load tools on-demand
/// - skill_load: load workflow skills
/// - knowledge_read / knowledge_list: read workflow docs and discover knowledge
/// - cron_*: schedule tasks
///
/// All other tools are loaded on-demand via tool_search (active_toolsets).
const CORE_TOOLS: &[&str] = &[
    "session_summarize",
    "tool_search",
    "skill_load",
    "knowledge_read", "knowledge_list",
    "cron_create", "cron_list", "cron_cancel",
    "memory_tree",
];

/// Map a builtin tool name to its toolset. Returns None for core tools.
pub(crate) fn tool_to_toolset(name: &str) -> Option<&'static str> {
    match name {
        "session_summarize"
        | "tool_search"
        | "skill_load"
        | "knowledge_read" | "knowledge_list"
        | "cron_create" | "cron_list" | "cron_cancel"
        | "memory_tree" => None,
        n if n.starts_with("file_") => Some("filesystem"),
        "shell_exec" => Some("shell"),
        n if n.starts_with("knowledge_") || n.starts_with("skill_") || n == "clone_evaluate" => Some("knowledge"),
        n if n.starts_with("memory_") => Some("memory"),
        n if n.starts_with("media_") || n.starts_with("image_") || n == "text_to_speech" || n == "speech_to_text" => Some("media"),
        n if n.starts_with("web_") => Some("web"),
        n if n.starts_with("agent_") || n.starts_with("train_") => Some("agent"),
        n if n.starts_with("location_") || n.starts_with("system_") || n == "user_profile" => Some("misc"),
        n if n.starts_with("process_") => Some("process"),
        "apply_patch" => Some("filesystem"),
        _ => Some("misc"),
    }
}

/// Builtin toolset names (used to distinguish from MCP toolsets).
const BUILTIN_TOOLSETS: &[&str] = &["filesystem", "shell", "knowledge", "memory", "media", "process", "web", "agent", "misc"];

/// Tools that remain available even when a skill restricts the tool list.
/// These are foundational: the agent must always be able to summarize
/// state and look up its own knowledge base. `skill_load` is deliberately
/// EXCLUDED so the LLM can't escape the skill-imposed scope.
pub(crate) const ALWAYS_AVAILABLE_WITH_SKILL: &[&str] = &[
    "session_summarize",
    "knowledge_read",
    "knowledge_list",
    "memory_tree",
    "tool_search",
];

/// Filter a tool list by `max_tool_level` (discovery mode).
/// Keeps ALWAYS_AVAILABLE_WITH_SKILL tools + tools at or below `max_tool_level`
/// (excluding Dangerous-level tools, which are never allowed through this path).
pub(crate) fn filter_tools_by_skill_allowed(
    tools: Vec<ToolDefinition>,
    max_tool_level: types::tool::PermissionLevel,
) -> Vec<ToolDefinition> {
    tools
        .into_iter()
        .filter(|t| {
            if ALWAYS_AVAILABLE_WITH_SKILL.contains(&t.name.as_str()) {
                return true;
            }
            let level = tool_permission_level(&t.name);
            level <= max_tool_level && level != types::tool::PermissionLevel::Dangerous
        })
        .collect()
}

/// Get the permission level for a tool by name.
/// Delegates to the centralized `PermissionLevel::for_tool()`.
pub(crate) fn tool_permission_level(name: &str) -> types::tool::PermissionLevel {
    types::tool::PermissionLevel::for_tool(name)
}

/// Filter tools by a channel's maximum permission level.
/// Tools exceeding the max are removed — the LLM never sees them.
pub(crate) fn filter_tools_by_channel_permission(
    tools: Vec<ToolDefinition>,
    max_permission: types::tool::PermissionLevel,
) -> Vec<ToolDefinition> {
    // If max is Dangerous (the default), no filtering needed
    if max_permission == types::tool::PermissionLevel::Dangerous {
        return tools;
    }
    tools
        .into_iter()
        .filter(|t| tool_permission_level(&t.name) <= max_permission)
        .collect()
}

/// Build tool definitions for delegate_{name} tools from subagent configs.
/// Each subagent becomes a single tool the parent agent can call to delegate work.
pub(crate) fn build_subagent_tool_definitions(subagents: &[SubagentConfig]) -> Vec<ToolDefinition> {
    subagents
        .iter()
        .map(|sa| ToolDefinition {
            name: format!("delegate_{}", sa.name),
            description: format!(
                "Delegate to the '{}' subagent. {} Use this tool when the task involves: {}",
                sa.name, sa.description, sa.trigger
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": {
                        "type": "string",
                        "description": format!("The task or message to delegate to the {} subagent", sa.name)
                    }
                },
                "required": ["message"]
            }),
        })
        .collect()
}

impl CarrierKernel {
    /// Collect the tools available to an agent using toolset mode.
    ///
    /// Always shows core tools + tool_search. Additional tools come ONLY from
    /// session.active_toolsets (populated by skill auto-match + tool_search on-demand).
    /// No auto-loading from manifest fields — skills declare toolsets, tool_search discovers the rest.
    pub(crate) fn available_tools(&self, agent_id: AgentId, active_toolsets: Option<&[String]>) -> Vec<ToolDefinition> {
        let all_builtins = runtime::tool_runner::builtin_tool_definitions();
        let entry = self.registry.get(agent_id);

        // Only use session-level active_toolsets (from skill match + tool_search)
        let combined: Vec<String> = active_toolsets
            .map(|s| s.to_vec())
            .unwrap_or_default();

        // Core tools always visible
        let mut tools: Vec<ToolDefinition> = all_builtins
            .iter()
            .filter(|t| CORE_TOOLS.contains(&t.name.as_str()))
            .cloned()
            .collect();

        // Add tools from each active toolset
        if let Ok(registry) = self.plugins.toolset_registry.read() {
            tracing::info!(
                active_toolsets = ?combined,
                registry_keys = ?registry.keys().collect::<Vec<_>>(),
                "available_tools: resolving active toolsets"
            );
            for ts_name in &combined {
                if let Some(toolset_tools) = registry.get(ts_name) {
                    tracing::info!(
                        toolset = %ts_name,
                        tools = ?toolset_tools.iter().map(|t| &t.name).collect::<Vec<_>>(),
                        "available_tools: toolset resolved"
                    );
                    tools.extend(toolset_tools.iter().cloned());
                } else {
                    tracing::warn!(toolset = %ts_name, "available_tools: toolset not found in registry");
                }
            }
        }

        // Apply tool_allowlist / tool_blocklist
        let (tool_allowlist, tool_blocklist) = entry
            .as_ref()
            .map(|e| (e.manifest.tool_allowlist.clone(), e.manifest.tool_blocklist.clone()))
            .unwrap_or_default();
        if !tool_allowlist.is_empty() {
            tools.retain(|t| tool_allowlist.iter().any(|a| a == &t.name));
        }
        if !tool_blocklist.is_empty() {
            tools.retain(|t| !tool_blocklist.iter().any(|b| b == &t.name));
        }

        // Remove shell_exec if exec_policy denies it
        let exec_blocks_shell = entry.as_ref().is_some_and(|e| {
            e.manifest
                .exec_policy
                .as_ref()
                .is_some_and(|p| p.mode == types::config::ExecSecurityMode::Deny)
        });
        if exec_blocks_shell {
            tools.retain(|t| t.name != "shell_exec");
        }

        // Filter by agent's max_tool_level — remove tools above the allowed level
        let max_level = entry
            .as_ref()
            .map(|e| e.manifest.max_tool_level)
            .unwrap_or(types::tool::PermissionLevel::Write);
        tools.retain(|t| {
            let level = types::tool::PermissionLevel::for_tool(&t.name);
            level <= max_level
        });

        // Add delegate_{name} tools for each subagent in the manifest
        if let Some(ref e) = entry {
            if !e.manifest.subagents.is_empty() {
                tools.extend(build_subagent_tool_definitions(&e.manifest.subagents));
            }
        }

        tools
    }

    /// Build the toolset registry from builtin modules and MCP tools.
    /// Must be called after MCP connections are established.
    pub(crate) fn build_toolset_registry(&self) {
        let mut registry: std::collections::HashMap<String, Vec<ToolDefinition>> =
            std::collections::HashMap::new();

        // Group builtin tools by toolset
        let all_builtins = runtime::tool_runner::builtin_tool_definitions();
        for tool in &all_builtins {
            if let Some(ts_name) = tool_to_toolset(&tool.name) {
                registry
                    .entry(ts_name.to_string())
                    .or_default()
                    .push(tool.clone());
            }
        }

        // Group MCP tools by server
        if let Ok(mcp_tools) = self.plugins.mcp_tools.lock() {
            let known_names: Vec<String> = self
                .plugins
                .mcp_connections
                .iter()
                .map(|e| e.value().name().to_string())
                .collect();
            let known_refs: Vec<&str> = known_names.iter().map(|s| s.as_str()).collect();

            for tool in mcp_tools.iter() {
                if let Some(server) =
                    runtime::mcp::extract_mcp_server_from_known(&tool.name, &known_refs)
                {
                    registry
                        .entry(server.to_string())
                        .or_default()
                        .push(tool.clone());
                }
            }
        }

        tracing::info!(
            toolset_count = registry.len(),
            toolsets = ?registry.keys().collect::<Vec<_>>(),
            "Built toolset registry"
        );

        if let Ok(mut reg) = self.plugins.toolset_registry.write() {
            *reg = registry;
        }
    }

    /// Build a compact toolset summary for the system prompt.
    fn build_toolset_summary(
        &self,
        active_toolsets: &[String],
    ) -> String {
        let registry = match self.plugins.toolset_registry.read() {
            Ok(r) => r.clone(),
            Err(_) => return String::new(),
        };
        if registry.is_empty() {
            return String::new();
        }

        let mut summary = String::from(
            "\n\n--- Toolsets ---\nTools listed as ACTIVE are already in your tool list — use them directly.\nTools listed as available can be loaded by calling tool_search(\"query\").\n\n",
        );

        // Sort: builtins first, MCP servers last
        let mut entries: Vec<_> = registry.iter().collect();
        entries.sort_by_key(|(name, _)| {
            if BUILTIN_TOOLSETS.contains(&name.as_str()) {
                0
            } else {
                1
            }
        });

        for (name, tools) in &entries {
            let is_active = active_toolsets.contains(name);
            let status = if is_active { "ACTIVE" } else { "available" };

            let examples: Vec<&str> = tools
                .iter()
                .take(3)
                .map(|t| {
                    let prefix = format!("mcp_{}_", name);
                    t.name.strip_prefix(&prefix).unwrap_or(&t.name)
                })
                .collect();
            let example_str = if tools.len() > 3 {
                format!(
                    "{}, ... ({} total)",
                    examples.join(", "),
                    tools.len()
                )
            } else {
                examples.join(", ")
            };

            summary.push_str(&format!(
                "- {} [{}]: {} tools ({})\n",
                name,
                status,
                tools.len(),
                example_str
            ));
        }

        // Filesystem MCP guidance
        if registry.keys().any(|s| s.contains("filesystem")) {
            summary.push_str(
                "IMPORTANT: For accessing files OUTSIDE your workspace directory, you MUST use \
                 the MCP filesystem tools (e.g. mcp_filesystem_read_file, mcp_filesystem_list_directory) \
                 instead of the built-in file_read/file_list/file_write tools, which are restricted to \
                 the workspace. The MCP filesystem server has been granted access to specific directories \
                 by the user.\n",
            );
        }

        summary
    }

    /// Build PromptContext and apply it to the manifest's system prompt.
    /// Shared between streaming and non-streaming message paths.
    #[allow(clippy::too_many_arguments)]
    /// Format a millisecond timestamp for display in memory hits.
    fn format_time_ms(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms)
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| ms.to_string())
    }

    /// Prefetch 7-day global digest for prompt injection.
    ///
    /// Queries the global tree for recent summaries and formats them
    /// as TreeMemoryHit entries for the prompt builder. Non-fatal on failure.
    fn prefetch_tree_memories(&self, owner_id: &str) -> Vec<runtime::prompt_builder::TreeMemoryHit> {
        use types::memory_tree::GlobalQuery;

        let req = GlobalQuery {
            owner_id,
            time_window_days: Some(7),
            query: None,
            limit: 3,
        };

        match self.memory.tree_query_global(&req) {
            Ok(resp) => resp
                .hits
                .iter()
                .take(3)
                .map(|h| runtime::prompt_builder::TreeMemoryHit {
                    scope: h.tree_scope.clone(),
                    kind: h.tree_kind.to_string(),
                    content: h.content.chars().take(500).collect(),
                    time_range: format!("{} — {}", Self::format_time_ms(h.time_range_start_ms), Self::format_time_ms(h.time_range_end_ms)),
                })
                .collect(),
            Err(e) => {
                tracing::debug!("Tree memory prefetch failed (non-fatal): {e}");
                Vec::new()
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn build_and_apply_prompt(
        &self,
        agent_id: &AgentId,
        manifest: &mut AgentManifest,
        tools: &[types::tool::ToolDefinition],
        sender_id: &Option<String>,
        sender_name: Option<String>,
        owner_id: &Option<String>,
        auto_matched_skill: Option<String>,
    ) {
        // Read user_name from the agent's KV namespace (per-sender memory)
        let sid = sender_id.as_deref().unwrap_or("");
        let oid = owner_id.as_deref().unwrap_or(sid);
        let user_name = self
            .memory
            .system_kv_get(*agent_id, sid, sid, "user_name")
            .ok()
            .flatten()
            .and_then(|v| v.as_str().map(String::from))
            .or_else(|| sender_name.clone());

        let peer_agents: Vec<(String, String, String)> = self
            .registry
            .list()
            .iter()
            .map(|a| {
                (
                    a.name.clone(),
                    format!("{:?}", a.state),
                    a.manifest.model.modality.clone(),
                )
            })
            .collect();

        // Load session for toolset summary
        let entry_ref = self.registry.get(*agent_id);
        let session = entry_ref
            .as_ref()
            .and_then(|e| self.memory.get_session(e.session_id).ok().flatten());
        let active = session
            .as_ref()
            .map(|s| s.active_toolsets.clone())
            .unwrap_or_default();

        let prompt_ctx = runtime::prompt_builder::PromptContext {
            agent_name: manifest.name.clone(),
            agent_description: manifest.description.clone(),
            base_system_prompt: manifest.model.system_prompt.clone(),
            granted_tools: tools.iter().map(|t| t.name.clone()).collect(),
            recalled_memories: vec![],
            tree_memories: self.prefetch_tree_memories(oid),
            skill_summary: String::new(),
            skill_prompt_context: String::new(),
            mcp_summary: self.build_toolset_summary(
                &active,
            ),
            workspace_path: manifest.workspace.as_ref().map(|p| p.display().to_string()),
            soul_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "SOUL.md")),
            user_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "USER.md")),
            memory_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "MEMORY.md")),
            user_name,
            channel_type: None,
            is_subagent: manifest
                .metadata
                .get("is_subagent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            is_autonomous: manifest.autonomous.is_some(),
            agents_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "AGENTS.md")),
            bootstrap_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "BOOTSTRAP.md")),
            workspace_context: manifest.workspace.as_ref().map(|w| {
                let mut ws_ctx = runtime::workspace_context::WorkspaceContext::detect(w);
                ws_ctx.build_context_section()
            }),
            identity_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "IDENTITY.md")),
            heartbeat_md: if manifest.autonomous.is_some() {
                manifest
                    .workspace
                    .as_ref()
                    .and_then(|w| read_identity_file(w, "HEARTBEAT.md"))
            } else {
                None
            },
            peer_agents,
            current_date: Some(
                chrono::Local::now()
                    .format("%A, %B %d, %Y (%Y-%m-%d %H:%M %Z)")
                    .to_string(),
            ),
            sender_id: sender_id.clone(),
            sender_name,
            user_profile_summary: sender_id.as_ref().and_then(|sid| {
                read_user_profile_summary(&self.config.home_dir, oid, &manifest.name, Some(sid))
            }),
            clone_system_prompt_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_identity_file(w, "system_prompt.md")),
            clone_skills_catalog: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_skills_catalog(w)),
            clone_style_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_style_samples(w)),
            clone_skills_prompts: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_workspace_skills_prompts(w)),
            knowledge_content: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_knowledge_content(w, Some(oid), sender_id.as_deref(), Some(&self.config.home_dir), Some(&manifest.name))),
            clone_agents_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_agents_directory(w)),
            evolution_rules_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_evolution_rules(w)),
            auto_matched_skill,
        };
        manifest.model.system_prompt =
            runtime::prompt_builder::build_system_prompt(&prompt_ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn td(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        }
    }

    #[test]
    fn filter_keeps_always_available() {
        let tools = vec![td("session_summarize"), td("knowledge_read"), td("knowledge_list"), td("shell_exec")];
        let filtered = filter_tools_by_skill_allowed(tools, types::tool::PermissionLevel::Write);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"session_summarize"));
        assert!(names.contains(&"knowledge_read"));
        assert!(names.contains(&"knowledge_list"));
        assert!(!names.contains(&"shell_exec"));
    }

    #[test]
    fn filter_all_none_level_tools_pass() {
        let tools = vec![td("tool_search"), td("skill_load"), td("session_summarize"), td("web_search")];
        let filtered = filter_tools_by_skill_allowed(tools, types::tool::PermissionLevel::Write);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"tool_search"));
        assert!(names.contains(&"skill_load"));
        assert!(names.contains(&"session_summarize"));
        assert!(names.contains(&"web_search"));
    }

    #[test]
    fn filter_by_level_write() {
        let tools = vec![
            td("session_summarize"),
            td("tool_search"),
            td("file_read"),
            td("file_write"),
            td("process_start"),
            td("shell_exec"),
        ];
        let filtered = filter_tools_by_skill_allowed(tools, types::tool::PermissionLevel::Write);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"session_summarize"));
        assert!(names.contains(&"tool_search"));
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"file_write"));
        assert!(!names.contains(&"process_start"));
        assert!(!names.contains(&"shell_exec"));
    }

    #[test]
    fn filter_by_level_execute() {
        let tools = vec![td("file_write"), td("process_start"), td("shell_exec")];
        let filtered = filter_tools_by_skill_allowed(tools, types::tool::PermissionLevel::Execute);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"file_write"));
        assert!(names.contains(&"process_start"));
        assert!(!names.contains(&"shell_exec"));
    }

    #[test]
    fn dangerous_never_passes() {
        let tools = vec![td("shell_exec"), td("process_kill"), td("agent_kill")];
        let filtered = filter_tools_by_skill_allowed(tools, types::tool::PermissionLevel::Dangerous);
        assert!(filtered.is_empty());
    }
}
