//! Core agent execution loop.
//!
//! The agent loop handles receiving a user message, recalling relevant memories,
//! calling the LLM, executing tool calls, and saving the conversation.
//!
//! The implementation is split across modules:
//! - `context` — LoopContext: bundles all mutable state and references
//! - `state`   — LoopState: runtime loop counters, budget, pressure, error tracking
//! - `helpers` — retry logic, fallback chain, loop detection, turn trimming/summary
//! - `end_turn` — handler for EndTurn / StopSequence
//! - `tool_use` — handler for ToolUse (tool execution, error tracking, discovery)
//! - `max_tokens` — handler for MaxTokens (continuation / partial response)
//!
//! ## Phase structure (O7 state machine)
//!
//! ```text
//! INIT → [PREPARE_TURN → LLM_CALL → DISPATCH → NEXT_TURN]* → TEARDOWN
//! ```

mod context;
mod helpers;
mod state;
mod end_turn;
mod tool_use;
mod max_tokens;

use crate::agent_loop::context::LoopContext;
use crate::agent_loop::state::LoopState;
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
pub use helpers::{MAX_RETRIES, BASE_RETRY_DELAY_MS, MAX_HISTORY_MESSAGES, LOOP_DETECTION_WINDOW, SOFT_LOOP_WINDOW};
pub use helpers::{tool_input_hash, detect_tool_loop, detect_soft_loop};

/// Maximum iterations in the agent loop before giving up.
const MAX_ITERATIONS: u32 = 25;

/// Overall timeout for the entire agent loop (seconds).
const AGENT_LOOP_TIMEOUT_SECS: u64 = 600;

const MAX_TEXT_RECOVERY_RETRIES: u32 = 2;

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

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Phase: INIT — build context, load session, restore state
// ---------------------------------------------------------------------------

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

    let loop_deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(AGENT_LOOP_TIMEOUT_SECS);

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

    let system_prompt = manifest.model.system_prompt.clone();
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

    // Inject canonical context
    if let Some(cc_msg) = manifest
        .metadata
        .get("canonical_context_msg")
        .and_then(|v| v.as_str())
    {
        if !cc_msg.is_empty() {
            messages.insert(0, Message::user(cc_msg));
        }
    }

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

    let ctx_window = context_window_tokens.unwrap_or(helpers::DEFAULT_CONTEXT_WINDOW);
    let context_budget = ContextBudget::new(ctx_window);

    let mut state = LoopState::new(max_iterations, loop_deadline, ctx_window);

    // O8: Restore last run summary from cross-session state
    if let Some(mh) = &memory_handle {
        let agent_key = format!("loop_state:{}", manifest.name);
        if let Ok(Some(val)) = mh.kv_get(
            &manifest.name,
            owner_id.unwrap_or(""),
            sender_id.unwrap_or(""),
            &agent_key,
        ) {
            if let Ok(last_run) = serde_json::from_value::<crate::agent_loop::state::LastRunSummary>(val) {
                info!(
                    agent = %manifest.name,
                    last_iterations = last_run.iterations,
                    last_stop_reason = %last_run.stop_reason,
                    last_outcome = ?last_run.outcome,
                    "Restored last run summary from cross-session state"
                );
                state.last_run = Some(last_run);
            }
        }
    }

    // Inject last run context into messages if available
    if let Some(last) = &state.last_run {
        let summary = format!(
            "📋 上次 loop 运行: {} 轮, 原因: {}, 结果: {:?}",
            last.iterations, last.stop_reason, last.outcome
        );
        messages.push(Message::system(&summary));
    }

    let mut ctx = LoopContext {
        manifest,
        user_message,
        agent_id_str,
        session,
        messages,
        session_base_len,
        memory,
        memory_handle,
        driver,
        brain,
        system_prompt,
        stream_tx,
        llm_concurrency_limit,
        tools_owned: tools.to_vec(),
        discovered_tool_names: std::collections::HashSet::new(),
        loaded_skills: std::collections::HashSet::new(),
        kernel,
        mcp_connections,
        fetch_engine,
        workspace_root,
        process_manager,
        context_budget,
        on_phase,
        hooks,
        sender_id,
        owner_id,
        channel_type,
        hand_allowed_env,
        context_window_tokens: ctx_window,
        state,
        detected_plan: None,
    };

    // ---- Main loop ----
    while ctx.state.iteration < ctx.state.max_iterations {
        let action = loop_iteration(&mut ctx).await?;
        match action {
            LoopAction::Continue => {}
            LoopAction::Complete(result) => return Ok(result),
            LoopAction::BreakForPlan => break,
        }
    }

    // ---- TEARDOWN ----
    teardown(&mut ctx).await
}

