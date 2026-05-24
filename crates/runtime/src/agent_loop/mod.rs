//! Core agent execution loop.
//!
//! The agent loop handles receiving a user message, recalling relevant memories,
//! calling the LLM, executing tool calls, and saving the conversation.

use crate::auth_cooldown::{CooldownVerdict, ProviderCooldown};
use crate::context_budget::{apply_context_guard, truncate_tool_result_dynamic, ContextBudget};
use crate::context_overflow::{recover_from_overflow, RecoveryStage};
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::{
    Brain, CompletionRequest, CompletionResponse, LlmDriver, LlmError, StreamEvent,
};
use crate::llm_errors;

use crate::mcp::McpConnection;
use crate::tool_context::ToolContext;
use crate::tool_runner;
use crate::web_fetch::WebFetchEngine;
use crate::text_tool_recovery::recover_text_tool_calls;
use memory::session::Session;
use memory::MemorySubstrate;
use types::agent::AgentManifest;
use types::error::{CarrierError, CarrierResult};
use types::message::{ContentBlock, Message, MessageContent, Role, StopReason, TokenUsage, TurnSummary};
use types::tool::ToolDefinition;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Maximum iterations in the agent loop before giving up.
const MAX_ITERATIONS: u32 = 25;

/// Maximum full messages to retain in session (3 turns × 2 = 6).
const MAX_RETAINED_MESSAGES: usize = 6;

/// Max tokens for turn summary generation.
const SUMMARY_MAX_TOKENS: u32 = 100;

/// Summary modality (fast/cheap).
const SUMMARY_MODALITY: &str = "fast";

/// Tool search recall limit (stage 1: how many candidates to retrieve).
const TOOL_SEARCH_RECALL_LIMIT: usize = 10;

/// Maximum retries for rate-limited or overloaded API calls.
const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds).
const BASE_RETRY_DELAY_MS: u64 = 1000;

/// Timeout for individual tool executions (seconds).
/// Raised from 60s to 120s for browser automation and long-running builds.
const TOOL_TIMEOUT_SECS: u64 = 120;

/// Overall timeout for the entire agent loop (seconds).
/// Prevents the agent from hanging indefinitely if the LLM API becomes
/// unresponsive. After this timeout, the loop is aborted and an error
/// is returned so the caller can notify the user.
/// Raised from 300s to 600s — compaction + multiple LLM calls + tool
/// execution easily exceed 300s with 30+ iterations.
const AGENT_LOOP_TIMEOUT_SECS: u64 = 1200;

/// Timeout for a single LLM API call (seconds).
/// Catches mid-stream hangs where the server goes silent after connection.
const PER_LLM_CALL_TIMEOUT_SECS: u64 = 180;

/// Maximum consecutive MaxTokens continuations before returning partial response.
/// Raised from 3 to 5 to allow longer-form generation.
const MAX_CONTINUATIONS: u32 = 5;

/// Maximum message history size before auto-trimming to prevent context overflow.
const MAX_HISTORY_MESSAGES: usize = 30;

/// Number of consecutive identical tool calls (same name AND same input) that
/// constitute a loop. Picked at 6 because:
/// - Below 4 risks blocking legitimate retries (e.g. eventual-consistency reads)
/// - Above 8 wastes API calls before kicking in
///
/// Same-name-different-input (e.g. paginated search) is NOT a loop.
const LOOP_DETECTION_WINDOW: usize = 6;

/// Default context window size (tokens) for token-based trimming.
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

/// Hash a tool input value for loop detection. Two calls with the same hash
/// are considered identical for loop-detection purposes.
fn tool_input_hash(input: &serde_json::Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let serialized = serde_json::to_string(input).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}

