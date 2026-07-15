//! Multi-step flow DAG executor (`run_flow`).
//!
//! Executes a [`types::flow::FlowDef`] with non-empty `steps` as a topologically
//! ordered DAG. Each step runs by `kind` (`agent_loop` / `chat`; `tool` and
//! later kinds deferred). Supports `when` conditionals, `output` selection
//! (`llm` / `file:<path>` / `json`), `final` selection, and basic `on_failure`
//! degradation. Execution state is recorded in the `flow_runs` table.
//!
//! This is stage 2 incremental C (straight-through; `user_input` suspend/resume,
//! `map`, and `flow_exec` are later stages). It mirrors `execute_plan`:
//! topological layers, a fresh session per `agent_loop` step, `run_agent_loop`
//! invoked with `stream_tx: None`.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::StreamExt;
use serde_json::Value;
use tracing::{info, warn};

use memory::FlowRunRow;
use runtime::agent_loop::{
    run_agent_loop, AgentLoopResult, TOOL_LONG_TIMEOUT_NAMES, TOOL_TIMEOUT_LONG_SECS,
    TOOL_TIMEOUT_SECS,
};
use runtime::kernel_handle::KernelHandle;
use runtime::llm_driver::{Brain, CompletionRequest};
use runtime::plugin::admin_store::is_admin;
use runtime::tool_context::ToolContext;
use runtime::tool_runner::execute_tool;
use types::agent::{AgentId, AgentManifest};
use types::error::CarrierError;
use types::flow::{FlowDef, StepDef, StepKind, StepOutputMode};
use types::message::{Message, TokenUsage};
use types::tool::ToolResult;

use crate::error::{KernelError, KernelResult};
use crate::kernel::CarrierKernel;

// Recursion depth of `flow_exec`/`map` sub-flow calls within a task. Limits
// nested sub-flow invocation (mirrors `AGENT_CALL_DEPTH` in tool_runner).
tokio::task_local! {
    pub(crate) static FLOW_DEPTH: Cell<u32>;
}

/// Maximum `flow_exec`/`map` nesting depth.
const MAX_FLOW_DEPTH: u32 = 5;

/// Keywords that cancel a flow when the user replies to a **failure** progress
/// report (as opposed to a `user_input` step, which carries its own
/// `cancel_keywords`). Failure often correlates with the LLM/network being down
/// (e.g. 402 Insufficient Balance), so keyword matching -- not an LLM parse --
/// is the most robust trigger. Case-insensitive substring match (see
/// [`decide_cancel`]).
const FAILURE_CANCEL_KEYWORDS: &[&str] = &["取消", "cancel", "放弃", "abort", "算了", "不要了"];

/// Outcome of a `run_flow` invocation. A flow either runs to completion
/// (`Completed`) or suspends at a `user_input` step awaiting the human's reply
/// (`Suspended`).
pub(crate) enum FlowOutcome {
    /// The flow finished. `result.response` is the agent reply; `final_value`
    /// is the final step's structured output (used by `flow_exec` callers to
    /// pass structured results up the chain).
    Completed {
        result: AgentLoopResult,
        final_value: Option<Value>,
    },
    /// The flow suspended at a `user_input` step. `question` is the prompt to
    /// send to the user as the (intermediate) reply; the run is persisted as
    /// `waiting` and resumes on the user's next message.
    Suspended {
        question: String,
        total_usage: TokenUsage,
        iterations: u32,
    },
}

/// Outcome of executing a `map` step. `Done` carries the collected results
/// array; `Suspended` means the map's inline body paused at a `user_input`
/// step, persisting `map_context` so the iteration can resume.
pub(crate) enum MapOutcome {
    Done(Value, TokenUsage, u32),
    Suspended {
        question: String,
        /// The body `user_input` step id that is now waiting.
        body_step_id: String,
        /// Serialized [`MapContext`] to persist in `flow_runs.map_context`.
        map_context_json: String,
        expires_at: Option<String>,
        usage: TokenUsage,
        iterations: u32,
    },
}

/// Outcome of executing one element's inline body. `Done` carries the body's
/// outputs (for cancel detection) + final value (collected); `Suspended` means
/// a body `user_input` paused, carrying the body's outputs so far
/// (`body_completed`) up to the map loop.
enum BodyOutcome {
    Done {
        outputs: HashMap<String, Value>,
        final_value: Option<Value>,
        usage: TokenUsage,
        iterations: u32,
    },
    Suspended {
        question: String,
        step_id: String,
        outputs: HashMap<String, Value>,
        expires_at: Option<String>,
        usage: TokenUsage,
        iterations: u32,
    },
}

/// Resume state for an inline body (one element's body paused at a
/// `user_input` step). `body_completed` are the body steps done before the
/// suspend; the reply becomes the `waiting_step_id` step's `{ decision, text }`.
struct BodyResume {
    body_completed: HashMap<String, Value>,
    waiting_step_id: String,
    user_reply: String,
    cancel_keywords: Vec<String>,
}

/// Map iteration progress persisted when an interactive map's body suspends.
/// Stored as JSON in `flow_runs.map_context`; `waiting_at` holds the body
/// `user_input` step id.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct MapContext {
    pub map_step_id: String,
    pub over: Vec<Value>,
    pub current_index: usize,
    pub collected: Vec<Value>,
    pub body_completed: HashMap<String, Value>,
    #[serde(rename = "as")]
    pub as_name: String,
}

/// State carried into `run_flow` when resuming a suspended flow. `pre_outputs`
/// are the completed steps' snapshots (deserialized from `flow_runs`), and the
/// user's reply becomes the `waiting_step_id` step's output
/// `{ decision, text }`. `map_context` is set when the waiting step is inside
/// an interactive map's body (stage E.2).
pub(crate) struct ResumeState {
    pub run_id: String,
    pub pre_outputs: HashMap<String, Value>,
    pub waiting_step_id: String,
    pub user_reply: String,
    pub cancel_keywords: Vec<String>,
    pub map_context: Option<MapContext>,
}

