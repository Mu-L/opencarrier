//! Tool resolution and prompt building — available_tools, MCP summary, system prompt.
//!
//! Assembles the tool set for each agent request: builtin tools filtered by
//! profile/allowlist/blocklist, MCP tools, plugin tools, and whitelist additions.
//! Also builds the structured system prompt via `PromptContext`.

use crate::kernel::CarrierKernel;
use crate::prompt_sources::{
    read_agents_directory, read_identity_file, read_knowledge_content, read_skills_catalog,
    read_style_samples, read_user_profile_summary, read_workspace_skills_prompts,
};
use carrier_types::agent::*;
use carrier_types::capability::Capability;
use carrier_types::tool::ToolDefinition;

impl CarrierKernel {
    /// Collect the tools available to an agent based on its profile and allowlists.
    ///
    /// If `capabilities.tools` is empty (or contains `"*"`), all tools are
    /// available (backwards compatible).
    pub(crate) fn available_tools(&self, agent_id: AgentId) -> Vec<ToolDefinition> {
        let all_builtins = carrier_runtime::tool_runner::builtin_tool_definitions();

        // Look up agent entry for profile, skill/MCP allowlists, and declared tools
        let entry = self.registry.get(agent_id);
        let (_skill_allowlist, mcp_allowlist, tool_profile) = entry
            .as_ref()
            .map(|e| {
                (
                    e.manifest.skills.clone(),
                    e.manifest.mcp_servers.clone(),
                    e.manifest.profile.clone(),
                )
            })
            .unwrap_or_default();

        // Extract the agent's declared tool list from capabilities.tools.
        // This is the primary mechanism: only send declared tools to the LLM.
        let declared_tools: Vec<String> = entry
            .as_ref()
            .map(|e| e.manifest.capabilities.tools.clone())
            .unwrap_or_default();

        // Only explicit "*" wildcard means unrestricted.
        // Empty declared_tools + no profile = only whitelist tools.
        let tools_unrestricted = declared_tools.iter().any(|t| t == "*");

        // Step 1: Filter builtin tools.
        // Priority: declared tools > ToolProfile > whitelist only.
        let has_tool_all = entry.as_ref().is_some_and(|_| {
            let caps = self.coordination.capabilities.list(agent_id);
            caps.iter().any(|c| matches!(c, Capability::ToolAll))
        });

        let mut all_tools: Vec<ToolDefinition> = if tools_unrestricted {
            // Explicit "*" — all builtins
            all_builtins
        } else if !declared_tools.is_empty() {
            // Agent declares specific tools — only include matching builtins
            all_builtins
                .into_iter()
                .filter(|t| declared_tools.iter().any(|d| d == &t.name))
                .collect()
        } else {
            // No declared tools — fall back to profile or whitelist only
            match &tool_profile {
                Some(profile)
                    if *profile != ToolProfile::Full && *profile != ToolProfile::Custom =>
                {
                    let allowed = profile.tools();
                    all_builtins
                        .into_iter()
                        .filter(|t| allowed.iter().any(|a| a == "*" || a == &t.name))
                        .collect()
                }
                Some(_) if has_tool_all => all_builtins,
                // No profile, no declared tools, no ToolAll — only whitelist tools
                _ => vec![],
            }
        };

        // Step 3: Add MCP tools (filtered by agent's MCP server allowlist,
        // then by declared tools).
        if let Ok(mcp_tools) = self.plugins.mcp_tools.lock() {
            let mcp_candidates: Vec<ToolDefinition> = if mcp_allowlist.is_empty() {
                mcp_tools.iter().cloned().collect()
            } else {
                let normalized: Vec<String> = mcp_allowlist
                    .iter()
                    .map(|s| carrier_runtime::mcp::normalize_name(s))
                    .collect();
                let known: Vec<&str> = normalized.iter().map(|s| s.as_str()).collect();
                mcp_tools
                    .iter()
                    .filter(|t| {
                        carrier_runtime::mcp::extract_mcp_server_from_known(&t.name, &known)
                            .is_some()
                    })
                    .cloned()
                    .collect()
            };
            // MCP tools are already filtered by mcp_servers allowlist above.
            // Since mcp_servers is an explicit opt-in (like a plugin), all tools
            // from declared servers are included automatically — no need to also
            // list them in capabilities.tools.
            all_tools.extend(mcp_candidates);
        }

        // Step 3.5: Add plugin tools (from dlopen-loaded shared libraries).
        if let Ok(guard) = self.plugins.plugin_tool_dispatcher.lock() {
            if let Some(ref dispatcher) = *guard {
                let plugin_defs = dispatcher.definitions();
                tracing::info!(
                    agent = %agent_id,
                    plugin_tools_total = plugin_defs.len(),
                    declared_tools_count = declared_tools.len(),
                    tools_unrestricted,
                    "Plugin tool filtering"
                );
                let mut matched = 0;
                let mut unmatched = Vec::new();
                for t in &plugin_defs {
                    if !tools_unrestricted && !declared_tools.iter().any(|d| d == &t.name) {
                        unmatched.push(t.name.clone());
                        continue;
                    }
                    matched += 1;
                    all_tools.push(t.clone());
                }
                tracing::info!(
                    agent = %agent_id,
                    matched,
                    unmatched_count = unmatched.len(),
                    "Plugin tool filter result"
                );
                tracing::info!(agent = %agent_id, ?unmatched, "All unmatched plugin tools");
                tracing::info!(agent = %agent_id, ?declared_tools, "Declared tools in manifest");
            }
        }

        // Step 4: Apply per-agent tool_allowlist/tool_blocklist overrides.
        // These are separate from capabilities.tools and act as additional filters.
        let (tool_allowlist, tool_blocklist) = entry
            .as_ref()
            .map(|e| {
                (
                    e.manifest.tool_allowlist.clone(),
                    e.manifest.tool_blocklist.clone(),
                )
            })
            .unwrap_or_default();

        if !tool_allowlist.is_empty() {
            all_tools.retain(|t| tool_allowlist.iter().any(|a| a == &t.name));
        }
        if !tool_blocklist.is_empty() {
            all_tools.retain(|t| !tool_blocklist.iter().any(|b| b == &t.name));
        }

        // Step 5: Remove shell_exec if exec_policy denies it.
        let exec_blocks_shell = entry.as_ref().is_some_and(|e| {
            e.manifest
                .exec_policy
                .as_ref()
                .is_some_and(|p| p.mode == carrier_types::config::ExecSecurityMode::Deny)
        });
        if exec_blocks_shell {
            all_tools.retain(|t| t.name != "shell_exec");
        }

        // Step 6: Union with global whitelist tools.
        // Whitelist tools are always available regardless of declaration.
        let whitelist = &self.config.whitelist_tools;
        if !whitelist.is_empty() {
            let existing_names: std::collections::HashSet<String> =
                all_tools.iter().map(|t| t.name.clone()).collect();
            let all_defs: Vec<ToolDefinition> = carrier_runtime::tool_runner::builtin_tool_definitions();
            let mut added = Vec::new();
            for def in all_defs {
                if whitelist.iter().any(|w| w == &def.name) && !existing_names.contains(&def.name) {
                    added.push(def.name.clone());
                    all_tools.push(def);
                }
            }
            // Also check MCP tools and plugin tools for whitelist matches
            if let Ok(mcp_tools) = self.plugins.mcp_tools.lock() {
                for def in mcp_tools.iter() {
                    if whitelist.iter().any(|w| w == &def.name)
                        && !existing_names.contains(&def.name)
                        && !added.iter().any(|a| a == &def.name)
                    {
                        added.push(def.name.clone());
                        all_tools.push(def.clone());
                    }
                }
            }
            if let Ok(guard) = self.plugins.plugin_tool_dispatcher.lock() {
                if let Some(ref dispatcher) = *guard {
                    for def in dispatcher.definitions() {
                        if whitelist.iter().any(|w| w == &def.name)
                            && !existing_names.contains(&def.name)
                            && !added.iter().any(|a| a == &def.name)
                        {
                            added.push(def.name.clone());
                            all_tools.push(def.clone());
                        }
                    }
                }
            }
            if !added.is_empty() {
                tracing::info!(
                    agent = %agent_id,
                    ?added,
                    "Whitelist tools added"
                );
            }
        } else {
            tracing::warn!(agent = %agent_id, "whitelist_tools is empty in config");
        }

        all_tools
    }

