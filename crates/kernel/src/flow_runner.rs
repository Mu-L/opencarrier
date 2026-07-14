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

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;
use tracing::{info, warn};

use memory::FlowRunRow;
use runtime::agent_loop::{run_agent_loop, AgentLoopResult};
use runtime::kernel_handle::KernelHandle;
use runtime::llm_driver::{Brain, CompletionRequest};
use types::agent::{AgentId, AgentManifest};
use types::error::CarrierError;
use types::flow::{FlowDef, StepDef, StepKind, StepOutputMode};
use types::message::{Message, TokenUsage};

use crate::error::{KernelError, KernelResult};
use crate::kernel::CarrierKernel;

/// Outcome of a `run_flow` invocation. A flow either runs to completion
/// (`Completed`) or suspends at a `user_input` step awaiting the human's reply
/// (`Suspended`).
pub(crate) enum FlowOutcome {
    /// The flow finished; the final step's output is the agent reply.
    Completed(AgentLoopResult),
    /// The flow suspended at a `user_input` step. `question` is the prompt to
    /// send to the user as the (intermediate) reply; the run is persisted as
    /// `waiting` and resumes on the user's next message.
    Suspended {
        question: String,
        total_usage: TokenUsage,
        iterations: u32,
    },
}

/// State carried into `run_flow` when resuming a suspended flow. `pre_outputs`
/// are the completed steps' snapshots (deserialized from `flow_runs`), and the
/// user's reply becomes the `waiting_step_id` step's output
/// `{ decision, text }`.
pub(crate) struct ResumeState {
    pub run_id: String,
    pub pre_outputs: HashMap<String, Value>,
    pub waiting_step_id: String,
    pub user_reply: String,
    pub cancel_keywords: Vec<String>,
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
    ) -> KernelResult<FlowOutcome> {
        let agent_name = self
            .registry
            .get(agent_id)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| agent_id.to_string());
        let driver = self.resolve_driver(manifest)?;
        let memory_handle: Option<Arc<dyn runtime::memory_handle::MemoryHandle>> =
            Some(Arc::new(crate::handle::MemorySubstrateHandle::new(Arc::clone(
                &self.memory,
            ))));

        // `input` is the template-context input (used by render/when/select);
        // it is also serialized into the flow_runs row on a fresh run.
        let input: Value = serde_json::json!({
            "user_message": user_message,
            "user_id": sender_id.unwrap_or(""),
        });

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
                // branch appends the reply turn).
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

                let dispatch: KernelResult<(String, TokenUsage, u32)> = match kind {
                    StepKind::AgentLoop => {
                        // base_system_prompt already carries the flow body (injected
                        // by prepare_agent_context); add only the step directive.
                        let step_system = format!(
                            "{base_system_prompt}\n\n## 当前步骤: {}\n{step_prompt}",
                            step.id,
                        );
                        let mut step_manifest = manifest.clone();
                        step_manifest.model.system_prompt = step_system;
                        let mut step_session = self
                            .memory
                            .create_session_async(agent_name.clone())
                            .await
                            .map_err(KernelError::Carrier)?;
                        let r = run_agent_loop(
                            &step_manifest,
                            &step_user_msg,
                            &mut step_session,
                            &self.memory,
                            driver.clone(),
                            tools,
                            kernel_handle.clone(),
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
                            memory_handle.clone(),
                            sender_id,
                            owner_id,
                            channel_type,
                            Some(self.runtime.llm_concurrency_limit.clone()),
                        )
                        .await
                        .map_err(KernelError::Carrier)?;
                        Ok((r.response, r.total_usage, r.iterations))
                    }
                    StepKind::Chat => {
                        let brain_ref = brain.ok_or_else(|| {
                            KernelError::Carrier(CarrierError::Internal(
                                "chat step requires a brain".into(),
                            ))
                        })?;
                        let task_text = step
                            .task
                            .as_deref()
                            .map(|t| render_template(t, &outputs, &input))
                            .unwrap_or_else(|| step_user_msg.clone());
                        let system = format!("{base_system_prompt}\n\n## 当前步骤: {}\n{task_text}", step.id);
                        let req = CompletionRequest {
                            model: String::new(),
                            messages: vec![Message::user(step_user_msg.clone())],
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
                        Ok((resp.text(), resp.usage, 1))
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
                    StepKind::Tool | StepKind::Unknown(_) => unreachable!(),
                };

                match dispatch {
                    Ok((final_msg, usage, iters)) => {
                        total_usage.input_tokens += usage.input_tokens;
                        total_usage.output_tokens += usage.output_tokens;
                        total_iterations += iters;

                        let out_val = select_output(step, &final_msg, &outputs, &input)?;
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
                        if let Some(_fb) = &step.on_failure {
                            warn!(
                                flow = %flow.name,
                                step = %step.id,
                                on_failure = ?step.on_failure,
                                error = %e,
                                "flow step failed, degrading (on_failure set)"
                            );
                            outputs.insert(
                                step.id.clone(),
                                Value::String(format!("[step {} failed: {:?}]", step.id, e)),
                            );
                            executed_order.push(step.id.clone());
                        } else {
                            failed = Some(e);
                            break 'outer;
                        }
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

        Ok(FlowOutcome::Completed(AgentLoopResult {
            response: final_response,
            total_usage,
            iterations: total_iterations,
            silent: false,
            directives: Default::default(),
            plan: None,
        }))
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
}
