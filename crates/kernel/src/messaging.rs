//! Agent message dispatch and execution — send, stream, WASM, Python, LLM.
//!
//! Handles the core agent communication paths: plain send, streaming send,
//! and module-type dispatch (WASM sandbox, Python subprocess, LLM agent loop).

use runtime::agent_loop::{run_agent_loop, run_agent_loop_streaming, AgentLoopResult};
use runtime::kernel_handle::KernelHandle;
use runtime::llm_driver::StreamEvent;
use runtime::python_runtime::{self, PythonConfig};
use runtime::sandbox::SandboxConfig;
use runtime::llm_driver::LlmDriver;
use types::agent::*;
use types::error::CarrierError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

use crate::capabilities::manifest_to_capabilities;
use crate::error::{KernelError, KernelResult};
use crate::kernel::CarrierKernel;
use crate::prompt_sources::touch_user_profile;
use crate::workspace::append_daily_memory_log;

/// Shared preparation context for LLM agent execution.
///
/// Both `send_message_streaming` and `execute_llm_agent` perform the same
/// session loading, compaction check, tool assembly, flow/subagent matching,
/// and manifest mutation steps before diverging at the actual LLM call.
/// This struct holds the results of that shared preparation.
struct PreparedContext {
    session: memory::session::Session,
    needs_compact: bool,
    tools: Vec<types::tool::ToolDefinition>,
    manifest: AgentManifest,
    driver: Arc<dyn LlmDriver>,
    ctx_window: Option<usize>,
    /// The auto-matched flow (if any), carrying the full parsed `FlowDef`.
    /// `flow_def.steps` non-empty => multi-step flow for `run_flow`.
    flow: Option<crate::prompt_sources::FlowMatch>,
}

