//! Core agent execution loop.
//!
//! The agent loop handles receiving a user message, recalling relevant memories,
//! calling the LLM, executing tool calls, and saving the conversation.
//!
//! The implementation is split across modules:
//! - `helpers` — retry logic, fallback chain, loop detection, turn trimming/summary
//! - `end_turn` — handler for EndTurn / StopSequence
//! - `tool_use` — handler for ToolUse (tool execution, error tracking, discovery)
//! - `max_tokens` — handler for MaxTokens (continuation / partial response)

mod helpers;
mod end_turn;
mod tool_use;
mod max_tokens;

use crate::context_budget::{apply_context_guard, ContextBudget};
use crate::context_overflow::{recover_from_overflow, RecoveryStage};
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::{
    Brain, CompletionRequest, CompletionResponse, LlmDriver, StreamEvent,
};

use crate::mcp::McpConnection;
use crate::text_tool_recovery::{recover_text_tool_calls, ToolSearchFn};
use crate::web_fetch::WebFetchEngine;
use memory::session::Session;
use memory::MemorySubstrate;
use types::agent::AgentManifest;
use types::error::{CarrierError, CarrierResult};
use types::message::{ContentBlock, Message, Role, StopReason, TokenUsage};
// Re-export for tests (via `use super::*`)
#[allow(unused_imports)]
pub(crate) use types::message::MessageContent;
use types::tool::ToolDefinition;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// Re-export constants that external modules (tests) reference.
pub use helpers::TOOL_TIMEOUT_SECS;
pub use helpers::TOOL_TIMEOUT_LONG_SECS;
pub use helpers::TOOL_LONG_TIMEOUT_NAMES;
pub use max_tokens::MAX_CONTINUATIONS;
// Re-export constants and functions used by tests via `use super::*`.
pub use helpers::{MAX_RETRIES, BASE_RETRY_DELAY_MS, MAX_HISTORY_MESSAGES, LOOP_DETECTION_WINDOW};
pub use helpers::{tool_input_hash, detect_tool_loop};

/// Maximum iterations in the agent loop before giving up.
const MAX_ITERATIONS: u32 = 25;

/// Overall timeout for the entire agent loop (seconds).
const AGENT_LOOP_TIMEOUT_SECS: u64 = 600;

/// Agent lifecycle phase within the execution loop.
/// Used for UX indicators (typing, reactions) without coupling to channel types.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopPhase {
    /// Agent is calling the LLM.
    Thinking,
    /// Agent is executing a tool.
    ToolUse { tool_name: String },
    /// Agent is streaming tokens.
    Streaming,
    /// Agent finished successfully.
    Done,
    /// Agent encountered an error.
    Error,
}

/// Callback for agent lifecycle phase changes.
/// Implementations should be non-blocking (fire-and-forget) to avoid slowing the loop.
pub type PhaseCallback = Arc<dyn Fn(LoopPhase) + Send + Sync>;

/// A step within a task plan produced by the `task_plan` tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskStep {
    pub id: String,
    pub prompt: String,
    pub depends_on: Vec<String>,
}

/// A task plan produced by the `task_plan` tool.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskPlan {
    pub title: String,
    pub steps: Vec<TaskStep>,
}

/// Result of an agent loop execution.
#[derive(Debug)]
pub struct AgentLoopResult {
    /// The final text response from the agent.
    pub response: String,
    /// Total token usage across all LLM calls.
    pub total_usage: TokenUsage,
    /// Number of iterations the loop ran.
    pub iterations: u32,
    /// True when the agent intentionally chose not to reply (NO_REPLY token or [[silent]]).
    pub silent: bool,
    /// Reply directives extracted from the agent's response.
    pub directives: types::message::ReplyDirectives,
    /// Task plan produced by the task_plan tool, if any.
    pub plan: Option<TaskPlan>,
}