// ---------------------------------------------------------------------------
// Loop iteration result
// ---------------------------------------------------------------------------

/// What the main loop should do after a single iteration.
enum LoopAction {
    /// Continue to the next iteration.
    Continue,
    /// Return this result to the caller (loop finished successfully).
    Complete(AgentLoopResult),
    /// Break out of the loop because a task_plan was detected.
    BreakForPlan,
}

// ---------------------------------------------------------------------------
// Single iteration: PREPARE_TURN → LLM_CALL → DISPATCH
// ---------------------------------------------------------------------------

async fn loop_iteration(ctx: &mut LoopContext<'_>) -> CarrierResult<LoopAction> {
    let iteration = ctx.state.iteration;
    debug!(iteration, "Streaming agent loop iteration");

    // ---- PREPARE_TURN ----
    prepare_turn(ctx);

    // ---- LLM_CALL ----
    let modality = select_modality(ctx);
    let response = match call_llm(ctx, &modality).await? {
        LlmCallOutcome::Response(resp) => resp,
        LlmCallOutcome::BudgetWrapUp(resp) => resp,
        LlmCallOutcome::FatalError(e) => return Err(e),
    };

    ctx.state.total_usage.input_tokens += response.usage.input_tokens;
    ctx.state.total_usage.output_tokens += response.usage.output_tokens;

    // ---- Text tool call recovery ----
    let response = match handle_text_recovery(ctx, response, &modality).await {
        TextRecoveryOutcome::Continue => {
            ctx.state.iteration += 1;
            return Ok(LoopAction::Continue);
        }
        TextRecoveryOutcome::Proceed(resp) => resp,
    };

    // ---- DISPATCH ----
    let action = dispatch(ctx, response, &modality).await?;
    ctx.state.iteration += 1;
    Ok(action)
}

// ---------------------------------------------------------------------------
// Phase: PREPARE_TURN — context recovery, guard, status injection
// ---------------------------------------------------------------------------

fn prepare_turn(ctx: &mut LoopContext<'_>) {
    // Extract tools slice before mutating messages, to satisfy borrow checker
    // without cloning. Both recover_from_overflow and apply_context_guard only
    // read tools (they don't modify it).
    let tools = ctx.tools_owned.clone();
    let system_prompt = ctx.system_prompt.clone();
    let context_window_tokens = ctx.context_window_tokens;

    // Context overflow recovery pipeline
    let recovery = recover_from_overflow(
        &mut ctx.messages,
        &system_prompt,
        &tools,
        context_window_tokens,
    );
    match &recovery {
        RecoveryStage::None => {}
        RecoveryStage::FinalError => {
            warn!("Context overflow unrecoverable — suggest /reset or /compact");
            if let Some(tx) = &ctx.stream_tx {
                if tx.try_send(StreamEvent::PhaseChange {
                    phase: "context_warning".to_string(),
                    detail: Some("Context overflow unrecoverable. Use /reset or /compact.".to_string()),
                }).is_err() {
                    warn!("Stream consumer disconnected while sending context overflow warning");
                }
            }
        }
        _ => {
            if let Some(tx) = &ctx.stream_tx {
                if tx.try_send(StreamEvent::PhaseChange {
                    phase: "context_warning".to_string(),
                    detail: Some("Older messages trimmed to stay within context limits. Use /compact for smarter summarization.".to_string()),
                }).is_err() {
                    warn!("Stream consumer disconnected while sending context trim warning");
                }
            }
        }
    }

    // Context guard: compact oversized tool results before LLM call
    apply_context_guard(&mut ctx.messages, &ctx.context_budget, &tools);

    // Phase callback
    if let Some(cb) = ctx.on_phase {
        if ctx.stream_tx.is_some() && ctx.state.iteration == 0 {
            cb(LoopPhase::Streaming);
        } else {
            cb(LoopPhase::Thinking);
        }
    }

    // Inject loop status every turn so the model always has full context.
    {
        let status_msg = ctx.state.build_status_message();
        let should_inject = ctx.messages.last().is_none_or(|m| {
            !m.content.text_content().starts_with("📊 Turn")
        });
        if should_inject {
            tracing::info!(
                iteration = ctx.state.iteration,
                remaining_secs = ctx.state.remaining_secs(),
                pressure = ?ctx.state.budget_state,
                context_pressure = ?ctx.state.context_pressure,
                error_tools = ?ctx.state.error_tracker.failed_tools().collect::<Vec<_>>(),
                "Injecting loop status"
            );
            ctx.messages.push(Message::system(status_msg));
        }
    }
}