impl CarrierKernel {
    /// Shared preparation for LLM agent execution: session loading, compaction
    /// check, core tool set assembly, flow/subagent classification, and manifest
    /// mutation. Returns a `PreparedContext` that both streaming and non-streaming
    /// paths consume before diverging at the actual LLM invocation.
    #[allow(clippy::too_many_arguments)]
    async fn prepare_agent_context(
        &self,
        agent_id: AgentId,
        message: &str,
        entry: &AgentEntry,
        sender_id: &Option<String>,
        sender_name: Option<String>,
        owner_id: &Option<String>,
        channel_type: &Option<String>,
        task_id: Option<&str>,
        resume_flow: Option<&memory::FlowRunRow>,
    ) -> KernelResult<PreparedContext> {
        // Load session: per-user when sender_id is present (multi-tenancy),
        // otherwise use the agent's default session.
        let agent_name = self.registry.get(agent_id).map(|e| e.name.clone()).unwrap_or_else(|| agent_id.to_string());
        let session = if let Some(ref sid) = sender_id {
            let user_label = format!("user:{}", sid);
            match self
                .memory
                .find_session_by_label_async(&agent_name, &user_label)
                .await
                .map_err(KernelError::Carrier)?
            {
                Some(s) => s,
                None => self
                    .memory
                    .create_session_with_label(agent_name.clone(), Some(&user_label))
                    .map_err(KernelError::Carrier)?,
            }
        } else {
            self.memory
                .get_session_async(entry.session_id)
                .await
                .map_err(KernelError::Carrier)?
                .unwrap_or_else(|| memory::session::Session {
                    id: entry.session_id,
                    agent_name: agent_name.clone(),
                    messages: Vec::new(),
                    context_window_tokens: 0,
                    turn_summaries: Vec::new(),
                    label: None,
                })
        };

        // Check if auto-compaction is needed
        let needs_compact = {
            use runtime::compactor::{
                estimate_token_count, needs_compaction as check_compact,
                needs_compaction_by_tokens, CompactionConfig,
            };
            let config = CompactionConfig::default();
            let by_messages = check_compact(&session, &config);
            let estimated = estimate_token_count(
                &session.messages,
                Some(&entry.manifest.model.system_prompt),
                None,
            );
            let by_tokens = needs_compaction_by_tokens(estimated, &config);
            if by_tokens && !by_messages {
                info!(
                    agent_id = %agent_id,
                    estimated_tokens = estimated,
                    messages = session.messages.len(),
                    "Token-based compaction triggered (messages below threshold but tokens above)"
                );
            }
            let by_quota = if let Some(headroom) = self.runtime.scheduler.token_headroom(agent_id) {
                let threshold = (headroom as f64 * 0.8) as u64;
                if estimated as u64 > threshold && session.messages.len() > 4 {
                    info!(
                        agent_id = %agent_id,
                        estimated_tokens = estimated,
                        quota_headroom = headroom,
                        "Quota-headroom compaction triggered (session would consume >80% of remaining quota)"
                    );
                    true
                } else {
                    false
                }
            } else {
                false
            };
            by_messages || by_tokens || by_quota
        };

        // Build agent's core tool set (bootstrap tools + delegate tools)
        let mut tools: Vec<types::tool::ToolDefinition> = runtime::tool_runner::builtin_tool_definitions(self.config.cli_exec.clone())
            .into_iter()
            .filter(|t| types::tool::CORE_TOOL_NAMES.contains(&t.name.as_str()))
            .collect();

        // Also include declarative API tools — they are always available to all
        // agents (registered via builtin_modules), but not in CORE_TOOL_NAMES.
        // Capabilities.tools filtering still applies downstream.
        let home_dir = types::config::home_dir();
        let api_tool_names: std::collections::HashSet<String> = {
            let mut names: std::collections::HashSet<String> = runtime::api_tools::loader::load_all_api_tools(&home_dir, entry.manifest.workspace.as_deref())
                .into_iter()
                .map(|t| t.name)
                .collect();
            // Include dynamically registered tools
            for dt in runtime::api_tools::register::dynamic_tools() {
                names.insert(dt.name);
            }
            names
        };
        if !api_tool_names.is_empty() {
            let all_builtins = runtime::tool_runner::builtin_tool_definitions(self.config.cli_exec.clone());
            for t in &all_builtins {
                if api_tool_names.contains(&t.name) {
                    tools.push(t.clone());
                }
            }
        }

        if !entry.manifest.subagents.is_empty() {
            tools.extend(types::agent::build_subagent_tool_definitions(&entry.manifest.subagents));
        }

        info!(
            agent = %entry.name,
            tool_count = tools.len(),
            "Agent core tool set assembled"
        );

        // Auto-match flow for prompt injection
        let brain_ref: Option<Arc<dyn runtime::llm_driver::Brain>> =
            Some(Arc::clone(&*self.brain.brain.read().unwrap_or_else(|e| {
                warn!("Brain RwLock poisoned, recovering");
                e.into_inner()
            }))
                as Arc<dyn runtime::llm_driver::Brain>);

        let (auto_matched_flow, flow_max_iterations, matched_flow) = if let Some(rf) = resume_flow {
            // Resume: load the flow by name WITHOUT an LLM classify call -- the
            // user's reply continues an already-matched flow, so re-classifying
            // would be wrong (and might match a different flow).
            match entry.manifest.workspace.as_ref() {
                Some(ws) => match crate::prompt_sources::load_flow_by_name(ws, &rf.flow_name) {
                    Some(flow) => {
                        let flow_name = flow.name.clone();
                        let flow_body = flow.body.clone();
                        let flow_max_iter = flow.max_iterations;

                        // Auto-discover flow-declared tools (same as classify branch)
                        let mut flow_warnings: Vec<String> = Vec::new();
                        if flow.tools.is_empty() {
                            flow_warnings.push(format!(
                                "Flow '{}' has no declared tools in its frontmatter. \
                                 If this flow requires tools, use flow_update to add a tools: [\"tool1\", \"tool2\"] field.",
                                flow_name
                            ));
                        }
                        for t in &flow.tools {
                            if !tools.iter().any(|d| d.name == *t) {
                                if let Some((_, def)) = self.search_tools(t, 1, entry.manifest.max_tool_level).into_iter().next() {
                                    tools.push(def);
                                } else {
                                    flow_warnings.push(format!(
                                        "Flow '{}' declared tool '{}' but it was not found in the tool catalog. \
                                         Use flow_update to remove or correct this tool declaration.",
                                        flow_name, t
                                    ));
                                }
                            }
                        }

                        info!(agent = %entry.name, flow = %flow_name, "Flow loaded for resume");

                        let mut flow_prompt = format!("**{}**\n{}", flow_name, flow_body);
                        if !flow_warnings.is_empty() {
                            flow_prompt.push_str(&format!("\n\n⚠️ **Flow Tool Warnings:**\n{}", flow_warnings.iter().map(|w| format!("- {}", w)).collect::<Vec<_>>().join("\n")));
                        }
                        (Some(flow_prompt), flow_max_iter, Some(flow))
                    }
                    None => {
                        warn!(agent = %entry.name, flow = %rf.flow_name, "resume: flow def not found, falling back to normal handling");
                        (None, None, None)
                    }
                },
                None => (None, None, None),
            }
        } else if let (Some(ws), Some(brain)) = (entry.manifest.workspace.as_ref(), brain_ref.as_ref()) {
            // Give the classifier recent conversation context so it can
            // match follow-up messages in multi-turn workflows (e.g.
            // charter-quoter after the user sends their phone in turn 2).
            let recent_turns: Vec<(String, String)> = session
                .turn_summaries
                .iter()
                .rev()
                .take(2)
                .rev()
                .map(|t| {
                    let intent = if t.user_intent.is_empty() { "(no intent)".to_string() } else { t.user_intent.clone() };
                    let outcome = if t.assistant_outcome.is_empty() { "(no outcome)".to_string() } else { t.assistant_outcome.clone() };
                    (intent, outcome)
                })
                .collect();
            match crate::prompt_sources::classify_flow_with_llm(message, ws, brain, &recent_turns).await {
                Some(flow) => {
                    let flow_name = flow.name.clone();
                    let flow_body = flow.body.clone();
                    let flow_max_iter = flow.max_iterations;

                    // Auto-discover flow-declared tools and collect diagnostics
                    let mut flow_warnings: Vec<String> = Vec::new();
                    if flow.tools.is_empty() {
                        flow_warnings.push(format!(
                            "Flow '{}' has no declared tools in its frontmatter. \
                             If this flow requires tools, use flow_update to add a tools: [\"tool1\", \"tool2\"] field.",
                            flow_name
                        ));
                    }
                    for t in &flow.tools {
                        if !tools.iter().any(|d| d.name == *t) {
                            if let Some((_, def)) = self.search_tools(t, 1, entry.manifest.max_tool_level).into_iter().next() {
                                tools.push(def);
                            } else {
                                flow_warnings.push(format!(
                                    "Flow '{}' declared tool '{}' but it was not found in the tool catalog. \
                                     Use flow_update to remove or correct this tool declaration.",
                                    flow_name, t
                                ));
                            }
                        }
                    }

                    info!(
                        agent = %entry.name,
                        flow = %flow_name,
                        "Flow classified by LLM"
                    );

                    let mut flow_prompt = format!("**{}**\n{}", flow_name, flow_body);
                    if !flow_warnings.is_empty() {
                        flow_prompt.push_str(&format!("\n\n⚠️ **Flow Tool Warnings:**\n{}", flow_warnings.iter().map(|w| format!("- {}", w)).collect::<Vec<_>>().join("\n")));
                    }

                    // The flow body is injected into the base system prompt for
                    // BOTH single- and multi-step flows. Multi-step execution
                    // (run_flow) receives this base prompt and adds per-step
                    // directives on top; the streaming path falls back to
                    // guided single-step execution if run_flow isn't wired there.
                    (
                        Some(flow_prompt),
                        flow_max_iter,
                        Some(flow),
                    )
                }
                None => (None, None, None)
            }
        } else {
            (None, None, None)
        };

        // Auto-match subagent trigger (only when no flow matched)
        let auto_matched_subagent = if auto_matched_flow.is_none() && !entry.manifest.subagents.is_empty() {
            if let Some(sa_match) = crate::prompt_sources::match_subagent_for_message(message, &entry.manifest.subagents) {
                info!(
                    agent = %entry.name,
                    subagent = %sa_match.name,
                    "Subagent trigger matched"
                );
                Some(sa_match.name.clone())
            } else {
                None
            }
        } else {
            None
        };

        // Subagent delegation from channel_type
        let subagent_config = if let Some(ref ct) = channel_type {
            if let Some(sa_name) = ct.strip_prefix("subagent:") {
                entry.manifest.subagents.iter().find(|s| s.name == sa_name).cloned()
            } else {
                None
            }
        } else {
            None
        };

        let driver = self.resolve_driver(&entry.manifest)?;
        let ctx_window: Option<usize> = None;

        let mut manifest = entry.manifest.clone();

        // Apply flow's max_iterations override
        if let Some(max_iter) = flow_max_iterations {
            manifest.autonomous.get_or_insert_with(Default::default).max_iterations = max_iter;
            info!(
                agent = %entry.name,
                max_iterations = max_iter,
                "Flow overrides max_iterations"
            );
        }

        // Apply subagent's max_iterations override
        if let Some(ref sa) = subagent_config {
            manifest.autonomous.get_or_insert_with(Default::default).max_iterations = sa.max_iterations;
            manifest.metadata.insert("is_subagent".to_string(), serde_json::json!(true));
            info!(
                agent = %entry.name,
                subagent = %sa.name,
                max_iterations = sa.max_iterations,
                "Subagent overrides max_iterations"
            );
        }

        // Combine flow and subagent auto-match for prompt injection
        let prompt_auto_match = auto_matched_flow.or_else(|| {
            auto_matched_subagent.map(|name| format!("**Auto-delegation: {}**\nThe user message matches the '{}' subagent. Call delegate_{} to handle this task.", name, name, name))
        });

        // L0 turn summaries from session
        let turn_summaries = session.turn_summaries.clone();

        // Drawer entries from kv memory
        let drawer_entries = self.prefetch_drawer_entries(&manifest.name, owner_id.as_deref().unwrap_or(sender_id.as_deref().unwrap_or("")));

        self.build_and_apply_prompt(&agent_id, &mut manifest, &tools, sender_id, sender_name, owner_id, prompt_auto_match.clone(), turn_summaries, drawer_entries, task_id.map(|s| s.to_string()));

        Ok(PreparedContext {
            session,
            needs_compact,
            tools,
            manifest,
            driver,
            ctx_window,
            flow: matched_flow,
        })
    }

