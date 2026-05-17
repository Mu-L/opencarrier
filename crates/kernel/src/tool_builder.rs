//! Tool resolution and prompt building — available_tools, toolset registry, system prompt.
//!
//! Assembles the tool set for each agent request using the toolset model:
//! - Core tools (always visible): session_summarize, tool_search
//! - Toolsets (on-demand): filesystem, shell, knowledge, media, misc, web, agent, + MCP servers
//! - Toolset activation: auto_load_toolsets (manifest) + active_toolsets (session-level)

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
    "memory_recall", "memory_list", "memory_search_entities",
];

/// Map a builtin tool name to its toolset. Returns None for core tools.
pub(crate) fn tool_to_toolset(name: &str) -> Option<&'static str> {
    match name {
        "session_summarize"
        | "tool_search"
        | "skill_load"
        | "knowledge_read" | "knowledge_list"
        | "cron_create" | "cron_list" | "cron_cancel"
        | "memory_recall" | "memory_list" | "memory_search_entities" => None,
        n if n.starts_with("file_") => Some("filesystem"),
        "shell_exec" => Some("shell"),
        n if n.starts_with("knowledge_") || n.starts_with("skill_") || n == "clone_evaluate" => Some("knowledge"),
        n if n.starts_with("memory_") => Some("memory"),
        n if n.starts_with("media_") || n.starts_with("image_") || n == "text_to_speech" || n == "speech_to_text" => Some("media"),
        n if n.starts_with("web_") => Some("web"),
        n if n.starts_with("agent_") || n.starts_with("train_") => Some("agent"),
        n if n.starts_with("location_") || n.starts_with("system_") || n == "user_profile" => Some("misc"),
        n if n.starts_with("docker_exec") || n.starts_with("process_") => Some("media"),
        "apply_patch" => Some("filesystem"),
        _ => Some("misc"),
    }
}

/// Builtin toolset names (used to distinguish from MCP toolsets).
const BUILTIN_TOOLSETS: &[&str] = &["filesystem", "shell", "knowledge", "memory", "media", "web", "agent", "misc"];

/// Tools that remain available even when a skill restricts the tool list.
/// These are foundational: the agent must always be able to summarize
/// state and look up its own knowledge base. `tool_search` and `skill_load`
/// are deliberately EXCLUDED so the LLM can't escape the skill-imposed scope.
pub(crate) const ALWAYS_AVAILABLE_WITH_SKILL: &[&str] = &[
    "session_summarize",
    "knowledge_read",
    "knowledge_list",
    "memory_recall",
    "memory_search_entities",
];

/// Match a tool name against a skill `allowed_tools` pattern.
/// Supports exact match (`file_write`) and suffix wildcard (`mcp_wechat_oa_*`).
pub(crate) fn matches_skill_pattern(tool_name: &str, pattern: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        tool_name.starts_with(prefix)
    } else {
        tool_name == pattern
    }
}

/// Filter a tool list down to the union of:
/// - tools matching any pattern in `allowed_patterns` (with `*` suffix wildcard)
/// - the `ALWAYS_AVAILABLE_WITH_SKILL` foundation set
///
/// A pattern of `"*"` alone disables filtering (keeps all tools).
pub(crate) fn filter_tools_by_skill_allowed(
    tools: Vec<ToolDefinition>,
    allowed_patterns: &[String],
) -> Vec<ToolDefinition> {
    if allowed_patterns.is_empty() || allowed_patterns.iter().any(|p| p == "*") {
        return tools;
    }
    tools
        .into_iter()
        .filter(|t| {
            ALWAYS_AVAILABLE_WITH_SKILL.contains(&t.name.as_str())
                || allowed_patterns
                    .iter()
                    .any(|p| matches_skill_pattern(&t.name, p))
        })
        .collect()
}

