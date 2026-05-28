//! Shared helper functions for the agent loop.
//!
//! Contains retry logic, fallback chain, loop detection, turn trimming,
//! and turn summary generation — pure utilities used by the main loop
//! and its branch handlers.

use crate::auth_cooldown::{CooldownVerdict, ProviderCooldown};
use crate::llm_driver::{
    Brain, CompletionRequest, CompletionResponse, LlmDriver, LlmError, StreamEvent,
};
use crate::llm_errors;
use std::sync::Arc;
use types::error::{CarrierError, CarrierResult};
use types::message::{ContentBlock, Message, MessageContent, Role, TurnSummary};
use tokio::sync::mpsc;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum retries for rate-limited or overloaded API calls.
pub const MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (milliseconds).
pub const BASE_RETRY_DELAY_MS: u64 = 1000;

/// Timeout for a single LLM API call (seconds).
/// Catches mid-stream hangs where the server goes silent after connection.
pub(in crate::agent_loop) const PER_LLM_CALL_TIMEOUT_SECS: u64 = 180;

/// Max tokens for turn summary generation.
pub(in crate::agent_loop) const SUMMARY_MAX_TOKENS: u32 = 150;

/// Summary modality (fast/cheap).
pub(in crate::agent_loop) const SUMMARY_MODALITY: &str = "fast";

/// Reasoning modality — expensive model for planning and complex inference.
pub(in crate::agent_loop) const REASONING_MODALITY: &str = "reasoning";

/// Pick the optimal modality for the current agent loop iteration.
///
/// Alternating strategy:
/// - Even turns (0, 2, 4, ...): `reasoning` — plan, decompose, review results
/// - Odd turns (1, 3, 5, ...): `chat` — execute tools, collect data
///
/// Falls back gracefully: no `reasoning` modality → use default, same as before.
pub(in crate::agent_loop) fn pick_modality(
    brain: Option<&std::sync::Arc<dyn crate::llm_driver::Brain>>,
    iteration: u32,
    default_modality: &str,
) -> String {
    let Some(brain) = brain else {
        return default_modality.to_string();
    };
    if iteration % 2 == 0 && brain.has_modality(REASONING_MODALITY) {
        tracing::info!(iteration, selected = REASONING_MODALITY, default = default_modality, "Adaptive modality: reasoning for planning/review turn");
        return REASONING_MODALITY.to_string();
    }
    tracing::info!(iteration, selected = default_modality, "Adaptive modality: chat for execution turn");
    default_modality.to_string()
}

/// Tool search recall limit (stage 1: how many candidates to retrieve).
pub(in crate::agent_loop) const TOOL_SEARCH_RECALL_LIMIT: usize = 10;

/// Timeout for individual tool executions (seconds).
/// Raised from 60s to 120s for browser automation and long-running builds.
pub const TOOL_TIMEOUT_SECS: u64 = 120;

/// Tools that need a longer timeout (image generation, browser automation).
pub const TOOL_TIMEOUT_LONG_SECS: u64 = 300;
pub const TOOL_LONG_TIMEOUT_NAMES: &[&str] =
    &["image_generate", "browser_navigate", "browser_execute"];

/// Maximum message history size before auto-trimming to prevent context overflow.
pub const MAX_HISTORY_MESSAGES: usize = 30;

/// Number of consecutive identical tool calls (same name AND same input) that
/// constitute a loop. Picked at 6 because:
/// - Below 4 risks blocking legitimate retries (e.g. eventual-consistency reads)
/// - Above 8 wastes API calls before kicking in
///
/// Same-name-different-input (e.g. paginated search) is NOT a loop.
pub const LOOP_DETECTION_WINDOW: usize = 6;

/// Default context window size (tokens) for token-based trimming.
pub(in crate::agent_loop) const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

// ---------------------------------------------------------------------------
// Loop detection
// ---------------------------------------------------------------------------

/// Hash a tool input value for loop detection. Two calls with the same hash
/// are considered identical for loop-detection purposes.
pub fn tool_input_hash(input: &serde_json::Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let serialized = serde_json::to_string(input).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    serialized.hash(&mut hasher);
    hasher.finish()
}