impl CarrierKernel {
    /// Execute a multi-step flow as a DAG. Returns the final step's output as an
    /// [`AgentLoopResult`] (`Completed`), or `Suspended` when a `user_input`
    /// step pauses execution to await the human's reply.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_flow(
        &self,
        agent_id: AgentId,
        flow: &FlowDef,
        base_system_prompt: &str,
        user_message: &str,
        session: &mut memory::session::Session,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        resume: Option<&ResumeState>,
        input_overrides: Option<&serde_json::Map<String, Value>>,
    ) -> KernelResult<FlowOutcome> {
        let agent_name = self
            .registry
            .get(agent_id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| agent_id.to_string());

        // `input` is the template-context input (used by render/when/select);
        // it is also serialized into the flow_runs row on a fresh run. For a
        // sub-flow invoked via `flow_exec`/`map`, `input_overrides` carries the
        // rendered `with` params (e.g. `topic`) so `{{ input.topic }}` resolves.
        let mut input_map = serde_json::Map::new();
        input_map.insert("user_message".into(), Value::String(user_message.to_string()));
        input_map.insert("user_id".into(), Value::String(sender_id.unwrap_or("").to_string()));
        if let Some(overrides) = input_overrides {
            for (k, v) in overrides {
                input_map.insert(k.clone(), v.clone());
            }
        }
        let input: Value = Value::Object(input_map);

        // Record the run (history/audit; suspend/resume). On resume we reuse the
        // existing flow_runs row instead of creating a new one.
        let run_id = match resume {
            Some(r) => r.run_id.clone(),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                let now = chrono::Utc::now().to_rfc3339();
                let input_json = input.to_string();
                let _ = self.memory.flow_runs().create(&FlowRunRow {
                    run_id: id.clone(),
                    session_id: session.id.0.to_string(),
                    agent_id: agent_id.to_string(),
                    sender_id: sender_id.unwrap_or("").to_string(),
                    flow_name: flow.name.clone(),
                    input: input_json,
                    completed_steps: "{}".into(),
                    waiting_at: None,
                    map_context: None,
                    status: "running".into(),
                    created_at: now.clone(),
                    updated_at: now,
                    expires_at: None,
                });
                id
            }
        };

        let layers = partition_flow_steps(&flow.steps)
            .map_err(|e| KernelError::Carrier(CarrierError::Internal(e)))?;

        info!(
            flow = %flow.name,
            layers = layers.len(),
            steps = flow.steps.len(),
            "Flow execution starting"
        );

        // On resume, pre-populate outputs with the already-completed steps and
        // seed `executed_order` in flow order so the final-selection fallback
        // works. On a fresh run both start empty.
        let mut outputs: HashMap<String, Value> = match resume {
            Some(r) => r.pre_outputs.clone(),
            None => HashMap::new(),
        };
        let mut executed_order: Vec<String> = if resume.is_some() {
            flow.steps
                .iter()
                .map(|s| s.id.clone())
                .filter(|id| outputs.contains_key(id))
                .collect()
        } else {
            Vec::new()
        };
        let mut total_usage = TokenUsage::default();
        let mut total_iterations = 0u32;

        let mut failed: Option<KernelError> = None;

        'outer: for (layer_idx, layer) in layers.iter().enumerate() {
            for step in layer {
                // `when` gate (skipped steps do not produce outputs).
                if let Some(when) = &step.when {
                    if !eval_when(when, &outputs, &input) {
                        info!(flow = %flow.name, step = %step.id, "flow step skipped (when=false)");
                        continue;
                    }
                }

                // Resume: skip steps already completed before the suspend point.
                if outputs.contains_key(&step.id) {
                    continue;
                }

                // Resume: the user's reply becomes the `user_input` step's
                // output `{ decision, text }`. No LLM call; do not append to
                // session here (the completion path or a subsequent `UserInput`
                // branch appends the reply turn). For a failure-pause resume
                // (waiting step is NOT a `user_input`), the reply instead
                // resolves to cancel (-> done) or retry (-> re-run this step).
                if let Some(r) = resume {
                    if step.id == r.waiting_step_id {
                        let is_user_input = step.kind.as_ref() == Some(&StepKind::UserInput);
                        if is_user_input {
                            let decision = if decide_cancel(&r.user_reply, &r.cancel_keywords) {
                                "cancel"
                            } else {
                                "proceed"
                            };
                            outputs.insert(
                                step.id.clone(),
                                serde_json::json!({ "decision": decision, "text": r.user_reply }),
                            );
                            executed_order.push(step.id.clone());
                            let completed =
                                serde_json::to_string(&outputs).unwrap_or_else(|_| "{}".into());
                            let _ = self
                                .memory
                                .flow_runs()
                                .update_status(&run_id, "running", &completed);
                            info!(
                                flow = %flow.name,
                                step = %step.id,
                                decision,
                                "flow resumed with user reply"
                            );
                            continue;
                        } else {
                            // Failure-pause resume: interpret the reply with the
                            // fixed failure-cancel keywords (a failed step has
                            // no per-step `cancel_keywords`).
                            let keywords: Vec<String> =
                                FAILURE_CANCEL_KEYWORDS.iter().map(|s| s.to_string()).collect();
                            if decide_cancel(&r.user_reply, &keywords) {
                                let completed = serde_json::to_string(&outputs)
                                    .unwrap_or_else(|_| "{}".into());
                                let _ = self
                                    .memory
                                    .flow_runs()
                                    .update_status(&run_id, "cancelled", &completed);
                                info!(
                                    flow = %flow.name,
                                    step = %step.id,
                                    "flow cancelled by user at failed step"
                                );
                                return Ok(FlowOutcome::Completed {
                                    result: AgentLoopResult {
                                        response: format!("已取消流程「{}」。", flow.name),
                                        total_usage,
                                        iterations: total_iterations,
                                        silent: false,
                                        directives: Default::default(),
                                        plan: None,
                                    },
                                    final_value: None,
                                });
                            }
                            // Retry: fall through to dispatch to re-run this step.
                            info!(
                                flow = %flow.name,
                                step = %step.id,
                                "retrying failed step"
                            );
                        }
                    }
                }

                let kind = step.kind.as_ref().ok_or_else(|| {
                    KernelError::Carrier(CarrierError::Internal(format!(
                        "step '{}' has no kind",
                        step.id
                    )))
                })?;
                if !kind.is_executable() {
                    failed = Some(KernelError::Carrier(CarrierError::Internal(format!(
                        "step '{}' kind '{:?}' not yet supported in run_flow",
                        step.id, kind
                    ))));
                    break 'outer;
                }

                let step_prompt = step
                    .prompt
                    .as_deref()
                    .map(|p| render_template(p, &outputs, &input))
                    .unwrap_or_default();
                let step_user_msg = if step_prompt.is_empty() {
                    user_message.to_string()
                } else {
                    step_prompt.clone()
                };

                let dispatch: KernelResult<(Value, TokenUsage, u32)> = match kind {
                    StepKind::AgentLoop => {
                        self.run_step_agent_loop(
                            step,
                            &step_prompt,
                            &step_user_msg,
                            base_system_prompt,
                            &agent_name,
                            manifest,
                            tools,
                            brain,
                            kernel_handle.clone(),
                            sender_id,
                            owner_id,
                            channel_type,
                            &outputs,
                            &input,
                        )
                        .await
                    }
                    StepKind::Chat => {
                        self.run_step_chat(
                            step,
                            &step_user_msg,
                            base_system_prompt,
                            brain,
                            &outputs,
                            &input,
                        )
                        .await
                    }
                    StepKind::UserInput => {
                        // Suspend the flow: send the rendered prompt to the
                        // user as the (intermediate) reply, persist the run as
                        // `waiting`, and return `Suspended`.
                        let question = if step_prompt.is_empty() {
                            "请回复以继续。".to_string()
                        } else {
                            step_prompt.clone()
                        };
                        // Record the user turn + the question in the canonical
                        // session (mirrors the completion path's append below).
                        let new_messages =
                            vec![Message::user(user_message), Message::assistant(&question)];
                        session.messages.extend_from_slice(&new_messages);
                        let _ = self
                            .memory
                            .save_session_append_async(
                                session.id,
                                &agent_name,
                                &new_messages,
                                session.context_window_tokens,
                                session.label.as_deref(),
                                None,
                            )
                            .await;
                        // Compute the deadline and mark the run waiting.
                        let timeout_secs = step
                            .timeout_hours
                            .map(|h| (h * 3600.0) as u64)
                            .unwrap_or(self.config.user_input_timeout_secs);
                        let expires =
                            chrono::Utc::now() + chrono::Duration::seconds(timeout_secs as i64);
                        let _ = self.memory.flow_runs().set_waiting(
                            &run_id,
                            &step.id,
                            None,
                            Some(&expires.to_rfc3339()),
                        );
                        let completed =
                            serde_json::to_string(&outputs).unwrap_or_else(|_| "{}".into());
                        let _ = self
                            .memory
                            .flow_runs()
                            .update_status(&run_id, "waiting", &completed);
                        info!(
                            flow = %flow.name,
                            step = %step.id,
                            "flow suspended at user_input step"
                        );
                        return Ok(FlowOutcome::Suspended {
                            question,
                            total_usage,
                            iterations: total_iterations,
                        });
                    }
                    StepKind::FlowExec => {
                        self.exec_flow_step(
                            step,
                            agent_id,
                            manifest,
                            tools,
                            brain,
                            kernel_handle.clone(),
                            sender_id,
                            owner_id,
                            channel_type,
                            &outputs,
                            &input,
                            &agent_name,
                        )
                        .await
                    }
                    StepKind::Map => {
                        match self
                            .exec_map_step(
                                step,
                                agent_id,
                                manifest,
                                tools,
                                brain,
                                kernel_handle.clone(),
                                sender_id,
                                owner_id,
                                channel_type,
                                &outputs,
                                &input,
                                &agent_name,
                                base_system_prompt,
                                user_message,
                                resume,
                            )
                            .await
                        {
                            Ok(MapOutcome::Done(v, u, i)) => Ok((v, u, i)),
                            Ok(MapOutcome::Suspended {
                                question,
                                body_step_id,
                                map_context_json,
                                expires_at,
                                usage,
                                iterations,
                            }) => {
                                total_usage.input_tokens += usage.input_tokens;
                                total_usage.output_tokens += usage.output_tokens;
                                total_iterations += iterations;
                                // Mirror the UserInput suspend path, but the
                                // waiting step is the body user_input and we
                                // persist map iteration progress.
                                let new_messages = vec![
                                    Message::user(user_message),
                                    Message::assistant(&question),
                                ];
                                session.messages.extend_from_slice(&new_messages);
                                let _ = self
                                    .memory
                                    .save_session_append_async(
                                        session.id,
                                        &agent_name,
                                        &new_messages,
                                        session.context_window_tokens,
                                        session.label.as_deref(),
                                        None,
                                    )
                                    .await;
                                let _ = self.memory.flow_runs().set_waiting(
                                    &run_id,
                                    &body_step_id,
                                    Some(&map_context_json),
                                    expires_at.as_deref(),
                                );
                                let completed = serde_json::to_string(&outputs)
                                    .unwrap_or_else(|_| "{}".into());
                                let _ = self
                                    .memory
                                    .flow_runs()
                                    .update_status(&run_id, "waiting", &completed);
                                info!(
                                    flow = %flow.name,
                                    step = %step.id,
                                    body_step = %body_step_id,
                                    "flow suspended inside map body"
                                );
                                return Ok(FlowOutcome::Suspended {
                                    question,
                                    total_usage,
                                    iterations: total_iterations,
                                });
                            }
                            // A map-step error (malformed `over`, a failing
                            // sub-flow in the batch, an interactive-map body
                            // error) routes through the dispatch Err branch ->
                            // failure-pause, consistent with other step kinds.
                            Err(e) => Err(e),
                        }
                    }
                    StepKind::Tool => {
                        self.run_step_tool(
                            step,
                            agent_id,
                            manifest,
                            brain,
                            kernel_handle.clone(),
                            sender_id,
                            owner_id,
                            channel_type,
                            &outputs,
                            &input,
                            &agent_name,
                        )
                        .await
                    }
                    StepKind::Unknown(_) => unreachable!(),
                };

                match dispatch {
                    Ok((out_val, usage, iters)) => {
                        total_usage.input_tokens += usage.input_tokens;
                        total_usage.output_tokens += usage.output_tokens;
                        total_iterations += iters;

                        outputs.insert(step.id.clone(), out_val);
                        executed_order.push(step.id.clone());
                        info!(
                            flow = %flow.name,
                            step = %step.id,
                            layer = layer_idx,
                            "flow step completed"
                        );
                        let completed = serde_json::to_string(&outputs).unwrap_or_else(|_| "{}".into());
                        let _ = self
                            .memory
                            .flow_runs()
                            .update_status(&run_id, "running", &completed);
                    }
                    Err(e) => {
                        // Runtime error -> failure-pause: build a progress
                        // report, persist the run as `waiting` at this (failed)
                        // step, and suspend so the user can retry or cancel.
                        // Definition errors (no kind / not executable) are
                        // caught above and hard-fail via `failed`; this arm is
                        // only reached for dispatch errors.
                        let report = build_failure_report(flow, step, &e, &outputs);
                        warn!(
                            flow = %flow.name,
                            step = %step.id,
                            error = %e,
                            "flow paused at failed step"
                        );
                        let new_messages =
                            vec![Message::user(user_message), Message::assistant(&report)];
                        session.messages.extend_from_slice(&new_messages);
                        let _ = self
                            .memory
                            .save_session_append_async(
                                session.id,
                                &agent_name,
                                &new_messages,
                                session.context_window_tokens,
                                session.label.as_deref(),
                                None,
                            )
                            .await;
                        // Reuse the user_input timeout window; no per-step
                        // timeout_hours applies to failure-pause.
                        let timeout_secs = self.config.user_input_timeout_secs;
                        let expires =
                            chrono::Utc::now() + chrono::Duration::seconds(timeout_secs as i64);
                        let _ = self.memory.flow_runs().set_waiting(
                            &run_id,
                            &step.id,
                            None,
                            Some(&expires.to_rfc3339()),
                        );
                        let completed =
                            serde_json::to_string(&outputs).unwrap_or_else(|_| "{}".into());
                        let _ = self
                            .memory
                            .flow_runs()
                            .update_status(&run_id, "waiting", &completed);
                        return Ok(FlowOutcome::Suspended {
                            question: report,
                            total_usage,
                            iterations: total_iterations,
                        });
                    }
                }
            }
        }

        if let Some(e) = failed {
            let completed =
                serde_json::to_string(&outputs).unwrap_or_else(|_| "{}".into());
            let _ = self
                .memory
                .flow_runs()
                .update_status(&run_id, "failed", &completed);
            return Err(e);
        }

        // Final output: explicit `final` step (if executed), else last executed.
        let final_id = flow
            .final_step
            .as_deref()
            .filter(|id| outputs.contains_key(*id))
            .map(|s| s.to_string())
            .or_else(|| executed_order.last().cloned());
        let final_response = final_id
            .as_deref()
            .and_then(|id| outputs.get(id))
            .map(value_to_string)
            .unwrap_or_default();
        let final_value = final_id
            .as_deref()
            .and_then(|id| outputs.get(id))
            .cloned();

        // Record the user exchange in the canonical session (run_agent_loop
        // would have done this for the single-step path).
        let new_messages = vec![
            Message::user(user_message),
            Message::assistant(&final_response),
        ];
        session.messages.extend_from_slice(&new_messages);
        let _ = self
            .memory
            .save_session_append_async(
                session.id,
                &agent_name,
                &new_messages,
                session.context_window_tokens,
                session.label.as_deref(),
                None,
            )
            .await;

        let completed = serde_json::to_string(&outputs).unwrap_or_else(|_| "{}".into());
        let _ = self
            .memory
            .flow_runs()
            .update_status(&run_id, "completed", &completed);

        info!(
            flow = %flow.name,
            final_step = ?final_id,
            steps_completed = outputs.len(),
            total_iterations,
            "Flow execution completed"
        );

        Ok(FlowOutcome::Completed {
            result: AgentLoopResult {
                response: final_response,
                total_usage,
                iterations: total_iterations,
                silent: false,
                directives: Default::default(),
                plan: None,
            },
            final_value,
        })
    }

    /// Run a single `agent_loop` step in its own fresh session and return its
    /// output value + usage. Shared by `run_flow` (top-level) and
    /// `exec_body_steps` (map body). Resolves the driver + memory handle here
    /// so callers need not thread them.
    #[allow(clippy::too_many_arguments)]
    async fn run_step_agent_loop(
        &self,
        step: &StepDef,
        step_prompt: &str,
        step_user_msg: &str,
        base_system_prompt: &str,
        agent_name: &str,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let driver = self.resolve_driver(manifest)?;
        let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
            Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(
                &self.memory,
            ))));
        // base_system_prompt already carries the flow body (injected by
        // prepare_agent_context); add only the step directive.
        let step_system = format!(
            "{base_system_prompt}\n\n## 当前步骤: {}\n{step_prompt}",
            step.id,
        );
        let mut step_manifest = manifest.clone();
        step_manifest.model.system_prompt = step_system;
        let mut step_session = self
            .memory
            .create_session_async(agent_name.to_string())
            .await
            .map_err(KernelError::Carrier)?;
        let r = run_agent_loop(
            &step_manifest,
            step_user_msg,
            &mut step_session,
            &self.memory,
            driver,
            tools,
            kernel_handle,
            None,
            Some(&self.plugins.mcp_connections),
            Some(&self.services.fetch_engine),
            manifest.workspace.as_deref(),
            None,
            Some(&self.coordination.hooks),
            None,
            Some(&self.coordination.process_manager),
            None,
            brain.cloned(),
            memory_handle,
            sender_id,
            owner_id,
            channel_type,
            Some(self.runtime.llm_concurrency_limit.clone()),
        )
        .await
        .map_err(KernelError::Carrier)?;
        let out_val = select_output(step, &r.response, outputs, input)?;
        Ok((out_val, r.total_usage, r.iterations))
    }

    /// Run a single `chat` step (one-shot LLM completion, no tools) and return
    /// its output value + usage. Shared by `run_flow` and `exec_body_steps`.
    async fn run_step_chat(
        &self,
        step: &StepDef,
        step_user_msg: &str,
        base_system_prompt: &str,
        brain: Option<&Arc<dyn Brain>>,
        outputs: &HashMap<String, Value>,
        input: &Value,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let brain_ref = brain.ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal("chat step requires a brain".into()))
        })?;
        let task_text = step
            .task
            .as_deref()
            .map(|t| render_template(t, outputs, input))
            .unwrap_or_else(|| step_user_msg.to_string());
        let system = format!("{base_system_prompt}\n\n## 当前步骤: {}\n{task_text}", step.id);
        let req = CompletionRequest {
            model: String::new(),
            messages: vec![Message::user(step_user_msg.to_string())],
            tools: Vec::new(),
            max_tokens: 4096,
            temperature: 0.7,
            system: Some(system),
            thinking: None,
            extra: Default::default(),
        };
        let resp = brain_ref
            .complete("fast", req)
            .await
            .map_err(KernelError::Carrier)?;
        let final_msg = resp.text();
        let out_val = select_output(step, &final_msg, outputs, input)?;
        Ok((out_val, resp.usage, 1))
    }

    /// Run a single `tool` step: resolve the tool by name, render `tool_args`
    /// templates, execute it via the shared `execute_tool` (with permission +
    /// admin-gate + timeout), and return its output. A tool error becomes an
    /// `Err` so `run_flow`'s `on_failure` can degrade. Shared by `run_flow`
    /// (top-level) and `exec_body_steps` (map body).
    #[allow(clippy::too_many_arguments)]
    async fn run_step_tool(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let tool_name = step.tool_name.as_deref().ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "tool step '{}' missing `tool`/`tool_name`",
                step.id
            )))
        })?;
        let rendered_args = render_value(&step.tool_args, outputs, input);

        // Assemble the ToolContext (mirrors runtime/agent_loop/tool_use.rs).
        let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
            Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(
                &self.memory,
            ))));
        let caller_id = agent_id.to_string();
        let workspace_root: Option<&Path> = manifest.workspace.as_deref();
        let is_clone_admin =
            matches!((sender_id, workspace_root), (Some(sid), Some(root)) if is_admin(root, sid));
        let tool_ctx = ToolContext {
            kernel: kernel_handle.as_ref(),
            memory: memory_handle.as_ref(),
            caller_agent_id: Some(&caller_id),
            mcp_connections: Some(&self.plugins.mcp_connections),
            fetch_engine: Some(&self.services.fetch_engine),
            allowed_env_vars: None,
            workspace_root,
            brain,
            exec_policy: manifest.exec_policy.as_ref(),
            cli_exec_config: manifest.cli_exec.as_ref(),
            process_manager: Some(&self.coordination.process_manager),
            sender_id,
            owner_id,
            home_dir: Some(self.config.home_dir.as_path()),
            agent_name: Some(agent_name),
            subagent_configs: if manifest.subagents.is_empty() {
                None
            } else {
                Some(&manifest.subagents)
            },
            channel_type,
            max_tool_level: manifest.max_tool_level,
            is_clone_admin,
        };

        let tool_use_id = format!("flow:{}:{}", step.id, tool_name);
        let timeout_secs = if TOOL_LONG_TIMEOUT_NAMES.contains(&tool_name) {
            TOOL_TIMEOUT_LONG_SECS
        } else {
            TOOL_TIMEOUT_SECS
        };
        let result = match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            execute_tool(&tool_use_id, tool_name, &rendered_args, &tool_ctx),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                warn!(
                    flow = "tool_step",
                    step = %step.id,
                    tool = tool_name,
                    "tool step timed out after {}s",
                    timeout_secs
                );
                ToolResult {
                    tool_use_id,
                    content: format!("Tool '{}' timed out after {}s.", tool_name, timeout_secs),
                    is_error: true,
                }
            }
        };

        if result.is_error {
            return Err(KernelError::Carrier(CarrierError::Internal(format!(
                "tool step '{}' ('{}') failed: {}",
                step.id, tool_name, result.content
            ))));
        }
        // Tool content is often JSON; parse to a structured value when possible,
        // else keep the raw string.
        let out_val = serde_json::from_str::<Value>(&result.content)
            .unwrap_or_else(|_| Value::String(result.content.clone()));
        // Tool execution uses no LLM tokens; count as one iteration.
        Ok((out_val, TokenUsage::default(), 1))
    }

    /// Execute a `flow_exec` step: invoke the named sub-flow once and return
    /// its final value as this step's output.
    #[allow(clippy::too_many_arguments)]
    async fn exec_flow_step(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        self.invoke_subflow(
            step, agent_id, manifest, tools, brain, kernel_handle, sender_id, owner_id,
            channel_type, outputs, input, agent_name,
        )
        .await
    }

    /// Execute a `map` step. Dispatches on the body form:
    /// - `body: [steps]` set -> interactive map (stage E.2): iterate `over`,
    ///   running the inline body per element; a body `user_input` suspends with
    ///   `map_context`.
    /// - else (`flow`+`with`) -> batch map (stage E.1): invoke the named
    ///   sub-flow per element, collect results.
    #[allow(clippy::too_many_arguments)]
    async fn exec_map_step(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
        base_system_prompt: &str,
        user_message: &str,
        resume: Option<&ResumeState>,
    ) -> KernelResult<MapOutcome> {
        if step.body.is_some() {
            self.exec_interactive_map(
                step,
                agent_id,
                manifest,
                tools,
                brain,
                kernel_handle,
                sender_id,
                owner_id,
                channel_type,
                outputs,
                input,
                agent_name,
                base_system_prompt,
                user_message,
                resume,
            )
            .await
        } else {
            let (v, u, i) = self
                .exec_map_batch(
                    step,
                    agent_id,
                    manifest,
                    tools,
                    brain,
                    kernel_handle,
                    sender_id,
                    owner_id,
                    channel_type,
                    outputs,
                    input,
                    agent_name,
                )
                .await?;
            Ok(MapOutcome::Done(v, u, i))
        }
    }

    /// Batch `map` (stage E.1): iterate `over`, running the named sub-flow once
    /// per element with the element bound to `as`, and collect final values.
    /// `step.parallel` (>1) runs sub-flows concurrently (order preserved);
    /// `None`/`1` is serial and short-circuits on the first error.
    #[allow(clippy::too_many_arguments)]
    async fn exec_map_batch(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        let over_tpl = step.over.as_deref().ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "map step '{}' missing `over`",
                step.id
            )))
        })?;
        let arr = render_over_array(step, over_tpl, outputs, input)?;
        let as_name = step.as_name.as_deref().unwrap_or("item").to_string();
        let parallel = step.parallel.unwrap_or(1).max(1) as usize;
        let mut collected: Vec<Value> = Vec::new();
        let mut total_usage = TokenUsage::default();
        let mut total_iters = 0u32;

        info!(
            flow = "map",
            step = %step.id,
            elements = arr.len(),
            parallel,
            "map step iterating"
        );
        if parallel <= 1 {
            // Serial: short-circuit on first error (preserves stage E.1 behavior).
            for element in arr {
                // Inject the element under `as_name` into a cloned outputs map so
                // bare `{{ as_name.field }}` templates resolve via resolve_path.
                let mut sub_outputs = outputs.clone();
                sub_outputs.insert(as_name.clone(), element);
                let (val, usage, iters) = self
                    .invoke_subflow(
                        step, agent_id, manifest, tools, brain, kernel_handle.clone(), sender_id,
                        owner_id, channel_type, &sub_outputs, input, agent_name,
                    )
                    .await?;
                collected.push(val);
                total_usage.input_tokens += usage.input_tokens;
                total_usage.output_tokens += usage.output_tokens;
                total_iters += iters;
            }
        } else {
            // Parallel: run up to `parallel` sub-flows concurrently while
            // yielding results in input order (`buffered` preserves order).
            // NOTE: concurrent siblings share the FLOW_DEPTH task_local Cell, so
            // the depth guard may over-count for deeply nested parallel maps --
            // acceptable (safe-fail at the limit, never data loss); each loop's
            // LLM calls are still bounded by the global llm_concurrency_limit.
            let futs = arr.into_iter().map(|element| {
                let mut sub_outputs = outputs.clone();
                sub_outputs.insert(as_name.clone(), element);
                // `Option<Arc<_>>` isn't Copy: clone once per element in the
                // sync closure, then move the owned clone into the async block.
                let kernel_handle = kernel_handle.clone();
                // `async move` owns `sub_outputs`/`kernel_handle` so the borrows
                // inside outlive the future; the `&self`/`&step`/... refs are Copy.
                async move {
                    self.invoke_subflow(
                        step, agent_id, manifest, tools, brain, kernel_handle, sender_id,
                        owner_id, channel_type, &sub_outputs, input, agent_name,
                    )
                    .await
                }
            });
            let results: Vec<KernelResult<(Value, TokenUsage, u32)>> =
                futures::stream::iter(futs).buffered(parallel).collect().await;
            // First error (in order) aborts; later results are discarded.
            for r in results {
                let (val, usage, iters) = r?;
                collected.push(val);
                total_usage.input_tokens += usage.input_tokens;
                total_usage.output_tokens += usage.output_tokens;
                total_iters += iters;
            }
        }
        info!(flow = "map", step = %step.id, collected = collected.len(), "map step completed");
        Ok((Value::Array(collected), total_usage, total_iters))
    }

    /// Interactive `map` (stage E.2): iterate `over` serially, running the
    /// inline `body` steps per element. A body `user_input` suspends the whole
    /// flow, persisting `map_context` (iteration progress) so the next user
    /// message resumes from the same element. A body cancel terminates the map
    /// (collected so far is preserved).
    #[allow(clippy::too_many_arguments)]
    async fn exec_interactive_map(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
        base_system_prompt: &str,
        user_message: &str,
        resume: Option<&ResumeState>,
    ) -> KernelResult<MapOutcome> {
        let body_steps = step.body.as_ref().ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "interactive map step '{}' missing `body`",
                step.id
            )))
        })?;
        // An interactive map can suspend per element (body `user_input`), so it
        // MUST run serially -- parallelism would break resume (which element is
        // waiting?). Reject `parallel > 1` up front.
        if step.parallel.unwrap_or(1) > 1 {
            return Err(KernelError::Carrier(CarrierError::Internal(format!(
                "interactive map step '{}' has body (can suspend) and must be serial (parallel<=1), got parallel={}",
                step.id,
                step.parallel.unwrap_or(1)
            ))));
        }
        let as_name = step.as_name.as_deref().unwrap_or("item").to_string();

        // Resume: reconstruct iteration state from map_context. Otherwise fresh.
        let (over, mut current_index, mut collected, mut body_completed, mut resuming_element) =
            if let Some(mc) = resume.and_then(|r| r.map_context.as_ref()) {
                if mc.map_step_id == step.id {
                    (
                        mc.over.clone(),
                        mc.current_index,
                        mc.collected.clone(),
                        mc.body_completed.clone(),
                        true,
                    )
                } else {
                    // map_context is for a different map step (defensive): fresh.
                    let over = render_over_array(step, step.over.as_deref().unwrap_or(""), outputs, input)?;
                    (over, 0usize, Vec::new(), HashMap::new(), false)
                }
            } else {
                let over_tpl = step.over.as_deref().ok_or_else(|| {
                    KernelError::Carrier(CarrierError::Internal(format!(
                        "map step '{}' missing `over`",
                        step.id
                    )))
                })?;
                let over = render_over_array(step, over_tpl, outputs, input)?;
                (over, 0usize, Vec::new(), HashMap::new(), false)
            };

        // When resuming, the body's waiting user_input step + the user's reply.
        let (body_step_id, user_reply, body_cancel_keywords) = if resuming_element {
            let bid = resume.unwrap().waiting_step_id.clone();
            let reply = resume.unwrap().user_reply.clone();
            let kws = body_steps
                .iter()
                .find(|s| s.id == bid)
                .map(|s| s.cancel_keywords.clone())
                .unwrap_or_default();
            (Some(bid), reply, kws)
        } else {
            (None, String::new(), Vec::new())
        };

        let mut acc_usage = TokenUsage::default();
        let mut acc_iters = 0u32;

        info!(
            flow = "map",
            step = %step.id,
            elements = over.len(),
            current_index,
            resuming = resuming_element,
            "interactive map step iterating"
        );

        while current_index < over.len() {
            let element = over[current_index].clone();
            let body_resume = if resuming_element {
                Some(BodyResume {
                    body_completed: body_completed.clone(),
                    waiting_step_id: body_step_id.clone().unwrap_or_default(),
                    user_reply: user_reply.clone(),
                    cancel_keywords: body_cancel_keywords.clone(),
                })
            } else {
                None
            };

            match self
                .exec_body_steps(
                    body_steps,
                    &as_name,
                    &element,
                    agent_id,
                    manifest,
                    tools,
                    brain,
                    kernel_handle.clone(),
                    sender_id,
                    owner_id,
                    channel_type,
                    outputs,
                    input,
                    agent_name,
                    base_system_prompt,
                    user_message,
                    body_resume.as_ref(),
                )
                .await?
            {
                BodyOutcome::Done {
                    outputs: bo,
                    final_value,
                    usage,
                    iterations,
                } => {
                    acc_usage.input_tokens += usage.input_tokens;
                    acc_usage.output_tokens += usage.output_tokens;
                    acc_iters += iterations;
                    // A resumed element means the user just replied: a cancel
                    // decision terminates the whole map (this element is NOT
                    // collected; prior elements are preserved).
                    if resuming_element
                        && body_step_id
                            .as_deref()
                            .and_then(|bid| bo.get(bid))
                            .and_then(|v| v.get("decision"))
                            .and_then(|v| v.as_str())
                            == Some("cancel")
                    {
                        info!(
                            flow = "map",
                            step = %step.id,
                            current_index,
                            "interactive map terminated by body cancel"
                        );
                        break;
                    }
                    collected.push(final_value.unwrap_or(Value::Null));
                    body_completed.clear();
                    current_index += 1;
                    resuming_element = false;
                }
                BodyOutcome::Suspended {
                    question,
                    step_id,
                    outputs: bo,
                    expires_at,
                    usage,
                    iterations,
                } => {
                    acc_usage.input_tokens += usage.input_tokens;
                    acc_usage.output_tokens += usage.output_tokens;
                    acc_iters += iterations;
                    body_completed = bo;
                    let mc = MapContext {
                        map_step_id: step.id.clone(),
                        over: over.clone(),
                        current_index,
                        collected: collected.clone(),
                        body_completed: body_completed.clone(),
                        as_name: as_name.clone(),
                    };
                    let map_context_json = serde_json::to_string(&mc)
                        .map_err(|e| KernelError::Carrier(CarrierError::Internal(e.to_string())))?;
                    info!(
                        flow = "map",
                        step = %step.id,
                        body_step = %step_id,
                        current_index,
                        "interactive map suspended at body user_input"
                    );
                    return Ok(MapOutcome::Suspended {
                        question,
                        body_step_id: step_id,
                        map_context_json,
                        expires_at,
                        usage: acc_usage,
                        iterations: acc_iters,
                    });
                }
            }
        }

        info!(
            flow = "map",
            step = %step.id,
            collected = collected.len(),
            "interactive map step completed"
        );
        Ok(MapOutcome::Done(Value::Array(collected), acc_usage, acc_iters))
    }

    /// Execute one element's inline body steps (the `body: [steps]` of an
    /// interactive map). Mirrors `run_flow`'s DAG loop (layers, `when` gates,
    /// skip-completed, resume-inject) but does NOT persist to `flow_runs` or
    /// append to the canonical session -- the parent map arm owns those.
    /// Returns `Suspended` when a body `user_input` pauses. Body supports only
    /// `agent_loop`/`chat`/`user_input`/`tool` (other kinds error).
    #[allow(clippy::too_many_arguments)]
    async fn exec_body_steps(
        &self,
        body_steps: &[StepDef],
        as_name: &str,
        element: &Value,
        agent_id: AgentId,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        parent_outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
        base_system_prompt: &str,
        user_message: &str,
        resume: Option<&BodyResume>,
    ) -> KernelResult<BodyOutcome> {
        // Body render context = parent outputs (for {{ parse_shots }} etc.)
        // overlaid with body-completed steps, then the current element under
        // `as_name` (so bare {{ ep.field }} resolves).
        let mut outputs = parent_outputs.clone();
        if let Some(r) = resume {
            for (k, v) in &r.body_completed {
                outputs.insert(k.clone(), v.clone());
            }
        }
        outputs.insert(as_name.to_string(), element.clone());

        let layers = partition_flow_steps(body_steps)
            .map_err(|e| KernelError::Carrier(CarrierError::Internal(e)))?;
        let mut executed_order: Vec<String> = if resume.is_some() {
            body_steps
                .iter()
                .map(|s| s.id.clone())
                .filter(|id| outputs.contains_key(id))
                .collect()
        } else {
            Vec::new()
        };
        let mut total_usage = TokenUsage::default();
        let mut total_iterations = 0u32;

        for layer in &layers {
            for step in layer {
                // `when` gate.
                if let Some(when) = &step.when {
                    if !eval_when(when, &outputs, input) {
                        continue;
                    }
                }
                // Skip already-completed body steps (resume).
                if outputs.contains_key(&step.id) {
                    continue;
                }
                // Resume-inject: the user's reply becomes the waiting
                // user_input step's output { decision, text }. No persistence
                // here (the parent map arm owns flow_runs).
                if let Some(r) = resume {
                    if step.id == r.waiting_step_id {
                        let decision = if decide_cancel(&r.user_reply, &r.cancel_keywords) {
                            "cancel"
                        } else {
                            "proceed"
                        };
                        outputs.insert(
                            step.id.clone(),
                            serde_json::json!({ "decision": decision, "text": r.user_reply }),
                        );
                        executed_order.push(step.id.clone());
                        continue;
                    }
                }

                let kind = step.kind.as_ref().ok_or_else(|| {
                    KernelError::Carrier(CarrierError::Internal(format!(
                        "body step '{}' has no kind",
                        step.id
                    )))
                })?;
                if !matches!(
                    kind,
                    StepKind::AgentLoop | StepKind::Chat | StepKind::UserInput | StepKind::Tool
                ) {
                    return Err(KernelError::Carrier(CarrierError::Internal(format!(
                        "body step '{}' kind '{:?}' not yet supported in map body",
                        step.id, kind
                    ))));
                }

                let step_prompt = step
                    .prompt
                    .as_deref()
                    .map(|p| render_template(p, &outputs, input))
                    .unwrap_or_default();
                let step_user_msg = if step_prompt.is_empty() {
                    user_message.to_string()
                } else {
                    step_prompt.clone()
                };

                if *kind == StepKind::UserInput {
                    let question = if step_prompt.is_empty() {
                        "请回复以继续。".to_string()
                    } else {
                        step_prompt.clone()
                    };
                    let timeout_secs = step
                        .timeout_hours
                        .map(|h| (h * 3600.0) as u64)
                        .unwrap_or(self.config.user_input_timeout_secs);
                    let expires =
                        chrono::Utc::now() + chrono::Duration::seconds(timeout_secs as i64);
                    return Ok(BodyOutcome::Suspended {
                        question,
                        step_id: step.id.clone(),
                        outputs,
                        expires_at: Some(expires.to_rfc3339()),
                        usage: total_usage,
                        iterations: total_iterations,
                    });
                }

                let (v, u, i) = if *kind == StepKind::AgentLoop {
                    self.run_step_agent_loop(
                        step,
                        &step_prompt,
                        &step_user_msg,
                        base_system_prompt,
                        agent_name,
                        manifest,
                        tools,
                        brain,
                        kernel_handle.clone(),
                        sender_id,
                        owner_id,
                        channel_type,
                        &outputs,
                        input,
                    )
                    .await?
                } else if *kind == StepKind::Tool {
                    self.run_step_tool(
                        step,
                        agent_id,
                        manifest,
                        brain,
                        kernel_handle.clone(),
                        sender_id,
                        owner_id,
                        channel_type,
                        &outputs,
                        input,
                        agent_name,
                    )
                    .await?
                } else {
                    // Chat
                    self.run_step_chat(
                        step,
                        &step_user_msg,
                        base_system_prompt,
                        brain,
                        &outputs,
                        input,
                    )
                    .await?
                };
                total_usage.input_tokens += u.input_tokens;
                total_usage.output_tokens += u.output_tokens;
                total_iterations += i;
                outputs.insert(step.id.clone(), v);
                executed_order.push(step.id.clone());
            }
        }

        let final_value = executed_order
            .last()
            .and_then(|id| outputs.get(id))
            .cloned();
        Ok(BodyOutcome::Done {
            outputs,
            final_value,
            usage: total_usage,
            iterations: total_iterations,
        })
    }

    /// Shared sub-flow invocation for `flow_exec` and `map`. Loads the sub-flow
    /// by name, builds its `input` from `step.with` (rendered) plus the parent
    /// `user_message`/`user_id` passthrough, runs it in a fresh session under a
    /// depth scope, and returns its final value + usage.
    #[allow(clippy::too_many_arguments)]
    async fn invoke_subflow(
        &self,
        step: &StepDef,
        agent_id: AgentId,
        manifest: &AgentManifest,
        tools: &[types::tool::ToolDefinition],
        brain: Option<&Arc<dyn Brain>>,
        kernel_handle: Option<Arc<dyn KernelHandle>>,
        sender_id: Option<&str>,
        owner_id: Option<&str>,
        channel_type: Option<&str>,
        outputs: &HashMap<String, Value>,
        input: &Value,
        agent_name: &str,
    ) -> KernelResult<(Value, TokenUsage, u32)> {
        // Depth guard against runaway recursion (flow_exec -> flow_exec -> ...).
        let current = FLOW_DEPTH.try_with(|d| d.get()).unwrap_or(0);
        if current >= MAX_FLOW_DEPTH {
            return Err(KernelError::Carrier(CarrierError::Internal(format!(
                "flow_exec depth limit ({}) exceeded at step '{}'",
                MAX_FLOW_DEPTH, step.id
            ))));
        }

        let flow_name = step.flow.as_deref().ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "step '{}' (flow_exec/map) missing `flow`",
                step.id
            )))
        })?;
        let workspace = manifest.workspace.as_deref().ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(
                "flow_exec requires a manifest workspace".into(),
            ))
        })?;
        let sub_match = crate::prompt_sources::load_flow_by_name(workspace, flow_name)
            .ok_or_else(|| {
                KernelError::Carrier(CarrierError::Internal(format!(
                    "flow_exec step '{}' references unknown flow '{}'",
                    step.id, flow_name
                )))
            })?;
        let sub_flow = &sub_match.flow_def;

        // Sub-flows invoked via flow_exec cannot suspend (no resume stack).
        // This includes interactive map bodies (which contain user_input).
        if flow_contains_user_input(sub_flow) {
            return Err(KernelError::Carrier(CarrierError::Internal(format!(
                "flow_exec sub-flow '{}' contains a user_input step (not allowed)",
                flow_name
            ))));
        }

        // Render each `with` value as a template against the current outputs
        // (for `map`, the element is already injected under `as_name`). These
        // become the sub-flow's `input.<key>` via `input_overrides`.
        let mut rendered_with: serde_json::Map<String, Value> = serde_json::Map::new();
        for (k, v) in &step.with {
            let tpl = v.as_str().unwrap_or("");
            let rendered = render_template(tpl, outputs, input);
            rendered_with.insert(k.clone(), Value::String(rendered));
        }
        // The root user_message is passed through (single-step sub-flows use it
        // as the topic via `{{ input.user_message }}`).
        let sub_user_msg = input
            .get("user_message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Fresh sub-session (independent of the parent's); sub-flow rows are
        // created for audit by run_flow itself.
        let mut sub_session = self
            .memory
            .create_session_async(agent_name.to_string())
            .await
            .map_err(KernelError::Carrier)?;
        let sub_base_prompt = format!(
            "{}\n\n## 子 flow: {}\n{}",
            manifest.model.system_prompt, sub_flow.name, sub_flow.body
        );

        info!(flow = %sub_flow.name, parent_step = %step.id, "flow_exec invoking sub-flow");
        let outcome = FLOW_DEPTH
            .scope(Cell::new(current + 1), async {
                // `Box::pin` breaks the recursive async future's infinite size
                // (run_flow -> dispatch -> invoke_subflow -> run_flow ...).
                Box::pin(self.run_flow(
                    agent_id,
                    sub_flow,
                    &sub_base_prompt,
                    &sub_user_msg,
                    &mut sub_session,
                    manifest,
                    tools,
                    brain,
                    kernel_handle,
                    sender_id,
                    owner_id,
                    channel_type,
                    None,
                    Some(&rendered_with),
                ))
                .await
            })
            .await?;

        match outcome {
            FlowOutcome::Completed { result, final_value } => {
                let val = final_value.unwrap_or_else(|| Value::String(result.response.clone()));
                Ok((val, result.total_usage, result.iterations))
            }
            FlowOutcome::Suspended { .. } => Err(KernelError::Carrier(CarrierError::Internal(
                format!(
                    "flow_exec sub-flow '{}' suspended unexpectedly (should be pre-checked)",
                    flow_name
                ),
            ))),
        }
    }
}

