//! Intent classifier — decides whether an inbound user message starts a new
//! conversation or continues the existing session.
//!
//! Uses a cheap LLM call (typically the "fast" modality) with a short prompt
//! and minimal context. The classifier protects against carry-over confusion
//! when an old session has an unfinished task that the new message is unrelated to.

use std::sync::Arc;

use crate::llm_driver::{CompletionRequest, LlmDriver};
use types::message::{Message, MessageContent};

/// Result of intent classification.
#[derive(Debug, Clone)]
pub struct IntentClassification {
    /// True if the message starts a new conversation.
    pub is_new: bool,
    /// Short human-readable explanation (for logs).
    pub reasoning: String,
}

const SYSTEM_PROMPT: &str = r#"你是一个对话边界判定器。

判断用户的新消息是不是"开启了一个新的对话/任务"，还是"延续之前的对话"。

判定原则：
- 如果上一轮助手在等待用户的具体回答（例如"请提供 AppID"、"确认大纲后回复'确认'"等），用户回的是相关内容 → 延续
- 用户明显切换主题、问完全无关的问题 → 新对话
- 用户说"重新开始"、"换个话题"、"新任务" → 新对话
- 用户简短回应（"嗯"、"好的"、"在吗"、"继续"）在已有对话里 → 延续；在空白处 → 新对话
- 模糊时倾向于"延续"

只输出 JSON，格式：{"is_new": true 或 false, "reasoning": "一句话原因"}
不要输出其他任何内容（不要 ```json 标记，不要解释）。
"#;

const PROMPT_MAX_PREV_CHARS: usize = 200;
const PROMPT_MAX_NEW_CHARS: usize = 200;

/// Build the user prompt with truncated previous + new messages.
fn build_prompt(last_assistant: Option<&str>, new_user: &str) -> String {
    let prev: String = last_assistant
        .map(|s| {
            let truncated: String = s.chars().take(PROMPT_MAX_PREV_CHARS).collect();
            format!("上一轮助手回复：{truncated}")
        })
        .unwrap_or_else(|| "（没有上一轮助手回复，这是用户的第一条消息）".to_string());
    let new_text: String = new_user.chars().take(PROMPT_MAX_NEW_CHARS).collect();
    format!("{prev}\n\n用户新消息：{new_text}")
}

/// Parse the LLM response. Accepts plain JSON or JSON wrapped in fences.
fn parse_response(text: &str) -> Option<IntentClassification> {
    let trimmed = text.trim();
    // Strip code fences if present
    let cleaned = if let Some(stripped) = trimmed.strip_prefix("```json") {
        stripped.trim_start().trim_end_matches("```").trim()
    } else if let Some(stripped) = trimmed.strip_prefix("```") {
        stripped.trim_start().trim_end_matches("```").trim()
    } else {
        trimmed
    };

    // Find the first `{` and last `}` to tolerate extra text
    let start = cleaned.find('{')?;
    let end = cleaned.rfind('}')?;
    if end <= start {
        return None;
    }
    let json_slice = &cleaned[start..=end];

    let parsed: serde_json::Value = serde_json::from_str(json_slice).ok()?;
    let is_new = parsed.get("is_new")?.as_bool()?;
    let reasoning = parsed
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(IntentClassification { is_new, reasoning })
}

/// Classify the intent of a new inbound user message.
///
/// On any error (network, parse, etc.) returns `Err` — callers should treat
/// errors as "new conversation" to avoid context contamination.
pub async fn classify_intent(
    driver: Arc<dyn LlmDriver>,
    model: &str,
    last_assistant: Option<&str>,
    new_user: &str,
) -> Result<IntentClassification, String> {
    let user_prompt = build_prompt(last_assistant, new_user);

    let request = CompletionRequest {
        model: model.to_string(),
        messages: vec![
            Message {
                role: types::message::Role::System,
                content: MessageContent::Text(SYSTEM_PROMPT.to_string()),
            },
            Message {
                role: types::message::Role::User,
                content: MessageContent::Text(user_prompt),
            },
        ],
        tools: Vec::new(),
        max_tokens: 200,
        temperature: 0.0,
        system: None,
        thinking: None,
        extra: serde_json::Value::Null,
    };

    let response = driver
        .complete(request)
        .await
        .map_err(|e| format!("intent LLM call failed: {e}"))?;

    let text = response.text();
    parse_response(&text).ok_or_else(|| format!("intent response parse failed: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_json() {
        let r = parse_response(r#"{"is_new": true, "reasoning": "切换话题"}"#).unwrap();
        assert!(r.is_new);
        assert_eq!(r.reasoning, "切换话题");
    }

    #[test]
    fn test_parse_with_code_fence() {
        let r = parse_response(
            "```json\n{\"is_new\": false, \"reasoning\": \"延续\"}\n```",
        )
        .unwrap();
        assert!(!r.is_new);
    }

    #[test]
    fn test_parse_with_surrounding_text() {
        let r = parse_response(r#"分析结果：{"is_new": true, "reasoning": "new task"} 完成"#).unwrap();
        assert!(r.is_new);
    }

    #[test]
    fn test_parse_missing_field() {
        assert!(parse_response(r#"{"reasoning": "no is_new field"}"#).is_none());
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_response("not json").is_none());
    }

    #[test]
    fn test_build_prompt_truncates() {
        let long_prev = "a".repeat(500);
        let long_new = "b".repeat(500);
        let p = build_prompt(Some(&long_prev), &long_new);
        // Should not contain the full strings
        assert!(p.len() < 1000);
    }

    #[test]
    fn test_build_prompt_no_prev() {
        let p = build_prompt(None, "hello");
        assert!(p.contains("第一条消息"));
        assert!(p.contains("hello"));
    }
}