// ---------------------------------------------------------------------------
// Phase: Select modality
// ---------------------------------------------------------------------------

fn select_modality(ctx: &mut LoopContext<'_>) -> String {
    let default_modality = if ctx.manifest.model.modality.is_empty() {
        "chat"
    } else {
        &ctx.manifest.model.modality
    };

    let is_reasoning = ctx.brain
        .as_ref()
        .is_some_and(|b| b.has_modality(helpers::REASONING_MODALITY));

    ctx.state.refresh_budget();

    if ctx.state.remaining_secs() < 120 && is_reasoning {
        if !ctx.state.budget_warning_sent {
            ctx.state.budget_warning_sent = true;
            info!(
                iteration = ctx.state.iteration,
                remaining_secs = ctx.state.remaining_secs(),
                "Budget tight: forcing reasoning for wrap-up decision"
            );
        }
        helpers::REASONING_MODALITY.to_string()
    } else {
        helpers::pick_modality(ctx.brain.as_ref(), ctx.state.iteration, default_modality)
    }
}

// ---------------------------------------------------------------------------
// Phase: LLM_CALL — with budget exhaustion wrap-up
// ---------------------------------------------------------------------------

enum LlmCallOutcome {
    Response(CompletionResponse),
    BudgetWrapUp(CompletionResponse),
    FatalError(CarrierError),
}

