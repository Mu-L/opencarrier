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
use crate::web_search::WebToolsContext;
use crate::text_tool_recovery::recover_text_tool_calls;
use memory::session::Session;
use memory::MemorySubstrate;
use types::agent::AgentManifest;
use types::error::{CarrierError, CarrierResult};
use types::memory::{Memory, MemoryFilter, MemorySource};
use types::message::{ContentBlock, Message, MessageContent, Role, StopReason, TokenUsage};
use types::tool::ToolDefinition;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Maximum iterations in the agent loop before giving up.
const MAX_ITERATIONS: u32 = 15;

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
const AGENT_LOOP_TIMEOUT_SECS: u64 = 600;

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

/// Parse a YAML frontmatter list field (e.g. `allowed_tools: [...]`) from text.
fn parse_frontmatter_list(content: &str, field: &str) -> Option<Vec<String>> {
    let pattern = format!("{field}:");
    let in_frontmatter = content.starts_with("---");
    let mut found = false;
    for line in content.lines() {
        if !in_frontmatter { break; }
        if line.trim() == "---" && found { break; }
        if let Some(rest) = line.strip_prefix(&pattern) {
            let rest = rest.trim();
            if rest.starts_with('[') && rest.ends_with(']') {
                let inner = &rest[1..rest.len()-1];
                return Some(inner.split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect());
            }
        }
        found = true;
    }
    None
}

