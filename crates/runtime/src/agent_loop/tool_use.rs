//! Handler for the ToolUse stop reason.
//!
//! When the LLM requests tool execution, this handler:
//! - Tracks tool calls for loop detection
//! - Executes each tool with timeout and truncation
//! - Handles flow_load deduplication
//! - Tracks consecutive tool errors
//! - Refreshes the tool list after tool_search / flow_load
//! - Detects task_plan and signals a loop break

use super::*;

use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
use crate::hooks::HookRegistry;
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::{Brain, StreamEvent};
use crate::tool_context::ToolContext;
use crate::tool_runner;
use crate::mcp::McpConnection;
use crate::web_fetch::WebFetchEngine;
use memory::MemorySubstrate;
use types::message::{ContentBlock, Message, MessageContent, Role};
use types::tool::ToolDefinition;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// When a tool fails this many consecutive times, escalate the feedback
/// to urge the LLM to change approach entirely. (Tools are NOT removed —
/// we educate, not punish.)
pub(in crate::agent_loop) const ERROR_ESCALATION_THRESHOLD: u32 = 3;

/// Action the main loop should take after handling a ToolUse.
pub(in crate::agent_loop) enum ToolUseAction {
    /// The loop should continue (normal tool execution completed).
    Continue,
    /// The loop should break — a task_plan was detected.
    BreakWithPlan(TaskPlan),
}

