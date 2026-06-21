//! OpenAI-compatible HTTP driver for LLM API calls.
//!
//! All LLM traffic goes through aginxbrain (OpenAI-compatible proxy),
//! so this driver only implements the OpenAI Chat Completions format.
//! aginxbrain handles provider-specific routing based on the model/tag.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError, StreamEvent};
use crate::think_filter::{FilterAction, StreamingThinkFilter};
use crate::USER_AGENT;
use async_trait::async_trait;
use types::message::{ContentBlock, MessageContent, Role, StopReason, TokenUsage};
use types::tool::ToolCall;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use futures::StreamExt;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// OpenAI driver struct
// ---------------------------------------------------------------------------

pub struct UnifiedHttpDriver {
    api_key: Zeroizing<String>,
    base_url: String,
    client: reqwest::Client,
}

impl UnifiedHttpDriver {
    pub fn new(api_key: String, base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(0)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_default();
        Self {
            api_key: Zeroizing::new(api_key),
            base_url,
            client,
        }
    }
}

// ---------------------------------------------------------------------------
// OpenAI format request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OaiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct OaiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OaiMessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OaiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    /// Always include reasoning_content — aginxbrain handles provider differences.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum OaiMessageContent {
    Text(String),
    Parts(Vec<OaiContentPart>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum OaiContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: OaiImageUrl },
    #[serde(rename = "input_audio")]
    InputAudio { input_audio: OaiInputAudio },
}

#[derive(Debug, Serialize)]
struct OaiImageUrl { url: String }

#[derive(Debug, Serialize)]
struct OaiInputAudio { data: String, format: String }

#[derive(Debug, Serialize, Deserialize)]
struct OaiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OaiFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiFunction { name: String, arguments: String }

#[derive(Debug, Serialize)]
struct OaiTool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OaiToolDef,
}

#[derive(Debug, Serialize)]
struct OaiToolDef { name: String, description: String, parameters: serde_json::Value }

#[derive(Debug, Deserialize)]
struct OaiResponse { choices: Vec<OaiChoice>, usage: Option<OaiUsage> }

#[derive(Debug, Deserialize)]
struct OaiChoice { message: OaiResponseMessage, finish_reason: Option<String> }