/// Partition flow steps into topological execution layers. Returns an error on
/// cycles or references to unknown step ids.
fn partition_flow_steps(steps: &[StepDef]) -> Result<Vec<Vec<&StepDef>>, String> {
    use std::collections::{HashMap, HashSet};

    let map: HashMap<&str, &StepDef> = steps.iter().map(|s| (s.id.as_str(), s)).collect();

    // Validate dependencies exist.
    for s in steps {
        for d in &s.depends_on {
            if !map.contains_key(d.as_str()) {
                return Err(format!("step '{}' depends on unknown step '{}'", s.id, d));
            }
        }
    }

    // Cycle detection (DFS).
    let mut visited: HashSet<String> = HashSet::new();
    for s in steps {
        if has_cycle(&s.id, &map, &mut HashSet::new(), &mut visited) {
            return Err(format!("dependency cycle detected involving step '{}'", s.id));
        }
    }

    // Layer assignment: layer = max(dep.layer) + 1, or 0 if no deps.
    let mut layer_of: HashMap<String, usize> = HashMap::new();
    let mut changed = true;
    while changed {
        changed = false;
        for s in steps {
            let computed = if s.depends_on.is_empty() {
                0
            } else {
                s.depends_on
                    .iter()
                    .filter_map(|d| layer_of.get(d))
                    .copied()
                    .max()
                    .map(|l| l + 1)
                    .unwrap_or(0)
            };
            let cur = layer_of.entry(s.id.clone()).or_insert(0);
            if computed > *cur {
                *cur = computed;
                changed = true;
            }
        }
    }

    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<&StepDef>> = vec![Vec::new(); max_layer + 1];
    for s in steps {
        let l = layer_of[&s.id];
        layers[l].push(s);
    }
    Ok(layers)
}

