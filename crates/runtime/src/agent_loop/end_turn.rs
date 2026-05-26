//! Handler for the EndTurn / StopSequence stop reasons.
//!
//! When the LLM finishes its turn, this handler processes the response:
//! parses directives, handles NO_REPLY/silent, retries empty responses,
//! strips tool call artifacts, persists the session, generates turn
//! summaries, ingests into tree memory, and fires hooks.

use super::*;
use crate::hooks::HookRegistry;
use crate::kernel_handle::KernelHandle;
use crate::llm_driver::Brain;
use crate::text_tool_recovery::strip_tool_call_artifacts;
use memory::MemorySubstrate;
use types::error::CarrierError;
use types::message::{Message, TokenUsage};
use tracing::{debug, info, warn};

/// Maximum full messages to retain in session (3 turns × 2 = 6).
pub(in crate::agent_loop) const MAX_RETAINED_MESSAGES: usize = 6;

/// Action the main loop should take after handling an EndTurn.
pub(in crate::agent_loop) enum EndTurnAction {
    /// The loop should continue (e.g. empty response retry).
    Retry,
    /// The loop should return this result to the caller.
    Complete(AgentLoopResult),
}

/// Handle a `StopReason::EndTurn | StopReason::StopSequence` response.
///
/// Returns an `EndTurnAction` indicating whether the loop should retry
/// (e.g. for empty responses) or complete with a result.
#[allow(clippy::too_many_arguments)]
pub(in crate::agent_loop) async fn handle_end_turn(
    response: &CompletionResponse,
    session: &mut Session,
    messages: &mut Vec<Message>,
    manifest: &AgentManifest,
    memory: &MemorySubstrate,
    kernel: Option<&Arc<dyn KernelHandle>>,
    brain: Option<&Arc<dyn Brain>>,
    hooks: Option<&HookRegistry>,
    on_phase: Option<&PhaseCallback>,
    session_base_len: usize,
    user_message: &str,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
    channel_type: Option<&str>,
    agent_id_str: &str,
    iteration: u32,
    total_usage: TokenUsage,
    any_tools_executed: bool,
) -> Result<EndTurnAction, CarrierError> {
    let text = response.text();

    // Parse reply directives from the streaming response text
    let (cleaned_text_s, parsed_directives_s) =
        crate::reply_directives::parse_directives(&text);
    let text = strip_tool_call_artifacts(&cleaned_text_s);

    // NO_REPLY: agent intentionally chose not to reply
    if text.trim() == "NO_REPLY" || parsed_directives_s.silent {
        debug!(agent = %manifest.name, "Agent chose NO_REPLY/silent  — silent completion");
        session
            .messages
            .push(Message::assistant("[no reply needed]".to_string()));
        let new_msgs = &session.messages[session_base_len..];
        memory
            .save_session_append_async(
                session.id,
                &session.agent_name,
                new_msgs,
                session.context_window_tokens,
                session.label.as_deref(),
                Some(&session.turn_summaries),
            )
            .await
            .map_err(|e| CarrierError::Memory(e.to_string()))?;
        return Ok(EndTurnAction::Complete(AgentLoopResult {
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
        }));
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
                *messages = crate::session_repair::validate_and_repair(messages);
            }
            messages.push(Message::assistant("[no response]".to_string()));
            messages.push(Message::user("Please provide your response.".to_string()));
            return Ok(EndTurnAction::Retry);
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
            "[Task completed — the agent executed tools but did not produce a text summary.]"
                .to_string()
        } else {
            "[The model returned an empty response. This usually means the model is overloaded, the context is too large, or the API key lacks credits. Try again or check /status.]"
                .to_string()
        }
    } else {
        text
    };
    let final_response = text.clone();
    session.messages.push(Message::assistant(text));

    // Prune NO_REPLY heartbeat turns to save context budget
    crate::session_repair::prune_heartbeat_turns(&mut session.messages, 10);

    // Generate turn summary for this conversation turn
    let turn_msgs = &session.messages[session_base_len..];
    if let Some(ref brain_ref) = brain {
        if let Some(mut summary) =
            super::helpers::generate_turn_summary(turn_msgs, brain_ref).await
        {
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
    super::helpers::trim_oldest_turns(&mut session.messages, MAX_RETAINED_MESSAGES);

    memory
        .save_session_append_async(
            session.id,
            &session.agent_name,
            &new_msgs,
            session.context_window_tokens,
            session.label.as_deref(),
            Some(&session.turn_summaries),
        )
        .await
        .map_err(|e| CarrierError::Memory(e.to_string()))?;

    // TODO(Phase 13): Tree memory remember will be restored here.

    // Fire-and-forget tree ingestion
    if let Some(kh) = kernel {
        let req = types::memory_tree::IngestRequest {
            owner_id: owner_id.unwrap_or("default").to_string(),
            agent_id: session.agent_name.to_string(),
            source_kind: "chat".to_string(),
            source_id: format!(
                "{}:{}",
                channel_type.unwrap_or("api"),
                sender_id.unwrap_or("unknown")
            ),
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
            agent_id: agent_id_str,
            event: types::agent::HookEvent::AgentLoopEnd,
            data: serde_json::json!({
                "iterations": iteration + 1,
                "response_length": final_response.len(),
            }),
        };
        let _ = hook_reg.fire(&ctx);
    }

    Ok(EndTurnAction::Complete(AgentLoopResult {
        response: final_response,
        total_usage,
        iterations: iteration + 1,
        silent: false,
        directives: Default::default(),
        plan: None,
    }))
}