#[derive(Debug, Deserialize)]
struct OaiResponseMessage {
    content: Option<serde_json::Value>,
    tool_calls: Option<Vec<OaiToolCall>>,
    reasoning_content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OaiUsage { prompt_tokens: u64, completion_tokens: u64 }

fn extract_reasoning_text(val: &serde_json::Value) -> String {
    val.as_str().unwrap_or("").to_string()
}

fn mime_to_audio_format(mime: &str) -> &str {
    match mime {
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/flac" => "flac",
        "audio/mp4" | "audio/m4a" => "mp4",
        "audio/webm" => "webm",
        _ => "mp3",
    }
}

// ---------------------------------------------------------------------------
// Message building
// ---------------------------------------------------------------------------

impl UnifiedHttpDriver {
    fn build_oai_messages(&self, request: &CompletionRequest) -> Vec<OaiMessage> {
        let mut oai_messages: Vec<OaiMessage> = Vec::new();

        if let Some(ref system) = request.system {
            if !system.is_empty() {
                oai_messages.push(OaiMessage {
                    role: "system".to_string(),
                    content: Some(OaiMessageContent::Text(system.clone())),
                    tool_calls: None,
                    tool_call_id: None,
                    reasoning_content: None,
                });
            }
        }

        for msg in &request.messages {
            match (&msg.role, &msg.content) {
                (Role::System, MessageContent::Text(text)) => {
                    if request.system.is_none() {
                        oai_messages.push(OaiMessage {
                            role: "system".to_string(),
                            content: Some(OaiMessageContent::Text(text.clone())),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                        });
                    }
                }
                (Role::User, MessageContent::Text(text)) => {
                    oai_messages.push(OaiMessage {
                        role: "user".to_string(),
                        content: Some(OaiMessageContent::Text(text.clone())),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                }
                (Role::Assistant, MessageContent::Text(text)) => {
                    oai_messages.push(OaiMessage {
                        role: "assistant".to_string(),
                        content: Some(OaiMessageContent::Text(text.clone())),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                    });
                }
                (Role::User, MessageContent::Blocks(blocks)) => {
                    let mut parts: Vec<OaiContentPart> = Vec::new();
                    let mut has_tool_results = false;
                    for block in blocks {
                        match block {
                            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                                has_tool_results = true;
                                oai_messages.push(OaiMessage {
                                    role: "tool".to_string(),
                                    content: Some(OaiMessageContent::Text(if content.is_empty() {
                                        "(empty)".to_string()
                                    } else {
                                        content.clone()
                                    })),
                                    tool_calls: None,
                                    tool_call_id: Some(tool_use_id.clone()),
                                    reasoning_content: None,
                                });
                            }
                            ContentBlock::Text { text, .. } => {
                                parts.push(OaiContentPart::Text { text: text.clone() });
                            }
                            ContentBlock::Image { data, media_type, .. } => {
                                parts.push(OaiContentPart::ImageUrl {
                                    image_url: OaiImageUrl {
                                        url: format!("data:{media_type};base64,{data}"),
                                    },
                                });
                            }
                            ContentBlock::Audio { data, media_type, .. } => {
                                parts.push(OaiContentPart::InputAudio {
                                    input_audio: OaiInputAudio {
                                        data: data.clone(),
                                        format: mime_to_audio_format(media_type).to_string(),
                                    },
                                });
                            }
                            ContentBlock::Thinking { .. } => {}
                            _ => {}
                        }
                    }
                    if !parts.is_empty() && !has_tool_results {
                        oai_messages.push(OaiMessage {
                            role: "user".to_string(),
                            content: Some(OaiMessageContent::Parts(parts)),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                        });
                    }
                }
                (Role::Assistant, MessageContent::Blocks(blocks)) => {
                    let mut text_parts = Vec::new();
                    let mut tc_list = Vec::new();
                    let mut reasoning_text = String::new();
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text, .. } => text_parts.push(text.clone()),
                            ContentBlock::ToolUse { id, name, input, .. } => {
                                tc_list.push(OaiToolCall {
                                    id: id.clone(),
                                    call_type: "function".to_string(),
                                    function: OaiFunction {
                                        name: name.clone(),
                                        arguments: serde_json::to_string(input).unwrap_or_default(),
                                    },
                                });
                            }
                            ContentBlock::Thinking { thinking } => {
                                reasoning_text = thinking.clone();
                            }
                            _ => {}
                        }
                    }
                    let has_tool_calls = !tc_list.is_empty();
                    oai_messages.push(OaiMessage {
                        role: "assistant".to_string(),
                        content: if text_parts.is_empty() {
                            if has_tool_calls { Some(OaiMessageContent::Text(String::new())) } else { None }
                        } else {
                            Some(OaiMessageContent::Text(text_parts.join("")))
                        },
                        tool_calls: if tc_list.is_empty() { None } else { Some(tc_list) },
                        tool_call_id: None,
                        // Always include reasoning_content — aginxbrain handles provider differences
                        reasoning_content: if reasoning_text.is_empty() { None } else { Some(reasoning_text) },
                    });
                }
                _ => {}
            }
        }

