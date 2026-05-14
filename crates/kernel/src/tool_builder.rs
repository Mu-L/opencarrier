//! Tool resolution and prompt building — available_tools, toolset registry, system prompt.
//!
//! Assembles the tool set for each agent request using the toolset model:
//! - Core tools (always visible): memory_store, memory_recall, session_summarize, use_toolset
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

/// Tool names that are always visible (core tools).
const CORE_TOOLS: &[&str] = &["memory_store", "memory_recall", "session_summarize", "use_toolset"];

/// Map a builtin tool name to its toolset. Returns None for core tools.
fn tool_to_toolset(name: &str) -> Option<&'static str> {
    match name {
        "memory_store" | "memory_recall" | "session_summarize" | "use_toolset" => None,
        n if n.starts_with("file_") => Some("filesystem"),
        "shell_exec" => Some("shell"),
        n if n.starts_with("knowledge_") || n.starts_with("skill_") || n == "clone_evaluate" => Some("knowledge"),
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
const BUILTIN_TOOLSETS: &[&str] = &["filesystem", "shell", "knowledge", "media", "web", "agent", "misc"];

impl CarrierKernel {
    /// Collect the tools available to an agent using toolset mode.
    ///
    /// Always shows core tools + use_toolset. Additional tools come from
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

        // When auto_load_toolsets is empty but capabilities.tools is non-empty
        // (legacy clones), auto-derive toolsets from declared tool names + mcp_servers
        if combined.is_empty() {
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
                    // mcp_servers in manifest are also toolsets
                    for server in &e.manifest.mcp_servers {
                        if !combined.contains(server) {
                            combined.push(server.clone());
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
            "\n\n--- Toolsets ---\nYou can activate additional tools by calling use_toolset(\"name\").\n\n",
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
    pub(crate) fn build_and_apply_prompt(
        &self,
        agent_id: &AgentId,
        manifest: &mut AgentManifest,
        tools: &[types::tool::ToolDefinition],
        sender_id: &Option<String>,
        sender_name: Option<String>,
        owner_id: &Option<String>,
    ) {
        // Read user_name from the agent's KV namespace (per-sender memory)
        let sid = sender_id.as_deref().unwrap_or("");
        let oid = owner_id.as_deref().unwrap_or(sid);
        let user_name = self
            .memory
            .structured_get(*agent_id, sid, sid, "user_name")
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
        };
        manifest.model.system_prompt =
            runtime::prompt_builder::build_system_prompt(&prompt_ctx);
    }
}