async fn call_llm(ctx: &mut LoopContext<'_>, modality: &str) -> CarrierResult<LlmCallOutcome> {
    let request = CompletionRequest {
        model: String::new(),
        messages: ctx.messages.clone(),
        tools: ctx.tools().to_vec(),
        max_tokens: ctx.manifest.model.max_tokens,
        temperature: ctx.manifest.model.temperature,
        system: Some(ctx.system_prompt.clone()),
        thinking: None,
        extra: Default::default(),
    };

    match helpers::call_with_fallback(
        ctx.brain.as_ref(),
        &*ctx.driver,
        modality,
        request,
        ctx.stream_tx.clone(),
        Some(ctx.state.deadline),
        ctx.llm_concurrency_limit.as_ref(),
    )
    .await
    {
        Ok(resp) => Ok(LlmCallOutcome::Response(resp)),
        Err(e) => {
            let err_str = format!("{e}");
            if err_str.contains("time budget exhausted")
                && ctx.brain.as_ref().is_some_and(|b| b.has_modality(helpers::REASONING_MODALITY))
            {
                warn!(
                    ctx.state.iteration,
                    "Time budget exhausted — attempting final reasoning wrap-up"
                );
                ctx.messages.push(Message::system(
                    "⏱️ Time budget is now exhausted. Based on everything you have \
                     so far, produce the best possible final answer. Do not call any \
                     more tools — just summarize and conclude.",
                ));

                let final_deadline = ctx.state.deadline
                    - std::time::Duration::from_secs(5);
                let final_request = CompletionRequest {
                    model: String::new(),
                    messages: ctx.messages.clone(),
                    tools: vec![],
                    max_tokens: ctx.manifest.model.max_tokens,
                    temperature: ctx.manifest.model.temperature,
                    system: Some(ctx.system_prompt.clone()),
                    thinking: None,
                    extra: Default::default(),
                };
                match helpers::call_with_fallback(
                    ctx.brain.as_ref(),
                    &*ctx.driver,
                    helpers::REASONING_MODALITY,
                    final_request,
                    ctx.stream_tx.clone(),
                    Some(final_deadline),
                    ctx.llm_concurrency_limit.as_ref(),
                )
                .await
                {
                    Ok(final_resp) => Ok(LlmCallOutcome::BudgetWrapUp(final_resp)),
                    Err(_) => Ok(LlmCallOutcome::FatalError(e)),
                }
            } else {
                Ok(LlmCallOutcome::FatalError(e))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Text tool call recovery
// ---------------------------------------------------------------------------

enum TextRecoveryOutcome {
    /// Retry this iteration (text recovery injected system messages).
    Continue,
    /// Proceed to dispatch with the (possibly modified) response.
    Proceed(CompletionResponse),
}

async fn handle_text_recovery(
    ctx: &mut LoopContext<'_>,
    mut response: CompletionResponse,
    modality: &str,
) -> TextRecoveryOutcome {
    if !matches!(
        response.stop_reason,
        StopReason::EndTurn | StopReason::StopSequence
    ) || !response.tool_calls.is_empty()
    {
        return TextRecoveryOutcome::Proceed(response);
    }

    let tool_search_fn: Option<ToolSearchFn> = ctx.kernel.as_ref().map(|k| {
        let k = k.clone();
        let max_level = ctx.manifest.max_tool_level;
        Box::new(move |name: &str| -> Option<ToolDefinition> {
            k.search_tools(name, 1, max_level).into_iter().next().map(|(_, def)| def)
        }) as ToolSearchFn
    });
    let result = recover_text_tool_calls(&response.text(), ctx.tools(), tool_search_fn);

    let has_discovered = !result.discovered_tools.is_empty();
    if has_discovered {
        for def in &result.discovered_tools {
            ctx.discovered_tool_names.insert(def.name.clone());
            info!(tool = %def.name, schema = %def.input_schema, "Discovered tool schema");
        }
        info!(found = result.discovered_tools.len(), "Auto-discovered tools from text-based tool call recovery");
        ctx.tools_owned.extend(result.discovered_tools);
    }

    if !result.calls.is_empty() {
        info!(count = result.calls.len(), "Recovered text-based tool calls → promoting to ToolUse");
        response.tool_calls = result.calls;
        response.stop_reason = StopReason::ToolUse;
        let mut new_blocks: Vec<ContentBlock> = Vec::new();
        for block in &response.content {
            if let ContentBlock::Text { .. } = block {
                new_blocks.push(block.clone());
            }
        }
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
        if ctx.state.text_recovery_retries >= MAX_TEXT_RECOVERY_RETRIES {
            warn!(
                agent = %ctx.manifest.name,
                retries = ctx.state.text_recovery_retries,
                ctx.state.iteration,
                "Giving up text-based tool recovery — LLM keeps outputting text instead of tool_use"
            );
            // O10: Inject guidance so the LLM cleans up raw tool-call text
            ctx.messages.push(Message::system(
                "你刚才用文本描述了工具调用，但多次重试后仍无法转为结构化调用。\
                 请不要再尝试工具调用，直接用自然语言回复用户。\
                 不要在回复中包含 [Called ...] 或工具调用的原始文本。"
            ));
        } else {
            ctx.state.text_recovery_retries += 1;
            warn!(
                agent = %ctx.manifest.name,
                tools = ?result.needs_retry,
                ctx.state.iteration,
                retry = ctx.state.text_recovery_retries,
                "LLM described tool calls as text — retrying with discovered tools"
            );
            let tool_names = result.needs_retry.join("、");
            ctx.messages.push(Message::assistant(format!("我需要调用工具：{tool_names}。")));
            ctx.messages.push(Message::system(
                "你刚才用文本描述了工具调用，但用户看到的是原始文本。这些工具已添加到你的可用工具列表中，请直接用 tool_use 功能调用，带上完整的参数。不要再输出 [Called ...] 格式的文本。"
            ));
            ctx.state.log_turn(
                modality,
                "text_recovery_retry",
                response.usage.input_tokens as u32,
                response.usage.output_tokens as u32,
                response.tool_calls.iter().map(|tc| tc.name.clone()).collect(),
                0,
            );
            return TextRecoveryOutcome::Continue;
        }
    }

    TextRecoveryOutcome::Proceed(response)
}

// ---------------------------------------------------------------------------
// Phase: DISPATCH — route by StopReason
// ---------------------------------------------------------------------------

async fn dispatch(
    ctx: &mut LoopContext<'_>,
    response: CompletionResponse,
    modality: &str,
) -> CarrierResult<LoopAction> {
    match response.stop_reason {
        StopReason::EndTurn | StopReason::StopSequence => {
            // EndTurn: sync messages, then delegate
            match end_turn::handle_end_turn(
                &response,
                ctx.session,
                &mut ctx.messages,
                ctx.manifest,
                ctx.memory,
                ctx.kernel.as_ref(),
                ctx.memory_handle.as_ref(),
                ctx.brain.as_ref(),
                ctx.hooks,
                ctx.on_phase,
                ctx.session_base_len,
                ctx.user_message,
                ctx.owner_id,
                ctx.sender_id,
                ctx.channel_type,
                &ctx.agent_id_str,
                ctx.state.iteration,
                ctx.state.total_usage,
                ctx.state.any_tools_executed,
            )
            .await?
            {
                end_turn::EndTurnAction::Retry => return Ok(LoopAction::Continue),
                end_turn::EndTurnAction::Complete(result) => {
                    ctx.persist_last_run(state::RunOutcome::Complete);
                    return Ok(LoopAction::Complete(result));
                }
            }
        }
        StopReason::ToolUse => {
            match tool_use::handle_tool_use(
                &mut { response },
                ctx.session,
                &mut ctx.messages,
                ctx.manifest,
                ctx.memory,
                ctx.kernel.as_ref(),
                ctx.memory_handle.as_ref(),
                ctx.brain.as_ref(),
                ctx.hooks,
                ctx.on_phase,
                &ctx.stream_tx,
                ctx.mcp_connections,
                ctx.fetch_engine,
                ctx.workspace_root,
                ctx.process_manager,
                &ctx.context_budget,
                &ctx.hand_allowed_env,
                ctx.sender_id,
                ctx.owner_id,
                ctx.channel_type,
                &mut ctx.state.consecutive_max_tokens,
                &mut ctx.state.any_tools_executed,
                &mut ctx.state.recent_tool_calls,
                &mut ctx.tools_owned,
                &mut ctx.discovered_tool_names,
                &mut ctx.loaded_skills,
                &mut ctx.state.error_tracker,
                ctx.session_base_len,
                ctx.state.iteration,
            )
            .await
            {
                tool_use::ToolUseAction::Continue => {}
                tool_use::ToolUseAction::BreakWithPlan(plan) => {
                    ctx.detected_plan = Some(plan);
                    return Ok(LoopAction::BreakForPlan);
                }
            }
        }
        StopReason::MaxTokens => {
            match max_tokens::handle_max_tokens(
                &response,
                ctx.session,
                &mut ctx.messages,
                ctx.memory,
                &ctx.stream_tx,
                &mut ctx.state.consecutive_max_tokens,
                ctx.hooks,
                &ctx.agent_id_str,
                ctx.manifest,
                ctx.state.iteration,
                ctx.state.total_usage,
                ctx.session_base_len,
            )
            .await
            {
                max_tokens::MaxTokensAction::Continue => {
                    ctx.state.log_turn(
                        modality,
                        "max_tokens_continue",
                        response.usage.input_tokens as u32,
                        response.usage.output_tokens as u32,
                        vec![],
                        0,
                    );
                }
                max_tokens::MaxTokensAction::Complete(result) => {
                    return Ok(LoopAction::Complete(result));
                }
            }
        }
    }
    Ok(LoopAction::Continue)
}







// ---------------------------------------------------------------------------
// Phase: TEARDOWN — save failure state, persist loop state, return error
// ---------------------------------------------------------------------------

async fn teardown(ctx: &mut LoopContext<'_>) -> CarrierResult<AgentLoopResult> {
    // O8: Persist last run summary before teardown
    ctx.persist_last_run(state::RunOutcome::MaxIterations);

    // O6: Single-track — sync before teardown save
    helpers::sync_loop_messages(&ctx.messages, ctx.session, ctx.session_base_len);

    // Plan B: on failure, save only user message + error summary (discard tool noise)
    {
        let discarded = ctx.session.messages.len() - ctx.session_base_len;
        let summary = format!(
            "[Agent loop failed: max iterations ({}) exceeded. {} messages discarded.]",
            ctx.state.max_iterations, discarded,
        );
        let user_msg = ctx.session.messages[ctx.session_base_len..]
            .iter()
            .find(|m| m.role == Role::User)
            .cloned()
            .unwrap_or_else(|| Message::user(ctx.user_message));
        let fail_msgs = vec![user_msg, Message::assistant(&summary)];
        if let Err(e) = ctx.memory.save_session_append_async(
            ctx.session.id, &ctx.session.agent_name, &fail_msgs,
            ctx.session.context_window_tokens, ctx.session.label.as_deref(),
            Some(&ctx.session.turn_summaries),
        ).await {
            warn!("Failed to save failure summary: {e}");
        }
    }

    // Fire AgentLoopEnd hook on max iterations exceeded
    if let Some(hook_reg) = ctx.hooks {
        let ctx_hook = crate::hooks::HookContext {
            agent_name: &ctx.manifest.name,
            agent_id: ctx.agent_id_str.as_str(),
            event: types::agent::HookEvent::AgentLoopEnd,
            data: serde_json::json!({
                "reason": "max_iterations_exceeded",
                "iterations": ctx.state.max_iterations,
            }),
        };
        let _ = hook_reg.fire(&ctx_hook);
    }

    // If task_plan was detected, return success with the plan
    if let Some(plan) = ctx.detected_plan.take() {
        return Ok(AgentLoopResult {
            response: format!("Plan '{}' created with {} steps. Executing...", plan.title, plan.steps.len()),
            total_usage: ctx.state.total_usage,
            iterations: ctx.state.iteration + 1,
            silent: false,
            directives: Default::default(),
            plan: Some(plan),
        });
    }

    Err(CarrierError::MaxIterationsExceeded(ctx.state.max_iterations))
}

#[cfg(test)]
mod tests;