        oai_messages
    }

    fn build_oai_request(&self, request: &CompletionRequest) -> OaiRequest {
        let mut messages = self.build_oai_messages(request);

        // Sanitize tool_call arguments: strict providers reject
        // non-JSON arguments like "null", empty strings, or malformed JSON.
        let mut removed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for msg in &mut messages {
            if let Some(calls) = &mut msg.tool_calls {
                calls.retain(|tc| {
                    let args = tc.function.arguments.trim();
                    let valid = !args.is_empty() && args != "null" && serde_json::from_str::<serde_json::Value>(args).is_ok();
                    if !valid {
                        warn!(tool = %tc.function.name, raw_args = %tc.function.arguments, "Removing tool_call with invalid arguments from request");
                        removed_ids.insert(tc.id.clone());
                    }
                    valid
                });
                if calls.is_empty() {
                    msg.tool_calls = None;
                }
            }
        }
        // Remove tool_result messages whose call was removed
        if !removed_ids.is_empty() {
            messages.retain(|msg| {
                if msg.role == "tool" {
                    if let Some(ref id) = msg.tool_call_id {
                        return !removed_ids.contains(id);
                    }
                }
                true
            });
        }

        let max_tokens = if request.max_tokens > 0 { Some(request.max_tokens) } else { None };
        let temperature = if request.temperature > 0.0 { Some(request.temperature) } else { None };

        let tools: Vec<OaiTool> = request.tools.iter().map(|t| {
            let schema = types::tool::normalize_schema_for_provider(&t.input_schema, "openai");
            if !schema.is_object() {
                warn!(tool = %t.name, "Tool schema is not an object after normalization, type={}", schema);
            }
            OaiTool {
                tool_type: "function".to_string(),
                function: OaiToolDef {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: schema,
                },
            }
        }).collect();

        let tool_choice = if tools.is_empty() { None } else { Some(serde_json::json!("auto")) };

        OaiRequest {
            model: request.model.clone(),
            messages,
            max_tokens,
            temperature,
            tools,
            tool_choice,
            stream: false,
            stream_options: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Non-streaming completion
// ---------------------------------------------------------------------------

impl UnifiedHttpDriver {
    async fn complete_openai(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut oai_request = self.build_oai_request(&request);
        let resp = self.send_openai_with_retry(&mut oai_request).await?;

        let body = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;

        // aginxbrain wraps some responses in {"code":"Success","output":{...}}
        // Try standard OpenAI format first; if missing `choices`, unwrap from `output`
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|e| LlmError::Parse(e.to_string()))?;
        let oai_json = if parsed.get("choices").is_some() {
            parsed
        } else if let Some(output) = parsed.get("output") {
            output.clone()
        } else {
            parsed
        };
        let oai_response: OaiResponse = serde_json::from_value(oai_json).map_err(|e| LlmError::Parse(e.to_string()))?;

        let choice = oai_response.choices.into_iter().next()
            .ok_or_else(|| LlmError::Parse("No choices in response".to_string()))?;

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        let mut media = None;

        if let Some(ref reasoning) = choice.message.reasoning_content {
            let text = extract_reasoning_text(reasoning);
            if !text.is_empty() {
                debug!(len = text.len(), "Captured reasoning_content from response");
                content.push(ContentBlock::Thinking { thinking: text });
            }
        }

        // content can be: String, Array of content parts (OpenAI), or Array with image URLs (aginxbrain)
        if let Some(content_val) = &choice.message.content {
            match content_val {
                serde_json::Value::String(text) => {
                    if !text.is_empty() {
                        let (cleaned, thinking) = extract_think_tags(text);
                        if let Some(think_text) = thinking {
                            if choice.message.reasoning_content.is_none() {
                                content.push(ContentBlock::Thinking { thinking: think_text });
                            }
                        }
                        if !cleaned.is_empty() {
                            content.push(ContentBlock::Text { text: cleaned, provider_metadata: None });
                        }
                    }
                }
                serde_json::Value::Array(parts) => {
                    let mut text_parts = Vec::new();
                    let mut image_urls = Vec::new();
                    for part in parts {
                        if let Some(s) = part.as_str() {
                            text_parts.push(s.to_string());
                        } else if let Some(url) = part.get("image").and_then(|v| v.as_str()) {
                            image_urls.push(url.to_string());
                        } else if let Some(url) = part.get("image_url")
                            .and_then(|v| v.get("url"))
                            .and_then(|v| v.as_str())
                        {
                            image_urls.push(url.to_string());
                        } else if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            text_parts.push(t.to_string());
                        }
                    }
                    if !text_parts.is_empty() {
                        let text = text_parts.join("");
                        let (cleaned, thinking) = extract_think_tags(&text);
                        if let Some(think_text) = thinking {
                            if choice.message.reasoning_content.is_none() {
                                content.push(ContentBlock::Thinking { thinking: think_text });
                            }
                        }
                        if !cleaned.is_empty() {
                            content.push(ContentBlock::Text { text: cleaned, provider_metadata: None });
                        }
                    }
                    if !image_urls.is_empty() {
                        let items: Vec<types::media::GeneratedImage> = image_urls.into_iter().map(|url| {
                            types::media::GeneratedImage {
                                data_base64: String::new(),
                                url: Some(url),
                            }
                        }).collect();
                        media = Some(types::media::MediaOutput::Images { items });
                    }
                }
                _ => {}
            }
        }

        let has_text = content.iter().any(|b| matches!(b, ContentBlock::Text { .. }));
        let has_thinking = content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. }));
        if has_thinking && !has_text && choice.message.tool_calls.is_none() && media.is_none() {
            let thinking_text = content.iter().find_map(|b| match b {
                ContentBlock::Thinking { thinking } => Some(thinking.as_str()),
                _ => None,
            }).unwrap_or("");
            let summary = extract_thinking_summary(thinking_text);
            debug!(summary_len = summary.len(), "Synthesizing text from thinking-only response");
            content.push(ContentBlock::Text { text: summary, provider_metadata: None });
        }

        if let Some(calls) = choice.message.tool_calls {
            for call in calls {
                let input: serde_json::Value = serde_json::from_str(&call.function.arguments).unwrap_or_default();
                content.push(ContentBlock::ToolUse {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    input: input.clone(),
                    provider_metadata: None,
                });
                tool_calls.push(ToolCall {
                    id: call.id,
                    name: call.function.name,
                    input,
                });
            }
        }

        let stop_reason = match choice.finish_reason.as_deref() {
            Some("stop") => StopReason::EndTurn,
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            _ => if !tool_calls.is_empty() { StopReason::ToolUse } else { StopReason::EndTurn },
        };

        let mut usage = oai_response.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        }).unwrap_or_default();

        if !content.is_empty() && usage.input_tokens == 0 && usage.output_tokens == 0 {
            debug!("Response has content but no usage stats — setting synthetic output_tokens=1");
            usage.output_tokens = 1;
        }

        Ok(CompletionResponse { content, stop_reason, tool_calls, usage, media })
    }

    /// OpenAI-specific retry with request body mutation.
    async fn send_openai_with_retry(&self, oai_request: &mut OaiRequest) -> Result<reqwest::Response, LlmError> {
        let max_retries: u8 = 3;
        for attempt in 0..=max_retries {
            let url = self.base_url.clone();
            debug!(url = %url, attempt, "Sending OpenAI API request");

            let builder = self.client
                .post(&url)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", self.api_key.as_str()))
                .json(&*oai_request);

            let resp = match builder.send().await {
                Ok(r) => r,
                Err(e) => {
                    let err_str = e.to_string();
                    if attempt < max_retries && (err_str.contains("error decoding")
                        || err_str.contains("error sending")
                        || err_str.contains("connection"))
                    {
                        let retry_ms = (attempt as u64 + 1) * 2000;
                        warn!(%err_str, attempt, retry_ms, "HTTP transport error, retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(retry_ms)).await;
                        continue;
                    }
                    return Err(LlmError::Http(err_str));
                }
            };
            let status = resp.status().as_u16();

            if resp.status().is_success() {
                return Ok(resp);
            }

            let body = resp.text().await.unwrap_or_default();

            // Log 400 errors with tool details for debugging provider schema issues
            if status == 400 && body.contains("arguments") && attempt == 0 {
                let problem_tools: Vec<&str> = oai_request.tools.iter()
                    .filter(|t| !t.function.parameters.is_object())
                    .map(|t| t.function.name.as_str())
                    .collect();
                let bad_msg_args: Vec<String> = oai_request.messages.iter()
                    .filter_map(|m| m.tool_calls.as_ref())
                    .flat_map(|calls| calls.iter())
                    .filter(|c| {
                        let s = c.function.arguments.trim();
                        s.is_empty() || s == "null" || serde_json::from_str::<serde_json::Value>(s).is_err()
                    })
                    .map(|c| format!("{}: {}...", c.function.name, &c.function.arguments[..c.function.arguments.len().min(80)]))
                    .collect();
                warn!(status, ?problem_tools, ?bad_msg_args, "Provider rejected tool arguments schema");
            }

            // 429 rate limit
            if status == 429 {
                if attempt < max_retries {
                    let retry_ms = (attempt as u64 + 1) * 2000;
                    warn!(status, retry_ms, "Rate limited, retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(retry_ms)).await;
                    continue;
                }
                return Err(LlmError::RateLimited { retry_after_ms: 5000 });
            }

            // Strip temperature for models that don't support it
            if status == 400 && oai_request.temperature.is_some() && attempt < max_retries
                && (body.contains("temperature") && (body.contains("unsupported_parameter") || body.contains("deprecated")))
            {
                warn!(model = %oai_request.model, "Stripping temperature for this model");
                oai_request.temperature = None;
                continue;
            }

            // Auto-cap max_tokens
            if status == 400 && body.contains("max_tokens") && attempt < max_retries {
                let current = oai_request.max_tokens.unwrap_or(4096);
                let cap = extract_max_tokens_limit(&body).unwrap_or(current / 2);
                warn!(old = current, new = cap, "Auto-capping max_tokens to model limit");
                oai_request.max_tokens = Some(cap);
                continue;
            }

            // Retry without tools
            let body_lower = body.to_lowercase();
            if !oai_request.tools.is_empty() && attempt < max_retries
                && (status == 500 || body_lower.contains("internal error")
                    || (status == 400 && (body_lower.contains("does not support tools")
                        || body_lower.contains("tool") && body_lower.contains("not supported"))))
            {
                warn!(model = %oai_request.model, status, "Model may not support tools, retrying without tools");
                oai_request.tools.clear();
                oai_request.tool_choice = None;
                continue;
            }

            return Err(LlmError::Api { status, message: crate::str_utils::safe_truncate_str(&body, 500).to_string() });
        }

        Err(LlmError::Api { status: 0, message: "Max retries exceeded".to_string() })
    }
}

// ===========================================================================
// Shared helper functions
// ===========================================================================

/// Maximum idle time (no bytes received) before aborting a streaming response.
const STREAM_IDLE_TIMEOUT_SECS: u64 = 120;

/// Read the next chunk from a byte stream with an idle timeout.
async fn next_chunk_with_idle_timeout(
    stream: &mut (impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Unpin),
) -> Result<Option<bytes::Bytes>, LlmError> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(STREAM_IDLE_TIMEOUT_SECS),
        stream.next(),
    )
    .await
    {
        Ok(Some(chunk_result)) => chunk_result.map(Some).map_err(|e| LlmError::Http(e.to_string())),
        Ok(None) => Ok(None),
        Err(_) => Err(LlmError::Http(format!(
            "Streaming idle timeout: no data received in {STREAM_IDLE_TIMEOUT_SECS}s"
        ))),
    }
}