/// Detect a tool-use loop: returns the (name, input_hash) of the looping call
/// if the last `window` entries are all the same (name, input_hash), else None.
pub fn detect_tool_loop(recent: &[(String, u64)], window: usize) -> Option<(String, u64)> {
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

// ---------------------------------------------------------------------------
// LLM retry / fallback
// ---------------------------------------------------------------------------

/// Call an LLM driver with automatic retry on rate-limit and overload errors.
///
/// Uses the `llm_errors` classifier for smart error handling and the
/// `ProviderCooldown` circuit breaker to prevent request storms.
pub(in crate::agent_loop) async fn call_with_retry(
    driver: &dyn LlmDriver,
    request: CompletionRequest,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    provider: Option<&str>,
    cooldown: Option<&ProviderCooldown>,
    deadline: Option<std::time::Instant>,
) -> CarrierResult<CompletionResponse> {
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
        // Compute per-call timeout: min(remaining budget, 180s)
        let per_call_timeout = match deadline {
            Some(dl) => {
                let remaining = dl.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    warn!(attempt, "Time budget exhausted before LLM attempt");
                    return Err(CarrierError::LlmDriver(
                        "Agent loop time budget exhausted".to_string(),
                    ));
                }
                std::cmp::min(remaining, std::time::Duration::from_secs(PER_LLM_CALL_TIMEOUT_SECS))
            }
            None => std::time::Duration::from_secs(PER_LLM_CALL_TIMEOUT_SECS),
        };

        let call = async {
            match &stream_tx {
                Some(tx) => driver.stream(request.clone(), tx.clone()).await,
                None => driver.complete(request.clone()).await,
            }
        };

        // For streaming mode, do NOT apply an overall timeout to driver.stream().
        // stream() reads ALL SSE events before returning; for long generations
        // (e.g. max_tokens=8192 article writing) total time can exceed 180s even
        // though the server is actively streaming.  The driver's built-in idle
        // timeout (120s of silence) is sufficient protection against hangs.
        // For non-streaming mode, keep the per-call timeout as before.
        let result = if stream_tx.is_some() {
            call.await
        } else {
            match tokio::time::timeout(per_call_timeout, call).await {
                Ok(r) => r,
                Err(_) => {
                    warn!(attempt, timeout_secs = per_call_timeout.as_secs(), "LLM call timed out");
                    last_error = Some("LLM call timed out".to_string());
                    if attempt == MAX_RETRIES {
                        return Err(CarrierError::LlmDriver(format!(
                            "LLM call timed out after {}s — server may be unresponsive",
                            per_call_timeout.as_secs()
                        )));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(
                        BASE_RETRY_DELAY_MS * 2u64.pow(attempt),
                    ))
                    .await;
                    continue;
                }
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
                let delay =
                    std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
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
                let delay =
                    std::cmp::max(retry_after_ms, BASE_RETRY_DELAY_MS * 2u64.pow(attempt));
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
pub(in crate::agent_loop) async fn call_with_fallback(
    brain: Option<&Arc<dyn Brain>>,
    fallback_driver: &dyn LlmDriver,
    modality: &str,
    request: CompletionRequest,
    stream_tx: Option<mpsc::Sender<StreamEvent>>,
    deadline: Option<std::time::Instant>,
) -> CarrierResult<CompletionResponse> {
    let Some(brain) = brain else {
        return call_with_retry(fallback_driver, request, stream_tx, None, None, deadline).await;
    };

    let endpoints = brain.endpoints_for(modality);
    if endpoints.is_empty() {
        return Err(CarrierError::LlmDriver(format!(
            "No available endpoints for modality '{modality}' — all endpoints circuit-broken or not configured"
        )));
    }

    let mut last_error: Option<CarrierError> = None;
    for ep in &endpoints {
        // Skip endpoint if insufficient time budget remains (need at least 30s)
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(std::time::Instant::now());
            if remaining.as_secs() < 30 {
                tracing::warn!(
                    endpoint = %ep.id,
                    remaining_secs = remaining.as_secs(),
                    "Skipping endpoint: insufficient time budget"
                );
                last_error = Some(CarrierError::LlmDriver(
                    "Agent loop time budget exhausted".to_string(),
                ));
                continue;
            }
        }
        if let Some(driver) = brain.driver_for_endpoint(&ep.id) {
            let mut req = request.clone();
            req.model = ep.model.clone();
            let start = std::time::Instant::now();
            let tx_arg = stream_tx.clone();
            match call_with_retry(&*driver, req, tx_arg, Some(&ep.provider), None, deadline).await {
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

// ---------------------------------------------------------------------------
// Turn trimming
// ---------------------------------------------------------------------------

/// Trim old messages from the session, keeping only the most recent N.
///
/// Messages are removed from the front of the list (oldest first).
/// The caller is responsible for having already generated TurnSummaries
/// for the turns being removed.
pub(in crate::agent_loop) fn trim_oldest_turns(messages: &mut Vec<Message>, max_retained: usize) {
    if messages.len() <= max_retained {
        return;
    }
    // Drain from the front until we're at the threshold.
    // We drain in pairs (user + assistant) to keep whole turns.
    let excess = messages.len() - max_retained;
    // Round up to the nearest even number to preserve turn boundaries
    let drain_count = if excess.is_multiple_of(2) { excess } else { excess + 1 };
    messages.drain(..drain_count.min(messages.len()));
}

// ---------------------------------------------------------------------------
// Turn summary
// ---------------------------------------------------------------------------

/// Generate a TurnSummary from the messages of a single conversation turn.
///
/// Extracts the user's intent and the assistant's outcome, then uses a
/// fast LLM call to produce a concise 1-2 sentence summary.
pub(in crate::agent_loop) async fn generate_turn_summary(
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
        "Summarize this conversation turn. Extract: user intent, outcome, and key facts.\n\n\
         User: {}\nAssistant: {}\n\n\
         Respond in this exact format:\n\
         INTENT: <what user wanted>\n\
         OUTCOME: <what was accomplished>\n\
         FACTS: <comma-separated key facts, or NONE>\n\n\
         Key facts include: user preferences, personal info (phone, email, accounts), \
         entity names (projects, organizations), decisions made, or events mentioned. \
         Omit procedural details and tool mechanics.",
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
        system: Some("You are a conversation summarizer. Be concise and precise. Always follow the requested format.".to_string()),
        thinking: None,
        extra: Default::default(),
    };

    match brain.complete(SUMMARY_MODALITY, request).await {
        Ok(response) => {
            let text = response.text().trim().to_string();
            if text.is_empty() {
                return None;
            }
            // Parse structured output
            let (user_intent, assistant_outcome, key_facts) = parse_summary_output(&text);
            Some(TurnSummary {
                turn_number: 0, // filled in by caller
                timestamp: chrono::Utc::now().to_rfc3339(),
                user_intent,
                assistant_outcome,
                tools_used,
                key_facts,
            })
        }
        Err(e) => {
            warn!("Turn summary generation failed: {}", e);
            None
        }
    }
}

/// Parse the structured summary output from the LLM.
fn parse_summary_output(text: &str) -> (String, String, Vec<String>) {
    let mut intent = String::new();
    let mut outcome = String::new();
    let mut facts = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("INTENT:") {
            intent = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("OUTCOME:") {
            outcome = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("FACTS:") {
            let rest = rest.trim();
            if !rest.eq_ignore_ascii_case("NONE") && !rest.is_empty() {
                facts = rest
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }

    // Fallback if structured parsing didn't work
    if intent.is_empty() && outcome.is_empty() {
        let parts: Vec<&str> = text.split("→").collect();
        if parts.len() >= 2 {
            intent = parts[0].trim().to_string();
            outcome = parts[1].trim().to_string();
        } else {
            intent = text.to_string();
        }
    }

    (intent, outcome, facts)
}
