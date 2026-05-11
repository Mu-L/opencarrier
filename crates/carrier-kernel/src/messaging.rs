//! Agent message dispatch and execution — send, stream, WASM, Python, LLM.
//!
//! Handles the core agent communication paths: plain send, streaming send,
//! and module-type dispatch (WASM sandbox, Python subprocess, LLM agent loop).

use carrier_runtime::agent_loop::{run_agent_loop, run_agent_loop_streaming, AgentLoopResult};
use carrier_runtime::kernel_handle::KernelHandle;
use carrier_runtime::llm_driver::StreamEvent;
use carrier_runtime::python_runtime::{self, PythonConfig};
use carrier_runtime::sandbox::SandboxConfig;
use carrier_types::agent::*;
use carrier_types::error::CarrierError;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

use crate::capabilities::manifest_to_capabilities;
use crate::error::{KernelError, KernelResult};
use crate::kernel::CarrierKernel;
use crate::prompt_sources::touch_user_profile;
use crate::workspace::append_daily_memory_log;

impl CarrierKernel {
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
        self.send_message_with_handle(agent_id, message, handle, None, None)
            .await
    }

    /// Send a multimodal message (text + images) to an agent and get a response.
    ///
    /// Send a message with an optional kernel handle for inter-agent tools.
    pub async fn send_message_with_handle(
        &self,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        self.send_message_with_handle_and_blocks(
            agent_id,
            message,
            kernel_handle,
            None,
            sender_id,
            sender_name,
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
    pub async fn send_message_with_handle_and_blocks(
        &self,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        content_blocks: Option<Vec<carrier_types::message::ContentBlock>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        // Acquire per-agent lock to serialize concurrent messages for the same agent.
        // This prevents session corruption when multiple messages arrive in quick
        // succession (e.g. rapid voice messages via Telegram). Messages for different
        // agents are not blocked — each agent has its own independent lock.
        let lock = self
            .runtime
            .agent_msg_locks
            .entry(agent_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

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
            // Default: LLM agent loop (builtin:chat or any unrecognized module)
            self.execute_llm_agent(
                &entry,
                agent_id,
                message,
                kernel_handle,
                content_blocks,
                sender_id,
                sender_name,
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
                let _ = self.registry.set_state(agent_id, AgentState::Running);

                // SECURITY: Record successful message in audit trail
                self.audit_log.record(
                    agent_id.to_string(),
                    carrier_runtime::audit::AuditAction::AgentMessage,
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
                    carrier_runtime::audit::AuditAction::AgentMessage,
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
    pub fn send_message_streaming(
        self: &Arc<Self>,
        agent_id: AgentId,
        message: &str,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
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
                                stop_reason: carrier_types::message::StopReason::EndTurn,
                                usage: result.total_usage,
                            })
                            .await;
                        kernel_clone
                            .runtime
                            .scheduler
                            .record_usage(agent_id, &result.total_usage);
                        let _ = kernel_clone
                            .registry
                            .set_state(agent_id, AgentState::Running);
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
        // Load session: use per-user session when sender_id is present (multi-tenancy),
        // otherwise use the agent's default session.
        let mut session = if let Some(ref sid) = sender_id {
            let user_label = format!("user:{}", sid);
            match self
                .memory
                .find_session_by_label(agent_id, &user_label)
                .map_err(KernelError::Carrier)?
            {
                Some(s) => s,
                None => self
                    .memory
                    .create_session_with_label(agent_id, Some(&user_label))
                    .map_err(KernelError::Carrier)?,
            }
        } else {
            self.memory
                .get_session(entry.session_id)
                .map_err(KernelError::Carrier)?
                .unwrap_or_else(|| carrier_memory::session::Session {
                    id: entry.session_id,
                    agent_id,
                    messages: Vec::new(),
                    context_window_tokens: 0,
                    label: None,
                })
        };

        // Check if auto-compaction is needed: message-count OR token-count OR quota-headroom trigger
        let needs_compact = {
            use carrier_runtime::compactor::{
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

        let tools = self.available_tools(agent_id);
        let tools = entry.mode.filter_tools(tools);
        let driver = self.resolve_driver(&entry.manifest)?;

        // Context window lookup disabled — model name managed by Brain
        let ctx_window: Option<usize> = None;

        let (tx, rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);
        let mut manifest = entry.manifest.clone();

        // Build the structured system prompt via prompt_builder
        {
            self.build_and_apply_prompt(&agent_id, &mut manifest, &tools, &sender_id, sender_name);
        }

        let memory = Arc::clone(&self.memory);
        // Build link context from user message (auto-extract URLs for the agent)
        let message_owned = if let Some(link_ctx) =
            carrier_runtime::link_understanding::build_link_context(message, &self.config.links)
        {
            format!("{message}{link_ctx}")
        } else {
            message.to_string()
        };
        let kernel_clone = Arc::clone(self);

        let handle = tokio::spawn(async move {
            // Clone Brain Arc before any .await so the RwLockReadGuard is dropped (not Send).
            let brain_ref: Option<Arc<dyn carrier_runtime::llm_driver::Brain>> =
                Some(Arc::clone(&*kernel_clone.brain.brain.read().unwrap())
                    as Arc<dyn carrier_runtime::llm_driver::Brain>);

            // Auto-compact if the session is large before running the loop
            if needs_compact {
                info!(agent_id = %agent_id, messages = session.messages.len(), "Auto-compacting session");
                match kernel_clone.compact_agent_session(agent_id).await {
                    Ok(msg) => {
                        info!(agent_id = %agent_id, "{msg}");
                        // Reload the session after compaction
                        if let Ok(Some(reloaded)) = memory.get_session(session.id) {
                            session = reloaded;
                        }
                    }
                    Err(e) => {
                        warn!(agent_id = %agent_id, "Auto-compaction failed: {e}");
                    }
                }
            }

            let messages_before = session.messages.len();

            // Create a phase callback that emits PhaseChange events to WS/SSE clients
            let phase_tx = tx.clone();
            let phase_cb: carrier_runtime::agent_loop::PhaseCallback =
                std::sync::Arc::new(move |phase| {
                    use carrier_runtime::agent_loop::LoopPhase;
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
                Some(&kernel_clone.services.web_ctx),
                Some(&kernel_clone.services.browser_ctx),
                manifest.workspace.as_deref(),
                Some(&phase_cb),
                if kernel_clone.config.docker.enabled {
                    Some(&kernel_clone.config.docker)
                } else {
                    None
                },
                Some(&kernel_clone.coordination.hooks),
                ctx_window,
                Some(&kernel_clone.coordination.process_manager),
                None,              // content_blocks (streaming path uses text only for now)
                brain_ref.clone(), // Brain for modality-based routing
                sender_id.as_deref(),
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
                Ok(result) => {
                    // Evolution hook — post-conversation auto-learning for clones
                    kernel_clone.maybe_run_evolution(&manifest, &message_owned, &result.response, sender_id.as_deref());

                    // Multi-tenancy: update user profile
                    if let Some(ref sid) = &sender_id {
                        touch_user_profile(&kernel_clone.config.home_dir, sid, &manifest.name);
                    }

                    // Append new messages to canonical session for cross-channel memory
                    if session.messages.len() > messages_before {
                        let _ = session.messages[messages_before..].to_vec();
                    }

                    // Write JSONL session mirror to workspace
                    if let Some(ref workspace) = manifest.workspace {
                        if let Err(e) = memory.write_jsonl_mirror(
                            &session,
                            &workspace.join("sessions"),
                            sender_id.as_deref(),
                            Some(&kernel_clone.config.home_dir),
                            Some(&manifest.name),
                        ) {
                            warn!("Failed to write JSONL session mirror (streaming): {e}");
                        }
                        // Append daily memory log (best-effort)
                        append_daily_memory_log(&kernel_clone.config.home_dir, &manifest.name, &result.response, sender_id.as_deref());
                    }

                    kernel_clone
                        .runtime
                        .scheduler
                        .record_usage(agent_id, &result.total_usage);

                    // Persist usage and check budget thresholds
                    let model = manifest.model.modality.clone();
                    match kernel_clone.metering.record_and_check(
                        &carrier_memory::usage::UsageRecord {
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

                    let _ = kernel_clone
                        .registry
                        .set_state(agent_id, AgentState::Running);

                    // Post-loop compaction check: if session now exceeds token threshold,
                    // trigger compaction in background for the next call.
                    {
                        use carrier_runtime::compactor::{
                            estimate_token_count, needs_compaction_by_tokens, CompactionConfig,
                        };
                        let config = CompactionConfig::default();
                        let estimated = estimate_token_count(&session.messages, None, None);
                        if needs_compaction_by_tokens(estimated, &config) {
                            let kc = kernel_clone.clone();
                            tokio::spawn(async move {
                                info!(agent_id = %agent_id, estimated_tokens = estimated, "Post-loop compaction triggered");
                                if let Err(e) = kc.compact_agent_session(agent_id).await {
                                    warn!(agent_id = %agent_id, "Post-loop compaction failed: {e}");
                                }
                            });
                        }
                    }

                    Ok(result)
                }
                Err(e) => {
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
            total_usage: carrier_types::message::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            iterations: 1,
            silent: false,
            directives: Default::default(),
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
            total_usage: carrier_types::message::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
            },
            iterations: 1,
            silent: false,
            directives: Default::default(),
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
        content_blocks: Option<Vec<carrier_types::message::ContentBlock>>,
        sender_id: Option<String>,
        sender_name: Option<String>,
    ) -> KernelResult<AgentLoopResult> {
        // Clone Brain Arc early so the RwLockReadGuard is dropped before any .await.
        let brain_ref: Option<Arc<dyn carrier_runtime::llm_driver::Brain>> =
            Some(Arc::clone(&*self.brain.brain.read().unwrap())
                as Arc<dyn carrier_runtime::llm_driver::Brain>);

        // Load session: use per-user session when sender_id is present (multi-tenancy),
        // otherwise use the agent's default session.
        let mut session = if let Some(ref sid) = sender_id {
            let user_label = format!("user:{}", sid);
            match self
                .memory
                .find_session_by_label(agent_id, &user_label)
                .map_err(KernelError::Carrier)?
            {
                Some(s) => s,
                None => self
                    .memory
                    .create_session_with_label(agent_id, Some(&user_label))
                    .map_err(KernelError::Carrier)?,
            }
        } else {
            self.memory
                .get_session(entry.session_id)
                .map_err(KernelError::Carrier)?
                .unwrap_or_else(|| carrier_memory::session::Session {
                    id: entry.session_id,
                    agent_id,
                    messages: Vec::new(),
                    context_window_tokens: 0,
                    label: None,
                })
        };

        // Pre-emptive compaction: compact before LLM call if session is large or quota headroom is low
        {
            use carrier_runtime::compactor::{
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
            let by_quota = if let Some(headroom) = self.runtime.scheduler.token_headroom(agent_id) {
                let threshold = (headroom as f64 * 0.8) as u64;
                estimated as u64 > threshold && session.messages.len() > 4
            } else {
                false
            };
            if by_messages || by_tokens || by_quota {
                info!(agent_id = %agent_id, messages = session.messages.len(), estimated_tokens = estimated, "Pre-emptive compaction before LLM call");
                match self.compact_agent_session(agent_id).await {
                    Ok(msg) => {
                        info!(agent_id = %agent_id, "{msg}");
                        if let Ok(Some(reloaded)) = self.memory.get_session(session.id) {
                            session = reloaded;
                        }
                    }
                    Err(e) => {
                        warn!(agent_id = %agent_id, "Pre-emptive compaction failed: {e}");
                    }
                }
            }
        }

        let messages_before = session.messages.len();

        let tools = self.available_tools(agent_id);
        let tools = entry.mode.filter_tools(tools);

        info!(
            agent = %entry.name,
            agent_id = %agent_id,
            tool_count = tools.len(),
            tool_names = ?tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            "Tools selected for LLM request"
        );

        // Apply model routing if configured (disabled in Stable mode)
        let mut manifest = entry.manifest.clone();

        self.build_and_apply_prompt(&agent_id, &mut manifest, &tools, &sender_id, sender_name);

        // Model routing is handled by Brain

        let driver = self.resolve_driver(&manifest)?;

        // Context window lookup disabled — model name managed by Brain
        let ctx_window: Option<usize> = None;

        // Snapshot output directory before the agent loop to detect new files
        let output_dir_before = sender_id.as_ref().and_then(|sid| {
            manifest.workspace.as_ref().map(|_ws| {
                let dir = carrier_types::config::sender_data_dir(&self.config.home_dir, sid, &manifest.name).join("output");
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
            carrier_runtime::link_understanding::build_link_context(message, &self.config.links)
        {
            format!("{message}{link_ctx}")
        } else {
            message.to_string()
        };

        let result = run_agent_loop(
            &manifest,
            &message_with_links,
            &mut session,
            &self.memory,
            driver,
            &tools,
            kernel_handle,
            Some(&self.plugins.mcp_connections),
            Some(&self.services.web_ctx),
            Some(&self.services.browser_ctx),
            manifest.workspace.as_deref(),
            None, // on_phase callback
            if self.config.docker.enabled {
                Some(&self.config.docker)
            } else {
                None
            },
            Some(&self.coordination.hooks),
            ctx_window,
            Some(&self.coordination.process_manager),
            content_blocks,
            brain_ref, // Brain for modality-based routing
            sender_id.as_deref(),
        )
        .await
        .map_err(KernelError::Carrier)?;

        // Detect new output files and append download URLs to the response
        let mut result = result;
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
        self.maybe_run_evolution(&manifest, message, &result.response, sender_id.as_deref());

        // Multi-tenancy: update user profile (touch last_seen, increment conversation_count)
        if let Some(ref sid) = sender_id {
            touch_user_profile(&self.config.home_dir, sid, &manifest.name);
        }

        // Append new messages to canonical session for cross-channel memory
        if session.messages.len() > messages_before {
            let _ = session.messages[messages_before..].to_vec();
        }

        // Write JSONL session mirror to workspace
        if let Some(ref workspace) = manifest.workspace {
            if let Err(e) = self.memory.write_jsonl_mirror(
                &session,
                &workspace.join("sessions"),
                sender_id.as_deref(),
                Some(&self.config.home_dir),
                Some(&manifest.name),
            ) {
                warn!("Failed to write JSONL session mirror: {e}");
            }
            // Append daily memory log (best-effort)
            append_daily_memory_log(&self.config.home_dir, &manifest.name, &result.response, sender_id.as_deref());
        }

        // Record usage and check budget thresholds
        let model = manifest.model.modality.clone();
        match self
            .metering
            .record_and_check(&carrier_memory::usage::UsageRecord {
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
}