/// Extract think tags from content text, returning (cleaned_text, thinking_content).
fn extract_think_tags(text: &str) -> (String, Option<String>) {
    let mut thinking_parts = Vec::new();
    let mut cleaned = String::with_capacity(text.len());
    let mut remaining = text;
    let open_tag = "<think>";
    let close_tag = "</think>";

    while let Some(start) = remaining.find(open_tag) {
        cleaned.push_str(&remaining[..start]);
        let after_open = start + open_tag.len();

        if let Some(end) = remaining[after_open..].find(close_tag) {
            let think_text = remaining[after_open..after_open + end].trim();
            if !think_text.is_empty() {
                thinking_parts.push(think_text.to_string());
            }
            remaining = &remaining[after_open + end + close_tag.len()..];
        } else {
            let thought = remaining[after_open..].trim();
            if !thought.is_empty() {
                thinking_parts.push(thought.to_string());
            }
            break;
        }
    }

    cleaned.push_str(remaining);

    let thinking = if thinking_parts.is_empty() {
        None
    } else {
        Some(thinking_parts.join("\n\n"))
    };

    (cleaned.trim().to_string(), thinking)
}

/// Extract a brief summary from thinking-only content.
fn extract_thinking_summary(thinking: &str) -> String {
    let paragraphs: Vec<&str> = thinking.split("\n\n").filter(|p| !p.trim().is_empty()).collect();
    if let Some(last) = paragraphs.last() {
        let trimmed = last.trim();
        if trimmed.len() > 200 {
            let end = trimmed.char_indices().take_while(|(i, _)| *i < 200).last().map(|(i, c)| i + c.len_utf8()).unwrap_or(0);
            format!("{}...", &trimmed[..end])
        } else {
            trimmed.to_string()
        }
    } else {
        "Thinking complete.".to_string()
    }
}