fn has_cycle(
    id: &str,
    map: &std::collections::HashMap<&str, &StepDef>,
    on_stack: &mut std::collections::HashSet<String>,
    visited: &mut std::collections::HashSet<String>,
) -> bool {
    use std::collections::HashSet;
    if on_stack.contains(id) {
        return true;
    }
    if visited.contains(id) {
        return false;
    }
    visited.insert(id.to_string());
    on_stack.insert(id.to_string());
    if let Some(s) = map.get(id) {
        for d in &s.depends_on {
            if has_cycle(d, map, on_stack, visited) {
                return true;
            }
        }
    }
    on_stack.remove(id);
    let _ = HashSet::<String>::new();
    false
}

/// Render a map step's `over` template and parse it as a JSON array. Errors
/// clearly if the template does not resolve to an array.
fn render_over_array(
    step: &StepDef,
    over_tpl: &str,
    outputs: &HashMap<String, Value>,
    input: &Value,
) -> KernelResult<Vec<Value>> {
    let over_str = render_template(over_tpl, outputs, input);
    serde_json::from_str::<Value>(&over_str)
        .map_err(|e| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "map step '{}' `over` did not resolve to a JSON array: {} (got: {})",
                step.id, e, over_str
            )))
        })?
        .as_array()
        .cloned()
        .ok_or_else(|| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "map step '{}' `over` resolved to a non-array",
                step.id
            )))
        })
}