/// Run the agent execution loop for a single user message.
///
/// This is the core of Carrier: it loads session context, recalls memories,
/// runs the LLM in a tool-use loop, and saves the updated session.
///
/// Pass `stream_tx = Some(tx)` to receive incremental `StreamEvent`s during
/// execution; pass `None` for a non-streaming (blocking) call.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop(
    manifest: &AgentManifest,
    user_message: &str,
    session: &mut Session,
    memory: &MemorySubstrate,
    driver: Arc<dyn LlmDriver>,
    tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    fetch_engine: Option<&WebFetchEngine>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
    brain: Option<Arc<dyn Brain>>,
    memory_handle: Option<Arc<dyn crate::memory_handle::MemoryHandle>>,
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
    llm_concurrency_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> CarrierResult<AgentLoopResult> {
    let timeout = std::time::Duration::from_secs(AGENT_LOOP_TIMEOUT_SECS);
    match tokio::time::timeout(
        timeout,
        run_agent_loop_impl(
            manifest, user_message, session, memory, driver, tools,
            kernel, stream_tx, mcp_connections, fetch_engine, workspace_root,
            on_phase, hooks, context_window_tokens, process_manager,
            user_content_blocks, brain, memory_handle, sender_id, owner_id, channel_type,
            llm_concurrency_limit,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            warn!(
                agent = %manifest.name,
                timeout_secs = AGENT_LOOP_TIMEOUT_SECS,
                "Agent loop timed out"
            );
            Err(CarrierError::LlmDriver(format!(
                "Agent loop timed out after {}s — the LLM API did not respond in time. Please try again later.",
                AGENT_LOOP_TIMEOUT_SECS
            )))
        }
    }
}