/// Handle a `StopReason::ToolUse` response.
///
/// Executes each tool call, handles loop detection, error tracking,
/// skill deduplication, dynamic tool discovery, and task_plan detection.
///
/// Returns a `ToolUseAction` indicating whether the loop should continue
/// or break (when a task_plan is produced).
#[allow(clippy::too_many_arguments)]
pub(in crate::agent_loop) async fn handle_tool_use(
    response: &mut CompletionResponse,
    session: &mut Session,
    messages: &mut Vec<Message>,
    manifest: &AgentManifest,
    memory: &MemorySubstrate,
    kernel: Option<&Arc<dyn KernelHandle>>,
    memory_handle: Option<&Arc<dyn crate::memory_handle::MemoryHandle>>,
    brain: Option<&Arc<dyn Brain>>,
    hooks: Option<&HookRegistry>,
    on_phase: Option<&PhaseCallback>,
    stream_tx: &Option<tokio::sync::mpsc::Sender<StreamEvent>>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    fetch_engine: Option<&WebFetchEngine>,
    workspace_root: Option<&Path>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    context_budget: &ContextBudget,
    hand_allowed_env: &[String],
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
    // Mutable loop state
    consecutive_max_tokens: &mut u32,
    any_tools_executed: &mut bool,
    recent_tool_calls: &mut Vec<(String, u64)>,
    tools_owned: &mut Vec<ToolDefinition>,
    discovered_tool_names: &mut std::collections::HashSet<String>,
    loaded_flows: &mut std::collections::HashSet<String>,
    error_tracker: &mut crate::agent_loop::state::ToolErrorTracker,
    // For task_plan save
    session_base_len: usize,
    iteration: u32,
) -> ToolUseAction {
    // Reset MaxTokens continuation counter on tool use
    *consecutive_max_tokens = 0;
    *any_tools_executed = true;

    let assistant_blocks = response.content.clone();

    // O6: Single-track — only push to messages, not session
    messages.push(Message {
        role: Role::Assistant,
        content: MessageContent::Blocks(assistant_blocks),
    });

    let caller_id_str = session.agent_name.to_string();

    // Track tool calls for loop detection BEFORE execution
    for tc in &response.tool_calls {
        recent_tool_calls.push((tc.name.clone(), super::helpers::tool_input_hash(&tc.input)));
    }
    if recent_tool_calls.len() > super::helpers::LOOP_DETECTION_WINDOW * 3 {
        let drain_count =
            recent_tool_calls.len() - super::helpers::LOOP_DETECTION_WINDOW * 2;
        recent_tool_calls.drain(..drain_count);
    }

    // Detect loop: same (name, input_hash) repeated LOOP_DETECTION_WINDOW times.
    // Instead of terminating the agent loop, remove the looping tool and
    // inject a system message so the LLM can continue with other tools.
    if let Some((looping_name, _)) = super::helpers::detect_tool_loop(
        recent_tool_calls,
        super::helpers::LOOP_DETECTION_WINDOW,
    ) {
        warn!(
            agent = %manifest.name,
            tool = %looping_name,
            consecutive = super::helpers::LOOP_DETECTION_WINDOW,
            iteration,
            "Tool loop detected — removing tool and continuing"
        );
        // Remove the looping tool from available tools
        tools_owned.retain(|t| t.name != looping_name);
        recent_tool_calls.clear();
        error_tracker.remove(&looping_name);
        // Inject a system message telling the LLM to stop using this tool
        let warning = format!(
            "工具 `{looping_name}` 连续多次返回相同结果，已被临时移除。请用其他方式完成任务，不要再用这个工具。如果是因为 flow 声明的工具未加载，请用 flow_update 修复 flow 的 tools 字段。"
        );
        messages.push(Message::system(&warning));
    }

    // Execute each tool call with timeout and truncation
    let mut tool_result_blocks = Vec::new();
    for tool_call in &response.tool_calls {
        // Canonicalize name up front so trailing punctuation from free-text
        // recovery (`web_search,`) and aliases hit the right tool path.
        let tool_name = types::tool_compat::normalize_tool_name(&tool_call.name).to_string();
        debug!(tool = %tool_name, id = %tool_call.id, "Executing tool");

        // Notify phase: ToolUse
        if let Some(cb) = on_phase {
            let sanitized: String = tool_name
                .chars()
                .filter(|c| !c.is_control())
                .take(64)
                .collect();
            cb(LoopPhase::ToolUse {
                tool_name: sanitized,
            });
        }

        // Fire BeforeToolCall hook (can block execution)
        if let Some(hook_reg) = hooks {
            let ctx = crate::hooks::HookContext {
                agent_name: &manifest.name,
                agent_id: &caller_id_str,
                event: types::agent::HookEvent::BeforeToolCall,
                data: serde_json::json!({
                    "tool_name": &tool_name,
                    "input": &tool_call.input,
                }),
            };
            if let Err(reason) = hook_reg.fire(&ctx) {
                tool_result_blocks.push(ContentBlock::ToolResult {
                    tool_use_id: tool_call.id.clone(),
                    tool_name: tool_name.clone(),
                    content: format!("Hook blocked tool '{tool_name}': {reason}"),
                    is_error: true,
                });
                continue;
            }
        }

        // Resolve effective exec policy (per-agent override or global)
        let effective_exec_policy = manifest.exec_policy.as_ref();

        let home_dir_buf = kernel.and_then(|k| k.home_dir());

        // Check if sender is a clone admin
        let is_clone_admin = if let (Some(sid), Some(root)) = (sender_id, workspace_root) {
            crate::plugin::admin_store::is_admin(root, sid)
        } else {
            false
        };

        let tool_ctx = ToolContext {
            kernel,
            memory: memory_handle,
            caller_agent_id: Some(&caller_id_str),
            mcp_connections,
            fetch_engine,
            allowed_env_vars: if hand_allowed_env.is_empty() {
                None
            } else {
                Some(hand_allowed_env)
            },
            workspace_root,
            brain,
            exec_policy: effective_exec_policy,
            cli_exec_config: manifest.cli_exec.as_ref(),

            process_manager,
            sender_id,
            owner_id,
            home_dir: home_dir_buf.as_deref(),
            agent_name: Some(&manifest.name),
            subagent_configs: if manifest.subagents.is_empty() {
                None
            } else {
                Some(&manifest.subagents)
            },
            channel_type,
            max_tool_level: manifest.max_tool_level,
            is_clone_admin,
        };

        // Timeout-wrapped execution
        let timeout_secs = if super::helpers::TOOL_LONG_TIMEOUT_NAMES.contains(&tool_name.as_str())
        {
            super::helpers::TOOL_TIMEOUT_LONG_SECS
        } else {
            super::helpers::TOOL_TIMEOUT_SECS
        };
        let result = match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            tool_runner::execute_tool(&tool_call.id, &tool_name, &tool_call.input, &tool_ctx),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                warn!(tool = %tool_name, "Tool execution timed out after {}s", timeout_secs);
                types::tool::ToolResult {
                    tool_use_id: tool_call.id.clone(),
                    content: format!("Tool '{tool_name}' timed out after {timeout_secs}s."),
                    is_error: true,
                }
            }
        };

        // Fire AfterToolCall hook
        if let Some(hook_reg) = hooks {
            let ctx = crate::hooks::HookContext {
                agent_name: &manifest.name,
                agent_id: caller_id_str.as_str(),
                event: types::agent::HookEvent::AfterToolCall,
                data: serde_json::json!({
                    "tool_name": &tool_name,
                    "result": &result.content,
                    "is_error": result.is_error,
                }),
            };
            let _ = hook_reg.fire(&ctx);
        }

        // Skill load deduplication: if the same skill was already loaded
        // in this agent loop, replace the full content with a short hint.
        // This prevents the LLM from looping on flow_load without executing.
        if tool_name == "flow_load" {
            let skill_name = tool_call.input["name"]
                .as_str()
                .unwrap_or("")
                .to_lowercase();
            if !skill_name.is_empty() {
                if loaded_flows.contains(&skill_name) {
                    warn!(
                        agent = %manifest.name,
                        skill = %skill_name,
                        iteration,
                        "flow_load called for already-loaded flow — returning dedup hint"
                    );
                    let dedup_msg = format!(
                        "Flow '{skill_name}' 已经加载过了，请直接按步骤执行，不要再调用 flow_load。"
                    );
                    tool_result_blocks.push(ContentBlock::ToolResult {
                        tool_use_id: result.tool_use_id,
                        tool_name: tool_name.clone(),
                        content: dedup_msg,
                        is_error: false,
                    });
                    continue;
                } else {
                    loaded_flows.insert(skill_name);
                }
            }
        }

        // Dynamic truncation based on context budget (replaces flat MAX_TOOL_RESULT_CHARS)
        let final_content = truncate_tool_result_dynamic(&result.content, context_budget);

        // Notify client of tool execution result (detect dead consumer)
        if let Some(tx) = stream_tx {
            let preview: String = final_content.chars().take(300).collect();
            if tx
                .send(StreamEvent::ToolExecutionResult {
                    id: tool_call.id.clone(),
                    name: tool_name.clone(),
                    result_preview: preview,
                    is_error: result.is_error,
                })
                .await
                .is_err()
            {
                warn!(agent = %manifest.name, "Stream consumer disconnected — continuing tool loop but will not stream further");
            }
        }

        tool_result_blocks.push(ContentBlock::ToolResult {
            tool_use_id: result.tool_use_id,
            tool_name: tool_name.clone(),
            content: final_content,
            is_error: result.is_error,
        });
    }

    // Detect tool errors and inject guidance to prevent fabrication
    let error_count = tool_result_blocks
        .iter()
        .filter(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
        .count();

    // Record success/failure in sliding window tracker (O5: replaces HashMap reset-on-success)
    let succeeded_tools: Vec<&str> = tool_result_blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                is_error: false,
                tool_name,
                ..
            } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    for name in &succeeded_tools {
        error_tracker.record(name, true);
    }

    if error_count > 0 {
        // Collect failed tool names AND their error messages for actionable feedback
        let failed_tools: Vec<(&str, &str)> = tool_result_blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    is_error: true,
                    tool_name,
                    content,
                    ..
                } => Some((tool_name.as_str(), content.as_str())),
                _ => None,
            })
            .collect();

        let failed_names: Vec<&str> = failed_tools.iter().map(|(n, _)| *n).collect();

        // Increment consecutive error counters (keep counting, but do NOT remove tools)
        for name in &failed_names {
            error_tracker.record(name, false);
        }

        info!(
            agent = %manifest.name,
            iteration,
            error_count,
            failed_tools = ?failed_names,
            "Tool errors in agent loop iteration"
        );

        // Build actionable, per-tool error analysis with escalating detail.
        // We do NOT remove tools — the LLM can still succeed with correct params.
        let mut guidance = String::from(
            "[工具错误分析 — 不要编造结果，也不要用相同参数重试。\n",
        );
        for (name, err_msg) in &failed_tools {
            let count = error_tracker.consecutive_failures(name).max(1);
            // Truncate long error messages for readability
            let short_err: String = err_msg.chars().take(200).collect();
            let ellipsis = if err_msg.chars().count() > 200 { "..." } else { "" };

            // Escalating detail based on how many times this tool has failed
            let suggestion = if count >= ERROR_ESCALATION_THRESHOLD {
                format!(
                    " ⚠️ 这个工具已经连续失败 {count} 次。你可能一直用错了方法——\
                     仔细看上面的错误原因，换个完全不同的参数或方案，或者直接告诉用户遇到了什么困难。"
                )
            } else if count == 2 {
                " 上次也失败了，请仔细确认参数后再试。".to_string()
            } else {
                String::new()
            };

            guidance.push_str(&format!(
                " ❌ {name} → {short_err}{ellipsis}{suggestion}\n"
            ));
        }
        guidance.push_str(
            "修正方法：分析上面的错误原因，用不同的参数或换一个合适的工具重试。]",
        );

        tool_result_blocks.push(ContentBlock::Text {
            text: guidance,
            provider_metadata: None,
        });
    }

    let tool_results_msg = Message {
        role: Role::User,
        content: MessageContent::Blocks(tool_result_blocks.clone()),
    };
    // O6: Single-track — only push to messages, not session
    messages.push(tool_results_msg);

    // Dynamic tool refresh (streaming path)
    let tools_may_have_changed = response.tool_calls.iter().any(|tc| {
        matches!(
            tc.name.as_str(),
            "train_write" | "file_write" | "tool_search" | "flow_load"
        )
    });
    if tools_may_have_changed {
        if let Some(kernel) = kernel {
            let _agent_id_str = session.agent_name.to_string();

            // Log flow_load calls
            let flow_load_count = response
                .tool_calls
                .iter()
                .filter(|tc| tc.name == "flow_load")
                .count();
            if flow_load_count > 0 {
                info!(count = flow_load_count, "Skill(s) loaded");
            }

            // tool_search: add found tools to the tools list so the LLM API
            // allows outputting tool_use for them on the next iteration.
            // The LLM already saw the tool definitions in the tool_search result,
            // but the API requires tools to be in CompletionRequest.tools for
            // structured tool_use output.
            let search_queries: Vec<&str> = response
                .tool_calls
                .iter()
                .filter(|tc| tc.name == "tool_search")
                .filter_map(|tc| tc.input.get("query").and_then(|v| v.as_str()))
                .collect();

            let mut found_tools: Vec<ToolDefinition> = Vec::new();
            let mut found_names: std::collections::HashSet<String> =
                std::collections::HashSet::new();

            for q in &search_queries {
                let results = kernel.search_tools(
                    q,
                    super::helpers::TOOL_SEARCH_RECALL_LIMIT,
                    manifest.max_tool_level,
                );
                for (_, def) in results {
                    if found_names.insert(def.name.clone()) {
                        found_tools.push(def);
                    }
                }
            }

            if !found_tools.is_empty() {
                // O11: Append discovered tools instead of evicting previous ones.
                // Previously, each tool_search would evict tools from the last search.
                // This caused the LLM to re-search when it needed tools from two
                // different contexts in the same conversation. Now we accumulate,
                // capped by MAX_TOTAL_TOOLS to prevent unbounded inflation.
                const MAX_TOTAL_TOOLS: usize = 32;
                let current_count = tools_owned.len();
                let remaining_capacity = MAX_TOTAL_TOOLS.saturating_sub(current_count);
                let to_add: Vec<_> = found_tools
                    .into_iter()
                    .filter(|t| !tools_owned.iter().any(|existing| existing.name == t.name))
                    .take(remaining_capacity)
                    .collect();
                if !to_add.is_empty() {
                    for t in &to_add {
                        discovered_tool_names.insert(t.name.clone());
                    }
                    info!(
                        found = to_add.len(),
                        total = current_count + to_add.len(),
                        "tool_search: adding discovered tools to CompletionRequest.tools"
                    );
                    tools_owned.extend(to_add);
                }
            }
        }
    }

    // Note: no per-iteration save here — save happens at loop end
    // (success -> full save, failure -> summary only)

    // Detect task_plan: extract plan data and break out of the loop
    if let Some(tc) = response
        .tool_calls
        .iter()
        .find(|tc| tc.name == "task_plan")
    {
        let title = tc.input["title"].as_str().unwrap_or("").to_string();
        let steps: Vec<TaskStep> = tc.input["steps"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| {
                        Some(TaskStep {
                            id: s["id"].as_str()?.to_string(),
                            prompt: s["prompt"].as_str()?.to_string(),
                            depends_on: s["depends_on"]
                                .as_array()
                                .map(|d| {
                                    d.iter()
                                        .filter_map(|v| v.as_str().map(String::from))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !steps.is_empty() {
            info!(
                plan_title = %title,
                steps = steps.len(),
                "task_plan detected — breaking out of agent loop"
            );
            // Save session before breaking (inline version of save_new! macro)
            super::helpers::sync_loop_messages(messages, session, session_base_len);
            let new_msgs = &session.messages[session_base_len..];
            if let Err(e) = memory
                .save_session_append_async(
                    session.id,
                    &session.agent_name,
                    new_msgs,
                    session.context_window_tokens,
                    session.label.as_deref(),
                    Some(&session.turn_summaries),
                )
                .await
            {
                warn!("Failed to save session before plan break: {e}");
            }
            return ToolUseAction::BreakWithPlan(TaskPlan { title, steps });
        }
    }

    ToolUseAction::Continue
}