/// True if a flow contains any `user_input` step, including inside interactive
/// map `body` blocks (recursive). Used to reject flow_exec sub-flows that would
/// suspend (no resume stack for sub-flows).
fn flow_contains_user_input(flow: &FlowDef) -> bool {
    flow.steps.iter().any(step_or_body_has_user_input)
}

fn step_or_body_has_user_input(step: &StepDef) -> bool {
    step.kind.as_ref() == Some(&StepKind::UserInput)
        || step
            .body
            .as_ref()
            .is_some_and(|body| body.iter().any(step_or_body_has_user_input))
}

/// Resolve a step's output to a JSON value based on its `output` mode.
fn select_output(
    step: &StepDef,
    final_msg: &str,
    outputs: &HashMap<String, Value>,
    input: &Value,
) -> KernelResult<Value> {
    match step.output_mode() {
        StepOutputMode::Llm => Ok(Value::String(final_msg.to_string())),
        StepOutputMode::Json => serde_json::from_str::<Value>(final_msg).map_err(|e| {
            KernelError::Carrier(CarrierError::Internal(format!(
                "step '{}' output:json parse failed: {}",
                step.id, e
            )))
        }),
        StepOutputMode::File(path) => {
            let rendered = render_template(&path, outputs, input);
            std::fs::read_to_string(&rendered).map(Value::String).map_err(|e| {
                KernelError::Carrier(CarrierError::Internal(format!(
                    "step '{}' output:file '{}' missing: {}",
                    step.id, rendered, e
                )))
            })
        }
    }
}