/// Detect a tool-use loop: returns the (name, input_hash) of the looping call
/// if the last `window` entries are all the same (name, input_hash), else None.
fn detect_tool_loop(recent: &[(String, u64)], window: usize) -> Option<(String, u64)> {
    if recent.len() < window {
        return None;
    }
    let tail = &recent[recent.len() - window..];
    let first = &tail[0];
    if tail.iter().all(|entry| entry == first) {
        Some(first.clone())
    } else {
        None
    }
}

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
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
) -> CarrierResult<AgentLoopResult> {
    let timeout = std::time::Duration::from_secs(AGENT_LOOP_TIMEOUT_SECS);
    match tokio::time::timeout(
        timeout,
        run_agent_loop_impl(
            manifest, user_message, session, memory, driver, tools,
            kernel, stream_tx, mcp_connections, fetch_engine, workspace_root,
            on_phase, hooks, context_window_tokens, process_manager,
            user_content_blocks, brain, sender_id, owner_id, channel_type,
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

/// Call an LLM driver with automatic retry on rate-limit and overload errors.
///
/// Uses the `llm_errors` classifier for smart error handling and the
/// `ProviderCooldown` circuit breaker to prevent request storms.
async fn call_with_retry(
    driver: &dyn LlmDriver,
    request: CompletionRequest,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    provider: Option<&str>,
    cooldown: Option<&ProviderCooldown>,
) -> CarrierResult<crate::llm_driver::CompletionResponse> {
    let is_stream = stream_tx.is_some();

    // Check circuit breaker before calling
    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
        match cooldown.check(provider) {
            CooldownVerdict::Reject {
                reason,
                retry_after_secs,
            } => {
                return Err(CarrierError::LlmDriver(format!(
                    "Provider '{provider}' is in cooldown ({reason}). Retry in {retry_after_secs}s."
                )));
            }
            CooldownVerdict::AllowProbe => {
                debug!(
                    provider,
                    is_stream, "Allowing probe request through circuit breaker"
                );
            }
            CooldownVerdict::Allow => {}
        }
    }

    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        let call = async {
            match &stream_tx {
                Some(tx) => driver.stream(request.clone(), tx.clone()).await,
                None => driver.complete(request.clone()).await,
            }
        };
        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(PER_LLM_CALL_TIMEOUT_SECS),
            call,
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                warn!(attempt, "LLM call timed out after {PER_LLM_CALL_TIMEOUT_SECS}s");
                last_error = Some("LLM call timed out".to_string());
                if attempt == MAX_RETRIES {
                    return Err(CarrierError::LlmDriver(format!(
                        "LLM call timed out after {}s — server may be unresponsive",
                        PER_LLM_CALL_TIMEOUT_SECS
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(
                    BASE_RETRY_DELAY_MS * 2u64.pow(attempt),
                ))
                .await;
                continue;
            }
        };
        match result {
            Ok(response) => {
                if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                    cooldown.record_success(provider);
                }
                return Ok(response);
            }
            Err(LlmError::RateLimited { retry_after_ms }) => {
                if attempt == MAX_RETRIES {
                    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                        cooldown.record_failure(provider, false);
                    }
                    return Err(CarrierError::LlmDriver(format!(
                        "Rate limited after {} retries",
                        MAX_RETRIES
                    )));
                }
                let delay = std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!(
                    attempt,
                    delay_ms = delay,
                    is_stream,
                    "Rate limited, retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some("Rate limited".to_string());
            }
            Err(LlmError::Overloaded { retry_after_ms }) => {
                if attempt == MAX_RETRIES {
                    if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                        cooldown.record_failure(provider, false);
                    }
                    return Err(CarrierError::LlmDriver(format!(
                        "Model overloaded after {} retries",
                        MAX_RETRIES
                    )));
                }
                let delay = std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
                warn!(
                    attempt,
                    delay_ms = delay,
                    is_stream,
                    "Model overloaded, retrying after delay"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                last_error = Some("Overloaded".to_string());
            }
            Err(e) => {
                let raw_error = e.to_string();
                let status = match &e {
                    LlmError::Api { status, .. } => Some(*status),
                    _ => None,
                };
                let classified = llm_errors::classify_error(&raw_error, status);
                warn!(
                    category = ?classified.category,
                    retryable = classified.is_retryable,
                    raw = %raw_error,
                    is_stream,
                    "LLM error classified: {}",
                    classified.sanitized_message
                );

                if let (Some(provider), Some(cooldown)) = (provider, cooldown) {
                    cooldown.record_failure(provider, classified.is_billing);
                }

                let user_msg = if classified.category == llm_errors::LlmErrorCategory::Format {
                    format!("{} — raw: {}", classified.sanitized_message, raw_error)
                } else {
                    classified.sanitized_message
                };
                return Err(CarrierError::LlmDriver(user_msg));
            }
        }
    }

    Err(CarrierError::LlmDriver(
        last_error.unwrap_or_else(|| "Unknown error".to_string()),
    ))
}

/// Call LLM with unified fallback across Brain endpoints.
/// When `stream_tx` is `Some`, uses streaming mode; otherwise non-streaming.
async fn call_with_fallback(
    brain: Option<&Arc<dyn Brain>>,
    fallback_driver: &dyn LlmDriver,
    modality: &str,
    request: CompletionRequest,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
) -> CarrierResult<CompletionResponse> {
    let Some(brain) = brain else {
        return call_with_retry(fallback_driver, request, stream_tx, None, None).await;
    };

    let endpoints = brain.endpoints_for(modality);
    if endpoints.is_empty() {
        return Err(CarrierError::LlmDriver(format!(
            "No available endpoints for modality '{modality}' — all endpoints circuit-broken or not configured"
        )));
    }

    let mut last_error: Option<CarrierError> = None;
    for ep in &endpoints {
        if let Some(driver) = brain.driver_for_endpoint(&ep.id) {
            let mut req = request.clone();
            req.model = ep.model.clone();
            let start = std::time::Instant::now();
            let tx_arg = stream_tx.clone();
            match call_with_retry(&*driver, req, tx_arg, Some(&ep.provider), None).await {
                Ok(response) => {
                    let latency = start.elapsed().as_millis() as u64;
                    brain.report(types::brain::EndpointReport {
                        endpoint_id: ep.id.clone(),
                        success: true,
                        latency_ms: latency,
                        error: None,
                    });
                    return Ok(response);
                }
                Err(e) => {
                    let latency = start.elapsed().as_millis() as u64;
                    let err_str = format!("{e}");
                    brain.report(types::brain::EndpointReport {
                        endpoint_id: ep.id.clone(),
                        success: false,
                        latency_ms: latency,
                        error: Some(err_str),
                    });
                    tracing::warn!(
                        endpoint = %ep.id,
                        error = %e,
                        "Endpoint failed in fallback chain, trying next"
                    );
                    last_error = Some(e);
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        CarrierError::LlmDriver(format!("All endpoints exhausted for modality '{modality}'"))
    }))
}

/// Run the agent execution loop with streaming support.
///
/// Like `run_agent_loop`, but sends `StreamEvent`s to the provided channel
/// as tokens arrive from the LLM. Tool execution happens between LLM calls
/// and is not streamed.
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
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
) -> CarrierResult<AgentLoopResult> {
    info!(agent = %manifest.name, "Starting agent loop");

    // Extract hand-allowed env vars from manifest metadata (set by kernel for hand settings)
    let hand_allowed_env: Vec<String> = manifest
        .metadata
        .get("hand_allowed_env")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // TODO(Phase 13): Tree memory recall will be restored here.
    // Fire BeforePromptBuild hook
    let agent_id_str = session.agent_id.clone();
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
    let mut system_prompt = manifest.model.system_prompt.clone();

    // Inject turn summaries into system prompt (L1 context layer)
    if !session.turn_summaries.is_empty() {
        let summaries_text = session
            .turn_summaries
            .iter()
            .map(|s| {
                format!(
                    "- Turn {}: {} → {} (tools: {})",
                    s.turn_number,
                    s.user_intent,
                    s.assistant_outcome,
                    s.tools_used.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        system_prompt.push_str(&format!(
            "\n\n## Previous conversation turns\n{}",
            summaries_text
        ));
    }

    // Track which messages existed before this agent loop started.
    // Used by merge-writes to append only our new messages to the session.
    let session_base_len = session.messages.len();

    // Helper for concurrency-safe session saves.
    macro_rules! save_new {
        () => {
            memory.save_session_append_async(
                session.id,
                &session.agent_id,
                &session.messages[session_base_len..],
                session.context_window_tokens,
                session.label.as_deref(),
                None,
            )
        };
    }

    // Add the user message to session history.
    // When content blocks are provided (e.g. text + image from a channel),
    // use multimodal message format so the LLM receives the image for vision.
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

    // Validate and repair session history (drop orphans, merge consecutive)
    let mut messages = crate::session_repair::validate_and_repair(&llm_messages);

    // Inject canonical context as the first user message (not in system prompt)
    // to keep the system prompt stable across turns for provider prompt caching.
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
    let final_response;

    // Safety valve: trim excessively long message histories to prevent context overflow.
    if messages.len() > MAX_HISTORY_MESSAGES {
        let trim_count = messages.len() - MAX_HISTORY_MESSAGES;
        warn!(
            agent = %manifest.name,
            total_messages = messages.len(),
            trimming = trim_count,
            "Trimming old messages to prevent context overflow"
        );
        crate::context_overflow::pair_aware_drain(&mut messages, trim_count);
    }

    // Use autonomous config max_iterations if set, else default
    let max_iterations = manifest
        .autonomous
        .as_ref()
        .map(|a| a.max_iterations)
        .unwrap_or(MAX_ITERATIONS);

    let mut consecutive_max_tokens: u32 = 0;

    // Build context budget from model's actual context window (or fallback to default)
    let ctx_window = context_window_tokens.unwrap_or(DEFAULT_CONTEXT_WINDOW);
    let context_budget = ContextBudget::new(ctx_window);
    let mut any_tools_executed = false;

    // Track task_plan produced during this loop
    let mut detected_plan: Option<TaskPlan> = None;

    // Track recent (tool_name, input_hash) for loop detection
    let mut recent_tool_calls: Vec<(String, u64)> = Vec::new();

    // Track consecutive tool errors: tool_name → count of consecutive errors
    let mut consecutive_tool_errors: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    const MAX_CONSECUTIVE_TOOL_ERRORS: u32 = 3;

    // Owned copy for loop detection tool removal
    let mut tools_owned: Vec<ToolDefinition> = tools.to_vec();
    let mut tools: &[ToolDefinition] = &tools_owned;
    // Track which tools were added by tool_search (not in the initial core set).
    // On a new tool_search, previous discovered tools are evicted before adding new ones.
    let mut discovered_tool_names: std::collections::HashSet<String> = std::collections::HashSet::new();

    for iteration in 0..max_iterations {
        debug!(iteration, "Streaming agent loop iteration");

        // Context overflow recovery pipeline (replaces emergency_trim_messages)
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
            model: String::new(), // Model set by Brain/endpoint, not agent
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

        // Stream LLM: Brain selects the driver + model, then we stream directly.
        // Brain handles routing (modality → endpoint) and fallback chain selection.
        // The actual streaming call goes through driver.stream() for real token-by-token output.
        // Call LLM with unified streaming fallback across Brain endpoints
        let modality = if manifest.model.modality.is_empty() {
            "chat"
        } else {
            &manifest.model.modality
        };
        let mut response = call_with_fallback(
            brain.as_ref(),
            &*driver,
            modality,
            request,
            stream_tx.clone(),
        )
        .await?;

        total_usage.input_tokens += response.usage.input_tokens;
        total_usage.output_tokens += response.usage.output_tokens;

        // Recover tool calls output as text (streaming path)
        if matches!(
            response.stop_reason,
            StopReason::EndTurn | StopReason::StopSequence
        ) && response.tool_calls.is_empty()
        {
            let recovered = recover_text_tool_calls(&response.text(), tools);
            if !recovered.is_empty() {
                info!(
                    count = recovered.len(),
                    "Recovered text-based tool calls  → promoting to ToolUse"
                );
                response.tool_calls = recovered;
                response.stop_reason = StopReason::ToolUse;
                let mut new_blocks: Vec<ContentBlock> = Vec::new();
                for tc in &response.tool_calls {
                    new_blocks.push(ContentBlock::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                        provider_metadata: None,
                    });
                }
                response.content = new_blocks;
            } else if let Some(ref kernel) = kernel {
                // LLM referenced a tool not in the current tools list (e.g. "[Called sqlite_query]").
                // Auto-discover those tools so the LLM can call them properly next iteration.
                let tool_name_set: std::collections::HashSet<&str> =
                    tools.iter().map(|t| t.name.as_str()).collect();
                let mut undiscovered: Vec<String> = Vec::new();
                for line in response.text().lines() {
                    let trimmed = line.trim();
                    let Some(after) = trimmed.strip_prefix("[Called ") else { continue };
                    let Some(close) = after.find(']') else { continue };
                    let inner = &after[..close];
                    let tool_name = inner
                        .find(|c: char| c == ' ' || c == ':' || c == '(' || c == '{')
                        .map(|pos| &inner[..pos])
                        .unwrap_or(inner);
                    if !tool_name.is_empty() && !tool_name.contains(' ') && !tool_name_set.contains(tool_name) {
                        undiscovered.push(tool_name.to_string());
                    }
                }
                if !undiscovered.is_empty() {
                    let mut found: Vec<types::tool::ToolDefinition> = Vec::new();
                    for name in &undiscovered {
                        if let Some((_, def)) = kernel.search_tools(name, 1, manifest.max_tool_level).into_iter().next() {
                            found.push(def);
                        }
                    }
                    if !found.is_empty() {
                        for def in &found {
                            discovered_tool_names.insert(def.name.clone());
                        }
                        info!(
                            found = found.len(),
                            requested = undiscovered.len(),
                            "Auto-discovered tools from [Called ...] pattern"
                        );
                        tools_owned.extend(found);
                        tools = &tools_owned;
                    }
                }
            }
        }

        match response.stop_reason {
            StopReason::EndTurn | StopReason::StopSequence => {
                let text = response.text();

                // Parse reply directives from the streaming response text
                let (cleaned_text_s, parsed_directives_s) =
                    crate::reply_directives::parse_directives(&text);
                let text = cleaned_text_s;

                // NO_REPLY: agent intentionally chose not to reply
                if text.trim() == "NO_REPLY" || parsed_directives_s.silent {
                    debug!(agent = %manifest.name, "Agent chose NO_REPLY/silent  — silent completion");
                    session
                        .messages
                        .push(Message::assistant("[no reply needed]".to_string()));
                    let new_msgs = &session.messages[session_base_len..];
                    memory
                        .save_session_append_async(session.id, &session.agent_id, new_msgs, session.context_window_tokens, session.label.as_deref(), None)
                        .await
                        .map_err(|e| CarrierError::Memory(e.to_string()))?;
                    return Ok(AgentLoopResult {
                        response: String::new(),
                        total_usage,
                        iterations: iteration + 1,
                        silent: true,
                        directives: types::message::ReplyDirectives {
                            reply_to: parsed_directives_s.reply_to,
                            current_thread: parsed_directives_s.current_thread,
                            silent: true,
                        },
                        plan: None,
                    });
                }

                // One-shot retry: if the LLM returns empty text with no tool use,
                // try once more before accepting the empty result.
                // Triggers on first call OR when input_tokens=0 (silently failed request).
                if text.trim().is_empty() && response.tool_calls.is_empty() {
                    let is_silent_failure =
                        response.usage.input_tokens == 0 && response.usage.output_tokens == 0;
                    if iteration == 0 || is_silent_failure {
                        warn!(
                            agent = %manifest.name,
                            iteration,
                            input_tokens = response.usage.input_tokens,
                            output_tokens = response.usage.output_tokens,
                            silent_failure = is_silent_failure,
                            "Empty response , retrying once"
                        );
                        // Re-validate messages before retry — the history may have
                        // broken tool_use/tool_result pairs that caused the failure.
                        if is_silent_failure {
                            messages = crate::session_repair::validate_and_repair(&messages);
                        }
                        messages.push(Message::assistant("[no response]".to_string()));
                        messages.push(Message::user("Please provide your response.".to_string()));
                        continue;
                    }
                }

                // Guard against empty response — covers both iteration 0 and post-tool cycles
                let text = if text.trim().is_empty() {
                    warn!(
                        agent = %manifest.name,
                        iteration,
                        input_tokens = total_usage.input_tokens,
                        output_tokens = total_usage.output_tokens,
                        messages_count = messages.len(),
                        "Empty response from LLM  — guard activated"
                    );
                    if any_tools_executed {
                        "[Task completed — the agent executed tools but did not produce a text summary.]".to_string()
                    } else {
                        "[The model returned an empty response. This usually means the model is overloaded, the context is too large, or the API key lacks credits. Try again or check /status.]".to_string()
                    }
                } else {
                    text
                };
                final_response = text.clone();
                session.messages.push(Message::assistant(text));

                // Prune NO_REPLY heartbeat turns to save context budget
                crate::session_repair::prune_heartbeat_turns(&mut session.messages, 10);

                // Generate turn summary for this conversation turn
                let turn_msgs = &session.messages[session_base_len..];
                if let Some(ref brain_ref) = brain {
                    if let Some(mut summary) = generate_turn_summary(turn_msgs, brain_ref).await {
                        summary.turn_number = session.turn_summaries.len() as u32 + 1;
                        info!(
                            agent = %manifest.name,
                            turn = summary.turn_number,
                            intent = %summary.user_intent,
                            outcome = %summary.assistant_outcome,
                            "Turn summary generated"
                        );
                        session.turn_summaries.push(summary);
                    }
                }

                // Capture new messages BEFORE trim — session_base_len becomes invalid after trim.
                let new_msgs: Vec<Message> = session.messages[session_base_len..].to_vec();

                // Trim old messages if over retention threshold
                trim_oldest_turns(&mut session.messages, MAX_RETAINED_MESSAGES);

                memory
                    .save_session_append_async(
                        session.id,
                        &session.agent_id,
                        &new_msgs,
                        session.context_window_tokens,
                        session.label.as_deref(),
                        Some(&session.turn_summaries),
                    )
                    .await
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;

                // TODO(Phase 13): Tree memory remember will be restored here.

                // Fire-and-forget tree ingestion
                if let Some(kh) = kernel.as_ref() {
                    let req = types::memory_tree::IngestRequest {
                        owner_id: owner_id.unwrap_or("default").to_string(),
                        agent_id: session.agent_id.to_string(),
                        source_kind: "chat".to_string(),
                        source_id: format!("{}:{}",
                            channel_type.unwrap_or("api"),
                            sender_id.unwrap_or("unknown")),
                        messages: vec![types::memory_tree::IngestMessage {
                            sender: sender_id.unwrap_or("user").to_string(),
                            content: user_message.to_string(),
                            timestamp_ms: chrono::Utc::now().timestamp_millis(),
                        }],
                        tags: vec![channel_type.unwrap_or("api").to_string()],
                    };
                    let kh = Arc::clone(kh);
                    tokio::spawn(async move {
                        if let Err(e) = kh.tree_ingest(req).await {
                            tracing::warn!(error = %e, "tree_ingest failed");
                        }
                    });
                }

                // Notify phase: Done
                if let Some(cb) = on_phase {
                    cb(LoopPhase::Done);
                }

                info!(
                    agent = %manifest.name,
                    iterations = iteration + 1,
                    tokens = total_usage.total(),
                    "Streaming agent loop completed"
                );

                // Fire AgentLoopEnd hook
                if let Some(hook_reg) = hooks {
                    let ctx = crate::hooks::HookContext {
                        agent_name: &manifest.name,
                        agent_id: agent_id_str.as_str(),
                        event: types::agent::HookEvent::AgentLoopEnd,
                        data: serde_json::json!({
                            "iterations": iteration + 1,
                            "response_length": final_response.len(),
                        }),
                    };
                    let _ = hook_reg.fire(&ctx);
                }

                return Ok(AgentLoopResult {
                    response: final_response,
                    total_usage,
                    iterations: iteration + 1,
                    silent: false,
                    directives: Default::default(),
                    plan: None,
                });
            }
            StopReason::ToolUse => {
                // Reset MaxTokens continuation counter on tool use
                consecutive_max_tokens = 0;
                any_tools_executed = true;

                let assistant_blocks = response.content.clone();

                session.messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_blocks.clone()),
                });
                messages.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(assistant_blocks),
                });

                let caller_id_str = session.agent_id.to_string();

                // Track tool calls for loop detection BEFORE execution
                for tc in &response.tool_calls {
                    recent_tool_calls.push((tc.name.clone(), tool_input_hash(&tc.input)));
                }
                if recent_tool_calls.len() > LOOP_DETECTION_WINDOW * 3 {
                    let drain_count = recent_tool_calls.len() - LOOP_DETECTION_WINDOW * 2;
                    recent_tool_calls.drain(..drain_count);
                }

                // Detect loop: same (name, input_hash) repeated LOOP_DETECTION_WINDOW times.
                // Instead of terminating the agent loop, remove the looping tool and
                // inject a system message so the LLM can continue with other tools.
                if let Some((looping_name, _)) = detect_tool_loop(&recent_tool_calls, LOOP_DETECTION_WINDOW) {
                    warn!(
                        agent = %manifest.name,
                        tool = %looping_name,
                        consecutive = LOOP_DETECTION_WINDOW,
                        iteration,
                        "Tool loop detected — removing tool and continuing"
                    );
                    // Remove the looping tool from available tools
                    tools_owned.retain(|t| t.name != looping_name);
                    tools = &tools_owned;
                    recent_tool_calls.clear();
                    // Inject a system message telling the LLM to stop using this tool
                    let warning = format!(
                        "工具 `{looping_name}` 连续多次返回相同结果，已被临时移除。请用其他方式完成任务，不要再用这个工具。"
                    );
                    messages.push(Message::system(&warning));
                }

                // Execute each tool call with timeout and truncation
                let mut tool_result_blocks = Vec::new();
                for tool_call in &response.tool_calls {
                    debug!(tool = %tool_call.name, id = %tool_call.id, "Executing tool");

                    // Notify phase: ToolUse
                    if let Some(cb) = on_phase {
                        let sanitized: String = tool_call
                            .name
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
                                "tool_name": &tool_call.name,
                                "input": &tool_call.input,
                            }),
                        };
                        if let Err(reason) = hook_reg.fire(&ctx) {
                            tool_result_blocks.push(ContentBlock::ToolResult {
                                tool_use_id: tool_call.id.clone(),
                                tool_name: tool_call.name.clone(),
                                content: format!(
                                    "Hook blocked tool '{}': {}",
                                    tool_call.name, reason
                                ),
                                is_error: true,
                            });
                            continue;
                        }
                    }

                    // Resolve effective exec policy (per-agent override or global)
                    let effective_exec_policy = manifest.exec_policy.as_ref();

                    let home_dir_buf = kernel.as_ref().and_then(|k| k.home_dir());
                    let tool_ctx = ToolContext {
                        kernel: kernel.as_ref(),
                        caller_agent_id: Some(&caller_id_str),
                        mcp_connections,
                        fetch_engine,
                        allowed_env_vars: if hand_allowed_env.is_empty() {
                            None
                        } else {
                            Some(&hand_allowed_env)
                        },
                        workspace_root,
                        brain: brain.as_ref(),
                        exec_policy: effective_exec_policy,

                        process_manager,
                        sender_id,
                        owner_id,
                        home_dir: home_dir_buf.as_deref(),
                        agent_name: Some(&manifest.name),
                        subagent_configs: if manifest.subagents.is_empty() { None } else { Some(&manifest.subagents) },
                        channel_type,
                        max_tool_level: manifest.max_tool_level,
                    };

                    // Timeout-wrapped execution
                    let result = match tokio::time::timeout(
                        Duration::from_secs(TOOL_TIMEOUT_SECS),
                        tool_runner::execute_tool(
                            &tool_call.id,
                            &tool_call.name,
                            &tool_call.input,
                            &tool_ctx,
                        ),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => {
                            warn!(tool = %tool_call.name, "Tool execution timed out after {}s", TOOL_TIMEOUT_SECS);
                            types::tool::ToolResult {
                                tool_use_id: tool_call.id.clone(),
                                content: format!(
                                    "Tool '{}' timed out after {}s.",
                                    tool_call.name, TOOL_TIMEOUT_SECS
                                ),
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
                                "tool_name": &tool_call.name,
                                "result": &result.content,
                                "is_error": result.is_error,
                            }),
                        };
                        let _ = hook_reg.fire(&ctx);
                    }

                    // Dynamic truncation based on context budget (replaces flat MAX_TOOL_RESULT_CHARS)
                    let final_content = truncate_tool_result_dynamic(&result.content, &context_budget);

                    // Notify client of tool execution result (detect dead consumer)
                    if let Some(tx) = &stream_tx {
                        let preview: String = final_content.chars().take(300).collect();
                        if tx
                            .send(StreamEvent::ToolExecutionResult {
                                id: tool_call.id.clone(),
                                name: tool_call.name.clone(),
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
                        tool_name: tool_call.name.clone(),
                        content: final_content,
                        is_error: result.is_error,
                    });
                }

                // Detect tool errors and inject guidance to prevent fabrication
                let error_count = tool_result_blocks
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolResult { is_error: true, .. }))
                    .count();

                // Track which tools succeeded this iteration (to reset their error counter)
                let succeeded_tools: std::collections::HashSet<&str> = tool_result_blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult { is_error: false, tool_name, .. } => Some(tool_name.as_str()),
                        _ => None,
                    })
                    .collect();
                for name in &succeeded_tools {
                    consecutive_tool_errors.remove(*name);
                }

                if error_count > 0 {
                    // Collect failed tool names to detect repeated failures
                    let failed_tools: Vec<&str> = tool_result_blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult { is_error: true, tool_name, .. } => {
                                Some(tool_name.as_str())
                            }
                            _ => None,
                        })
                        .collect();

                    // Increment consecutive error counters
                    for name in &failed_tools {
                        *consecutive_tool_errors.entry(name.to_string()).or_insert(0) += 1;
                    }

                    // Remove tools that have failed too many times consecutively
                    let mut removed_tools = Vec::new();
                    for (name, count) in &consecutive_tool_errors {
                        if *count >= MAX_CONSECUTIVE_TOOL_ERRORS && tools_owned.iter().any(|t| t.name == *name) {
                            warn!(
                                agent = %manifest.name,
                                tool = %name,
                                consecutive_errors = count,
                                "Tool failed {MAX_CONSECUTIVE_TOOL_ERRORS} times consecutively — removing"
                            );
                            tools_owned.retain(|t| t.name != *name);
                            tools = &tools_owned;
                            removed_tools.push(name.clone());
                        }
                    }
                    for name in &removed_tools {
                        consecutive_tool_errors.remove(name);
                    }

                    info!(
                        agent = %manifest.name,
                        iteration,
                        error_count,
                        failed_tools = ?failed_tools,
                        "Tool errors in agent loop iteration"
                    );

                    let mut guidance = format!(
                        "[System: {} tool(s) returned errors. Report the error honestly \
                         to the user. Do NOT fabricate results or pretend the tool succeeded. \
                         Do NOT retry the same failed tool call. \
                         If a search or fetch failed, tell the user it failed and suggest \
                         alternatives instead of making up data.]",
                        error_count
                    );
                    if !removed_tools.is_empty() {
                        guidance.push_str(&format!(
                            " 工具 {} 连续失败已被移除，请勿再调用。",
                            removed_tools.join(", ")
                        ));
                    }
                    tool_result_blocks.push(ContentBlock::Text {
                        text: guidance,
                        provider_metadata: None,
                    });
                }

                let tool_results_msg = Message {
                    role: Role::User,
                    content: MessageContent::Blocks(tool_result_blocks.clone()),
                };
                session.messages.push(tool_results_msg.clone());
                messages.push(tool_results_msg);

                // Dynamic tool refresh (streaming path)
                let tools_may_have_changed = response.tool_calls.iter().any(|tc| {
                    matches!(
                        tc.name.as_str(),
                        "train_write" | "file_write" | "tool_search" | "skill_load"
                    )
                });
                if tools_may_have_changed {
                    if let Some(ref kernel) = kernel {
                        let _agent_id_str = session.agent_id.to_string();

                        // Log skill_load calls
                        let skill_load_count = response.tool_calls.iter()
                            .filter(|tc| tc.name == "skill_load")
                            .count();
                        if skill_load_count > 0 {
                            info!(count = skill_load_count, "Skill(s) loaded");
                        }

                        // tool_search: add found tools to the tools list so the LLM API
                        // allows outputting tool_use for them on the next iteration.
                        // The LLM already saw the tool definitions in the tool_search result,
                        // but the API requires tools to be in CompletionRequest.tools for
                        // structured tool_use output.
                        let search_queries: Vec<&str> = response.tool_calls.iter()
                            .filter(|tc| tc.name == "tool_search")
                            .filter_map(|tc| tc.input.get("query").and_then(|v| v.as_str()))
                            .collect();

                        let mut found_tools: Vec<types::tool::ToolDefinition> = Vec::new();
                        let mut found_names: std::collections::HashSet<String> = std::collections::HashSet::new();

                        for q in &search_queries {
                            let results = kernel.search_tools(q, TOOL_SEARCH_RECALL_LIMIT, manifest.max_tool_level);
                            for (_, def) in results {
                                if found_names.insert(def.name.clone()) {
                                    found_tools.push(def);
                                }
                            }
                        }

                        if !found_tools.is_empty() {
                            // Evict previously discovered tools before adding new ones.
                            // Each tool_search represents a new intent — old discoveries
                            // are stale and waste tokens in CompletionRequest.tools.
                            if !discovered_tool_names.is_empty() {
                                let before = tools_owned.len();
                                let stale: std::collections::HashSet<String> = discovered_tool_names.drain().collect();
                                tools_owned.retain(|t| !stale.contains(&t.name));
                                let evicted = before - tools_owned.len();
                                if evicted > 0 {
                                    info!(evicted, "tool_search: evicted previous discovered tools");
                                }
                            }

                            // Add discovered tools so the LLM API allows structured
                            // tool_use output. Cap total to prevent unbounded inflation.
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
                            tools = &tools_owned;
                        }
                    }
                }

                // Note: no per-iteration save here — save happens at loop end
                // (success → full save, failure → summary only)

                // Detect task_plan: extract plan data and break out of the loop
                if let Some(tc) = response.tool_calls.iter().find(|tc| tc.name == "task_plan") {
                    let title = tc.input["title"].as_str().unwrap_or("").to_string();
                    let steps: Vec<TaskStep> = tc.input["steps"].as_array()
                        .map(|arr| arr.iter().filter_map(|s| {
                            Some(TaskStep {
                                id: s["id"].as_str()?.to_string(),
                                prompt: s["prompt"].as_str()?.to_string(),
                                depends_on: s["depends_on"].as_array()
                                    .map(|d| d.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                                    .unwrap_or_default(),
                            })
                        }).collect())
                        .unwrap_or_default();
                    if !steps.is_empty() {
                        info!(
                            plan_title = %title,
                            steps = steps.len(),
                            "task_plan detected — breaking out of agent loop"
                        );
                        detected_plan = Some(TaskPlan { title, steps });
                        // Save session before breaking
                        if let Err(e) = save_new!().await {
                            warn!("Failed to save session before plan break: {e}");
                        }
                        break;
                    }
                }
            }
            StopReason::MaxTokens => {
                consecutive_max_tokens += 1;
                if consecutive_max_tokens >= MAX_CONTINUATIONS {
                    let text = response.text();
                    let text = if text.trim().is_empty() {
                        "[Partial response — token limit reached with no text output.]".to_string()
                    } else {
                        text
                    };
                    session.messages.push(Message::assistant(&text));
                    if let Err(e) = save_new!().await {
                        warn!("Failed to save session on max continuations: {e}");
                    }
                    warn!(
                        iteration,
                        consecutive_max_tokens,
                        "Max continuations reached , returning partial response"
                    );
                    // Fire AgentLoopEnd hook
                    if let Some(hook_reg) = hooks {
                        let ctx = crate::hooks::HookContext {
                            agent_name: &manifest.name,
                            agent_id: agent_id_str.as_str(),
                            event: types::agent::HookEvent::AgentLoopEnd,
                            data: serde_json::json!({
                                "iterations": iteration + 1,
                                "reason": "max_continuations",
                            }),
                        };
                        let _ = hook_reg.fire(&ctx);
                    }
                    return Ok(AgentLoopResult {
                        response: text,
                        total_usage,
                        iterations: iteration + 1,
                        silent: false,
                        directives: Default::default(),
                        plan: None,
                    });
                }
                let text = response.text();
                session.messages.push(Message::assistant(&text));
                messages.push(Message::assistant(&text));
                session.messages.push(Message::user("Please continue."));
                messages.push(Message::user("Please continue."));
                warn!(iteration, "Max tokens hit , continuing");
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
            session.id, &session.agent_id, &fail_msgs,
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
            iterations: max_iterations, // approximate — loop was broken early
            silent: false,
            directives: Default::default(),
            plan: Some(plan),
        });
    }

    Err(CarrierError::MaxIterationsExceeded(max_iterations))
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
    sender_id: Option<&str>,
    owner_id: Option<&str>,
    channel_type: Option<&str>,
) -> CarrierResult<AgentLoopResult> {
    run_agent_loop(
        manifest, user_message, session, memory, driver, tools,
        kernel, Some(stream_tx), mcp_connections, fetch_engine, workspace_root,
        on_phase, hooks, context_window_tokens, process_manager,
        user_content_blocks, brain, sender_id, owner_id, channel_type,
    ).await
}

