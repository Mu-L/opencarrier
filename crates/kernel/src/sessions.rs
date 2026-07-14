//! Session management and agent lifecycle — reset, compact, configure, kill.
//!
//! Handles session CRUD, session compaction, agent configuration mutations
//! (model, flows, MCP servers, tool filters), and agent termination.

use types::agent::*;
use types::error::CarrierError;
use tracing::{debug, info, warn};

use crate::error::{KernelError, KernelResult};
use crate::kernel::CarrierKernel;

impl CarrierKernel {
    /// Reset an agent's session — auto-saves a summary to memory, then clears messages
    /// and creates a fresh session ID.
    pub fn reset_session(&self, agent_id: AgentId) -> KernelResult<()> {
        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        // Auto-save session context to workspace memory before clearing
        if let Ok(Some(old_session)) = self.memory.get_session(entry.session_id) {
            if old_session.messages.len() >= 2 {
                self.save_session_summary(agent_id, &entry, &old_session);
            }
        }

        // Delete the old session
        if let Err(e) = self.memory.delete_session(entry.session_id) {
            warn!(agent_id = %agent_id, error = %e, "Failed to delete old session");
        }

        // Create a fresh session
        let agent_name = entry.name.clone();
        let new_session = self
            .memory
            .create_session(agent_name)
            .map_err(KernelError::Carrier)?;

        // Update registry with new session ID
        self.registry
            .update_session_id(agent_id, new_session.id)
            .map_err(KernelError::Carrier)?;

        // Reset quota tracking so /new clears "token quota exceeded"
        self.runtime.scheduler.reset_usage(agent_id);

        info!(agent_id = %agent_id, "Session reset (summary saved to memory)");
        Ok(())
    }

    /// Clear ALL conversation history for an agent (sessions + canonical).
    ///
    /// Creates a fresh empty session afterward so the agent is still usable.
    pub fn clear_agent_history(&self, agent_id: AgentId) -> KernelResult<()> {
        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        // Delete all regular sessions
        if let Err(e) = self.memory.delete_agent_sessions(&entry.name) {
            warn!(agent = %entry.name, error = %e, "Failed to delete agent sessions");
        }

        // Delete canonical (cross-channel) session

        // Create a fresh session
        let agent_name = entry.name.clone();
        let new_session = self
            .memory
            .create_session(agent_name)
            .map_err(KernelError::Carrier)?;

        // Update registry with new session ID
        self.registry
            .update_session_id(agent_id, new_session.id)
            .map_err(KernelError::Carrier)?;

        info!(agent_id = %agent_id, "All agent history cleared");
        Ok(())
    }

    /// List all sessions for a specific agent.
    pub fn list_agent_sessions(&self, agent_id: AgentId) -> KernelResult<Vec<serde_json::Value>> {
        // Verify agent exists
        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        let mut sessions = self
            .memory
            .list_agent_sessions(&entry.name)
            .map_err(KernelError::Carrier)?;

        // Mark the active session
        for s in &mut sessions {
            if let Some(obj) = s.as_object_mut() {
                let is_active = obj
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(|sid| sid == entry.session_id.0.to_string())
                    .unwrap_or(false);
                obj.insert("active".to_string(), serde_json::json!(is_active));
            }
        }

        Ok(sessions)
    }

    /// Create a new named session for an agent.
    pub fn create_agent_session(
        &self,
        agent_id: AgentId,
        label: Option<&str>,
    ) -> KernelResult<serde_json::Value> {
        // Verify agent exists
        let _entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        let agent_name = self.registry.get(agent_id).map(|e| e.name.clone()).unwrap_or_else(|| agent_id.to_string());
        let session = self
            .memory
            .create_session_with_label(agent_name, label)
            .map_err(KernelError::Carrier)?;

        // Switch to the new session
        self.registry
            .update_session_id(agent_id, session.id)
            .map_err(KernelError::Carrier)?;

        info!(agent_id = %agent_id, label = ?label, "Created new session");

        Ok(serde_json::json!({
            "session_id": session.id.0.to_string(),
            "label": session.label,
        }))
    }