    /// Send a message to an agent and get a response.
    ///
    /// Automatically upgrades the kernel handle from `self_handle` so that
    /// agent turns triggered by cron, channels, events, or inter-agent calls
    /// have full access to kernel tools (cron_create, agent_send, etc.).
    pub async fn send_message(
        &self,
        agent_id: AgentId,
        message: &str,
    ) -> KernelResult<AgentLoopResult> {
        let handle: Option<Arc<dyn KernelHandle>> = self
            .coordination
            .self_handle
            .get()
            .and_then(|w| w.upgrade())
            .map(|arc| arc as Arc<dyn KernelHandle>);
        self.send_message_with_handle(agent_id, message, handle, None, None, None, None, None)
            .await
    }

    /// Send a multimodal message (text + images) to an agent and get a response.
    ///
    /// Send a message with an optional kernel handle for inter-agent tools.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message_with_handle(
        &self,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
        owner_id: Option<String>,
        channel_type: Option<String>,
        task_id: Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        self.send_message_with_handle_and_blocks(
            agent_id,
            message,
            kernel_handle,
            None,
            sender_id,
            sender_name,
            owner_id,
            channel_type,
            task_id,
        )
        .await
    }

    /// Send a message with optional content blocks and an optional kernel handle.
    ///
    /// When `content_blocks` is `Some`, the LLM agent loop receives structured
    /// multimodal content (text + images) instead of just a text string. This
    /// enables vision models to process images sent from channels like Telegram.
    ///
    /// Per-agent locking ensures that concurrent messages for the same agent
    /// are serialized (preventing session corruption), while messages for
    /// different agents run in parallel.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message_with_handle_and_blocks(
        &self,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        content_blocks: Option<Vec<types::message::ContentBlock>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
        owner_id: Option<String>,
        channel_type: Option<String>,
        task_id: Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        // NOTE: The per-owner execution lock has been removed. Concurrent messages
        // for the same agent+owner now run in parallel (like nginx). Session
        // consistency is maintained by `save_session_append_async` which uses
        // per-session write locks and merge-writes.

        // LLM concurrency is now enforced per-call inside the agent loop
        // (call_with_retry), not at the agent-loop level. This means a stuck
        // agent only holds a semaphore slot for the duration of a single LLM
        // call (~180-300s), not the entire loop.

        // Enforce quota before running the agent loop
        self.runtime
            .scheduler
            .check_quota(agent_id)
            .map_err(KernelError::Carrier)?;

        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        // Dispatch based on module type
        let result = if entry.manifest.module.starts_with("wasm:") {
            self.execute_wasm_agent(&entry, message, kernel_handle)
                .await
        } else if entry.manifest.module.starts_with("python:") {
            self.execute_python_agent(&entry, agent_id, message).await
        } else {
            // Resume detection: if this sender has a suspended (waiting) flow
            // run for this agent, the message is the `user_input` reply --
            // resume the flow instead of starting a new conversation.
            let resume_row: Option<memory::FlowRunRow> = sender_id
                .as_ref()
                .and_then(|sid| {
                    self.memory
                        .flow_runs()
                        .list_pending(sid, &agent_id.to_string())
                        .ok()
                        .and_then(|v| v.into_iter().next())
                })
                .filter(|r| {
                    r.expires_at
                        .as_deref()
                        .is_none_or(|exp| exp > chrono::Utc::now().to_rfc3339().as_str())
                });

            // Intent classifier: decide whether to continue the current session
            // or open a new one. Skips for empty sessions, when disabled, or
            // when resuming a suspended flow (the reply continues the flow's
            // session, so rotation would be wrong).
            if resume_row.is_none()
                && entry.manifest.intent_classifier_enabled.unwrap_or(true)
            {
                if let Err(e) = self
                    .maybe_rotate_session_by_intent(agent_id, &entry, message)
                    .await
                {
                    tracing::warn!(agent_id = %agent_id, error = %e, "Intent classifier failed; opening new session as fallback");
                    // Fallback: open new session on classifier error.
                    let agent_name = self.registry.get(agent_id).map(|e| e.name.clone()).unwrap_or_else(|| agent_id.to_string());
                    if let Ok(new_session) = self.memory.create_session_async(agent_name).await {
                        if let Err(e) = self.registry.update_session_id(agent_id, new_session.id) {
                            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to update session ID in registry");
                        }
                    }
                }
            }
            // Re-fetch entry — session_id may have changed
            let entry = self.registry.get(agent_id).ok_or_else(|| {
                KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
            })?;
            // Default: LLM agent loop (builtin:chat or any unrecognized module)
            self.execute_llm_agent(
                &entry,
                agent_id,
                message,
                kernel_handle,
                content_blocks,
                sender_id,
                sender_name,
                owner_id,
                channel_type.clone(),
                task_id,
                resume_row.as_ref(),
            )
            .await
        };

        match result {
            Ok(result) => {
                // Record token usage for quota tracking
                self.runtime
                    .scheduler
                    .record_usage(agent_id, &result.total_usage);

                // Update last active time
                if let Err(e) = self.registry.set_state(agent_id, AgentState::Running) {
                    tracing::warn!(agent_id = %agent_id, error = %e, "Failed to set agent state to Running");
                }

                // SECURITY: Record successful message in audit trail
                self.audit_log.record(
                    agent_id.to_string(),
                    runtime::audit::AuditAction::AgentMessage,
                    format!(
                        "tokens_in={}, tokens_out={}",
                        result.total_usage.input_tokens, result.total_usage.output_tokens
                    ),
                    "ok",
                );

                Ok(result)
            }
            Err(e) => {
                // SECURITY: Record failed message in audit trail
                self.audit_log.record(
                    agent_id.to_string(),
                    runtime::audit::AuditAction::AgentMessage,
                    "agent loop failed",
                    format!("error: {e}"),
                );

                // Record the failure in supervisor for health reporting
                self.runtime.supervisor.record_panic();
                warn!(agent_id = %agent_id, error = %e, "Agent loop failed — recorded in supervisor");
                Err(e)
            }
        }
    }

    /// Send a message to an agent with streaming responses.
    ///
    /// Returns a receiver for incremental `StreamEvent`s and a `JoinHandle`
    /// that resolves to the final `AgentLoopResult`. The caller reads stream
    /// events while the agent loop runs, then awaits the handle for final stats.
    ///
    /// WASM and Python agents don't support true streaming — they execute
    /// synchronously and emit a single `TextDelta` + `ContentComplete` pair.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_message_streaming(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
        owner_id: Option<String>,
        channel_type: Option<String>,
    ) -> KernelResult<(
        tokio::sync::mpsc::Receiver<StreamEvent>,
        tokio::task::JoinHandle<KernelResult<AgentLoopResult>>,
    )> {
        // Enforce quota before spawning the streaming task
        self.runtime
            .scheduler
            .check_quota(agent_id)
            .map_err(KernelError::Carrier)?;

        let entry = self.registry.get(agent_id).ok_or_else(|| {
            KernelError::Carrier(CarrierError::AgentNotFound(agent_id.to_string()))
        })?;

        let is_wasm = entry.manifest.module.starts_with("wasm:");
        let is_python = entry.manifest.module.starts_with("python:");

        // Non-LLM modules: execute non-streaming and emit results as stream events
        if is_wasm || is_python {
            let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);
            let kernel_clone = Arc::clone(self);
            let message_owned = message.to_string();
            let entry_clone = entry.clone();

            let handle = tokio::spawn(async move {
                let result = if is_wasm {
                    kernel_clone
                        .execute_wasm_agent(&entry_clone, &message_owned, kernel_handle)
                        .await
                } else {
                    kernel_clone
                        .execute_python_agent(&entry_clone, agent_id, &message_owned)
                        .await
                };

                match result {
                    Ok(result) => {
                        // Emit the complete response as a single text delta
                        let _ = tx
                            .send(StreamEvent::TextDelta {
                                text: result.response.clone(),
                            })
                            .await;
                        let _ = tx
                            .send(StreamEvent::ContentComplete {
                                stop_reason: types::message::StopReason::EndTurn,
                                usage: result.total_usage,
                            })
                            .await;
                        kernel_clone
                            .runtime
                            .scheduler
                            .record_usage(agent_id, &result.total_usage);
                        if let Err(e) = kernel_clone
                            .registry
                            .set_state(agent_id, AgentState::Running)
                        {
                            tracing::warn!(agent_id = %agent_id, error = %e, "Failed to set agent state to Running");
                        }
                        Ok(result)
                    }
                    Err(e) => {
                        kernel_clone.runtime.supervisor.record_panic();
                        warn!(agent_id = %agent_id, error = %e, "Non-LLM agent failed");
                        Err(e)
                    }
                }
            });

            return Ok((rx, handle));
        }

        // LLM agent: true streaming via agent loop
        let ctx = self.prepare_agent_context(
            agent_id, message, &entry, &sender_id, sender_name, &owner_id, &channel_type, None, None,
        ).await?;
        let PreparedContext { mut session, needs_compact, tools, manifest, driver, ctx_window, .. } = ctx;

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);

        let memory = Arc::clone(&self.memory);
        // Build link context from user message (auto-extract URLs for the agent)
        let message_owned = if let Some(link_ctx) =
            runtime::link_understanding::build_link_context(message, &self.config.links)
        {
            format!("{message}{link_ctx}")
        } else {
            message.to_string()
        };
        let kernel_clone = Arc::clone(self);

        let handle = tokio::spawn(async move {
            // Clone Brain Arc before any .await so the RwLockReadGuard is dropped (not Send).
            let brain_ref: Option<Arc<dyn runtime::llm_driver::Brain>> =
                Some(Arc::clone(&*kernel_clone.brain.brain.read().unwrap_or_else(|e| {
                    warn!("Brain RwLock poisoned, recovering");
                    e.into_inner()
                }))
                    as Arc<dyn runtime::llm_driver::Brain>);

            // Extract MemoryHandle from kernel.
            let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
                Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(&kernel_clone.memory)))
                    as Arc<dyn runtime::memory_handle::MemoryHandle>);

            // Auto-compact if the session is large before running the loop
            if needs_compact {
                info!(agent_id = %agent_id, messages = session.messages.len(), "Auto-compacting session");
                match kernel_clone.compact_agent_session(agent_id, session.id).await {
                    Ok(msg) => {
                        info!(agent_id = %agent_id, "{msg}");
                        // Reload the session after compaction
                        if let Ok(Some(reloaded)) = memory.get_session_async(session.id).await {
                            session = reloaded;
                        }
                    }
                    Err(e) => {
                        warn!(agent_id = %agent_id, "Auto-compaction failed: {e}");
                    }
                }
            }

            // Create a phase callback that emits PhaseChange events to WS/SSE clients
            let phase_tx = tx.clone();
            let phase_cb: runtime::agent_loop::PhaseCallback =
                std::sync::Arc::new(move |phase| {
                    use runtime::agent_loop::LoopPhase;
                    let (phase_str, detail) = match &phase {
                        LoopPhase::Thinking => ("thinking".to_string(), None),
                        LoopPhase::ToolUse { tool_name } => {
                            ("tool_use".to_string(), Some(tool_name.clone()))
                        }
                        LoopPhase::Streaming => ("streaming".to_string(), None),
                        LoopPhase::Done => ("done".to_string(), None),
                        LoopPhase::Error => ("error".to_string(), None),
                    };
                    let event = StreamEvent::PhaseChange {
                        phase: phase_str,
                        detail,
                    };
                    let _ = phase_tx.try_send(event);
                });

            let result = run_agent_loop_streaming(
                &manifest,
                &message_owned,
                &mut session,
                &memory,
                driver,
                &tools,
                kernel_handle,
                tx,
                Some(&kernel_clone.plugins.mcp_connections),
                Some(&kernel_clone.services.fetch_engine),
                manifest.workspace.as_deref(),
                Some(&phase_cb),
                Some(&kernel_clone.coordination.hooks),
                ctx_window,
                Some(&kernel_clone.coordination.process_manager),
                None,              // content_blocks (streaming path uses text only for now)
                brain_ref.clone(), // Brain for modality-based routing
                memory_handle.clone(), // Memory handle for kv/tree operations
                sender_id.as_deref(),
                owner_id.as_deref(),
                channel_type.as_deref(),
                Some(kernel_clone.runtime.llm_concurrency_limit.clone()),
            )
            .await;

            // Drop the phase callback immediately after the streaming loop
            // completes. It holds a clone of the stream sender (`tx`), which
            // keeps the mpsc channel alive. If we don't drop it here, the
            // WS/SSE stream_task won't see channel closure until this entire
            // spawned task exits (after all post-processing below). This was
            // causing 20-45s hangs where the client received phase:done but
            // never got the response event (the upstream WS would die from
            // ping timeout before post-processing finished).
            drop(phase_cb);

            match result {
                Ok(mut result) => {
                    // Clean up running_tasks entry
                    kernel_clone.runtime.running_tasks.remove(&agent_id);

                    // task_plan in streaming path: log warning, plan not auto-executed
                    // (streaming clients expect real-time output; plan execution is for
                    // non-streaming/cron paths)
                    if result.plan.is_some() {
                        warn!("task_plan produced in streaming path — plan execution skipped (not supported in streaming mode)");
                        result.plan = None;
                    }

                    // Evolution hook — post-conversation auto-learning for clones
                    kernel_clone.maybe_run_evolution(&manifest, &message_owned, &result.response, owner_id.as_deref(), sender_id.as_deref());

                    // Multi-tenancy: update user profile
                    if let Some(ref sid) = &sender_id {
                        touch_user_profile(&kernel_clone.config.home_dir, owner_id.as_deref().unwrap_or(sid), &manifest.name, Some(sid));
                    }

                    // Write JSONL session mirror to workspace
                    if let Some(ref workspace) = manifest.workspace {
                        if let Err(e) = memory.write_jsonl_mirror(
                            &session,
                            &workspace.join("sessions"),
                            owner_id.as_deref(),
                            sender_id.as_deref(),
                            Some(&kernel_clone.config.home_dir),
                            Some(&manifest.name),
                        ) {
                            warn!("Failed to write JSONL session mirror (streaming): {e}");
                        }
                        // Append daily memory log (best-effort)
                        append_daily_memory_log(&kernel_clone.config.home_dir, &manifest.name, &result.response, owner_id.as_deref(), sender_id.as_deref());
                    }

                    kernel_clone
                        .runtime
                        .scheduler
                        .record_usage(agent_id, &result.total_usage);

                    // Persist usage and check budget thresholds
                    let model = manifest.model.modality.clone();
                    match kernel_clone.metering.record_and_check(
                        &memory::usage::UsageRecord {
                            agent_id,
                            model: model.clone(),
                            input_tokens: result.total_usage.input_tokens,
                            output_tokens: result.total_usage.output_tokens,
                            tool_calls: result.iterations.saturating_sub(1),
                        },
                    ) {
                        Ok(Some(alert)) => kernel_clone.handle_budget_alert(&alert),
                        Err(e) => warn!("Failed to record metering: {e}"),
                        _ => {}
                    }

                    if let Err(e) = kernel_clone
                        .registry
                        .set_state(agent_id, AgentState::Running)
                    {
                        tracing::warn!(agent_id = %agent_id, error = %e, "Failed to set agent state to Running");
                    }

                    // Post-loop compaction check: if session now exceeds token threshold,
                    // trigger compaction in background for the next call.
                    {
                        use runtime::compactor::{
                            estimate_token_count, needs_compaction_by_tokens, CompactionConfig,
                        };
                        let config = CompactionConfig::default();
                        let estimated = estimate_token_count(&session.messages, None, None);
                        if needs_compaction_by_tokens(estimated, &config) {
                            let compact_session_id = session.id;
                            let kc = kernel_clone.clone();
                            tokio::spawn(async move {
                                info!(agent_id = %agent_id, estimated_tokens = estimated, "Post-loop compaction triggered");
                                if let Err(e) = kc.compact_agent_session(agent_id, compact_session_id).await {
                                    warn!(agent_id = %agent_id, "Post-loop compaction failed: {e}");
                                }
                            });
                        }
                    }

                    Ok(result)
                }
                Err(e) => {
                    // Clean up running_tasks entry
                    kernel_clone.runtime.running_tasks.remove(&agent_id);

                    kernel_clone.runtime.supervisor.record_panic();
                    warn!(agent_id = %agent_id, error = %e, "Streaming agent loop failed");
                    Err(KernelError::Carrier(e))
                }
            }
        });

        // Store abort handle for cancellation support
        self.runtime
            .running_tasks
            .insert(agent_id, handle.abort_handle());

        Ok((rx, handle))
    }

    // ── Module dispatch: WASM / Python / LLM ───────────────────

    /// Execute a WASM module agent.
    ///
    /// Loads the `.wasm` or `.wat` file, maps manifest capabilities into
    /// `SandboxConfig`, and runs through the `WasmSandbox` engine.
    async fn execute_wasm_agent(
        &self,
        entry: &AgentEntry,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
    ) -> KernelResult<AgentLoopResult> {
        let module_path = entry.manifest.module.strip_prefix("wasm:").unwrap_or("");
        let wasm_path = self.resolve_module_path(module_path);

        info!(agent = %entry.name, path = %wasm_path.display(), "Executing WASM agent");

        let wasm_bytes = std::fs::read(&wasm_path).map_err(|e| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "Failed to read WASM module '{}': {e}",
                wasm_path.display()
            )))
        })?;

        // Map manifest capabilities to sandbox capabilities
        let caps = manifest_to_capabilities(&entry.manifest);
        let sandbox_config = SandboxConfig {
            fuel_limit: entry.manifest.resources.max_cpu_time_ms * 100_000,
            max_memory_bytes: entry.manifest.resources.max_memory_bytes as usize,
            capabilities: caps,
            timeout_secs: Some(30),
        };

        let input = serde_json::json!({
            "message": message,
            "agent_id": entry.id.to_string(),
            "agent_name": entry.name,
        });

        let result = self
            .runtime
            .wasm_sandbox
            .execute(
                &wasm_bytes,
                input,
                sandbox_config,
                kernel_handle,
                &entry.id.to_string(),
            )
            .await
            .map_err(|e| {
                KernelError::Carrier(CarrierError::Internal(format!(
                    "WASM execution failed: {e}"
                )))
            })?;

        // Extract response text from WASM output JSON
        let response = result
            .output
            .get("response")
            .and_then(|v| v.as_str())
            .or_else(|| result.output.get("text").and_then(|v| v.as_str()))
            .or_else(|| result.output.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| serde_json::to_string(&result.output).unwrap_or_default());

        info!(
            agent = %entry.name,
            fuel_consumed = result.fuel_consumed,
            "WASM agent execution complete"
        );

        Ok(AgentLoopResult {
            response,
            total_usage: types::message::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            iterations: 1,
            silent: false,
            directives: Default::default(),
            plan: None,
        })
    }

    /// Execute a Python script agent.
    ///
    /// Delegates to `python_runtime::run_python_agent()` via subprocess.
    async fn execute_python_agent(
        &self,
        entry: &AgentEntry,
        agent_id: AgentId,
        message: &str,
    ) -> KernelResult<AgentLoopResult> {
        let script_path = entry.manifest.module.strip_prefix("python:").unwrap_or("");
        let resolved_path = self.resolve_module_path(script_path);

        info!(agent = %entry.name, path = %resolved_path.display(), "Executing Python agent");

        let config = PythonConfig {
            timeout_secs: (entry.manifest.resources.max_cpu_time_ms / 1000).max(30),
            working_dir: Some(
                resolved_path
                    .parent()
                    .unwrap_or(Path::new("."))
                    .to_string_lossy()
                    .to_string(),
            ),
            ..PythonConfig::default()
        };

        let context = serde_json::json!({
            "agent_name": entry.name,
            "system_prompt": entry.manifest.model.system_prompt,
        });

        let result = python_runtime::run_python_agent(
            &resolved_path.to_string_lossy(),
            &agent_id.to_string(),
            message,
            &context,
            &config,
        )
        .await
        .map_err(|e| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "Python execution failed: {e}"
            )))
        })?;

        info!(agent = %entry.name, "Python agent execution complete");

        Ok(AgentLoopResult {
            response: result.response,
            total_usage: types::message::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            iterations: 1,
            silent: false,
            directives: Default::default(),
            plan: None,
        })
    }

    /// Execute the default LLM-based agent loop.
    #[allow(clippy::too_many_arguments)]
    async fn execute_llm_agent(
        &self,
        entry: &AgentEntry,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        content_blocks: Option<Vec<types::message::ContentBlock>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
        owner_id: Option<String>,
        channel_type: Option<String>,
        task_id: Option<String>,
        resume: Option<&memory::FlowRunRow>,
    ) -> KernelResult<AgentLoopResult> {
        // Prepare shared context (session, tools, flow/subagent matching, manifest)
        let ctx = self.prepare_agent_context(
            agent_id, message, entry, &sender_id, sender_name, &owner_id, &channel_type, task_id.as_deref(), resume,
        ).await?;
        let PreparedContext { mut session, needs_compact, tools, manifest, flow, .. } = ctx;

        // Execute compaction if needed
        if needs_compact {
            match self.compact_agent_session(agent_id, session.id).await {
                Ok(msg) => {
                    info!(agent_id = %agent_id, "{msg}");
                    if let Ok(Some(reloaded)) = self.memory.get_session_async(session.id).await {
                        session = reloaded;
                    }
                }
                Err(e) => {
                    warn!(agent_id = %agent_id, "Pre-emptive compaction failed: {e}");
                }
            }
        }

        // Re-acquire Brain reference for LLM call and plan execution
        let brain_ref: Option<Arc<dyn runtime::llm_driver::Brain>> =
            Some(Arc::clone(&*self.brain.brain.read().unwrap_or_else(|e| {
                warn!("Brain RwLock poisoned, recovering");
                e.into_inner()
            }))
                as Arc<dyn runtime::llm_driver::Brain>);

        // Extract MemoryHandle from kernel.
        let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
            Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(&self.memory))));

        // Model routing is handled by Brain

        let driver = self.resolve_driver(&manifest)?;

        // Context window lookup disabled — model name managed by Brain
        let ctx_window: Option<usize> = None;

        // Snapshot output directory before the agent loop to detect new files
        let output_dir_before = sender_id.as_ref().and_then(|sid| {
            manifest.workspace.as_ref().map(|_ws| {
                let oid = owner_id.as_deref().unwrap_or(sid);
                let dir = types::config::sender_data_dir(&self.config.home_dir, oid, &manifest.name, Some(sid)).join("output");
                let existing = std::fs::read_dir(&dir)
                    .ok()
                    .map(|rd| {
                        rd.filter_map(|e| e.ok())
                            .map(|e| e.file_name().to_string_lossy().to_string())
                            .collect::<std::collections::HashSet<String>>()
                    })
                    .unwrap_or_default();
                (dir, existing)
            })
        });

        // Build link context from user message (auto-extract URLs for the agent)
        let message_with_links = if let Some(link_ctx) =
            runtime::link_understanding::build_link_context(message, &self.config.links)
        {
            format!("{message}{link_ctx}")
        } else {
            message.to_string()
        };

        // Resume guard: if we came in to resume a flow but its definition is no
        // longer findable (deleted/renamed between suspend and resume), mark the
        // run failed and fall back to a normal single-step reply.
        let resume = match (resume, &flow) {
            (Some(rf), None) => {
                let completed = rf.completed_steps.clone();
                let _ = self.memory.flow_runs().update_status(&rf.run_id, "failed", &completed);
                warn!(agent = %entry.name, run_id = %rf.run_id, flow = %rf.flow_name, "resume aborted: flow def not found, marked failed");
                None
            }
            (r, _) => r,
        };

        let is_multi_step = flow
            .as_ref()
            .is_some_and(|fm| !fm.flow_def.steps.is_empty());

        let mut result = if is_multi_step {
            // Multi-step flow: execute as a DAG via run_flow.
            let fm = flow.as_ref().expect("checked above");
            let base_prompt = manifest.model.system_prompt.clone();
            // Build resume state when continuing a suspended flow.
            let resume_state = resume.map(|rf| {
                let pre_outputs: std::collections::HashMap<String, serde_json::Value> =
                    serde_json::from_str(&rf.completed_steps).unwrap_or_default();
                let waiting_step_id = rf.waiting_at.clone().unwrap_or_default();
                let cancel_keywords = fm
                    .flow_def
                    .steps
                    .iter()
                    .find(|s| s.id == waiting_step_id)
                    .map(|s| s.cancel_keywords.clone())
                    .unwrap_or_default();
                info!(agent = %entry.name, flow = %fm.name, run_id = %rf.run_id, step = %waiting_step_id, "Resuming suspended flow");
                crate::flow_runner::ResumeState {
                    run_id: rf.run_id.clone(),
                    pre_outputs,
                    waiting_step_id,
                    user_reply: message_with_links.clone(),
                    cancel_keywords,
                }
            });
            info!(
                agent = %entry.name,
                flow = %fm.name,
                steps = fm.flow_def.steps.len(),
                resuming = resume_state.is_some(),
                "Executing multi-step flow via run_flow"
            );
            let outcome = self
                .run_flow(
                    agent_id,
                    &fm.flow_def,
                    &base_prompt,
                    &message_with_links,
                    &mut session,
                    &manifest,
                    &tools,
                    brain_ref.as_ref(),
                    kernel_handle.clone(),
                    sender_id.as_deref(),
                    owner_id.as_deref(),
                    channel_type.as_deref(),
                    resume_state.as_ref(),
                )
                .await?;
            match outcome {
                crate::flow_runner::FlowOutcome::Completed(r) => r,
                crate::flow_runner::FlowOutcome::Suspended { question, total_usage, iterations } => {
                    // The flow paused at a `user_input` step: the question IS the
                    // reply to send. Skip plan/file/evolution post-processing.
                    let r = AgentLoopResult {
                        response: question,
                        total_usage,
                        iterations,
                        silent: false,
                        directives: Default::default(),
                        plan: None,
                    };
                    return self.finalize_suspended(r, agent_id, &manifest, &session, &sender_id, &owner_id).await;
                }
            }
        } else {
            run_agent_loop(
                &manifest,
                &message_with_links,
                &mut session,
                &self.memory,
                driver,
                &tools,
                kernel_handle.clone(),
                None, // stream_tx: non-streaming path
                Some(&self.plugins.mcp_connections),
                Some(&self.services.fetch_engine),
                manifest.workspace.as_deref(),
                None, // on_phase callback
                Some(&self.coordination.hooks),
                ctx_window,
                Some(&self.coordination.process_manager),
                content_blocks,
                brain_ref.clone(), // Brain for modality-based routing
                memory_handle.clone(), // Memory handle for kv/tree operations
                sender_id.as_deref(),
                owner_id.as_deref(),
                channel_type.as_deref(),
                Some(self.runtime.llm_concurrency_limit.clone()),
            )
            .await
            .map_err(KernelError::Carrier)?
        };

        // Detect new output files and append download URLs to the response

        // If agent produced a task_plan, execute it
        if let Some(plan) = result.plan.take() {
            info!(
                agent = %entry.name,
                plan_title = %plan.title,
                steps = plan.steps.len(),
                "Executing task_plan"
            );
            result = self.execute_plan(
                agent_id,
                &plan,
                &manifest,
                &tools,
                brain_ref.as_ref(),
                kernel_handle.clone(),
                sender_id.clone(),
                owner_id.clone(),
                channel_type.clone(),
            ).await?;
        }

        if let (Some((dir, before)), Some(ref sid), Some(ref ext_url)) =
            (&output_dir_before, &sender_id, &self.config.external_url)
        {
            let after: std::collections::HashSet<String> = std::fs::read_dir(dir)
                .ok()
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            let new_files: Vec<&String> = after.iter().filter(|f| !before.contains(*f)).collect();
            if !new_files.is_empty() {
                let base = ext_url.trim_end_matches('/');
                let aid = agent_id.to_string();
                let links: Vec<String> = new_files
                    .iter()
                    .map(|f| format!("{base}/api/agents/{aid}/output/{f}?sender_id={sid}"))
                    .collect();
                result.response.push_str("\n\n📎 生成的文件:\n");
                for link in &links {
                    result.response.push_str(&format!("- {link}\n"));
                }
            }
        }

        // Evolution hook — post-conversation auto-learning for clones
        self.maybe_run_evolution(&manifest, message, &result.response, owner_id.as_deref(), sender_id.as_deref());

        // Multi-tenancy: update user profile (touch last_seen, increment conversation_count)
        if let Some(ref sid) = sender_id {
            touch_user_profile(&self.config.home_dir, owner_id.as_deref().unwrap_or(sid), &manifest.name, Some(sid));
        }

        // Append new messages to canonical session for cross-channel memory

        // Write JSONL session mirror to workspace
        if let Some(ref workspace) = manifest.workspace {
            if let Err(e) = self.memory.write_jsonl_mirror(
                &session,
                &workspace.join("sessions"),
                owner_id.as_deref(),
                sender_id.as_deref(),
                Some(&self.config.home_dir),
                Some(&manifest.name),
            ) {
                warn!("Failed to write JSONL session mirror: {e}");
            }
            // Append daily memory log (best-effort)
            append_daily_memory_log(&self.config.home_dir, &manifest.name, &result.response, owner_id.as_deref(), sender_id.as_deref());
        }

        // Record usage and check budget thresholds
        let model = manifest.model.modality.clone();
        match self
            .metering
            .record_and_check(&memory::usage::UsageRecord {
                agent_id,
                model: model.clone(),
                input_tokens: result.total_usage.input_tokens,
                output_tokens: result.total_usage.output_tokens,
                tool_calls: result.iterations.saturating_sub(1),
            }) {
            Ok(Some(alert)) => self.handle_budget_alert(&alert),
            Err(e) => warn!("Failed to record metering: {e}"),
            _ => {}
        }

        Ok(result)
    }

    /// Light post-processing for a suspended flow: the `user_input` question is
    /// the reply the channel sends. Records the user-profile touch, JSONL
    /// session mirror, and metering, but skips plan execution, output-file
    /// detection, and evolution (those belong to a completed turn). The
    /// question was already appended to the session inside `run_flow`.
    async fn finalize_suspended(
        &self,
        r: AgentLoopResult,
        agent_id: AgentId,
        manifest: &AgentManifest,
        session: &memory::session::Session,
        sender_id: &Option<String>,
        owner_id: &Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        if let Some(ref sid) = sender_id {
            touch_user_profile(&self.config.home_dir, owner_id.as_deref().unwrap_or(sid), &manifest.name, Some(sid));
        }

        if let Some(ref workspace) = manifest.workspace {
            if let Err(e) = self.memory.write_jsonl_mirror(
                session,
                &workspace.join("sessions"),
                owner_id.as_deref(),
                sender_id.as_deref(),
                Some(&self.config.home_dir),
                Some(&manifest.name),
            ) {
                warn!("Failed to write JSONL session mirror: {e}");
            }
        }

        let model = manifest.model.modality.clone();
        match self
            .metering
            .record_and_check(&memory::usage::UsageRecord {
                agent_id,
                model,
                input_tokens: r.total_usage.input_tokens,
                output_tokens: r.total_usage.output_tokens,
                tool_calls: r.iterations.saturating_sub(1),
            }) {
            Ok(Some(alert)) => self.handle_budget_alert(&alert),
            Err(e) => warn!("Failed to record metering: {e}"),
            _ => {}
        }

        Ok(r)
    }

    /// Handle a budget threshold alert — log prominently and store for API exposure.
    pub(crate) fn handle_budget_alert(&self, alert: &crate::metering::BudgetAlert) {
        warn!(
            percent = alert.percent,
            used = alert.used_tokens,
            limit = alert.limit_tokens,
            "BUDGET ALERT: {}% of monthly token budget consumed ({}/{} tokens) — \
             configure alert_channel and alert_recipient in [budget] to receive notifications",
            alert.percent,
            alert.used_tokens,
            alert.limit_tokens
        );

        // Channel dispatch will be added in a follow-up via the plugin bridge.
        // The alert is exposed through the /api/budget endpoint and the
        // MeteringEngine's get_budget_status() method.
    }

    /// Resolve a module path relative to the kernel's home directory.
    ///
    /// If the path is absolute, return it as-is. Otherwise, resolve relative
    /// to `config.home_dir`.
    pub(crate) fn resolve_module_path(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.config.home_dir.join(path)
        }
    }

    /// Execute a task plan — run steps with topological ordering and parallel layers.
    #[allow(clippy::too_many_arguments)]
    async fn execute_plan(
        &self,
        agent_id: AgentId,
        plan: &runtime::agent_loop::TaskPlan,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn runtime::llm_driver::Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<String>,
        owner_id: Option<String>,
        channel_type: Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        use std::collections::HashMap;
        use std::sync::Arc;

        let mut step_outputs: HashMap<String, String> = HashMap::new();
        let mut total_usage = types::message::TokenUsage::default();
        let mut total_iterations = 0u32;

        let driver = self.resolve_driver(manifest)?;

        // Partition steps into parallel execution layers
        let layers = partition_steps_by_layers(&plan.steps);

        info!(
            plan_title = %plan.title,
            layers = layers.len(),
            total_steps = plan.steps.len(),
            "Plan execution starting"
        );

        for (layer_idx, layer) in layers.iter().enumerate() {
            let mut layer_handles = Vec::new();

            for step in layer {
                // Build step message: prompt + predecessor outputs
                let mut message = format!("## Task: {}\n\n{}", step.id, step.prompt);
                for dep_id in &step.depends_on {
                    if let Some(output) = step_outputs.get(dep_id) {
                        message.push_str(&format!("\n\n## Output from step '{}':\n{}", dep_id, output));
                    }
                }

                // Each step gets its own session
                let agent_name = self.registry.get(agent_id).map(|e| e.name.clone()).unwrap_or_else(|| agent_id.to_string());
                let step_session = self.memory.create_session_async(agent_name)
                    .await
                    .map_err(KernelError::Carrier)?;

                // Clone Arc references for the spawned task
                let memory = Arc::clone(&self.memory);
                let kh = kernel_handle.clone();
                let driver_clone = driver.clone();
                let brain_clone = brain.map(Arc::clone);
                let mh_clone: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
                    Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(&memory))));
                let tools_owned = tools.to_vec();
                let manifest_clone = manifest.clone();
                let sid = sender_id.clone();
                let oid = owner_id.clone();
                let ct = channel_type.clone();
                let ws = manifest.workspace.clone();
                let step_id = step.id.clone();

                info!(
                    step = %step_id,
                    layer = layer_idx,
                    depends_on = ?step.depends_on,
                    "Starting plan step"
                );

                let sem_clone = self.runtime.llm_concurrency_limit.clone();
                let mcp_arc = Arc::clone(&self.plugins.mcp_connections);

                let handle = tokio::spawn(async move {
                    let mut session = step_session;
                    let result = runtime::agent_loop::run_agent_loop(
                        &manifest_clone, &message, &mut session,
                        &memory, driver_clone, &tools_owned,
                        kh, None,
                        Some(&*mcp_arc),
                        None,   // fetch_engine: not available in spawned task
                        ws.as_deref(),
                        None,   // on_phase
                        None,   // hooks: not available in spawned task
                        None,   // context_window_tokens
                        None,   // process_manager
                        None,   // user_content_blocks
                        brain_clone,
                        mh_clone,
                        sid.as_deref(), oid.as_deref(),
                        ct.as_deref(),
                        Some(sem_clone),
                    ).await;
                    (step_id, result, session)
                });
                layer_handles.push(handle);
            }

            // Wait for all steps in this layer to complete
            for handle in layer_handles {
                match handle.await {
                    Ok((step_id, Ok(step_result), session)) => {
                        let _ = self.memory.save_session_async(&session).await;
                        info!(
                            step = %step_id,
                            iterations = step_result.iterations,
                            response_len = step_result.response.len(),
                            "Plan step completed"
                        );
                        step_outputs.insert(step_id, step_result.response);
                        total_usage.input_tokens += step_result.total_usage.input_tokens;
                        total_usage.output_tokens += step_result.total_usage.output_tokens;
                        total_iterations += step_result.iterations;
                    }
                    Ok((step_id, Err(e), _)) => {
                        warn!(step = %step_id, error = %e, "Plan step failed");
                        return Err(KernelError::Carrier(CarrierError::Internal(
                            format!("Plan step '{}' failed: {}", step_id, e)
                        )));
                    }
                    Err(e) => {
                        return Err(KernelError::Carrier(CarrierError::Internal(
                            format!("Plan step panicked: {}", e)
                        )));
                    }
                }
            }
        }

        // Final result = last step's output
        let final_output = plan.steps.last()
            .and_then(|s| step_outputs.get(&s.id))
            .cloned()
            .unwrap_or_default();

        info!(
            plan_title = %plan.title,
            total_iterations,
            steps_completed = step_outputs.len(),
            "Plan execution completed"
        );

        Ok(AgentLoopResult {
            response: final_output,
            total_usage,
            iterations: total_iterations,
            silent: false,
            directives: Default::default(),
            plan: None,
        })
    }
}