/// Streaming variant of [`run_agent_loop`].
///
/// Equivalent to calling `run_agent_loop` with `stream_tx = Some(tx)`.
/// Kept as a convenience wrapper for existing call sites.
#[allow(clippy::too_many_arguments)]
pub async fn run_agent_loop_streaming(
    manifest: &AgentManifest,
    user_message: &str,
    session: &mut Session,
    memory: &MemorySubstrate,
    driver: Arc<dyn LlmDriver>,
    tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: mpsc::Sender<StreamEvent>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    fetch_engine: Option<&WebFetchEngine>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
    brain: Option<Arc<dyn Brain>>,
    memory_handle: Option<Arc<dyn crate::memory_handle::MemoryHandle>>,
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
    llm_concurrency_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> CarrierResult<AgentLoopResult> {
    run_agent_loop(
        manifest, user_message, session, memory, driver, tools,
        kernel, Some(stream_tx), mcp_connections, fetch_engine, workspace_root,
        on_phase, hooks, context_window_tokens, process_manager,
        user_content_blocks, brain, memory_handle, sender_id, owner_id, channel_type,
        llm_concurrency_limit,
    ).await
}

/// Core agent loop implementation.
///
/// Orchestrates the LLM call → response → tool-use cycle, delegating each
/// `StopReason` branch to its dedicated handler module.
#[allow(clippy::too_many_arguments)]
async fn run_agent_loop_impl(
    manifest: &AgentManifest,
    user_message: &str,
    session: &mut Session,
    memory: &MemorySubstrate,
    driver: Arc<dyn LlmDriver>,
    tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    fetch_engine: Option<&WebFetchEngine>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
    brain: Option<Arc<dyn Brain>>,
    memory_handle: Option<Arc<dyn crate::memory_handle::MemoryHandle>>,
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
    llm_concurrency_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> CarrierResult<AgentLoopResult> {
    info!(agent = %manifest.name, "Starting agent loop");

    // Compute a deadline for all LLM calls within this loop, so that
    // call_with_fallback respects the overall time budget even when
    // multiple endpoints and retries are involved.
    let loop_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(AGENT_LOOP_TIMEOUT_SECS);

    // Extract hand-allowed env vars from manifest metadata (set by kernel for hand settings)
    let hand_allowed_env: Vec<String> = manifest
        .metadata
        .get("hand_allowed_env")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Fire BeforePromptBuild hook
    let agent_id_str = session.agent_name.clone();
    if let Some(hook_reg) = hooks {
        let ctx = crate::hooks::HookContext {
            agent_name: &manifest.name,
            agent_id: agent_id_str.as_str(),
            event: types::agent::HookEvent::BeforePromptBuild,
            data: serde_json::json!({
                "system_prompt": &manifest.model.system_prompt,
                "user_message": user_message,
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    // Build the system prompt — base prompt comes from kernel (prompt_builder).
    let system_prompt = manifest.model.system_prompt.clone();

    // Turn summaries are now injected via prompt_builder (Section 4.2).
    // No ad-hoc injection here.

    // Track which messages existed before this agent loop started.
    let session_base_len = session.messages.len();

    // Add the user message to session history.
    if let Some(blocks) = user_content_blocks {
        session.messages.push(Message::user_with_blocks(blocks));
    } else {
        session.messages.push(Message::user(user_message));
    }

    let llm_messages: Vec<Message> = session
        .messages
        .iter()
        .filter(|m| m.role != Role::System)
        .cloned()
        .collect();

    let mut messages = crate::session_repair::validate_and_repair(&llm_messages);

    // Inject canonical context as the first user message
    if let Some(cc_msg) = manifest
        .metadata
        .get("canonical_context_msg")
        .and_then(|v| v.as_str())
    {
        if !cc_msg.is_empty() {
            messages.insert(0, Message::user(cc_msg));
        }
    }

    let mut total_usage = TokenUsage::default();

    // Safety valve: trim excessively long message histories
    if messages.len() > helpers::MAX_HISTORY_MESSAGES {
        let trim_count = messages.len() - helpers::MAX_HISTORY_MESSAGES;
        warn!(
            agent = %manifest.name,
            total_messages = messages.len(),
            trimming = trim_count,
            "Trimming old messages to prevent context overflow"
        );
        crate::context_overflow::pair_aware_drain(&mut messages, trim_count);
    }

    let max_iterations = manifest
        .autonomous
        .as_ref()
        .map(|a| a.max_iterations)
        .unwrap_or(MAX_ITERATIONS);

    let mut consecutive_max_tokens: u32 = 0;
    let mut text_recovery_retries: u32 = 0;
    const MAX_TEXT_RECOVERY_RETRIES: u32 = 2;

    let ctx_window = context_window_tokens.unwrap_or(helpers::DEFAULT_CONTEXT_WINDOW);
    let context_budget = ContextBudget::new(ctx_window);
    let mut any_tools_executed = false;

    let mut detected_plan: Option<TaskPlan> = None;
    let mut recent_tool_calls: Vec<(String, u64)> = Vec::new();
    let mut consecutive_tool_errors: std::collections::HashMap<String, u32> = std::collections::HashMap::new();

    let mut tools_owned: Vec<ToolDefinition> = tools.to_vec();
    let mut tools: &[ToolDefinition] = &tools_owned;
    let mut discovered_tool_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut loaded_skills: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut budget_warning_sent = false;

    for iteration in 0..max_iterations {
        debug!(iteration, "Streaming agent loop iteration");

        // Context overflow recovery pipeline
        let recovery =
            recover_from_overflow(&mut messages, &system_prompt, tools, ctx_window);
        match &recovery {
            RecoveryStage::None => {}
            RecoveryStage::FinalError => {
                warn!("Context overflow unrecoverable — suggest /reset or /compact");
                if let Some(tx) = &stream_tx {
                    if tx.send(StreamEvent::PhaseChange {
                        phase: "context_warning".to_string(),
                        detail: Some("Context overflow unrecoverable. Use /reset or /compact.".to_string()),
                    }).await.is_err() {
                        warn!("Stream consumer disconnected while sending context overflow warning");
                    }
                }
            }
            _ => {
                if let Some(tx) = &stream_tx {
                    if tx.send(StreamEvent::PhaseChange {
                        phase: "context_warning".to_string(),
                        detail: Some("Older messages trimmed to stay within context limits. Use /compact for smarter summarization.".to_string()),
                    }).await.is_err() {
                        warn!("Stream consumer disconnected while sending context trim warning");
                    }
                }
            }
        }

        // Context guard: compact oversized tool results before LLM call
        apply_context_guard(&mut messages, &context_budget, tools);

        let request = CompletionRequest {
            model: String::new(),
            messages: messages.clone(),
            tools: tools.to_vec(),
            max_tokens: manifest.model.max_tokens,
            temperature: manifest.model.temperature,
            system: Some(system_prompt.clone()),
            thinking: None,
            extra: Default::default(),
        };

        if let Some(cb) = on_phase {
            if stream_tx.is_some() && iteration == 0 {
                cb(LoopPhase::Streaming);
            } else {
                cb(LoopPhase::Thinking);
            }
        }

        let default_modality = if manifest.model.modality.is_empty() {
            "chat"
        } else {
            &manifest.model.modality
        };

        // Inject loop status into every reasoning turn so the LLM can make
        // informed decisions about whether to continue or wrap up.
        let remaining_secs = loop_deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_secs();
        let is_reasoning = brain
            .as_ref()
            .is_some_and(|b| b.has_modality(helpers::REASONING_MODALITY));

        if is_reasoning && iteration.is_multiple_of(2) {
            // Build status hint: iteration count, time remaining, budget pressure level
            let pressure = if remaining_secs > 300 {
                "comfortable"
            } else if remaining_secs > 120 {
                "moderate"
            } else if remaining_secs > 60 {
                "tight"
            } else {
                "critical"
            };
            let mut status_msg = format!(
                "📊 Loop status: iteration {}/{} | ~{}s remaining | budget: {pressure}",
                iteration + 1,
                max_iterations,
                remaining_secs,
            );
            // Append tool error history so reasoning knows what's been failing.
            if !consecutive_tool_errors.is_empty() {
                let errors: Vec<String> = consecutive_tool_errors
                    .iter()
                    .map(|(name, count)| format!("{name}(×{count})"))
                    .collect();
                status_msg.push_str(&format!(
                    "\n⚠️ 连续出错工具: {} — 这些工具可能用错了参数，换不同方式调用。",
                    errors.join(", ")
                ));
            }
            // Only inject if the last system message isn't already a loop status
            let should_inject = messages.last().is_none_or(|m| {
                !m.content.text_content().starts_with("📊 Loop status")
            });
            if should_inject {
                tracing::info!(
                    iteration,
                    remaining_secs,
                    pressure,
                    error_tools = ?consecutive_tool_errors.keys().collect::<Vec<_>>(),
                    "Injecting loop status for reasoning decision"
                );
                messages.push(Message::system(status_msg));
            }
        }

        // When time is tight/critical, force reasoning modality so the LLM
        // can make a deliberate wrap-up decision instead of blindly continuing
        // with tool calls that won't complete in time.
        let modality = if remaining_secs < 120
            && is_reasoning
        {
            if !budget_warning_sent {
                budget_warning_sent = true;
                info!(
                    iteration,
                    remaining_secs,
                    "Budget tight: forcing reasoning for wrap-up decision"
                );
            }
            helpers::REASONING_MODALITY.to_string()
        } else {
            helpers::pick_modality(brain.as_ref(), iteration, default_modality)
        };
        let mut response = match helpers::call_with_fallback(
            brain.as_ref(),
            &*driver,
            &modality,
            request,
            stream_tx.clone(),
            Some(loop_deadline),
            llm_concurrency_limit.as_ref(),
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                // If the error is budget exhaustion and we haven't tried a
                // final wrap-up yet, give reasoning one last chance to
                // produce a partial answer from whatever we have so far.
                let err_str = format!("{e}");
                if err_str.contains("time budget exhausted")
                    && brain.as_ref().is_some_and(|b| b.has_modality(helpers::REASONING_MODALITY))
                {
                    warn!(
                        iteration,
                        "Time budget exhausted — attempting final reasoning wrap-up"
                    );
                    messages.push(Message::system(
                        "⏱️ Time budget is now exhausted. Based on everything you have \
                         so far, produce the best possible final answer. Do not call any \
                         more tools — just summarize and conclude.",
                    ));

                    // Build a minimal request for the final reasoning call.
                    // Use a shorter per-call timeout so we don't overshoot.
                    let final_deadline = loop_deadline
                        - std::time::Duration::from_secs(5);  // 5s safety margin
                    let final_request = CompletionRequest {
                        model: String::new(),
                        messages: messages.clone(),
                        tools: vec![],  // no tools — force text output
                        max_tokens: manifest.model.max_tokens,
                        temperature: manifest.model.temperature,
                        system: Some(system_prompt.clone()),
                        thinking: None,
                        extra: Default::default(),
                    };
                    match helpers::call_with_fallback(
                        brain.as_ref(),
                        &*driver,
                        helpers::REASONING_MODALITY,
                        final_request,
                        stream_tx.clone(),
                        Some(final_deadline),
                        llm_concurrency_limit.as_ref(),
                    )
                    .await
                    {
                        Ok(final_resp) => final_resp,
                        Err(_) => return Err(e),  // final attempt failed, return original
                    }
                } else {
                    return Err(e);
                }
            }
        };

        total_usage.input_tokens += response.usage.input_tokens;
        total_usage.output_tokens += response.usage.output_tokens;

        // Recover tool calls output as text (streaming path)
        if matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence
        ) && response.tool_calls.is_empty()
        {
            let tool_search_fn: Option<ToolSearchFn> = kernel.as_ref().map(|k| {
                let k = k.clone();
                let max_level = manifest.max_tool_level;
                Box::new(move |name: &str| -> Option<ToolDefinition> {
                    k.search_tools(name, 1, max_level).into_iter().next().map(|(_, def)| def)
                }) as ToolSearchFn
            });
            let result = recover_text_tool_calls(&response.text(), tools, tool_search_fn);

            let has_discovered = !result.discovered_tools.is_empty();
            if has_discovered {
                for def in &result.discovered_tools {
                    discovered_tool_names.insert(def.name.clone());
                    info!(tool = %def.name, schema = %def.input_schema, "Discovered tool schema");
                }
                info!(found = result.discovered_tools.len(), "Auto-discovered tools from text-based tool call recovery");
                tools_owned.extend(result.discovered_tools);
                tools = &tools_owned;
            }

            if !result.calls.is_empty() {
                info!(count = result.calls.len(), "Recovered text-based tool calls → promoting to ToolUse");
                response.tool_calls = result.calls;
                response.stop_reason = StopReason::ToolUse;
                let mut new_blocks: Vec<ContentBlock> = Vec::new();
                // Keep existing Text blocks from the LLM response
                for block in &response.content {
                    if let ContentBlock::Text { .. } = block {
                        new_blocks.push(block.clone());
                    }
                }
                // Append the recovered ToolUse blocks
                for tc in &response.tool_calls {
                    new_blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                        provider_metadata: None,
                    });
                }
                response.content = new_blocks;
            } else if has_discovered || !result.needs_retry.is_empty() {
                if text_recovery_retries >= MAX_TEXT_RECOVERY_RETRIES {
                    warn!(
                        agent = %manifest.name,
                        retries = text_recovery_retries,
                        iteration,
                        "Giving up text-based tool recovery — LLM keeps outputting text instead of tool_use"
                    );
                } else {
                    text_recovery_retries += 1;
                    warn!(
                        agent = %manifest.name,
                        tools = ?result.needs_retry,
                        iteration,
                        retry = text_recovery_retries,
                        "LLM described tool calls as text — retrying with discovered tools"
                    );
                    let tool_names = result.needs_retry.join("、");
                    messages.push(Message::assistant(format!("我需要调用工具：{tool_names}。")));
                    messages.push(Message::system(
                        "你刚才用文本描述了工具调用，但用户看到的是原始文本。这些工具已添加到你的可用工具列表中，请直接用 tool_use 功能调用，带上完整的参数。不要再输出 [Called ...] 格式的文本。"
                    ));
                    continue;
                }
            }
        }

        match response.stop_reason {
            StopReason::EndTurn | StopReason::StopSequence => {
                match end_turn::handle_end_turn(
                    &response,
                    session,
                    &mut messages,
                    manifest,
                    memory,
                    kernel.as_ref(),
                    memory_handle.as_ref(),
                    brain.as_ref(),
                    hooks,
                    on_phase,
                    session_base_len,
                    user_message,
                    owner_id,
                    sender_id,
                    channel_type,
                    &agent_id_str,
                    iteration,
                    total_usage,
                    any_tools_executed,
                )
                .await?
                {
                    end_turn::EndTurnAction::Retry => continue,
                    end_turn::EndTurnAction::Complete(result) => return Ok(result),
                }
            }
            StopReason::ToolUse => {
                match tool_use::handle_tool_use(
                    &mut response,
                    session,
                    &mut messages,
                    manifest,
                    memory,
                    kernel.as_ref(),
                    memory_handle.as_ref(),
                    brain.as_ref(),
                    hooks,
                    on_phase,
                    &stream_tx,
                    mcp_connections,
                    fetch_engine,
                    workspace_root,
                    process_manager,
                    &context_budget,
                    &hand_allowed_env,
                    sender_id,
                    owner_id,
                    channel_type,
                    &mut consecutive_max_tokens,
                    &mut any_tools_executed,
                    &mut recent_tool_calls,
                    &mut tools_owned,
                    &mut discovered_tool_names,
                    &mut loaded_skills,
                    &mut consecutive_tool_errors,
                    session_base_len,
                    iteration,
                )
                .await
                {
                    tool_use::ToolUseAction::Continue => {}
                    tool_use::ToolUseAction::BreakWithPlan(plan) => {
                        detected_plan = Some(plan);
                        break;
                    }
                }
                // Update tools slice after tool_use handler may have modified tools_owned
                tools = &tools_owned;
            }
            StopReason::MaxTokens => {
                match max_tokens::handle_max_tokens(
                    &response,
                    session,
                    &mut messages,
                    memory,
                    &stream_tx,
                    &mut consecutive_max_tokens,
                    hooks,
                    &agent_id_str,
                    manifest,
                    iteration,
                    total_usage,
                    session_base_len,
                )
                .await
                {
                    max_tokens::MaxTokensAction::Continue => {}
                    max_tokens::MaxTokensAction::Complete(result) => return Ok(result),
                }
            }
        }
    }

    // Plan B: on failure, save only user message + error summary (discard tool noise)
    {
        let discarded = session.messages.len() - session_base_len;
        let summary = format!(
            "[Agent loop failed: max iterations ({}) exceeded. {} messages discarded.]",
            max_iterations, discarded,
        );
        let user_msg = session.messages[session_base_len..]
            .iter()
            .find(|m| m.role == Role::User)
            .cloned()
            .unwrap_or_else(|| Message::user(user_message));
        let fail_msgs = vec![user_msg, Message::assistant(&summary)];
        if let Err(e) = memory.save_session_append_async(
            session.id, &session.agent_name, &fail_msgs,
            session.context_window_tokens, session.label.as_deref(),
            Some(&session.turn_summaries),
        ).await {
            warn!("Failed to save failure summary: {e}");
        }
    }

    // Fire AgentLoopEnd hook on max iterations exceeded
    if let Some(hook_reg) = hooks {
        let ctx = crate::hooks::HookContext {
            agent_name: &manifest.name,
            agent_id: agent_id_str.as_str(),
            event: types::agent::HookEvent::AgentLoopEnd,
            data: serde_json::json!({
                "reason": "max_iterations_exceeded",
                "iterations": max_iterations,
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    // If task_plan was detected, return success with the plan
    if let Some(plan) = detected_plan {
        return Ok(AgentLoopResult {
            response: format!("Plan '{}' created with {} steps. Executing...", plan.title, plan.steps.len()),
            total_usage,
            iterations: max_iterations,
            silent: false,
            directives: Default::default(),
            plan: Some(plan),
        });
    }

    Err(CarrierError::MaxIterationsExceeded(max_iterations))
}

#[cfg(test)]
mod tests;
