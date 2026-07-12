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

/// Maximum full messages to retain in session (6 turns × 2 = 12).
pub(in crate::agent_loop) const MAX_RETAINED_MESSAGES: usize = 12;

/// Action the main loop should take after handling an EndTurn.
pub(in crate::agent_loop) enum EndTurnAction {
    /// The loop should continue (e.g. empty response retry).
    Retry,
    /// The loop should return this result to the caller.
    Complete(AgentLoopResult),
}

/// Count consecutive trailing assistant "[no response]" messages — each is a
/// previous empty-response retry marker. Used to detect sustained gateway
/// silent failures and stop retrying before the session bloats into a loop.
fn count_trailing_retries(messages: &[Message]) -> usize {
    let mut count = 0;
    for m in messages.iter().rev() {
        if matches!(m.role, types::message::Role::Assistant) {
            if matches!(&m.content, types::message::MessageContent::Text(t) if t == "[no response]")
            {
                count += 1;
            } else {
                break; // a different assistant message ends the streak
            }
        }
        // Non-assistant messages (user "Please respond" prompts, tool results)
        // between retries don't break the streak.
    }
    count
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
    _kernel: Option<&Arc<dyn KernelHandle>>,
    memory_handle: Option<&Arc<dyn crate::memory_handle::MemoryHandle>>,
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
        // O6: Single-track — sync loop messages before pushing the final response
        super::helpers::sync_loop_messages(messages, session, session_base_len);
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

    // One-shot retry with sustained-failure protection: if the LLM returns
    // empty text with no tool use, try once more before accepting the empty
    // result. Triggers on first call OR when input_tokens==0 (silent gateway
    // failure — the response looks bogus and input wasn't processed).
    //
    // To avoid session bloat when the gateway is sustained-broken (each retry
    // adds two messages, and if retries keep failing we loop until max_iters),
    // count trailing "[no response]" markers already in the history and stop
    // after MAX_SILENT_RETRIES consecutive silent failures.
    if text.trim().is_empty() && response.tool_calls.is_empty() {
        let is_silent_failure = response.usage.input_tokens == 0;
        let trailing_retries = count_trailing_retries(messages);
        const MAX_SILENT_RETRIES: usize = 2; // 3 total attempts, then give up
        let exhausted = is_silent_failure && trailing_retries >= MAX_SILENT_RETRIES;
        let should_retry = (iteration == 0 || is_silent_failure) && !exhausted;
        if should_retry {
            warn!(
                agent = %manifest.name,
                iteration,
                input_tokens = response.usage.input_tokens,
                output_tokens = response.usage.output_tokens,
                silent_failure = is_silent_failure,
                trailing_retries,
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
        if exhausted {
            warn!(
                agent = %manifest.name,
                iteration,
                trailing_retries,
                messages_count = messages.len(),
                "Silent gateway failure persisted across {} retries; stopping to avoid session bloat, falling back",
                trailing_retries,
            );
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
            "(已执行操作,但这次没能生成回复文字。请稍后重试,或重新说一下你的需求。)"
                .to_string()
        } else {
            "(模型这次没有返回内容,可能是服务繁忙或上下文过长。请稍后重试,或简化一下你的请求。)"
                .to_string()
        }
    } else {
        text
    };
    let final_response = text.clone();
    // O6: Single-track — sync loop messages before pushing the final response
    super::helpers::sync_loop_messages(messages, session, session_base_len);
    session.messages.push(Message::assistant(text));

    // Prune NO_REPLY heartbeat turns to save context budget
    crate::session_repair::prune_heartbeat_turns(&mut session.messages, 10);

    // Generate turn summary for this conversation turn.
    // Clamp to the current message count — prune_heartbeat_turns may have
    // removed messages that were included in session_base_len.
    let base = session_base_len.min(session.messages.len());
    let turn_msgs = &session.messages[base..];
    if let Some(brain_ref) = brain {
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

            // Extract knowledge from turn summary and write to drawer
            if !summary.key_facts.is_empty() {
                if let Some(mh) = memory_handle {
                    let agent_name = &manifest.name;
                    let oid = owner_id.unwrap_or("");
                    let uid = sender_id.unwrap_or("");
                    extract_and_merge_knowledge(&summary, mh, agent_name, oid, uid);
                }
            }

            session.turn_summaries.push(summary);
        }
    }

    // Capture new messages BEFORE trim — session_base_len becomes invalid after trim.
    // Also clamp with .min() because prune_heartbeat_turns (line 188) may have removed
    // messages that were counted in session_base_len.
    let new_msgs: Vec<Message> = session.messages[session_base_len.min(session.messages.len())..].to_vec();

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
    if let Some(mh) = memory_handle {
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
            user_id: sender_id.map(|s| s.to_string()),
        };
        let mh = Arc::clone(mh);
        tokio::spawn(async move {
            if let Err(e) = mh.tree_ingest(req).await {
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

// ---------------------------------------------------------------------------
// Knowledge extraction from turn summaries → drawer (kv)
// ---------------------------------------------------------------------------

/// Drawer key prefixes for state-type data (merge/dedup).
const STATE_PREFIXES: &[&str] = &["profile.", "preference.", "entity.", "fact."];

/// Classify a key fact from the turn summary into a drawer key-value pair.
///
/// Rules:
/// - Phone/email → profile.*
/// - Preference (likes, wants) → preference.*
/// - Named entities (accounts, projects, orgs) → entity.*
/// - Facts (rules, constraints) → fact.*
/// - Decisions, events → event.YYYY-MM-DD.specific
fn classify_fact(fact: &str) -> Option<(String, Vec<String>)> {
    let lower = fact.to_lowercase();

    // Profile: personal identifiers
    if lower.contains("phone") || lower.contains("手机") {
        return Some(("profile.phone_numbers".to_string(), vec![fact.to_string()]));
    }
    if lower.contains("email") || lower.contains("邮箱") {
        return Some(("profile.email".to_string(), vec![fact.to_string()]));
    }

    // Preference
    if lower.contains("prefers") || lower.contains("likes") || lower.contains("wants")
        || lower.contains("偏好") || lower.contains("喜欢")
    {
        return Some(("preference.general".to_string(), vec![fact.to_string()]));
    }

    // Entity: accounts, projects, organizations
    if lower.contains("account") || lower.contains("公众号") || lower.contains("workspace")
        || lower.contains("项目")
    {
        return Some(("entity.accounts".to_string(), vec![fact.to_string()]));
    }

    // Event: decisions, scheduled items
    if lower.contains("decided") || lower.contains("决定") || lower.contains("scheduled")
        || lower.contains("计划")
    {
        let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let key = format!("event.{}.decision", date);
        return Some((key, vec![fact.to_string()]));
    }

    // Default: entity as catch-all for named items
    Some(("entity.misc".to_string(), vec![fact.to_string()]))
}

/// Extract knowledge from a turn summary and merge into the drawer.
fn extract_and_merge_knowledge(
    summary: &types::message::TurnSummary,
    memory_handle: &Arc<dyn crate::memory_handle::MemoryHandle>,
    agent_name: &str,
    owner_id: &str,
    user_id: &str,
) {
    for fact in &summary.key_facts {
        if let Some((key, new_values)) = classify_fact(fact) {
            merge_drawer_value(memory_handle, agent_name, owner_id, user_id, &key, new_values);
        }
    }
}

/// Merge new values into a drawer key.
///
/// - State-type keys (profile/preference/entity/fact): read existing → merge dedup → write back
/// - Timeline-type keys (event.*): read existing → append → write back
fn merge_drawer_value(
    memory_handle: &Arc<dyn crate::memory_handle::MemoryHandle>,
    agent_name: &str,
    owner_id: &str,
    user_id: &str,
    key: &str,
    new_values: Vec<String>,
) {
    let is_state = STATE_PREFIXES.iter().any(|p| key.starts_with(p));

    // Read existing value
    let existing = memory_handle
        .kv_get(agent_name, owner_id, user_id, key)
        .ok()
        .flatten();

    let merged = match existing {
        Some(serde_json::Value::Array(arr)) => {
            let mut current: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();

            if is_state {
                // Merge dedup: add new values that don't already exist
                for v in new_values {
                    if !current.contains(&v) {
                        current.push(v);
                    }
                }
            } else {
                // Timeline: append all
                current.extend(new_values);
            }
            current
        }
        Some(serde_json::Value::String(s)) => {
            let mut current = vec![s];
            if is_state {
                for v in new_values {
                    if !current.contains(&v) {
                        current.push(v);
                    }
                }
            } else {
                current.extend(new_values);
            }
            current
        }
        Some(_other) => {
            // Non-array/string existing value — overwrite with new
            new_values
        }
        None => {
            // No existing value — just write new
            new_values
        }
    };

    let value = serde_json::Value::Array(
        merged.into_iter().map(serde_json::Value::String).collect(),
    );

    if let Err(e) = memory_handle.kv_set(agent_name, owner_id, user_id, key, value) {
        debug!("Failed to write drawer key '{}': {}", key, e);
    } else {
        info!(agent = agent_name, key = key, "Drawer entry updated");
    }
}