/// Partition task plan steps into parallel execution layers using topological ordering.
///
/// Steps in the same layer have no dependencies on each other and can run in parallel.
/// Each layer only contains steps whose dependencies are all in earlier layers.
fn partition_steps_by_layers(steps: &[runtime::agent_loop::TaskStep]) -> Vec<Vec<&runtime::agent_loop::TaskStep>> {
    use std::collections::HashMap;

    let step_map: HashMap<&str, &runtime::agent_loop::TaskStep> = steps.iter()
        .map(|s| (s.id.as_str(), s))
        .collect();

    let mut layer_of: HashMap<String, usize> = HashMap::new();

    // Compute layer for each step: layer = max(dep.layer) + 1, or 0 if no deps
    // Process in topological order (simple iterative approach)
    let mut changed = true;
    while changed {
        changed = false;
        for step in steps {
            let computed_layer = if step.depends_on.is_empty() {
                0
            } else {
                step.depends_on.iter()
                    .filter_map(|dep| layer_of.get(dep))
                    .max()
                    .map(|&l| l + 1)
                    .unwrap_or(0)
            };
            let current = layer_of.entry(step.id.clone()).or_insert(0);
            if computed_layer > *current {
                *current = computed_layer;
                changed = true;
            }
        }
    }

    // Assign layer 0 to any step not yet assigned (shouldn't happen but safety)
    for step in steps {
        layer_of.entry(step.id.clone()).or_insert(0);
    }

    // Group by layer
    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<&runtime::agent_loop::TaskStep>> = vec![Vec::new(); max_layer + 1];
    for step in steps {
        if let Some(&layer) = layer_of.get(&step.id) {
            layers[layer].push(step_map[step.id.as_str()]);
        }
    }

    layers
}
