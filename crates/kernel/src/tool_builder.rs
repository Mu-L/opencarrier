//! Tool resolution and prompt building — available_tools, toolset registry, system prompt.
//!
//! Assembles the tool set for each agent request using the toolset model:
//! - Core tools (always visible): session_summarize, tool_search, etc.
//! - Toolsets (on-demand): filesystem, shell, knowledge, media, misc, web, agent, + MCP servers
//! - Toolset resolution: skill-declared toolsets queried directly from registry at prompt-build time

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
/// All other tools are loaded on-demand via tool_search.
const CORE_TOOLS: &[&str] = &[
    "session_summarize",
    "tool_search",
    "skill_load",
    "knowledge_read", "knowledge_list",
    "file_read", "file_list",
    "cron_create", "cron_list", "cron_cancel",
    "memory_tree",
    "task_plan",
];

/// Map a builtin tool name to its toolset. Returns None for core tools.
pub(crate) fn tool_to_toolset(name: &str) -> Option<&'static str> {
    match name {
        "session_summarize"
        | "tool_search"
        | "skill_load"
        | "knowledge_read" | "knowledge_list"
        | "file_read" | "file_list"
        | "cron_create" | "cron_list" | "cron_cancel"
        | "memory_tree"
        | "task_plan" => None,
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

/// Get the permission level for a tool by name.
/// Delegates to the centralized `PermissionLevel::for_tool()`.
pub(crate) fn tool_permission_level(name: &str) -> types::tool::PermissionLevel {
    types::tool::PermissionLevel::for_tool(name)
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
    /// Collect tool definitions for an agent request.
    /// Returns: core tools + skill-declared individual tools + delegate tools.
    /// No filtering — all definitions are sent to the LLM. Permission checks
    /// happen at execution time in execute_tool().
    pub(crate) fn available_tools(
        &self,
        agent_id: AgentId,
        skill_tools: Option<&[String]>,
    ) -> Vec<ToolDefinition> {
        let entry = self.registry.get(agent_id);

        // Core tools always included
        let mut tools: Vec<ToolDefinition> = runtime::tool_runner::builtin_tool_definitions()
            .into_iter()
            .filter(|t| CORE_TOOLS.contains(&t.name.as_str()))
            .collect();

        // Add individual tools declared by the skill (looked up by name from registry)
        if let Some(tool_names) = skill_tools {
            if let Ok(registry) = self.plugins.toolset_registry.read() {
                let existing_names: std::collections::HashSet<&str> =
                    tools.iter().map(|t| t.name.as_str()).collect();
                let mut found_tools: Vec<ToolDefinition> = Vec::new();
                for tool_name in tool_names {
                    if existing_names.contains(tool_name.as_str()) {
                        continue;
                    }
                    if found_tools.iter().any(|t| t.name == *tool_name) {
                        continue;
                    }
                    for (_, toolset_tools) in registry.iter() {
                        if let Some(found) = toolset_tools.iter().find(|t| t.name == *tool_name) {
                            found_tools.push(found.clone());
                            break;
                        }
                    }
                }
                tools.extend(found_tools);
            }
        }

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
        active_tools: &[String],
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
            // A toolset is "ACTIVE" if any of its tools are in the active list
            let is_active = tools.iter().any(|t| active_tools.contains(&t.name));
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
        skill_tools: Option<Vec<String>>,
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

        let active = skill_tools.unwrap_or_default();

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