/// Get the permission level for a tool by name.
/// Uses the builtin module dispatch to look up each tool's declared level.
/// Falls back to Dangerous for unknown tools (fail-safe).
fn tool_permission_level(name: &str) -> types::tool::PermissionLevel {
    use types::tool::PermissionLevel;
    match name {
        // None — pure queries, no side effects
        "session_summarize"
        | "knowledge_list" | "knowledge_read" | "skill_load"
        | "tool_search" | "agent_find" | "agent_list"
        | "train_read" | "train_list" | "train_knowledge_list"
        | "train_knowledge_read" | "train_evaluate" | "user_profile"
        | "task_list" | "schedule_list" | "cron_list"
        | "a2a_discover" | "clone_evaluate"
        | "knowledge_lint" | "knowledge_index" | "knowledge_extract"
        | "train_knowledge_lint"
        | "memory_recall" | "memory_list" | "memory_query_topic"
        | "memory_search_entities" | "memory_drill_down"
        | "memory_fetch_leaves" => PermissionLevel::None,

        // ReadOnly — reads from external sources
        "file_read" | "file_list" | "file_convert"
        | "web_fetch" | "web_search"
        | "image_analyze" | "media_describe" | "media_transcribe"
        | "speech_to_text" | "location_get" | "system_time" => PermissionLevel::ReadOnly,

        // Write — writes within sandbox
        "file_write"
        | "knowledge_add" | "knowledge_remove" | "knowledge_import"
        | "knowledge_heal" | "skill_create" | "skill_update"
        | "apply_patch" | "train_write"
        | "image_generate" | "text_to_speech" | "canvas_present"
        | "task_post" | "task_claim" | "task_complete"
        | "event_publish" | "schedule_create" | "schedule_delete"
        | "cron_create" | "cron_cancel"
        | "memory_ingest" => PermissionLevel::Write,

        // Execute — cross-boundary writes
        "docker_exec" | "process_start" | "process_poll"
        | "process_write" | "process_list"
        | "agent_send" | "agent_spawn" | "agent_restart"
        | "a2a_send" => PermissionLevel::Execute,

        // Subagent delegation — delegate_* tools are Execute level
        n if n.starts_with("delegate_") => PermissionLevel::Execute,

        // Dangerous — irreversible operations
        "shell_exec" | "process_kill" | "agent_kill" => PermissionLevel::Dangerous,

        // Unknown tools default to Dangerous (fail-safe)
        _ => PermissionLevel::Dangerous,
    }
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
    /// Always shows core tools + tool_search. Additional tools come from
    /// auto_load_toolsets (manifest) + active_toolsets (session-level).
    pub(crate) fn available_tools(&self, agent_id: AgentId, active_toolsets: Option<&[String]>) -> Vec<ToolDefinition> {
        let all_builtins = runtime::tool_runner::builtin_tool_definitions();
        let entry = self.registry.get(agent_id);

        // Merge auto_load + active toolsets
        let auto_load = entry
            .as_ref()
            .map(|e| e.manifest.auto_load_toolsets.clone())
            .unwrap_or_default();
        let active: Vec<String> = active_toolsets
            .map(|s| s.to_vec())
            .unwrap_or_default();
        let mut combined = auto_load;
        for ts in &active {
            if !combined.contains(ts) {
                combined.push(ts.clone());
            }
        }

        // Auto-derive toolsets from capabilities.tools — always runs so that
        // agents declaring tools in capabilities get them loaded regardless of
        // auto_load_toolsets. Follows Claude's approach: with <50 tools, load
        // everything upfront for better accuracy and no search round-trips.
        if let Some(ref e) = entry {
            let declared = &e.manifest.capabilities.tools;
            if !declared.is_empty() && !declared.iter().any(|t| t == "*") {
                for tool_name in declared {
                    if let Some(ts) = tool_to_toolset(tool_name) {
                        let ts_str = ts.to_string();
                        if !combined.contains(&ts_str) {
                            combined.push(ts_str);
                        }
                    }
                    // MCP tool names: extract server and activate that toolset
                    if let Some(server) = runtime::mcp::extract_mcp_server_from_known(
                        tool_name,
                        BUILTIN_TOOLSETS,
                    ) {
                        let ts_str = server.to_string();
                        if !combined.contains(&ts_str) {
                            combined.push(ts_str);
                        }
                    }
                }
            }
        }

        // Auto-activate toolsets for tools in whitelist_tools
        let whitelist = &self.config.whitelist_tools;
        if !whitelist.is_empty() {
            for tool_name in whitelist {
                if let Some(ts) = tool_to_toolset(tool_name) {
                    if !combined.contains(&ts.to_string()) {
                        combined.push(ts.to_string());
                    }
                }
                // MCP whitelist tools: extract server name and activate that toolset
                if let Some(server) = runtime::mcp::extract_mcp_server_from_known(
                    tool_name,
                    BUILTIN_TOOLSETS,
                ) {
                    let ts_name = server.to_string();
                    if !combined.contains(&ts_name) {
                        combined.push(ts_name);
                    }
                }
            }
        }

        // All agents need knowledge toolset to read their own knowledge base
        if !combined.contains(&"knowledge".to_string()) {
            combined.push("knowledge".to_string());
        }

        // Core tools always visible
        let mut tools: Vec<ToolDefinition> = all_builtins
            .iter()
            .filter(|t| CORE_TOOLS.contains(&t.name.as_str()))
            .cloned()
            .collect();

        // Add tools from each active/auto toolset
        if let Ok(registry) = self.plugins.toolset_registry.read() {
            for ts_name in &combined {
                if let Some(toolset_tools) = registry.get(ts_name) {
                    tools.extend(toolset_tools.iter().cloned());
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

        // Add whitelist tools that aren't already included (covers edge cases
        // like MCP whitelist tools whose server toolset isn't auto-activated)
        if !whitelist.is_empty() {
            let existing_names: std::collections::HashSet<String> =
                tools.iter().map(|t| t.name.clone()).collect();
            // Check builtins
            let all_defs = runtime::tool_runner::builtin_tool_definitions();
            for def in &all_defs {
                if whitelist.iter().any(|w| w == &def.name)
                    && !existing_names.contains(&def.name)
                {
                    tools.push(def.clone());
                }
            }
            // Check MCP tools
            if let Ok(mcp_tools) = self.plugins.mcp_tools.lock() {
                for def in mcp_tools.iter() {
                    if whitelist.iter().any(|w| w == &def.name)
                        && !existing_names.contains(&def.name)
                    {
                        tools.push(def.clone());
                    }
                }
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
        active_toolsets: &[String],
        auto_load_toolsets: &[String],
        mcp_allowlist: &[String],
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
            // Filter by mcp_allowlist for MCP toolsets
            let is_builtin = BUILTIN_TOOLSETS.contains(&name.as_str());
            if !is_builtin && !mcp_allowlist.is_empty() {
                let normalized = runtime::mcp::normalize_name(name);
                if !mcp_allowlist
                    .iter()
                    .any(|a| runtime::mcp::normalize_name(a) == normalized)
                {
                    continue;
                }
            }

            let is_active = active_toolsets.contains(name) || auto_load_toolsets.contains(name);
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
            tree_memories: vec![],
            skill_summary: String::new(),
            skill_prompt_context: String::new(),
            mcp_summary: self.build_toolset_summary(
                &active,
                &manifest.auto_load_toolsets,
                &manifest.mcp_servers,
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
    fn skill_pattern_exact_match() {
        assert!(matches_skill_pattern("file_write", "file_write"));
        assert!(!matches_skill_pattern("file_write", "file_read"));
        assert!(!matches_skill_pattern("file_writer", "file_write"));
    }

    #[test]
    fn skill_pattern_suffix_wildcard() {
        assert!(matches_skill_pattern("mcp_wechat_oa_publish", "mcp_wechat_oa_*"));
        assert!(matches_skill_pattern("mcp_wechat_oa_", "mcp_wechat_oa_*"));
        assert!(!matches_skill_pattern("mcp_feishu_publish", "mcp_wechat_oa_*"));
        assert!(matches_skill_pattern("web_search", "web_*"));
        assert!(matches_skill_pattern("web_fetch", "web_*"));
    }

    #[test]
    fn filter_keeps_always_available() {
        let tools = vec![td("session_summarize"), td("knowledge_read"), td("knowledge_list"), td("shell_exec")];
        let allowed = vec!["web_search".to_string()]; // doesn't match any tool
        let filtered = filter_tools_by_skill_allowed(tools, &allowed);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"session_summarize"));
        assert!(names.contains(&"knowledge_read"));
        assert!(names.contains(&"knowledge_list"));
        assert!(!names.contains(&"shell_exec"));
    }

    #[test]
    fn filter_keeps_allowed_tools() {
        let tools = vec![
            td("web_search"),
            td("web_fetch"),
            td("file_write"),
            td("file_delete"),
            td("shell_exec"),
        ];
        let allowed = vec!["web_search".to_string(), "web_fetch".to_string(), "file_write".to_string()];
        let filtered = filter_tools_by_skill_allowed(tools, &allowed);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"web_search"));
        assert!(names.contains(&"web_fetch"));
        assert!(names.contains(&"file_write"));
        assert!(!names.contains(&"file_delete"));
        assert!(!names.contains(&"shell_exec"));
    }

    #[test]
    fn filter_keeps_wildcard_matches() {
        let tools = vec![
            td("mcp_wechat_oa_publish"),
            td("mcp_wechat_oa_draft"),
            td("mcp_feishu_send"),
            td("file_write"),
        ];
        let allowed = vec!["mcp_wechat_oa_*".to_string(), "file_write".to_string()];
        let filtered = filter_tools_by_skill_allowed(tools, &allowed);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"mcp_wechat_oa_publish"));
        assert!(names.contains(&"mcp_wechat_oa_draft"));
        assert!(names.contains(&"file_write"));
        assert!(!names.contains(&"mcp_feishu_send"));
    }

    #[test]
    fn filter_star_disables_filtering() {
        let tools = vec![td("shell_exec"), td("file_delete"), td("web_search")];
        let allowed = vec!["*".to_string()];
        let filtered = filter_tools_by_skill_allowed(tools, &allowed);
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn filter_excludes_tool_search_when_skill_active() {
        // Critical: tool_search and skill_load are NOT in ALWAYS_AVAILABLE_WITH_SKILL.
        // When a skill is matched, the LLM must stay within its allowed_tools scope.
        let tools = vec![td("tool_search"), td("skill_load"), td("session_summarize"), td("web_search")];
        let allowed = vec!["web_search".to_string()];
        let filtered = filter_tools_by_skill_allowed(tools, &allowed);
        let names: Vec<&str> = filtered.iter().map(|t| t.name.as_str()).collect();
        assert!(!names.contains(&"tool_search"));
        assert!(!names.contains(&"skill_load"));
        assert!(names.contains(&"session_summarize"));
        assert!(names.contains(&"web_search"));
    }
}