    /// Build a compact MCP server/tool summary for the system prompt so the
    /// agent knows what external tool servers are connected.
    fn build_mcp_summary(&self, mcp_allowlist: &[String]) -> String {
        let tools = match self.plugins.mcp_tools.lock() {
            Ok(t) => t.clone(),
            Err(_) => return String::new(),
        };
        if tools.is_empty() {
            return String::new();
        }

        // Normalize allowlist for matching
        let normalized: Vec<String> = mcp_allowlist
            .iter()
            .map(|s| carrier_runtime::mcp::normalize_name(s))
            .collect();

        // Collect known server names from live connections for correct grouping.
        // DashMap iteration doesn't block tool calls.
        let known_names: Vec<String> = self
            .plugins
            .mcp_connections
            .iter()
            .map(|e| e.value().name().to_string())
            .collect();
        let known_refs: Vec<&str> = known_names.iter().map(|s| s.as_str()).collect();

        // Group tools by MCP server using known-names resolver
        let mut servers: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        let mut tool_count = 0usize;
        for tool in &tools {
            let server =
                carrier_runtime::mcp::extract_mcp_server_from_known(&tool.name, &known_refs)
                    .map(String::from)
                    .unwrap_or_else(|| "unknown".to_string());

            // Filter by MCP allowlist if set
            if !mcp_allowlist.is_empty() && !normalized.iter().any(|n| n == &server) {
                continue;
            }

            // Extract the original tool name (after the mcp_{server}_ prefix)
            let prefix = format!("mcp_{}_", server);
            let tool_display = tool.name.strip_prefix(&prefix).unwrap_or(&tool.name);

            servers
                .entry(server)
                .or_default()
                .push(tool_display.to_string());
            tool_count += 1;
        }
        if tool_count == 0 {
            return String::new();
        }
        let mut summary = format!("\n\n--- Connected MCP Servers ({} tools) ---\n", tool_count);
        for (server, tool_names) in &servers {
            summary.push_str(&format!(
                "- {server}: {} tools ({})\n",
                tool_names.len(),
                tool_names.join(", ")
            ));
        }
        summary
            .push_str("MCP tools are prefixed with mcp_{server}_ and work like regular tools.\n");
        // Add filesystem-specific guidance when a filesystem MCP server is connected
        let has_filesystem = servers.keys().any(|s| s.contains("filesystem"));
        if has_filesystem {
            summary.push_str(
                "IMPORTANT: For accessing files OUTSIDE your workspace directory, you MUST use \
                 the MCP filesystem tools (e.g. mcp_filesystem_read_file, mcp_filesystem_list_directory) \
                 instead of the built-in file_read/file_list/file_write tools, which are restricted to \
                 the workspace. The MCP filesystem server has been granted access to specific directories \
                 by the user.",
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
        tools: &[carrier_types::tool::ToolDefinition],
        sender_id: &Option<String>,
        sender_name: Option<String>,
    ) {
        let mcp_tool_count = self.plugins.mcp_tools.lock().map(|t| t.len()).unwrap_or(0);
        // Read user_name from the agent's KV namespace (per-sender memory)
        let sid = sender_id.as_deref().unwrap_or("");
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

        let prompt_ctx = carrier_runtime::prompt_builder::PromptContext {
            agent_name: manifest.name.clone(),
            agent_description: manifest.description.clone(),
            base_system_prompt: manifest.model.system_prompt.clone(),
            granted_tools: tools.iter().map(|t| t.name.clone()).collect(),
            recalled_memories: vec![],
            skill_summary: String::new(),
            skill_prompt_context: String::new(),
            mcp_summary: if mcp_tool_count > 0 {
                self.build_mcp_summary(&manifest.mcp_servers)
            } else {
                String::new()
            },
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
                let mut ws_ctx = carrier_runtime::workspace_context::WorkspaceContext::detect(w);
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
                read_user_profile_summary(&self.config.home_dir, sid, &manifest.name)
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
                .and_then(|w| read_knowledge_content(w, sender_id.as_deref(), Some(&self.config.home_dir))),
            clone_agents_md: manifest
                .workspace
                .as_ref()
                .and_then(|w| read_agents_directory(w)),
        };
        manifest.model.system_prompt =
            carrier_runtime::prompt_builder::build_system_prompt(&prompt_ctx);
    }
}