/// Generate a TurnSummary from the messages of a single conversation turn.
///
/// Extracts the user's intent and the assistant's outcome, then uses a
/// fast LLM call to produce a concise 1-2 sentence summary.
async fn generate_turn_summary(
    turn_msgs: &[Message],
    brain: &Arc<dyn Brain>,
) -> Option<TurnSummary> {
    // Helper to extract text from a message
    fn extract_text(msg: &Message) -> String {
        match &msg.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    // Extract user message (first in the slice)
    let user_text = turn_msgs
        .iter()
        .find(|m| m.role == Role::User)
        .map(extract_text)
        .unwrap_or_default();

    // Extract assistant response (last assistant message)
    let assistant_text = turn_msgs
        .iter()
        .rfind(|m| m.role == Role::Assistant)
        .map(extract_text)
        .unwrap_or_default();

    // Collect tool names used this turn
    let mut tools_used = Vec::new();
    for msg in turn_msgs {
        if let MessageContent::Blocks(blocks) = &msg.content {
            for block in blocks {
                if let ContentBlock::ToolUse { name, .. } = block {
                    if !tools_used.contains(name) {
                        tools_used.push(name.clone());
                    }
                }
            }
        }
    }

    let prompt = format!(
        "Summarize this conversation turn in 1-2 sentences. \
         Focus on what was accomplished, not how.\n\n\
         User: {}\nAssistant: {}\n\n\
         Format: User wanted X → Agent did Y",
        user_text, assistant_text
    );

    let request = CompletionRequest {
        model: String::new(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text(prompt),
        }],
        tools: Vec::new(),
        max_tokens: SUMMARY_MAX_TOKENS,
        temperature: 0.3,
        system: Some(
            "You are a conversation summarizer. Be concise.".to_string(),
        ),
        thinking: None,
        extra: Default::default(),
    };

    match brain.complete(SUMMARY_MODALITY, request).await {
        Ok(response) => {
            let text = response.text().trim().to_string();
            if text.is_empty() {
                return None;
            }
            // Parse "User wanted X → Agent did Y" format
            let parts: Vec<&str> = text.split("→").collect();
            let (user_intent, assistant_outcome) = if parts.len() >= 2 {
                (parts[0].trim().to_string(), parts[1].trim().to_string())
            } else {
                (user_text.clone(), text)
            };
            Some(TurnSummary {
                turn_number: 0, // filled in by caller
                timestamp: chrono::Utc::now().to_rfc3339(),
                user_intent,
                assistant_outcome,
                tools_used,
            })
        }
        Err(e) => {
            warn!("Turn summary generation failed: {}", e);
            None
        }
    }
}

/// Trim old messages from the session, keeping only the most recent N.
///
/// Messages are removed from the front of the list (oldest first).
/// The caller is responsible for having already generated TurnSummaries
/// for the turns being removed.
fn trim_oldest_turns(messages: &mut Vec<Message>, max_retained: usize) {
    if messages.len() <= max_retained {
        return;
    }
    // Drain from the front until we're at the threshold.
    // We drain in pairs (user + assistant) to keep whole turns.
    let excess = messages.len() - max_retained;
    // Round up to the nearest even number to preserve turn boundaries
    let drain_count = if excess % 2 == 0 {
        excess
    } else {
        excess + 1
    };
    messages.drain(..drain_count.min(messages.len()));
}


#[cfg(test)]
mod tests;