/// Map a tool name to its toolset name (mirrors kernel::tool_builder::tool_to_toolset).
fn tool_to_toolset_name(name: &str) -> Option<String> {
    // Core tools don't belong to a toolset
    match name {
        "memory_store" | "memory_recall" | "memory_list"
        | "session_summarize" | "tool_search"
        | "skill_load" | "knowledge_read" | "knowledge_list"
        | "cron_create" | "cron_list" | "cron_cancel" => return None,
        _ => {}
    }
    if name.starts_with("file_") || name == "apply_patch" { return Some("filesystem".to_string()); }
    if name == "shell_exec" { return Some("shell".to_string()); }
    if name.starts_with("knowledge_") || name.starts_with("skill_") || name == "clone_evaluate" { return Some("knowledge".to_string()); }
    if name.starts_with("media_") || name.starts_with("image_") || name == "text_to_speech" || name == "speech_to_text"
        || name.starts_with("docker_exec") || name.starts_with("process_") { return Some("media".to_string()); }
    if name.starts_with("web_") { return Some("web".to_string()); }
    if name.starts_with("agent_") || name.starts_with("train_") { return Some("agent".to_string()); }
    if name.starts_with("location_") || name.starts_with("system_") || name == "user_profile" { return Some("misc".to_string()); }
    // MCP tools: extract server name from prefix like mcp_browser_*
    if let Some(rest) = name.strip_prefix("mcp_") {
        if let Some(pos) = rest.find('_') {
            return Some(rest[..pos].to_string());
        }
    }
    // Browser tools without mcp_ prefix
    if name.starts_with("browser_") { return Some("browser".to_string()); }
    Some("misc".to_string())
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
    available_tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    web_ctx: Option<&WebToolsContext>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    docker_config: Option<&types::config::DockerSandboxConfig>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
    brain: Option<Arc<dyn Brain>>,
    sender_id: Option<&str>,
    owner_id: Option<&str>,
) -> CarrierResult<AgentLoopResult> {
    let timeout = std::time::Duration::from_secs(AGENT_LOOP_TIMEOUT_SECS);
    match tokio::time::timeout(
        timeout,
        run_agent_loop_impl(
            manifest, user_message, session, memory, driver, available_tools,
            kernel, stream_tx, mcp_connections, web_ctx, workspace_root,
            on_phase, docker_config, hooks, context_window_tokens, process_manager,
            user_content_blocks, brain, sender_id, owner_id,
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
        let result = match &stream_tx {
            Some(tx) => driver.stream(request.clone(), tx.clone()).await,
            None => driver.complete(request.clone()).await,
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
    available_tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    web_ctx: Option<&WebToolsContext>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    docker_config: Option<&types::config::DockerSandboxConfig>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
    brain: Option<Arc<dyn Brain>>,
    sender_id: Option<&str>,
    owner_id: Option<&str>,
) -> CarrierResult<AgentLoopResult> {
    info!(agent = %manifest.name, "Starting agent loop");

    // Extract hand-allowed env vars from manifest metadata (set by kernel for hand settings)
    let hand_allowed_env: Vec<String> = manifest
        .metadata
        .get("hand_allowed_env")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    // Recall relevant memories via text search
    let memories = memory
        .recall(
            user_message,
            5,
            Some(MemoryFilter {
                agent_id: Some(session.agent_id),
                ..Default::default()
            }),
        )
        .await
        .unwrap_or_default();

    // Fire BeforePromptBuild hook
    let agent_id_str = session.agent_id.0.to_string();
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

    // Build the system prompt — base prompt comes from kernel (prompt_builder),
    // we append recalled memories here since they are resolved at loop time.
    let mut system_prompt = manifest.model.system_prompt.clone();
    if !memories.is_empty() {
        let mem_pairs: Vec<(String, String)> = memories
            .iter()
            .map(|m| (String::new(), m.content.clone()))
            .collect();
        system_prompt.push_str("\n\n");
        system_prompt.push_str(&crate::prompt_builder::build_memory_section(&mem_pairs));
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

    // Track recent (tool_name, input_hash) for loop detection
    let mut recent_tool_calls: Vec<(String, u64)> = Vec::new();

    // Shadow with an owned Vec so we can refresh mid-loop when skills are installed
    let mut tools_owned: Vec<ToolDefinition> = available_tools.to_vec();
    let mut available_tools: &[ToolDefinition] = &tools_owned;

    for iteration in 0..max_iterations {
        debug!(iteration, "Streaming agent loop iteration");

        // Context overflow recovery pipeline (replaces emergency_trim_messages)
        let recovery =
            recover_from_overflow(&mut messages, &system_prompt, available_tools, ctx_window);
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
        apply_context_guard(&mut messages, &context_budget, available_tools);

        let request = CompletionRequest {
            model: String::new(), // Model set by Brain/endpoint, not agent
            messages: messages.clone(),
            tools: available_tools.to_vec(),
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
            let recovered = recover_text_tool_calls(&response.text(), available_tools);
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
                    memory
                        .save_session_async(session)
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

                memory
                    .save_session_async(session)
                    .await
                    .map_err(|e| CarrierError::Memory(e.to_string()))?;

                // Remember this interaction
                let interaction_text = format!(
                    "User asked: {}\nI responded: {}",
                    user_message, final_response
                );
                let _ = memory
                    .remember(
                        session.agent_id,
                        &interaction_text,
                        MemorySource::Conversation,
                        "episodic",
                        HashMap::new(),
                    )
                    .await;

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

                let allowed_tool_names: Vec<String> =
                    available_tools.iter().map(|t| t.name.clone()).collect();
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
                // When detected, hard-break out of the agent loop — returning an error
                // result to the LLM doesn't work because the LLM ignores it and retries
                // the same call. We end the turn with a user-facing notice instead.
                if let Some((looping_name, _)) = detect_tool_loop(&recent_tool_calls, LOOP_DETECTION_WINDOW) {
                    warn!(
                        agent = %manifest.name,
                        tool = %looping_name,
                        consecutive = LOOP_DETECTION_WINDOW,
                        iteration,
                        "Tool loop detected — terminating agent loop"
                    );
                    let notice = format!(
                        "⚠️ 检测到工具循环：连续 {LOOP_DETECTION_WINDOW}+ 次调用 `{looping_name}` 都失败或返回相同结果，已中止本次任务。请换个思路或检查相关配置后重试。"
                    );
                    session.messages.push(Message::assistant(&notice));
                    memory
                        .save_session_async(session)
                        .await
                        .map_err(|e| CarrierError::Memory(e.to_string()))?;
                    return Ok(AgentLoopResult {
                        response: notice,
                        total_usage,
                        iterations: iteration + 1,
                        silent: false,
                        directives: Default::default(),
                    });
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
                        allowed_tools: Some(&allowed_tool_names),
                        caller_agent_id: Some(&caller_id_str),
                        mcp_connections,
                        web_ctx,
                        allowed_env_vars: if hand_allowed_env.is_empty() {
                            None
                        } else {
                            Some(&hand_allowed_env)
                        },
                        workspace_root,
                        brain: brain.as_ref(),
                        exec_policy: effective_exec_policy,

                        docker_config,
                        process_manager,
                        sender_id,
                        owner_id,
                        home_dir: home_dir_buf.as_deref(),
                        agent_name: Some(&manifest.name),
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
                    info!(
                        agent = %manifest.name,
                        iteration,
                        error_count,
                        failed_tools = ?failed_tools,
                        "Tool errors in agent loop iteration"
                    );
                    tool_result_blocks.push(ContentBlock::Text {
                        text: format!(
                            "[System: {} tool(s) returned errors. Report the error honestly \
                             to the user. Do NOT fabricate results or pretend the tool succeeded. \
                             Do NOT retry the same failed tool call. \
                             If a search or fetch failed, tell the user it failed and suggest \
                             alternatives instead of making up data.]",
                            error_count
                        ),
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
                        let agent_id_str = session.agent_id.to_string();

                        // Collect toolsets to activate from skill_load and tool_search
                        let mut toolsets_to_activate: Vec<String> = Vec::new();

                        // --- skill_load: parse allowed_tools from the skill content ---
                        let skill_load_results: Vec<(String, String)> = response.tool_calls.iter()
                            .filter(|tc| tc.name == "skill_load")
                            .filter_map(|tc| {
                                let name = tc.input.get("name").and_then(|v| v.as_str()).unwrap_or("");
                                tool_result_blocks.iter().find_map(|block| {
                                    if let types::message::ContentBlock::ToolResult { tool_use_id, content, .. } = block {
                                        if tool_use_id == &tc.id {
                                            Some((name.to_string(), content.clone()))
                                        } else { None }
                                    } else { None }
                                })
                            })
                            .collect();

                        for (skill_name, content) in &skill_load_results {
                            // Parse allowed_tools from frontmatter
                            if let Some(allowed) = parse_frontmatter_list(content, "allowed_tools") {
                                for tool_name in &allowed {
                                    if let Some(ts) = tool_to_toolset_name(tool_name) {
                                        if !toolsets_to_activate.contains(&ts) {
                                            toolsets_to_activate.push(ts);
                                        }
                                    }
                                }
                                info!(skill = skill_name, tools = ?allowed, "Skill loaded — activating toolsets");
                            }
                        }

                        // --- tool_search: search and collect matching toolsets ---
                        let search_queries: Vec<&str> = response.tool_calls.iter()
                            .filter(|tc| tc.name == "tool_search")
                            .filter_map(|tc| tc.input.get("query").and_then(|v| v.as_str()))
                            .collect();

                        for q in &search_queries {
                            let results = kernel.search_tools(q, 5);
                            for (ts_name, _) in results {
                                if !toolsets_to_activate.contains(&ts_name) {
                                    toolsets_to_activate.push(ts_name);
                                }
                            }
                        }

                        let is_dynamic_activation = !toolsets_to_activate.is_empty();
                        let new_tools = if is_dynamic_activation {
                            let mut added_tools: Vec<types::tool::ToolDefinition> = Vec::new();
                            for ts_name in &toolsets_to_activate {
                                if let Some(tools) = kernel.activate_toolset(&agent_id_str, ts_name) {
                                    info!(toolset = ts_name, tools_count = tools.len(), "Toolset activated");
                                    added_tools.extend(tools);
                                }
                            }
                            if added_tools.is_empty() { None } else { Some(added_tools) }
                        } else {
                            kernel.refresh_tools(&agent_id_str)
                        };

                        if let Some(new_tools) = new_tools {
                            if is_dynamic_activation {
                                // Append new toolset tools to existing list (按需加载)
                                let existing_names: std::collections::HashSet<String> =
                                    available_tools.iter().map(|t| t.name.clone()).collect();
                                let fresh: Vec<_> = new_tools.into_iter()
                                    .filter(|t| !existing_names.contains(&t.name))
                                    .collect();
                                if !fresh.is_empty() {
                                    info!(added = fresh.len(), total = available_tools.len() + fresh.len(), "Tools added from tool_search");
                                    tools_owned = available_tools.to_vec();
                                    tools_owned.extend(fresh);
                                    available_tools = &tools_owned;
                                }
                            } else {
                                // Full refresh (after train_write/file_write) — replace entire list
                                if new_tools.len() != available_tools.len() {
                                    let added = new_tools.len().saturating_sub(available_tools.len());
                                    info!(added, total = new_tools.len(), "Tool list refreshed");
                                    tools_owned = new_tools;
                                    available_tools = &tools_owned;
                                }
                            }
                        }
                    }
                }

                if let Err(e) = memory.save_session_async(session).await {
                    warn!("Failed to interim-save session: {e}");
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
                    if let Err(e) = memory.save_session_async(session).await {
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

    if let Err(e) = memory.save_session_async(session).await {
        warn!("Failed to save session on max iterations: {e}");
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
    available_tools: &[ToolDefinition],
    kernel: Option<Arc<dyn KernelHandle>>,
    stream_tx: mpsc::Sender<StreamEvent>,
    mcp_connections: Option<&dashmap::DashMap<String, McpConnection>>,
    web_ctx: Option<&WebToolsContext>,
    workspace_root: Option<&Path>,
    on_phase: Option<&PhaseCallback>,
    docker_config: Option<&types::config::DockerSandboxConfig>,
    hooks: Option<&crate::hooks::HookRegistry>,
    context_window_tokens: Option<usize>,
    process_manager: Option<&crate::process_manager::ProcessManager>,
    user_content_blocks: Option<Vec<ContentBlock>>,
    brain: Option<Arc<dyn Brain>>,
    sender_id: Option<&str>,
    owner_id: Option<&str>,
) -> CarrierResult<AgentLoopResult> {
    run_agent_loop(
        manifest, user_message, session, memory, driver, available_tools,
        kernel, Some(stream_tx), mcp_connections, web_ctx, workspace_root,
        on_phase, docker_config, hooks, context_window_tokens, process_manager,
        user_content_blocks, brain, sender_id, owner_id,
    ).await
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::text_tool_recovery::{
        parse_dash_dash_args, parse_json_tool_call_object,
    };
    use crate::llm_driver::{CompletionResponse, LlmError};
    use async_trait::async_trait;
    use types::tool::ToolCall;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_max_iterations_constant() {
        assert_eq!(MAX_ITERATIONS, 15);
    }

    #[test]
    fn test_retry_constants() {
        assert_eq!(MAX_RETRIES, 3);
        assert_eq!(BASE_RETRY_DELAY_MS, 1000);
    }

    #[test]
    fn test_dynamic_truncate_short_unchanged() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        let budget = ContextBudget::new(200_000);
        let short = "Hello, world!";
        assert_eq!(truncate_tool_result_dynamic(short, &budget), short);
    }

    #[test]
    fn test_dynamic_truncate_over_limit() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        let budget = ContextBudget::new(200_000);
        let long = "x".repeat(budget.per_result_cap() + 10_000);
        let result = truncate_tool_result_dynamic(&long, &budget);
        assert!(result.len() <= budget.per_result_cap() + 200);
        assert!(result.contains("[TRUNCATED:"));
    }

    #[test]
    fn test_dynamic_truncate_newline_boundary() {
        use crate::context_budget::{truncate_tool_result_dynamic, ContextBudget};
        // Small budget to force truncation
        let budget = ContextBudget::new(1_000);
        let content = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = truncate_tool_result_dynamic(&content, &budget);
        // Should break at a newline, not mid-line
        let before_marker = result.split("[TRUNCATED:").next().unwrap();
        let trimmed = before_marker.trim_end();
        assert!(!trimmed.is_empty());
    }

    #[test]
    fn test_max_continuations_constant() {
        assert_eq!(MAX_CONTINUATIONS, 5);
    }

    #[test]
    fn test_tool_timeout_constant() {
        assert_eq!(TOOL_TIMEOUT_SECS, 120);
    }

    #[test]
    fn test_max_history_messages() {
        assert_eq!(MAX_HISTORY_MESSAGES, 30);
    }

    // --- Loop detection ---

    fn make_call(name: &str, input: serde_json::Value) -> (String, u64) {
        (name.to_string(), tool_input_hash(&input))
    }

    #[test]
    fn test_loop_detection_blocks_consecutive_same_call() {
        let recent: Vec<(String, u64)> = (0..LOOP_DETECTION_WINDOW)
            .map(|_| make_call("web_search", serde_json::json!({"q": "rust"})))
            .collect();
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_some(), "Should detect loop with same call repeated");
        assert_eq!(result.unwrap().0, "web_search");
    }

    #[test]
    fn test_loop_detection_allows_pagination() {
        // Same tool name but different inputs (pagination) — not a loop
        let recent: Vec<(String, u64)> = (0..LOOP_DETECTION_WINDOW)
            .map(|i| make_call("web_search", serde_json::json!({"q": format!("rust page {}", i)})))
            .collect();
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_none(), "Pagination with different queries should not be flagged");
    }

    #[test]
    fn test_loop_detection_requires_full_window() {
        // 5 same calls is below threshold of 6
        let recent: Vec<(String, u64)> = (0..5)
            .map(|_| make_call("web_search", serde_json::json!({"q": "rust"})))
            .collect();
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_none(), "Below-threshold count should not trigger");
    }

    #[test]
    fn test_loop_detection_breaks_on_different_tool() {
        // 5 web_search + 1 web_fetch + 5 web_search → no loop (window is 6, last 6 are mixed)
        let mut recent: Vec<(String, u64)> = (0..5)
            .map(|_| make_call("web_search", serde_json::json!({"q": "rust"})))
            .collect();
        recent.push(make_call("web_fetch", serde_json::json!({"url": "https://example.com"})));
        recent.extend(
            (0..5).map(|_| make_call("web_search", serde_json::json!({"q": "rust"})))
        );
        let result = detect_tool_loop(&recent, LOOP_DETECTION_WINDOW);
        assert!(result.is_none(), "Mixed tail should not trigger loop detection");
    }

    #[test]
    fn test_loop_detection_window_constant() {
        assert_eq!(LOOP_DETECTION_WINDOW, 6);
    }

    // --- Integration tests for empty response guards ---

    fn test_manifest() -> AgentManifest {
        AgentManifest {
            name: "test-agent".to_string(),
            model: types::agent::ModelConfig {
                system_prompt: "You are a test agent.".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Mock driver that simulates: first call returns ToolUse with no text,
    /// second call returns EndTurn with empty text. This reproduces the bug
    /// where the LLM ends with no text after a tool-use cycle.
    struct EmptyAfterToolUseDriver {
        call_count: AtomicU32,
    }

    impl EmptyAfterToolUseDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for EmptyAfterToolUseDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // First call: LLM wants to use a tool (with no text block)
                Ok(CompletionResponse {
                    content: vec![ContentBlock::ToolUse {
                        id: "tool_1".to_string(),
                        name: "fake_tool".to_string(),
                        input: serde_json::json!({"query": "test"}),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::ToolUse,
                    tool_calls: vec![ToolCall {
                        id: "tool_1".to_string(),
                        name: "fake_tool".to_string(),
                        input: serde_json::json!({"query": "test"}),
                    }],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 5,
                    },
                    media: None,
                })
            } else {
                // Second call: LLM returns EndTurn with EMPTY text (the bug)
                Ok(CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 0,
                    },
                    media: None,
                })
            }
        }
    }

    /// Mock driver that returns empty text with MaxTokens stop reason,
    /// repeated MAX_CONTINUATIONS times to trigger the max continuations path.
    struct EmptyMaxTokensDriver;

    #[async_trait]
    impl LlmDriver for EmptyMaxTokensDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::MaxTokens,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
                media: None,
            })
        }
    }

    /// Mock driver that returns normal text (sanity check).
    struct NormalDriver;

    #[async_trait]
    impl LlmDriver for NormalDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![ContentBlock::Text {
                    text: "Hello from the agent!".to_string(),
                    provider_metadata: None,
                }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 8,
                },
                media: None,
            })
        }
    }

    #[tokio::test]
    async fn test_empty_response_after_tool_use_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());

        let result = run_agent_loop(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[], // no tools registered — the tool call will fail, which is fine
            None, // kernel
            None, // stream_tx
            None, // mcp_connections
            None, // web_ctx
            None, // workspace_root
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Loop should complete without error");

        // The response MUST NOT be empty — it should contain our fallback text
        assert!(
            !result.response.trim().is_empty(),
            "Response should not be empty after tool use, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("Task completed"),
            "Expected fallback message, got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_tool_error_injects_no_fabrication_guidance() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());

        run_agent_loop(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[], // no tools registered — the tool call will fail, which is fine
            None, // kernel
            None, // stream_tx
            None, // mcp_connections
            None, // web_ctx
            None, // workspace_root
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Loop should complete without error");

        let guidance_seen = session.messages.iter().any(|msg| {
            match &msg.content {
            MessageContent::Blocks(blocks) => blocks.iter().any(|block| {
                matches!(block, ContentBlock::Text { text, .. } if text.contains("tool(s) returned errors"))
            }),
            _ => false,
        }
        });

        assert!(
            guidance_seen,
            "Expected tool error guidance in session messages after failed tool call"
        );
    }

    #[tokio::test]
    async fn test_empty_response_max_tokens_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyMaxTokensDriver);

        let result = run_agent_loop(
            &manifest,
            "Tell me something long",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Loop should complete without error");

        // Should hit MAX_CONTINUATIONS and return fallback instead of empty
        assert!(
            !result.response.trim().is_empty(),
            "Response should not be empty on max tokens, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("token limit"),
            "Expected max-tokens fallback message, got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_normal_response_not_replaced_by_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NormalDriver);

        let result = run_agent_loop(
            &manifest,
            "Say hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Loop should complete without error");

        // Normal response should pass through unchanged
        assert_eq!(result.response, "Hello from the agent!");
    }

    #[tokio::test]
    async fn test_streaming_empty_response_after_tool_use_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyAfterToolUseDriver::new());
        let (tx, _rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Do something with tools",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            tx,
            None,
            None,
            None,
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Streaming loop should complete without error");

        assert!(
            !result.response.trim().is_empty(),
            "Streaming response should not be empty after tool use, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("Task completed"),
            "Expected fallback message in streaming, got: {:?}",
            result.response
        );
    }

    /// Mock driver that returns empty text on first call (EndTurn), then normal text on second.
    /// This tests the one-shot retry logic for iteration 0 empty responses.
    struct EmptyThenNormalDriver {
        call_count: AtomicU32,
    }

    impl EmptyThenNormalDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for EmptyThenNormalDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // First call: empty EndTurn (triggers retry)
                Ok(CompletionResponse {
                    content: vec![],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 10,
                        output_tokens: 0,
                    },
                    media: None,
                })
            } else {
                // Second call (retry): normal response
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Recovered after retry!".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 15,
                        output_tokens: 8,
                    },
                    media: None,
                })
            }
        }
    }

    /// Mock driver that always returns empty EndTurn (no recovery on retry).
    /// Tests that the fallback message appears when retry also fails.
    struct AlwaysEmptyDriver;

    #[async_trait]
    impl LlmDriver for AlwaysEmptyDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
                media: None,
            })
        }
    }

    #[tokio::test]
    async fn test_empty_first_response_retries_and_recovers() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyThenNormalDriver::new());

        let result = run_agent_loop(
            &manifest,
            "Hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None,
            None,
            None,
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Loop should recover via retry");

        assert_eq!(result.response, "Recovered after retry!");
        assert_eq!(
            result.iterations, 2,
            "Should have taken 2 iterations (retry)"
        );
    }

    #[tokio::test]
    async fn test_empty_first_response_fallback_when_retry_also_empty() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(AlwaysEmptyDriver);

        let result = run_agent_loop(
            &manifest,
            "Hello",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            None, // stream_tx
            None,
            None,
            None,
            None,
            None,
            None,
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Loop should complete with fallback");

        // No tools were executed, so should get the empty response message
        assert!(
            result.response.contains("empty response"),
            "Expected empty response fallback (no tools executed), got: {:?}",
            result.response
        );
    }

    #[tokio::test]
    async fn test_max_history_messages_constant() {
        assert_eq!(MAX_HISTORY_MESSAGES, 30);
    }

    #[tokio::test]
    async fn test_streaming_empty_response_max_tokens_returns_fallback() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(EmptyMaxTokensDriver);
        let (tx, _rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Tell me something long",
            &mut session,
            &memory,
            driver,
            &[],
            None,
            tx,
            None,
            None,
            None,
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Streaming loop should complete without error");

        assert!(
            !result.response.trim().is_empty(),
            "Streaming response should not be empty on max tokens, got: {:?}",
            result.response
        );
        assert!(
            result.response.contains("token limit"),
            "Expected max-tokens fallback in streaming, got: {:?}",
            result.response
        );
    }

    #[test]
    fn test_recover_text_tool_calls_basic() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            r#"Let me search for that. <function=web_search>{"query":"rust async"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust async");
        assert!(calls[0].id.starts_with("recovered_"));
    }

    #[test]
    fn test_recover_text_tool_calls_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=hack_system>{"cmd":"rm -rf /"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Unknown tools should be rejected");
    }

    #[test]
    fn test_recover_text_tool_calls_invalid_json() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=web_search>not valid json</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Invalid JSON should be skipped");
    }

    #[test]
    fn test_recover_text_tool_calls_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read a file".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = r#"<function=web_search>{"query":"hello"}</function> then <function=read_file>{"path":"a.txt"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "read_file");
    }

    #[test]
    fn test_recover_text_tool_calls_no_pattern() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Just a normal response with no tool calls.";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_text_tool_calls_empty_tools() {
        let text = r#"<function=web_search>{"query":"hello"}</function>"#;
        let calls = recover_text_tool_calls(text, &[]);
        assert!(calls.is_empty(), "No tools = no recovery");
    }

    // --- Deep edge-case tests for text-to-tool recovery ---

    #[test]
    fn test_recover_text_tool_calls_nested_json() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=web_search>{"query":"rust","filters":{"lang":"en","year":2024}}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["filters"]["lang"], "en");
    }

    #[test]
    fn test_recover_text_tool_calls_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Sure, let me search that for you.\n\n<function=web_search>{\"query\":\"rust async programming\"}</function>\n\nI'll get back to you with results.";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["query"], "rust async programming");
    }

    #[test]
    fn test_recover_text_tool_calls_whitespace_in_json() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Some models emit pretty-printed JSON
        let text = "<function=web_search>\n  {\"query\": \"hello world\"}\n</function>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["query"], "hello world");
    }

    #[test]
    fn test_recover_text_tool_calls_unclosed_tag() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Missing </function> — should gracefully skip
        let text = r#"<function=web_search>{"query":"test"}"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty(), "Unclosed tag should be skipped");
    }

    #[test]
    fn test_recover_text_tool_calls_missing_closing_bracket() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        // Missing > after tool name
        let text = r#"<function=web_search{"query":"test"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        // The parser finds > inside JSON, will likely produce invalid tool name
        // or invalid JSON — either way, should not panic
        // (just verifying no panic / no bad behavior)
        let _ = calls;
    }

    #[test]
    fn test_recover_text_tool_calls_empty_json_object() {
        let tools = vec![ToolDefinition {
            name: "list_files".into(),
            description: "List".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function=list_files>{}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "list_files");
        assert_eq!(calls[0].input, serde_json::json!({}));
    }

    #[test]
    fn test_recover_text_tool_calls_mixed_valid_invalid() {
        let tools = vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "read_file".into(),
                description: "Read".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        // First: valid, second: unknown tool, third: valid
        let text = r#"<function=web_search>{"q":"a"}</function> <function=unknown>{"x":1}</function> <function=read_file>{"path":"b"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2, "Should recover 2 valid, skip 1 unknown");
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "read_file");
    }

    // --- Variant 2 pattern tests: <function>NAME{JSON}</function> ---

    #[test]
    fn test_recover_variant2_basic() {
        let tools = vec![ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function>web_fetch{"url":"https://example.com"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_fetch");
        assert_eq!(calls[0].input["url"], "https://example.com");
    }

    #[test]
    fn test_recover_variant2_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function>unknown_tool{"q":"test"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn test_recover_variant2_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Let me search for that. <function>web_search{"query":"rust lang"}</function> I'll find the answer."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn test_recover_both_variants_mixed() {
        let tools = vec![
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_fetch".into(),
                description: "Fetch".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        // Mix of variant 1 and variant 2
        let text = r#"<function=web_search>{"q":"a"}</function> <function>web_fetch{"url":"https://x.com"}</function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[1].name, "web_fetch");
    }

    #[test]
    fn test_recover_tool_tag_variant() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"I'll run that for you. <tool>exec{"command":"ls -la"}</tool>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_markdown_code_block() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "I'll execute that command:\n```\nexec {\"command\": \"ls -la\"}\n```";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_markdown_code_block_with_lang() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "```json\nweb_search {\"query\": \"rust\"}\n```";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn test_recover_backtick_wrapped() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Let me run `exec {"command":"pwd"}` for you."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "exec");
        assert_eq!(calls[0].input["command"], "pwd");
    }

    #[test]
    fn test_recover_backtick_ignores_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"Try `unknown_tool {"key":"val"}` instead."#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_no_duplicates_across_patterns() {
        let tools = vec![ToolDefinition {
            name: "exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        // Same call in both function tag and tool tag — should only appear once
        let text =
            r#"<function=exec>{"command":"ls"}</function> <tool>exec{"command":"ls"}</tool>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
    }

    // --- Pattern 6: [TOOL_CALL]...[/TOOL_CALL] tests (issue #354) ---

    #[test]
    fn test_recover_tool_call_block_json() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute shell command".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_tool_call_block_arrow_syntax() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute shell command".into(),
            input_schema: serde_json::json!({}),
        }];
        // Exact format from issue #354
        let text = "[TOOL_CALL]\n{tool => \"shell_exec\", args => {\n--command \"ls -F /\"\n}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -F /");
    }

    #[test]
    fn test_recover_tool_call_block_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "[TOOL_CALL]\n{\"name\": \"hack_system\", \"arguments\": {\"cmd\": \"rm -rf /\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_tool_call_block_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "shell_exec".into(),
                description: "Execute".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "file_read".into(),
                description: "Read".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}\n[/TOOL_CALL]\nSome text.\n[TOOL_CALL]\n{\"name\": \"file_read\", \"arguments\": {\"path\": \"/tmp/test.txt\"}}\n[/TOOL_CALL]";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn test_recover_tool_call_block_unclosed() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        // Unclosed [TOOL_CALL] — pattern 6 skips it, but pattern 8 (bare JSON)
        // still finds the valid JSON tool call object.
        let text = "[TOOL_CALL]\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1, "Bare JSON fallback should recover this");
        assert_eq!(calls[0].name, "shell_exec");
    }

    // --- Pattern 7: <tool_call>JSON</tool_call> tests (Qwen3, issue #332) ---

    #[test]
    fn test_recover_tool_call_xml_basic() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>\n{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}\n</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_tool_call_xml_with_surrounding_text() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "I'll search for that.\n\n<tool_call>\n{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust async\"}}\n</tool_call>\n\nLet me get results.";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust async");
    }

    #[test]
    fn test_recover_tool_call_xml_function_field() {
        let tools = vec![ToolDefinition {
            name: "file_read".into(),
            description: "Read".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"function\": \"file_read\", \"arguments\": {\"path\": \"/etc/hosts\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
    }

    #[test]
    fn test_recover_tool_call_xml_parameters_field() {
        let tools = vec![ToolDefinition {
            name: "web_fetch".into(),
            description: "Fetch".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"web_fetch\", \"parameters\": {\"url\": \"https://example.com\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_fetch");
        assert_eq!(calls[0].input["url"], "https://example.com");
    }

    #[test]
    fn test_recover_tool_call_xml_stringified_args() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"shell_exec\", \"arguments\": \"{\\\"command\\\": \\\"pwd\\\"}\"}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "pwd");
    }

    #[test]
    fn test_recover_tool_call_xml_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_call>{\"name\": \"hack_system\", \"arguments\": {\"cmd\": \"rm -rf /\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_tool_call_xml_multiple() {
        let tools = vec![
            ToolDefinition {
                name: "shell_exec".into(),
                description: "Execute".into(),
                input_schema: serde_json::json!({}),
            },
            ToolDefinition {
                name: "web_search".into(),
                description: "Search".into(),
                input_schema: serde_json::json!({}),
            },
        ];
        let text = "<tool_call>{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}</tool_call>\n<tool_call>{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust\"}}</tool_call>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[1].name, "web_search");
    }

    // --- Pattern 8: Bare JSON tool call object tests ---

    #[test]
    fn test_recover_bare_json_tool_call() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "I'll run that: {\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls -la\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_bare_json_no_false_positive() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "The config looks like {\"debug\": true, \"level\": \"info\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_bare_json_skipped_when_tags_found() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<function=shell_exec>{\"command\":\"ls\"}</function> {\"name\": \"shell_exec\", \"arguments\": {\"command\": \"pwd\"}}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].input["command"], "ls");
    }

    // --- Pattern 9: XML-attribute style <function name="..." parameters="..." /> ---

    #[test]
    fn test_recover_xml_attribute_basic() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="web_search" parameters="{&quot;query&quot;: &quot;best crypto 2024&quot;}" />"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "best crypto 2024");
    }

    #[test]
    fn test_recover_xml_attribute_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="unknown_tool" parameters="{&quot;x&quot;: 1}" />"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    #[test]
    fn test_recover_xml_attribute_non_selfclosing() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = r#"<function name="shell_exec" parameters="{&quot;command&quot;: &quot;ls&quot;}"></function>"#;
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
    }

    // --- Pattern 10: <|plugin|>...<|endofblock|> tests ---

    #[test]
    fn test_recover_plugin_block() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<|plugin|>\n{\"name\": \"web_search\", \"arguments\": {\"query\": \"rust\"}}\n<|endofblock|>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust");
    }

    #[test]
    fn test_recover_plugin_block_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "<|plugin|>\n{\"name\": \"hack\", \"arguments\": {\"cmd\": \"rm\"}}\n<|endofblock|>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 11: Action/Action Input tests ---

    #[test]
    fn test_recover_action_input() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Action: web_search\nAction Input: {\"query\": \"rust programming\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
        assert_eq!(calls[0].input["query"], "rust programming");
    }

    #[test]
    fn test_recover_action_input_unknown_tool() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "Action: unknown_tool\nAction Input: {\"key\": \"value\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 12: name + JSON on next line tests ---

    #[test]
    fn test_recover_name_json_nextline() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "shell_exec\n{\"command\": \"ls -la\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell_exec");
        assert_eq!(calls[0].input["command"], "ls -la");
    }

    #[test]
    fn test_recover_name_json_nextline_unknown() {
        let tools = vec![ToolDefinition {
            name: "shell_exec".into(),
            description: "Execute".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "unknown_tool\n{\"command\": \"ls\"}";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Pattern 13: <tool_use> tests ---

    #[test]
    fn test_recover_tool_use_block() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text =
            "<tool_use>{\"name\": \"web_search\", \"arguments\": {\"query\": \"test\"}}</tool_use>";
        let calls = recover_text_tool_calls(text, &tools);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "web_search");
    }

    #[test]
    fn test_recover_tool_use_block_unknown() {
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search".into(),
            input_schema: serde_json::json!({}),
        }];
        let text = "<tool_use>{\"name\": \"hack\", \"arguments\": {\"cmd\": \"rm\"}}</tool_use>";
        let calls = recover_text_tool_calls(text, &tools);
        assert!(calls.is_empty());
    }

    // --- Helper function tests ---

    #[test]
    fn test_parse_dash_dash_args_basic() {
        let result = parse_dash_dash_args("{--command \"ls -F /\"}");
        assert_eq!(result["command"], "ls -F /");
    }

    #[test]
    fn test_parse_dash_dash_args_multiple() {
        let result = parse_dash_dash_args("{--file \"test.txt\", --verbose}");
        assert_eq!(result["file"], "test.txt");
        assert_eq!(result["verbose"], true);
    }

    #[test]
    fn test_parse_dash_dash_args_unquoted_value() {
        let result = parse_dash_dash_args("{--count 5}");
        assert_eq!(result["count"], "5");
    }

    #[test]
    fn test_parse_json_tool_call_object_standard() {
        let tool_names = vec!["shell_exec"];
        let result = parse_json_tool_call_object(
            "{\"name\": \"shell_exec\", \"arguments\": {\"command\": \"ls\"}}",
            &tool_names,
        );
        assert!(result.is_some());
        let (name, args) = result.unwrap();
        assert_eq!(name, "shell_exec");
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn test_parse_json_tool_call_object_function_field() {
        let tool_names = vec!["web_fetch"];
        let result = parse_json_tool_call_object(
            "{\"function\": \"web_fetch\", \"parameters\": {\"url\": \"https://x.com\"}}",
            &tool_names,
        );
        assert!(result.is_some());
        let (name, args) = result.unwrap();
        assert_eq!(name, "web_fetch");
        assert_eq!(args["url"], "https://x.com");
    }

    #[test]
    fn test_parse_json_tool_call_object_unknown_tool() {
        let tool_names = vec!["shell_exec"];
        let result =
            parse_json_tool_call_object("{\"name\": \"unknown\", \"arguments\": {}}", &tool_names);
        assert!(result.is_none());
    }

    // --- End-to-end integration test: text-as-tool-call recovery through agent loop ---

    /// Mock driver that simulates a Groq/Llama model outputting tool calls as text.
    /// Call 1: Returns text with `<function=web_search>...</function>` (EndTurn, no tool_calls)
    /// Call 2: Returns a normal text response (after tool result is provided)
    struct TextToolCallDriver {
        call_count: AtomicU32,
    }

    impl TextToolCallDriver {
        fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LlmDriver for TextToolCallDriver {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let call = self.call_count.fetch_add(1, Ordering::Relaxed);
            if call == 0 {
                // Simulate Groq/Llama: tool call as text, not in tool_calls field
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: r#"Let me search for that. <function=web_search>{"query":"rust async"}</function>"#.to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![], // BUG: no tool_calls!
                    usage: TokenUsage {
                        input_tokens: 20,
                        output_tokens: 15,
                    },
                media: None,
                })
            } else {
                // After tool result, return normal response
                Ok(CompletionResponse {
                    content: vec![ContentBlock::Text {
                        text: "Based on the search results, Rust async is great!".to_string(),
                        provider_metadata: None,
                    }],
                    stop_reason: StopReason::EndTurn,
                    tool_calls: vec![],
                    usage: TokenUsage {
                        input_tokens: 30,
                        output_tokens: 12,
                    },
                    media: None,
                })
            }
        }
    }

    #[tokio::test]
    async fn test_text_tool_call_recovery_e2e() {
        // This is THE critical test: a model outputs a tool call as text,
        // the recovery code detects it, promotes it to ToolUse, executes the tool,
        // and the agent loop continues to produce a final response.
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(TextToolCallDriver::new());

        // Provide web_search as an available tool so recovery can match it
        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let result = run_agent_loop(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            None, // stream_tx
            None,
            None,
            None,
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Agent loop should complete");

        // The response should contain the second call's output, NOT the raw function tag
        assert!(
            !result.response.contains("<function="),
            "Response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Should have at least 2 iterations (tool call + final response), got: {}",
            result.iterations
        );
        // Verify the final text response came through
        assert!(
            result.response.contains("search results") || result.response.contains("Rust async"),
            "Expected final response text, got: {:?}",
            result.response
        );
    }

    /// Mock driver that returns NO text-based tool calls — just normal text.
    /// Verifies recovery does NOT interfere with normal flow.
    #[tokio::test]
    async fn test_normal_flow_unaffected_by_recovery() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(NormalDriver);

        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({}),
        }];

        let result = run_agent_loop(
            &manifest,
            "Say hello",
            &mut session,
            &memory,
            driver,
            &tools, // tools available but not used
            None,
            None, // stream_tx
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Normal loop should complete");

        assert_eq!(result.response, "Hello from the agent!");
        assert_eq!(
            result.iterations, 1,
            "Normal response should complete in 1 iteration"
        );
    }

    // --- Streaming path: text-as-tool-call recovery ---

    #[tokio::test]
    async fn test_text_tool_call_recovery_streaming_e2e() {
        let memory = memory::MemorySubstrate::open_in_memory(0.01).unwrap();
        let agent_id = types::agent::AgentId::new();
        let mut session = memory::session::Session {
            id: types::agent::SessionId::new(),
            agent_id,
            messages: Vec::new(),
            context_window_tokens: 0,
            label: None,
            active_toolsets: vec![],
        };
        let manifest = test_manifest();
        let driver: Arc<dyn LlmDriver> = Arc::new(TextToolCallDriver::new());

        let tools = vec![ToolDefinition {
            name: "web_search".into(),
            description: "Search the web".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }];

        let (tx, mut rx) = mpsc::channel(64);

        let result = run_agent_loop_streaming(
            &manifest,
            "Search for rust async programming",
            &mut session,
            &memory,
            driver,
            &tools,
            None,
            tx,
            None,
            None,
            None,
            None, // on_phase
            None, // docker_config
            None, // hooks
            None, // context_window_tokens
            None, // process_manager
            None, // user_content_blocks
            None, // brain
            None, // sender_id
            None, // owner_id
        )
        .await
        .expect("Streaming loop should complete");

        // Same assertions as non-streaming
        assert!(
            !result.response.contains("<function="),
            "Streaming: response should not contain raw function tags, got: {:?}",
            result.response
        );
        assert!(
            result.iterations >= 2,
            "Streaming: should have at least 2 iterations, got: {}",
            result.iterations
        );

        // Drain the stream channel to verify events were sent
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events.is_empty(), "Should have received stream events");
    }
}