    /// Switch an agent to an existing session by session ID.
    pub fn switch_agent_session(
        &self,
        agent_id: AgentId,
        session_id: SessionId,
    ) -> KernelResult<()> {
        // Verify agent exists
        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        // Verify session exists and belongs to this agent
        let session = self
            .memory
            .get_session(session_id)
            .map_err(KernelError::Carrier)?
            .ok_or_else(|| {
                KernelError::Carrier(CarrierError::Internal("Session not found".to_string()))
            })?;

        if session.agent_name != entry.name {
            return Err(KernelError::Carrier(CarrierError::Internal(
                "Session belongs to a different agent".to_string(),
            )));
        }

        self.registry
            .update_session_id(agent_id, session_id)
            .map_err(KernelError::Carrier)?;

        info!(agent_id = %agent_id, session_id = %session_id.0, "Switched session");
        Ok(())
    }

    /// Save a summary of the current session to agent memory before reset.
    fn save_session_summary(
        &self,
        agent_id: AgentId,
        entry: &AgentEntry,
        session: &memory::session::Session,
    ) {
        use types::message::{MessageContent, Role};

        // Take last 10 messages (or all if fewer)
        let recent = &session.messages[session.messages.len().saturating_sub(10)..];

        // Extract key topics from user messages
        let topics: Vec<&str> = recent
            .iter()
            .filter(|m| m.role == Role::User)
            .filter_map(|m| match &m.content {
                MessageContent::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();

        if topics.is_empty() {
            return;
        }

        // Generate a slug from first user message (first 6 words, slugified)
        let slug: String = topics[0]
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join("-")
            .to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .take(60)
            .collect();

        let date = chrono::Utc::now().format("%Y-%m-%d");
        let summary = format!(
            "Session on {date}: {slug}\n\nKey exchanges:\n{}",
            topics
                .iter()
                .take(5)
                .enumerate()
                .map(|(i, t)| {
                    let truncated = types::truncate_str(t, 200);
                    format!("{}. {}", i + 1, truncated)
                })
                .collect::<Vec<_>>()
                .join("\n")
        );

        // Save to structured memory store (key = "session_{date}_{slug}")
        let key = format!("session_{date}_{slug}");
        if let Err(e) = self.memory.system_kv_set(
            &agent_id.to_string(),
            "",
            "",
            &key,
            serde_json::Value::String(summary.clone()),
        ) {
            warn!(agent_id = %agent_id, error = %e, "Failed to save session summary to KV store");
        }

        // Also write to workspace memory/ dir if workspace exists
        if let Some(ref workspace) = entry.manifest.workspace {
            let mem_dir = workspace.join("memory");
            let filename = format!("{date}-{slug}.md");
            if let Err(e) = std::fs::write(mem_dir.join(&filename), &summary) {
                warn!(agent_id = %agent_id, error = %e, "Failed to write session summary to workspace");
            }
        }

        debug!(
            agent_id = %agent_id,
            key = %key,
            "Saved session summary to memory before reset"
        );
    }

    // ── Agent configuration mutations ──────────────────────────

    /// Switch an agent's modality (resolved to model by Brain at inference time).
    ///
    /// The `model` parameter is the modality name (e.g. "chat", "fast", "vision").
    /// Brain maps the modality to the actual provider/model/endpoint.
    pub fn set_agent_model(&self, agent_id: AgentId, model: &str) -> KernelResult<()> {
        // Model/provider management moved to Brain — this updates modality only
        let modality = model.to_string();

        self.registry
            .update_modality(agent_id, modality.clone())
            .map_err(KernelError::Carrier)?;
        info!(agent_id = %agent_id, modality = %modality, "Agent modality updated");

        // Persist the updated entry
        if let Some(entry) = self.registry.get(agent_id) {
            if let Err(e) = self.memory.save_agent(&entry) {
                warn!(agent_id = %agent_id, error = %e, "Failed to persist agent after modality update");
            }
        }

        // Clear canonical session to prevent memory poisoning from old model's responses
        debug!(agent_id = %agent_id, "Cleared canonical session after model switch");

        Ok(())
    }

    /// Update an agent's flow allowlist. Empty = all flows (backward compat).
    pub fn set_agent_flows(&self, agent_id: AgentId, flows: Vec<String>) -> KernelResult<()> {
        self.registry
            .update_flows(agent_id, flows.clone())
            .map_err(KernelError::Carrier)?;

        if let Some(entry) = self.registry.get(agent_id) {
            if let Err(e) = self.memory.save_agent(&entry) {
                warn!(agent_id = %agent_id, error = %e, "Failed to persist agent after flows update");
            }
        }

        info!(agent_id = %agent_id, flows = ?flows, "Agent flows updated");
        Ok(())
    }

    /// Update an agent's MCP server allowlist. Empty = all servers (backward compat).
    pub fn set_agent_mcp_servers(
        &self,
        agent_id: AgentId,
        servers: Vec<String>,
    ) -> KernelResult<()> {
        // Validate server names if allowlist is non-empty
        if !servers.is_empty() {
            let mut known_servers: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for entry in self.plugins.mcp_connections.iter() {
                known_servers.insert(entry.key().clone());
            }
            for name in &servers {
                let normalized = runtime::mcp::normalize_name(name);
                if !known_servers.contains(&normalized) {
                    return Err(KernelError::Carrier(CarrierError::Internal(format!(
                        "Unknown MCP server: {name}"
                    ))));
                }
            }
        }

        self.registry
            .update_mcp_servers(agent_id, servers.clone())
            .map_err(KernelError::Carrier)?;

        if let Some(entry) = self.registry.get(agent_id) {
            if let Err(e) = self.memory.save_agent(&entry) {
                warn!(agent_id = %agent_id, error = %e, "Failed to persist agent update");
            }
        }

        info!(agent_id = %agent_id, servers = ?servers, "Agent MCP servers updated");
        Ok(())
    }

    /// Update an agent's tool allowlist and/or blocklist.
    pub fn set_agent_tool_filters(
        &self,
        agent_id: AgentId,
        allowlist: Option<Vec<String>>,
        blocklist: Option<Vec<String>>,
    ) -> KernelResult<()> {
        self.registry
            .update_tool_filters(agent_id, allowlist.clone(), blocklist.clone())
            .map_err(KernelError::Carrier)?;

        if let Some(entry) = self.registry.get(agent_id) {
            if let Err(e) = self.memory.save_agent(&entry) {
                warn!(agent_id = %agent_id, error = %e, "Failed to persist agent update");
            }
        }

        info!(
            agent_id = %agent_id,
            allowlist = ?allowlist,
            blocklist = ?blocklist,
            "Agent tool filters updated"
        );
        Ok(())
    }

    /// Cancel an agent's currently running LLM task.
    pub fn stop_agent_run(&self, agent_id: AgentId) -> KernelResult<bool> {
        if let Some((_, handle)) = self.runtime.running_tasks.remove(&agent_id) {
            handle.abort();
            info!(agent_id = %agent_id, "Agent run cancelled");
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Compact an agent's session using LLM-based summarization.
    ///
    /// Replaces the existing text-truncation compaction with an intelligent
    /// LLM-generated summary of older messages, keeping only recent messages.
    pub async fn compact_agent_session(&self, agent_id: AgentId, session_id: types::agent::SessionId) -> KernelResult<String> {
        use runtime::compactor::{compact_session, needs_compaction, needs_compaction_by_tokens, estimate_token_count, CompactionConfig};

        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        let session = self
            .memory
            .get_session(session_id)
            .map_err(KernelError::Carrier)?
            .unwrap_or_else(|| memory::session::Session {
                id: session_id,
                agent_name: entry.name.clone(),
                messages: Vec::new(),
                context_window_tokens: 0,
                    turn_summaries: Vec::new(),
                label: None,
            });

        let config = CompactionConfig::default();

        let by_messages = needs_compaction(&session, &config);
        let by_tokens = {
            let estimated = estimate_token_count(&session.messages, None, None);
            needs_compaction_by_tokens(estimated, &config)
        };

        if !by_messages && !by_tokens {
            return Ok(format!(
                "No compaction needed ({} messages, threshold {})",
                session.messages.len(),
                config.threshold
            ));
        }

        // Use "fast" modality for compaction (cheaper, faster); fall back to agent modality
        let compaction_modality = if self.brain_read().has_modality("fast") { "fast" } else { &entry.manifest.model.modality };
        let compaction_model = self.brain_read().model_for(compaction_modality);
        let driver = {
            let brain = self.brain_read();
            let endpoints = brain.endpoints_for(compaction_modality);
            if let Some(ep) = endpoints.first() {
                brain.driver_for_endpoint(&ep.id).ok_or_else(|| KernelError::Carrier(CarrierError::LlmDriver(format!(
                    "No driver for compaction modality '{compaction_modality}'"
                ))))?
            } else {
                self.resolve_driver(&entry.manifest)?
            }
        };

        let result = compact_session(driver, &compaction_model, &session, &config)
            .await
            .map_err(|e| KernelError::Carrier(CarrierError::Internal(e)))?;

        // Post-compaction audit: validate and repair the kept messages
        let (repaired_messages, repair_stats) =
            runtime::session_repair::validate_and_repair_with_stats(&result.kept_messages);

        // Prepend summary as a User message. Role::User is required (not System) because
        // agent_loop filters out Role::System messages from the LLM request. The split
        // alignment in compact_session ensures kept[0] is Assistant, so the summary
        // (User) + kept[0] (Assistant) pair won't be merged by validate_and_repair.
        let mut final_messages = vec![types::message::Message::user(&result.summary)];
        final_messages.extend(repaired_messages);

        // Also update the regular session with the repaired messages
        let mut updated_session = session;
        updated_session.messages = final_messages;
        self.memory
            .save_session(&updated_session)
            .map_err(KernelError::Carrier)?;

        // Build result message with audit summary
        let mut msg = format!(
            "Compacted {} messages into summary ({} chars), kept {} recent messages.",
            result.compacted_count,
            result.summary.len(),
            updated_session.messages.len()
        );

        let repairs = repair_stats.orphaned_results_removed
            + repair_stats.synthetic_results_inserted
            + repair_stats.duplicates_removed
            + repair_stats.messages_merged;
        if repairs > 0 {
            msg.push_str(&format!(" Post-audit: repaired ({} orphaned removed, {} synthetic inserted, {} merged, {} deduped).",
                repair_stats.orphaned_results_removed,
                repair_stats.synthetic_results_inserted,
                repair_stats.messages_merged,
                repair_stats.duplicates_removed,
            ));
        } else {
            msg.push_str(" Post-audit: clean.");
        }

        Ok(msg)
    }

    /// Run the intent classifier and switch the agent to a new session if the
    /// new message is judged to start a new conversation.
    ///
    /// Returns Ok(()) on success (whether or not a rotation occurred). Returns
    /// Err on infrastructure failure (DB, LLM, etc.); the caller is expected
    /// to fall back gracefully.
    pub async fn maybe_rotate_session_by_intent(
        &self,
        agent_id: AgentId,
        entry: &AgentEntry,
        new_user_msg: &str,
    ) -> KernelResult<()> {
        use runtime::intent_classifier::classify_intent;
        use types::message::{MessageContent, Role};

        // Load current session. If missing or empty, nothing to classify against.
        let session = match self.memory.get_session(entry.session_id) {
            Ok(Some(s)) if !s.messages.is_empty() => s,
            _ => return Ok(()),
        };

        // Extract last assistant message text for classifier context.
        let last_assistant: Option<String> = session
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .and_then(|m| match &m.content {
                MessageContent::Text(t) => Some(t.clone()),
                MessageContent::Blocks(blocks) => {
                    let text: String = blocks
                        .iter()
                        .filter_map(|b| match b {
                            types::message::ContentBlock::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if text.is_empty() { None } else { Some(text) }
                }
            });

        // Prefer the "fast" modality for cheap/quick classification.
        let modality = if self.brain_read().has_modality("fast") {
            "fast"
        } else {
            &entry.manifest.model.modality
        };
        let model = self.brain_read().model_for(modality);
        let driver = {
            let brain = self.brain_read();
            let endpoints = brain.endpoints_for(modality);
            if let Some(ep) = endpoints.first() {
                brain.driver_for_endpoint(&ep.id).ok_or_else(|| {
                    KernelError::Carrier(CarrierError::LlmDriver(format!(
                        "No driver for classifier modality '{modality}'"
                    )))
                })?
            } else {
                self.resolve_driver(&entry.manifest)?
            }
        };

        let classification = classify_intent(driver, &model, last_assistant.as_deref(), new_user_msg)
            .await
            .map_err(|e| KernelError::Carrier(CarrierError::Internal(e)))?;

        if classification.is_new {
            tracing::info!(
                agent_id = %agent_id,
                reasoning = %classification.reasoning,
                "Intent: new conversation — rotating session"
            );
            let new_session = self
                .memory
                .create_session(entry.name.clone())
                .map_err(KernelError::Carrier)?;
            self.registry
                .update_session_id(agent_id, new_session.id)
                .map_err(KernelError::Carrier)?;
        } else {
            tracing::info!(
                agent_id = %agent_id,
                reasoning = %classification.reasoning,
                "Intent: continuing session"
            );
        }

        Ok(())
    }

    /// Generate a context window usage report for an agent.
    pub fn context_report(
        &self,
        agent_id: AgentId,
    ) -> KernelResult<runtime::compactor::ContextReport> {
        use runtime::compactor::generate_context_report;

        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        let session = self
            .memory
            .get_session(entry.session_id)
            .map_err(KernelError::Carrier)?
            .unwrap_or_else(|| memory::session::Session {
                id: entry.session_id,
                agent_name: entry.name.clone(),
                messages: Vec::new(),
                context_window_tokens: 0,
                    turn_summaries: Vec::new(),
                label: None,
            });

        let system_prompt = &entry.manifest.model.system_prompt;
        // Core tool set (same as messaging.rs — other tools found via tool_search)
        let mut tools: Vec<types::tool::ToolDefinition> = runtime::tool_runner::builtin_tool_definitions(self.config.cli_exec.clone())
            .into_iter()
            .filter(|t| types::tool::CORE_TOOL_NAMES.contains(&t.name.as_str()))
            .collect();
        if !entry.manifest.subagents.is_empty() {
            tools.extend(types::agent::build_subagent_tool_definitions(&entry.manifest.subagents));
        }
        // Use 200K default or the model's known context window
        let context_window = if session.context_window_tokens > 0 {
            session.context_window_tokens
        } else {
            200_000
        };

        Ok(generate_context_report(
            &session.messages,
            Some(system_prompt),
            Some(&tools),
            context_window as usize,
        ))
    }

    /// Kill an agent.
    pub fn kill_agent(&self, agent_id: AgentId) -> KernelResult<()> {
        let entry = self
            .registry
            .remove(agent_id)
            .map_err(KernelError::Carrier)?;
        self.runtime.background.stop_agent(agent_id);
        self.runtime.scheduler.unregister(agent_id);
        self.coordination.capabilities.revoke_all(agent_id);
        self.coordination.event_bus.unsubscribe_agent(agent_id);

        // Remove cron jobs so they don't linger as orphans (#504)
        let cron_removed = self.cron_scheduler.remove_agent_jobs(agent_id);
        if cron_removed > 0 {
            if let Err(e) = self.cron_scheduler.persist() {
                warn!("Failed to persist cron jobs after agent deletion: {e}");
            }
        }

        // Remove from persistent storage
        if let Err(e) = self.memory.remove_agent(agent_id) {
            warn!(agent_id = %agent_id, error = %e, "Failed to remove agent from persistent storage");
        }

        // Clean up per-agent runtime resources to prevent leaks
        self.runtime.running_tasks.remove(&agent_id);
        if let Ok(mut bindings) = self.coordination.bindings.lock() {
            bindings.retain(|b| b.agent != entry.name);
        }

        // SECURITY: Record agent kill in audit trail
        self.audit_log.record(
            agent_id.to_string(),
            runtime::audit::AuditAction::AgentKill,
            format!("name={}", entry.name),
            "ok",
        );

        info!(agent = %entry.name, id = %agent_id, "Agent killed");
        Ok(())
    }
}