/// Render `{{ ... }}` templates. Supports `{{ outputs.id }}`, `{{ outputs.id.field }}`,
/// `{{ input.key }}`, and bare `{{ id }}` (treated as `outputs.id`). Unresolved
/// expressions are left intact.
fn render_template(tpl: &str, outputs: &HashMap<String, Value>, input: &Value) -> String {
    let mut out = String::new();
    let mut rest = tpl;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let expr = after[..end].trim();
                let original = &rest[start..start + 2 + end + 2]; // full "{{ ... }}"
                match resolve_path(expr, outputs, input) {
                    Some(v) => out.push_str(&value_to_string(&v)),
                    None => out.push_str(original),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                out.push_str(after);
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Recursively render `{{ }}` templates inside a JSON value tree: each string
/// leaf is rendered via [`render_template`], object/array structure and
/// non-string leaves are preserved. Used to render a `tool` step's `tool_args`.
fn render_value(v: &Value, outputs: &HashMap<String, Value>, input: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(render_template(s, outputs, input)),
        Value::Object(m) => {
            let rendered: serde_json::Map<String, Value> = m
                .iter()
                .map(|(k, vv)| (k.clone(), render_value(vv, outputs, input)))
                .collect();
            Value::Object(rendered)
        }
        Value::Array(a) => Value::Array(a.iter().map(|vv| render_value(vv, outputs, input)).collect()),
        other => other.clone(),
    }
}

/// Resolve a dotted path (`outputs.id.field`, `input.key`, or bare `id`) to a
/// JSON value. Bare paths are treated as `outputs.<path>`.
fn resolve_path(
    path: &str,
    outputs: &HashMap<String, Value>,
    input: &Value,
) -> Option<Value> {
    let path = path.trim();
    let (root, rest): (&str, &str) = if let Some(p) = path.strip_prefix("outputs.") {
        ("outputs", p)
    } else if let Some(p) = path.strip_prefix("input.") {
        ("input", p)
    } else {
        ("outputs", path)
    };
    if rest.is_empty() {
        return None;
    }
    let parts: Vec<&str> = rest.split('.').collect();
    let mut cur: Value = if root == "outputs" {
        outputs.get(parts[0])?.clone()
    } else {
        input.get(parts[0])?.clone()
    };
    for f in &parts[1..] {
        cur = cur.get(f)?.clone();
    }
    Some(cur)
}

/// Evaluate a `when` expression: `LHS == 'rhs'` / `LHS != 'rhs'` (a missing LHS
/// -> false, so chains of skips propagate). A bare expression is truthy if it
/// resolves.
fn eval_when(expr: &str, outputs: &HashMap<String, Value>, input: &Value) -> bool {
    let expr = expr.trim();
    if let Some((lhs, rhs)) = expr.split_once("==") {
        let lhs_val = resolve_path(lhs, outputs, input);
        let rhs_str = rhs.trim().trim_matches('\'').trim_matches('"');
        lhs_val
            .map(|v| value_to_string(&v).trim() == rhs_str)
            .unwrap_or(false)
    } else if let Some((lhs, rhs)) = expr.split_once("!=") {
        let lhs_val = resolve_path(lhs, outputs, input);
        let rhs_str = rhs.trim().trim_matches('\'').trim_matches('"');
        lhs_val
            .map(|v| value_to_string(&v).trim() != rhs_str)
            .unwrap_or(false)
    } else {
        resolve_path(expr, outputs, input).is_some()
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Decide whether a `user_input` reply cancels the flow: true if the reply
/// case-insensitively contains any of the `cancel_keywords`. Empty keywords
/// => never cancel.
fn decide_cancel(reply: &str, keywords: &[String]) -> bool {
    let reply_lower = reply.to_lowercase();
    keywords
        .iter()
        .any(|kw| !kw.is_empty() && reply_lower.contains(&kw.to_lowercase()))
}

/// Build a human-readable progress report when a step fails at runtime (tool
/// error, LLM error, sub-flow error, ...). Lists completed steps (with a short
/// summary of their output), the failed step (with the error), and pending
/// steps, then prompts the user to retry or cancel. The report doubles as the
/// `Suspended { question }` payload so it flows through the same messaging path
/// as a `user_input` suspend.
fn build_failure_report(
    flow: &FlowDef,
    failed_step: &StepDef,
    err: &KernelError,
    outputs: &HashMap<String, Value>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("流程「{}」执行中断：\n", flow.name));
    for s in &flow.steps {
        if s.id == failed_step.id {
            lines.push(format!("❌ {}  失败：{}", s.id, err));
        } else if let Some(v) = outputs.get(&s.id) {
            let summary = truncate_summary(&value_to_string(v), 50);
            lines.push(format!("✅ {}  {}", s.id, summary));
        } else {
            lines.push(format!("⏳ {}  （未执行）", s.id));
        }
    }
    lines.push(format!(
        "\n回复「重试」重新执行「{}」，或「取消」终止流程。",
        failed_step.id
    ));
    lines.join("\n")
}

/// Truncate a string to at most `max` characters (by Unicode scalar), appending
/// `…` when truncated. Keeps multi-byte (CJK) output readable.
fn truncate_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::flow::StepDef;

    fn step(id: &str, deps: &[&str]) -> StepDef {
        StepDef {
            id: id.into(),
            kind: Some(StepKind::Chat),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn partition_linear() {
        let steps = vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])];
        let layers = partition_flow_steps(&steps).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0][0].id, "a");
        assert_eq!(layers[2][0].id, "c");
    }

    #[test]
    fn partition_parallel_layer() {
        let steps = vec![step("a", &[]), step("b", &[]), step("c", &["a", "b"])];
        let layers = partition_flow_steps(&steps).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0].len(), 2);
        assert_eq!(layers[1].len(), 1);
    }

    #[test]
    fn partition_detects_cycle() {
        let steps = vec![step("a", &["b"]), step("b", &["a"])];
        assert!(partition_flow_steps(&steps).is_err());
    }

    #[test]
    fn partition_unknown_dep() {
        let steps = vec![step("a", &["missing"])];
        assert!(partition_flow_steps(&steps).is_err());
    }

    #[test]
    fn render_outputs_and_input() {
        let mut outputs = HashMap::new();
        outputs.insert("draft".into(), Value::String("hello".into()));
        let input = serde_json::json!({"user_message": "hi"});
        let r = render_template("{{ outputs.draft }} | {{ input.user_message }}", &outputs, &input);
        assert_eq!(r, "hello | hi");
    }

    #[test]
    fn render_bare_is_outputs() {
        let mut outputs = HashMap::new();
        outputs.insert("draft".into(), Value::String("hello".into()));
        let input = serde_json::json!({});
        assert_eq!(render_template("{{ draft }}", &outputs, &input), "hello");
    }

    #[test]
    fn render_unresolved_kept() {
        let outputs = HashMap::new();
        let input = serde_json::json!({});
        assert_eq!(render_template("{{ outputs.missing }}", &outputs, &input), "{{ outputs.missing }}");
    }

    #[test]
    fn when_eq_true() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "review".into(),
            serde_json::json!({"decision": "revise"}),
        );
        let input = serde_json::json!({});
        assert!(eval_when("review.decision == 'revise'", &outputs, &input));
        assert!(!eval_when("review.decision == 'proceed'", &outputs, &input));
    }

    #[test]
    fn when_missing_lhs_is_false() {
        let outputs = HashMap::new();
        let input = serde_json::json!({});
        // skipped step (no output) -> false (chain skip)
        assert!(!eval_when("review.decision == 'revise'", &outputs, &input));
        assert!(!eval_when("review.decision != 'cancel'", &outputs, &input));
    }

    #[test]
    fn when_review_decision_not_cancel() {
        let mut outputs = HashMap::new();
        let input = serde_json::json!({});
        // proceed -> downstream `when: review.decision != 'cancel'` runs
        outputs.insert("review".into(), serde_json::json!({"decision": "proceed"}));
        assert!(eval_when("review.decision != 'cancel'", &outputs, &input));
        // cancel -> downstream gated step is skipped
        outputs.insert("review".into(), serde_json::json!({"decision": "cancel"}));
        assert!(!eval_when("review.decision != 'cancel'", &outputs, &input));
    }

    #[test]
    fn decide_cancel_matches() {
        let kw = vec!["取消".to_string(), "cancel".to_string(), "算了".to_string()];
        assert!(decide_cancel("算了吧", &kw));
        assert!(decide_cancel("please cancel now", &kw));
        assert!(decide_cancel("取消", &kw));
        assert!(!decide_cancel("继续生成", &kw));
        assert!(!decide_cancel("ok", &kw));
        // empty keywords -> never cancel
        assert!(!decide_cancel("取消", &[]));
        // case-insensitive
        assert!(decide_cancel("CANCEL please", &kw));
    }

    #[test]
    fn select_output_json() {
        let step = StepDef {
            id: "p".into(),
            output: Some("json".into()),
            ..Default::default()
        };
        let outputs = HashMap::new();
        let input = serde_json::json!({});
        let v = select_output(&step, r#"{"a":1}"#, &outputs, &input).unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn select_output_json_parse_fail() {
        let step = StepDef {
            id: "p".into(),
            output: Some("json".into()),
            ..Default::default()
        };
        let outputs = HashMap::new();
        let input = serde_json::json!({});
        assert!(select_output(&step, "not json", &outputs, &input).is_err());
    }

    #[test]
    fn render_as_binding_resolves_element_fields() {
        // map injects the current element under the `as` name into a cloned
        // outputs map; bare `{{ shot.prompt }}` then resolves via resolve_path.
        let mut outputs = HashMap::new();
        outputs.insert(
            "shot".into(),
            serde_json::json!({"prompt": "a sunset", "duration": 3}),
        );
        let input = serde_json::json!({});
        assert_eq!(
            render_template("{{ shot.prompt }}", &outputs, &input),
            "a sunset"
        );
        assert_eq!(
            render_template("{{ shot.duration }}", &outputs, &input),
            "3"
        );
    }

    #[test]
    fn partition_handles_flow_exec_and_map_steps() {
        // flow_exec/map steps participate in topological layering like any step.
        let steps = vec![
            StepDef {
                id: "gen".into(),
                kind: Some(StepKind::Chat),
                output: Some("json".into()),
                ..Default::default()
            },
            StepDef {
                id: "batch".into(),
                kind: Some(StepKind::Map),
                over: Some("{{ gen }}".into()),
                as_name: Some("shot".into()),
                flow: Some("shot-image".into()),
                depends_on: vec!["gen".into()],
                ..Default::default()
            },
            StepDef {
                id: "merge".into(),
                kind: Some(StepKind::FlowExec),
                flow: Some("video-merger".into()),
                depends_on: vec!["batch".into()],
                ..Default::default()
            },
        ];
        let layers = partition_flow_steps(&steps).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0][0].id, "gen");
        assert_eq!(layers[1][0].id, "batch");
        assert_eq!(layers[2][0].id, "merge");
    }

    #[test]
    fn flow_contains_user_input_detects_body() {
        use types::flow::FlowDef;
        // Batch map (no body) -> no user_input.
        let batch = FlowDef {
            steps: vec![StepDef {
                id: "batch".into(),
                kind: Some(StepKind::Map),
                over: Some("{{ x }}".into()),
                flow: Some("sub".into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(!flow_contains_user_input(&batch));

        // Interactive map body with user_input -> detected (recursive).
        let interactive = FlowDef {
            steps: vec![StepDef {
                id: "per_ep".into(),
                kind: Some(StepKind::Map),
                over: Some("{{ eps }}".into()),
                body: Some(vec![
                    StepDef {
                        id: "write".into(),
                        kind: Some(StepKind::Chat),
                        ..Default::default()
                    },
                    StepDef {
                        id: "review".into(),
                        kind: Some(StepKind::UserInput),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(flow_contains_user_input(&interactive));

        // Top-level user_input (no body) -> detected.
        let top = FlowDef {
            steps: vec![StepDef {
                id: "review".into(),
                kind: Some(StepKind::UserInput),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(flow_contains_user_input(&top));
    }

    #[test]
    fn partition_handles_map_with_body() {
        // A map step carrying an inline body still participates in topological
        // layering; the body steps are NOT part of the top-level DAG.
        let steps = vec![
            StepDef {
                id: "eps".into(),
                kind: Some(StepKind::Chat),
                output: Some("json".into()),
                ..Default::default()
            },
            StepDef {
                id: "per_ep".into(),
                kind: Some(StepKind::Map),
                over: Some("{{ eps }}".into()),
                as_name: Some("ep".into()),
                depends_on: vec!["eps".into()],
                body: Some(vec![
                    StepDef {
                        id: "write".into(),
                        kind: Some(StepKind::Chat),
                        ..Default::default()
                    },
                    StepDef {
                        id: "review".into(),
                        kind: Some(StepKind::UserInput),
                        depends_on: vec!["write".into()],
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            },
        ];
        let layers = partition_flow_steps(&steps).unwrap();
        assert_eq!(layers.len(), 2);
        assert_eq!(layers[0][0].id, "eps");
        assert_eq!(layers[1][0].id, "per_ep");
        // Body does not leak into the top-level partition.
        assert_eq!(layers[1].len(), 1);
    }

    #[test]
    fn map_context_roundtrip() {
        let mc = MapContext {
            map_step_id: "per_ep".into(),
            over: vec![serde_json::json!({"index": 1}), serde_json::json!({"index": 2})],
            current_index: 1,
            collected: vec![serde_json::json!({"decision": "proceed"})],
            body_completed: {
                let mut m = HashMap::new();
                m.insert("write".into(), Value::String("ep1 text".into()));
                m
            },
            as_name: "ep".into(),
        };
        let json = serde_json::to_string(&mc).unwrap();
        // `as` is the serialized key for as_name.
        assert!(json.contains("\"as\":\"ep\""));
        assert!(json.contains("\"current_index\":1"));
        let back: MapContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.map_step_id, "per_ep");
        assert_eq!(back.current_index, 1);
        assert_eq!(back.over.len(), 2);
        assert_eq!(back.collected.len(), 1);
        assert_eq!(back.as_name, "ep");
        assert_eq!(
            back.body_completed.get("write").and_then(|v| v.as_str()),
            Some("ep1 text")
        );
    }

    #[test]
    fn render_value_renders_string_leaves() {
        // String leaves are templates; object/array structure and non-string
        // leaves (numbers, bools) are preserved.
        let mut outputs = HashMap::new();
        outputs.insert("name".into(), Value::String("晨曦".into()));
        outputs.insert("count".into(), serde_json::json!(3));
        let input = serde_json::json!({"user_message": "hi"});
        let args = serde_json::json!({
            "title": "{{ name }}",
            "raw": "literal text",
            "n": 5,
            "flag": true,
            "nested": ["{{ name }}", 7, {"deep": "{{ name }}"}]
        });
        let rendered = render_value(&args, &outputs, &input);
        assert_eq!(rendered["title"].as_str(), Some("晨曦"));
        assert_eq!(rendered["raw"].as_str(), Some("literal text"));
        assert_eq!(rendered["n"].as_i64(), Some(5));
        assert_eq!(rendered["flag"].as_bool(), Some(true));
        assert_eq!(rendered["nested"][0].as_str(), Some("晨曦"));
        assert_eq!(rendered["nested"][1].as_i64(), Some(7));
        assert_eq!(rendered["nested"][2]["deep"].as_str(), Some("晨曦"));
    }

    #[test]
    fn build_failure_report_lists_all_steps() {
        use types::flow::FlowDef;
        let flow = FlowDef {
            name: "draft-review".into(),
            steps: vec![
                StepDef {
                    id: "draft".into(),
                    kind: Some(StepKind::Chat),
                    ..Default::default()
                },
                StepDef {
                    id: "read".into(),
                    kind: Some(StepKind::Tool),
                    ..Default::default()
                },
                StepDef {
                    id: "publish".into(),
                    kind: Some(StepKind::Chat),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // `draft` completed, `read` failed, `publish` pending.
        let mut outputs = HashMap::new();
        outputs.insert("draft".into(), Value::String("a draft about cats".into()));
        let failed = &flow.steps[1];
        let err = KernelError::Carrier(CarrierError::Internal(
            "file '/tmp/x' not found".into(),
        ));
        let report = build_failure_report(&flow, failed, &err, &outputs);

        // Completed step shows id + truncated summary.
        assert!(report.contains("✅ draft"));
        assert!(report.contains("a draft about cats"));
        // Failed step shows id + error.
        assert!(report.contains("❌ read"));
        assert!(report.contains("file '/tmp/x' not found"));
        // Pending step shows id + (未执行).
        assert!(report.contains("⏳ publish"));
        assert!(report.contains("（未执行）"));
        // Retry/cancel hint mentions the failed step id.
        assert!(report.contains("重试"));
        assert!(report.contains("取消"));
        assert!(report.contains("「read」"));
        // Header names the flow.
        assert!(report.contains("draft-review"));
    }

    #[test]
    fn build_failure_report_truncates_long_summary() {
        use types::flow::FlowDef;
        let flow = FlowDef {
            name: "f".into(),
            steps: vec![
                StepDef {
                    id: "big".into(),
                    kind: Some(StepKind::Chat),
                    ..Default::default()
                },
                StepDef {
                    id: "boom".into(),
                    kind: Some(StepKind::Tool),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let long = "x".repeat(200);
        let mut outputs = HashMap::new();
        outputs.insert("big".into(), Value::String(long));
        let failed = &flow.steps[1];
        let err = KernelError::Carrier(CarrierError::Internal("boom".into()));
        let report = build_failure_report(&flow, failed, &err, &outputs);
        // Summary capped at 50 chars + ellipsis.
        let big_line = report
            .lines()
            .find(|l| l.starts_with("✅ big"))
            .unwrap();
        let summary = big_line.strip_prefix("✅ big  ").unwrap();
        assert_eq!(summary.chars().count(), 51); // 50 + …
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn failure_cancel_keywords_match() {
        // decide_cancel against FAILURE_CANCEL_KEYWORDS.
        let kw: Vec<String> = FAILURE_CANCEL_KEYWORDS.iter().map(|s| s.to_string()).collect();
        assert!(decide_cancel("取消", &kw));
        assert!(decide_cancel("please cancel now", &kw));
        assert!(decide_cancel("算了吧", &kw));
        assert!(decide_cancel("ABORT mission", &kw));
        // Non-cancel replies do not match (retry intent).
        assert!(!decide_cancel("重试", &kw));
        assert!(!decide_cancel("继续", &kw));
        assert!(!decide_cancel("再试一次", &kw));
        assert!(!decide_cancel("ok retry", &kw));
    }
}