/// Extract max_tokens limit from error body.
fn extract_max_tokens_limit(body: &str) -> Option<u32> {
    let idx = body.find("max_tokens")?;
    let after = &body[idx + "max_tokens".len()..];
    let start = after.find(|c: char| c.is_ascii_digit())?;
    let digits: String = after[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

// ===========================================================================
// LlmDriver implementation
// ===========================================================================

#[async_trait]
impl LlmDriver for UnifiedHttpDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        self.complete_openai(request).await
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        let mut oai_request = self.build_oai_request(&request);
        oai_request.stream = true;
        oai_request.stream_options = Some(serde_json::json!({"include_usage": true}));

        let resp = self.send_openai_with_retry(&mut oai_request).await?;

        let mut buffer = String::new();
        let mut text_content = String::new();
        let mut reasoning_content = String::new();
        let mut think_filter = StreamingThinkFilter::new();
        let mut tool_accum: Vec<(String, String, String)> = Vec::new();
        let mut finish_reason: Option<String> = None;
        let mut usage = TokenUsage::default();

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk) = next_chunk_with_idle_timeout(&mut byte_stream).await? {
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].trim_end().to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.is_empty() || line.starts_with(':') { continue; }

                let data = match line.strip_prefix("data:") {
                    Some(d) => d.trim_start(),
                    None => continue,
                };
                if data == "[DONE]" { continue; }

                let json: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if let Some(u) = json.get("usage") {
                    if let Some(pt) = u["prompt_tokens"].as_u64() { usage.input_tokens = pt; }
                    if let Some(ct) = u["completion_tokens"].as_u64() { usage.output_tokens = ct; }
                }

                let choices = match json["choices"].as_array() {
                    Some(c) => c,
                    None => continue,
                };

                for choice in choices {
                    let delta = &choice["delta"];

                    if let Some(text) = delta["content"].as_str() {
                        if !text.is_empty() {
                            text_content.push_str(text);
                            for action in think_filter.process(text) {
                                match action {
                                    FilterAction::EmitText(t) => {
                                        let _ = tx.send(StreamEvent::TextDelta { text: t }).await;
                                    }
                                    FilterAction::EmitThinking(t) => {
                                        let _ = tx.send(StreamEvent::ThinkingDelta { text: t }).await;
                                    }
                                }
                            }
                        }
                    }

                    if let Some(reasoning) = delta["reasoning_content"].as_str() {
                        if !reasoning.is_empty() {
                            reasoning_content.push_str(reasoning);
                            let _ = tx.send(StreamEvent::ThinkingDelta { text: reasoning.to_string() }).await;
                        }
                    }

                    if let Some(calls) = delta["tool_calls"].as_array() {
                        for call in calls {
                            let idx = call["index"].as_u64().unwrap_or(0) as usize;
                            if idx > 100 {
                                warn!(idx = idx, "tool_calls index exceeds 100, skipping");
                                continue;
                            }
                            while tool_accum.len() <= idx {
                                tool_accum.push((String::new(), String::new(), String::new()));
                            }
                            if let Some(id) = call["id"].as_str() {
                                tool_accum[idx].0 = id.to_string();
                            }
                            if let Some(func) = call.get("function") {
                                if let Some(name) = func["name"].as_str() {
                                    tool_accum[idx].1 = name.to_string();
                                    let _ = tx.send(StreamEvent::ToolUseStart {
                                        id: tool_accum[idx].0.clone(),
                                        name: name.to_string(),
                                    }).await;
                                }
                                if let Some(args) = func["arguments"].as_str() {
                                    tool_accum[idx].2.push_str(args);
                                    let _ = tx.send(StreamEvent::ToolInputDelta { text: args.to_string() }).await;
                                }
                            }
                        }
                    }

                    if let Some(fr) = choice["finish_reason"].as_str() {
                        if !fr.is_empty() { finish_reason = Some(fr.to_string()); }
                    }
                }
            }
        }

        // Flush think filter
        for action in think_filter.flush() {
            match action {
                FilterAction::EmitText(t) => { let _ = tx.send(StreamEvent::TextDelta { text: t }).await; }
                FilterAction::EmitThinking(t) => { let _ = tx.send(StreamEvent::ThinkingDelta { text: t }).await; }
            }
        }

        // Build content
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        if !reasoning_content.is_empty() {
            content.push(ContentBlock::Thinking { thinking: reasoning_content });
        }

        if !text_content.is_empty() {
            let (clean_text, thinking) = extract_think_tags(&text_content);
            if let Some(th) = thinking {
                content.push(ContentBlock::Thinking { thinking: th });
            }
            if !clean_text.is_empty() {
                content.push(ContentBlock::Text { text: clean_text, provider_metadata: None });
            }
        }

        for (id, name, args_json) in &tool_accum {
            let input: serde_json::Value = serde_json::from_str(args_json)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            content.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                provider_metadata: None,
            });
            tool_calls.push(ToolCall { id: id.clone(), name: name.clone(), input });
            let _ = tx.send(StreamEvent::ToolUseEnd {
                id: id.clone(),
                name: name.clone(),
                input: serde_json::from_str(args_json).unwrap_or_default(),
            }).await;
        }

        if content.is_empty() && tool_accum.is_empty() {
            content.push(ContentBlock::Text { text: text_content.clone(), provider_metadata: None });
        }

        let stop_reason = match finish_reason.as_deref() {
            Some("stop") => StopReason::EndTurn,
            Some("tool_calls") => StopReason::ToolUse,
            Some("length") => StopReason::MaxTokens,
            _ => if !tool_accum.is_empty() { StopReason::ToolUse } else { StopReason::EndTurn },
        };

        if usage.output_tokens == 0 && (!content.is_empty() || !tool_accum.is_empty()) {
            usage.output_tokens = 1;
        }

        let response = CompletionResponse { content, stop_reason, tool_calls, usage, media: None };
        let _ = tx.send(StreamEvent::ContentComplete { stop_reason: response.stop_reason, usage: response.usage }).await;
        Ok(response)
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_think_tags() {
        let (cleaned, thinking) = extract_think_tags("hello world");
        assert_eq!(cleaned, "hello world");
        assert!(thinking.is_none());
    }

    #[test]
    fn test_extract_thinking_summary() {
        let summary = extract_thinking_summary("Line one\n\nLine two\n\nLine three");
        assert_eq!(summary, "Line three");
    }

    #[test]
    fn test_mime_to_audio_format() {
        assert_eq!(mime_to_audio_format("audio/mpeg"), "mp3");
        assert_eq!(mime_to_audio_format("audio/wav"), "wav");
        assert_eq!(mime_to_audio_format("audio/unknown"), "mp3");
    }
}
