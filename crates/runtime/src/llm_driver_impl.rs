//! Unified HTTP driver for all LLM API formats.
//!
//! A single `UnifiedHttpDriver` dispatches by `ApiFormat` to format-specific
//! request building and response parsing. Shared HTTP infrastructure (auth,
//! retry, error classification) is handled once.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError, StreamEvent};
use crate::think_filter::{FilterAction, StreamingThinkFilter};
use crate::llm_errors::classify_error;
use crate::USER_AGENT;
use async_trait::async_trait;
use types::brain::{ApiFormat, AuthHeaderType};
use types::media::{GeneratedImage, MediaOutput};
use types::message::{ContentBlock, Message, MessageContent, Role, StopReason, TokenUsage};
use types::tool::ToolCall;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use futures::StreamExt;
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// Unified driver struct
// ---------------------------------------------------------------------------

pub struct UnifiedHttpDriver {
    format: ApiFormat,
    api_key: Zeroizing<String>,
    secret_key: Option<Zeroizing<String>>,
    base_url: String,
    auth_header: AuthHeaderType,
    client: reqwest::Client,
}

impl UnifiedHttpDriver {
    pub fn new(
        format: ApiFormat,
        api_key: String,
        secret_key: Option<String>,
        base_url: String,
        auth_header: AuthHeaderType,
    ) -> Self {
        let timeout = match format {
            ApiFormat::DashScopeTts => 60,
            ApiFormat::MiniMaxSearch | ApiFormat::GlmSearch | ApiFormat::ZhipuSearch => 15,
            _ => 300,
        };
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(timeout))
            .build()
            .unwrap_or_default();
        Self {
            format,
            api_key: Zeroizing::new(api_key),
            secret_key: secret_key.map(Zeroizing::new),
            base_url,
            auth_header,
            client,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared HTTP layer
// ---------------------------------------------------------------------------

impl UnifiedHttpDriver {
    /// Send an authenticated POST request with automatic retry on rate limits.
    async fn send_request(
        &self,
        url: &str,
        body: &impl Serialize,
        extra_headers: &[(&str, &str)],
    ) -> Result<reqwest::Response, LlmError> {
        let max_retries: u8 = 3;
        for attempt in 0..=max_retries {
            let mut builder = self
                .client
                .post(url)
                .header("content-type", "application/json")
                .json(body);
            builder = self.apply_auth(builder);
            for (k, v) in extra_headers {
                builder = builder.header(*k, *v);
            }

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

            let body_text = resp.text().await.unwrap_or_default();
            let classified = classify_error(&body_text, Some(status));

            if classified.is_retryable && attempt < max_retries {
                let retry_ms = classified.suggested_delay_ms.unwrap_or_else(|| (attempt as u64 + 1) * 2000);
                warn!(status, retry_ms, "Retrying request");
                tokio::time::sleep(std::time::Duration::from_millis(retry_ms)).await;
                continue;
            }

            return Err(match classified.category {
                crate::llm_errors::LlmErrorCategory::RateLimit => LlmError::RateLimited {
                    retry_after_ms: classified.suggested_delay_ms.unwrap_or(5000),
                },
                crate::llm_errors::LlmErrorCategory::Overloaded => LlmError::Overloaded {
                    retry_after_ms: classified.suggested_delay_ms.unwrap_or(5000),
                },
                crate::llm_errors::LlmErrorCategory::Auth => LlmError::AuthenticationFailed(classified.sanitized_message),
                crate::llm_errors::LlmErrorCategory::ModelNotFound => LlmError::ModelNotFound(classified.sanitized_message),
                _ => LlmError::Api {
                    status,
                    message: crate::str_utils::safe_truncate_str(&body_text, 500).to_string(),
                },
            });
        }

        Err(LlmError::Api {
            status: 0,
            message: "Max retries exceeded".to_string(),
        })
    }

    /// Send an authenticated GET request (for task polling).
    async fn send_get(&self, url: &str) -> Result<reqwest::Response, LlmError> {
        let builder = self.client.get(url);
        let builder = self.apply_auth(builder);
        let resp = builder
            .send()
            .await
            .map_err(|e| LlmError::Http(e.to_string()))?;
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&body, 500).to_string(),
            })
        }
    }

    /// Apply authentication headers based on format.
    fn apply_auth(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let key = self.api_key.as_str();
        if key.is_empty() {
            return builder;
        }
        match self.format {
            ApiFormat::OpenAI | ApiFormat::OpenAIImages => match self.auth_header {
                AuthHeaderType::ApiKey => {
                    builder = builder.header("api-key", key);
                }
                AuthHeaderType::Bearer => {
                    builder = builder.header("authorization", format!("Bearer {key}"));
                }
            },
            ApiFormat::Anthropic => {
                builder = builder.header("x-api-key", key);
                builder = builder.header("anthropic-version", "2023-06-01");
            }
            ApiFormat::Gemini => {
                builder = builder.header("x-goog-api-key", key);
            }
            ApiFormat::Kling => {
                if let Ok(jwt) = self.generate_jwt() {
                    builder = builder.header("authorization", format!("Bearer {jwt}"));
                }
            }
            _ => {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }
        }
        builder
    }

    /// Generate JWT for Kling authentication (HMAC-SHA256).
    fn generate_jwt(&self) -> Result<String, LlmError> {
        use base64::Engine;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        type HmacSha256 = Hmac<Sha256>;

        let access_key = self.api_key.as_str();
        let secret = self.secret_key.as_ref().ok_or_else(|| {
            LlmError::Config("Kling requires secret_key for JWT".to_string())
        })?;

        let header = r#"{"alg":"HS256","typ":"JWT"}"#;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let payload = format!(
            r#"{{"iss":"{access_key}","exp":{},"nbf":{}}}"#,
            now + 1800,
            now.saturating_sub(5)
        );

        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header_b64 = engine.encode(header.as_bytes());
        let payload_b64 = engine.encode(payload.as_bytes());
        let signing_input = format!("{header_b64}.{payload_b64}");

        let mut mac = HmacSha256::new_from_slice(secret.as_str().as_bytes())
            .map_err(|e| LlmError::Config(format!("HMAC init failed: {e}")))?;
        mac.update(signing_input.as_bytes());
        let sig = mac.finalize().into_bytes();
        let sig_b64 = engine.encode(sig);

        Ok(format!("{signing_input}.{sig_b64}"))
    }

    /// Poll an async task until completion.
    async fn poll_until_complete(
        &self,
        poll_url: &str,
        check_status: impl Fn(&serde_json::Value) -> PollStatus,
    ) -> Result<serde_json::Value, LlmError> {
        let max_duration = std::time::Duration::from_secs(300);
        let interval = std::time::Duration::from_secs(5);
        let start = std::time::Instant::now();

        loop {
            tokio::time::sleep(interval).await;
            if start.elapsed() > max_duration {
                return Err(LlmError::Api {
                    status: 0,
                    message: "Task polling timed out".to_string(),
                });
            }

            let resp = self.send_get(poll_url).await?;
            let result: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| LlmError::Parse(e.to_string()))?;

            match check_status(&result) {
                PollStatus::Completed(data) => return Ok(data),
                PollStatus::Failed(msg) => {
                    return Err(LlmError::Api {
                        status: 0,
                        message: msg,
                    })
                }
                PollStatus::Pending => continue,
            }
        }
    }

    /// Extract text prompt from the last message.
    fn extract_prompt(request: &CompletionRequest) -> String {
        request
            .messages
            .last()
            .map(|m| match &m.content {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            })
            .unwrap_or_default()
    }

    /// Extract text query from the last message.
    fn extract_query(request: &CompletionRequest) -> String {
        request
            .messages
            .last()
            .map(|m| m.content.text_content())
            .unwrap_or_default()
    }
}

enum PollStatus {
    Completed(serde_json::Value),
    Failed(String),
    Pending,
}

// ---------------------------------------------------------------------------
// LlmDriver implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LlmDriver for UnifiedHttpDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        match self.format {
            ApiFormat::OpenAI => self.complete_openai(request).await,
            ApiFormat::Anthropic => self.complete_anthropic(request).await,
            ApiFormat::Gemini => self.complete_gemini(request).await,
            ApiFormat::DashScopeImage => self.complete_dashscope_image(request).await,
            ApiFormat::DashScopeTts => self.complete_dashscope_tts(request).await,
            ApiFormat::DashScopeVideo => self.complete_dashscope_video(request).await,
            ApiFormat::Kling => self.complete_kling(request).await,
            ApiFormat::MiniMaxImage => self.complete_minimax_image(request).await,
            ApiFormat::MiniMaxSearch => self.complete_minimax_search(request).await,
            ApiFormat::GlmSearch => self.complete_glm_search(request).await,
            ApiFormat::ZhipuSearch => self.complete_zhipu_search(request).await,
            ApiFormat::OpenAIImages => self.complete_openai_images(request).await,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        match self.format {
            ApiFormat::OpenAI | ApiFormat::OpenAIImages => self.stream_openai(request, tx).await,
            ApiFormat::Anthropic => self.stream_anthropic(request, tx).await,
            ApiFormat::Gemini => self.stream_gemini(request, tx).await,
            _ => {
                // Simple formats: fall back to complete() + single TextDelta
                let response = self.complete(request).await?;
                let text = response.text();
                if !text.is_empty() {
                    let _ = tx.send(StreamEvent::TextDelta { text }).await;
                }
                let _ = tx.send(StreamEvent::ContentComplete {
                    stop_reason: response.stop_reason,
                    usage: response.usage,
                }).await;
                Ok(response)
            }
        }
    }
}

// ===========================================================================
// Simple format implementations
// ===========================================================================

impl UnifiedHttpDriver {
    // --- MiniMax Search ---
    async fn complete_minimax_search(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let query = Self::extract_query(&request);
        if query.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Search query is required".to_string() });
        }

        let max = if request.max_tokens > 0 { request.max_tokens.min(20) } else { 10 };
        let body = serde_json::json!({ "q": query });

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        let mut output = format!("Search results for '{query}':\n\n");
        let mut count = 0u32;

        if let Some(organic) = result.get("organic").and_then(|o| o.as_array()) {
            for item in organic {
                if count >= max { break; }
                let title = item.get("title").and_then(|t| t.as_str()).unwrap_or("");
                let link = item.get("link").and_then(|l| l.as_str()).unwrap_or("");
                let snippet = item.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
                let date = item.get("date").and_then(|d| d.as_str()).unwrap_or("");
                if title.is_empty() && link.is_empty() { continue; }
                count += 1;
                let date_str = if date.is_empty() { String::new() } else { format!(" ({date})") };
                output.push_str(&format!("{count}. {title}\n   URL: {link}\n   {snippet}{date_str}\n\n"));
            }
        }

        if count == 0 {
            return Err(LlmError::Api { status: 200, message: format!("No results found for '{query}' (MiniMax search)") });
        }

        Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: output, provider_metadata: None }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage { input_tokens: 0, output_tokens: count as u64 },
            tool_calls: vec![],
            media: None,
        })
    }

    // --- GLM Search ---
    async fn complete_glm_search(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let query = Self::extract_query(&request);
        if query.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Search query is required".to_string() });
        }

        let count = if request.max_tokens > 0 { request.max_tokens.min(20) } else { 10 };
        let mut body = serde_json::json!({ "search_query": query, "search_engine": "search_std", "count": count });
        if let Some(recency) = request.extra.get("search_recency_filter").and_then(|v| v.as_str()) {
            body["search_recency_filter"] = serde_json::Value::String(recency.to_string());
        }

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        let mut output = format!("Search results for '{query}':\n\n");
        let mut found = 0u32;

        if let Some(results) = result.get("search_result").and_then(|r| r.as_array()) {
            for item in results {
                if found >= count { break; }
                let title = item.get("title").and_then(|t| t.as_str()).unwrap_or("");
                let link = item.get("link").and_then(|l| l.as_str()).unwrap_or("");
                let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let date = item.get("publish_date").and_then(|d| d.as_str()).unwrap_or("");
                if title.is_empty() && link.is_empty() { continue; }
                found += 1;
                let date_str = if date.is_empty() { String::new() } else { format!(" ({date})") };
                output.push_str(&format!("{found}. {title}\n   URL: {link}\n   {content}{date_str}\n\n"));
            }
        }

        if found == 0 {
            return Err(LlmError::Api { status: 200, message: format!("No results found for '{query}' (GLM search)") });
        }

        Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: output, provider_metadata: None }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage { input_tokens: 0, output_tokens: found as u64 },
            tool_calls: vec![],
            media: None,
        })
    }

    // --- Zhipu Search ---
    async fn complete_zhipu_search(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let query = Self::extract_query(&request);
        if query.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Search query is required".to_string() });
        }

        let count = if request.max_tokens > 0 { request.max_tokens.min(20) } else { 10 };
        let mut body = serde_json::json!({
            "search_query": query,
            "search_engine": "search_std",
            "count": count,
        });
        if let Some(recency) = request.extra.get("search_recency_filter").and_then(|v| v.as_str()) {
            body["search_recency_filter"] = serde_json::Value::String(recency.to_string());
        }

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        let mut output = format!("Search results for '{query}':\n\n");
        let mut found = 0u32;

        if let Some(results) = result.get("search_result").and_then(|r| r.as_array()) {
            for item in results {
                if found >= count { break; }
                let title = item.get("title").and_then(|t| t.as_str()).unwrap_or("");
                let link = item.get("link").and_then(|l| l.as_str()).unwrap_or("");
                let content = item.get("content").and_then(|c| c.as_str()).unwrap_or("");
                let date = item.get("publish_date").and_then(|d| d.as_str()).unwrap_or("");
                if title.is_empty() && link.is_empty() { continue; }
                found += 1;
                let date_str = if date.is_empty() { String::new() } else { format!(" ({date})") };
                output.push_str(&format!("{found}. {title}\n   URL: {link}\n   {content}{date_str}\n\n"));
            }
        }

        if found == 0 {
            return Err(LlmError::Api { status: 200, message: format!("No results found for '{query}' (Zhipu search)") });
        }

        Ok(CompletionResponse {
            content: vec![ContentBlock::Text { text: output, provider_metadata: None }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage { input_tokens: 0, output_tokens: found as u64 },
            tool_calls: vec![],
            media: None,
        })
    }

    // --- OpenAI Images ---
    async fn complete_openai_images(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = Self::extract_prompt(&request);
        if prompt.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Image generation requires a prompt".to_string() });
        }

        let n = request.extra.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        let mut body = serde_json::json!({ "model": request.model, "prompt": prompt, "n": n });
        if let Some(size) = request.extra.get("size").and_then(|v| v.as_str()) {
            body["size"] = serde_json::Value::String(size.to_string());
        }

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        let mut images = Vec::new();
        if let Some(data) = result.get("data").and_then(|d| d.as_array()) {
            for item in data {
                let url = item.get("url").and_then(|u| u.as_str()).map(String::from);
                let b64 = item.get("b64_json").and_then(|b| b.as_str()).unwrap_or("").to_string();
                if url.is_none() && b64.is_empty() { continue; }
                images.push(GeneratedImage { data_base64: b64, url });
            }
        }

        if images.is_empty() {
            return Err(LlmError::Parse("No images in response".to_string()));
        }

        Ok(CompletionResponse { media: Some(MediaOutput::Images { items: images }), ..Default::default() })
    }

    // --- MiniMax Image ---
    async fn complete_minimax_image(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = Self::extract_prompt(&request);
        if prompt.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Image generation requires a prompt".to_string() });
        }

        let n = request.extra.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
        let mut body = serde_json::json!({ "model": request.model, "prompt": prompt, "n": n, "response_format": "url" });
        if let Some(ar) = request.extra.get("aspect_ratio").and_then(|v| v.as_str()) {
            body["aspect_ratio"] = serde_json::Value::String(ar.to_string());
        }
        if let Some(po) = request.extra.get("prompt_optimizer").and_then(|v| v.as_bool()) {
            body["prompt_optimizer"] = serde_json::Value::Bool(po);
        }
        if let Some(seed) = request.extra.get("seed").and_then(|v| v.as_i64()) {
            body["seed"] = serde_json::Value::Number(serde_json::Number::from(seed));
        }

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        let mut images = Vec::new();
        if let Some(data) = result.get("data") {
            if let Some(urls) = data.get("image_urls").and_then(|u| u.as_array()) {
                for url_val in urls {
                    if let Some(url) = url_val.as_str() {
                        images.push(GeneratedImage { data_base64: String::new(), url: Some(url.to_string()) });
                    }
                }
            }
            if let Some(b64s) = data.get("image_base64").and_then(|b| b.as_array()) {
                for b64_val in b64s {
                    if let Some(b64) = b64_val.as_str() {
                        images.push(GeneratedImage { data_base64: b64.to_string(), url: None });
                    }
                }
            }
        }
        // Fallback: OpenAI-style data[].url / data[].b64_json
        if images.is_empty() {
            if let Some(data) = result.get("data").and_then(|d| d.as_array()) {
                for item in data {
                    let url = item.get("url").and_then(|u| u.as_str()).map(String::from);
                    let b64 = item.get("b64_json").and_then(|b| b.as_str()).unwrap_or("").to_string();
                    if url.is_none() && b64.is_empty() { continue; }
                    images.push(GeneratedImage { data_base64: b64, url });
                }
            }
        }

        if images.is_empty() {
            return Err(LlmError::Parse("No images in MiniMax response".to_string()));
        }

        Ok(CompletionResponse { media: Some(MediaOutput::Images { items: images }), ..Default::default() })
    }

    // --- DashScope Image ---
    async fn complete_dashscope_image(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = Self::extract_prompt(&request);
        if prompt.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Image generation requires a prompt".to_string() });
        }

        let size = request.extra.get("size").and_then(|v| v.as_str()).unwrap_or("1280*1280");
        let n = request.extra.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        let body = serde_json::json!({
            "model": request.model,
            "input": { "messages": [{ "role": "user", "content": [{ "text": prompt }] }] },
            "parameters": { "prompt_extend": true, "watermark": false, "n": n, "size": size }
        });

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        if let Some(code) = result.get("code").and_then(|c| c.as_str()) {
            if code != "Success" && code != "200" {
                let msg = result.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
                return Err(LlmError::Api { status: 400, message: format!("DashScope image error ({code}): {msg}") });
            }
        }

        let mut images = Vec::new();
        if let Some(results) = result.pointer("/output/results").and_then(|r| r.as_array()) {
            for item in results {
                let url = item.get("url").and_then(|u| u.as_str()).map(|s| s.to_string());
                let b64 = item.get("b64_image").and_then(|b| b.as_str()).unwrap_or("").to_string();
                images.push(GeneratedImage { data_base64: b64, url });
            }
        }

        if images.is_empty() {
            return Err(LlmError::Parse("No images in DashScope response".to_string()));
        }

        Ok(CompletionResponse { media: Some(MediaOutput::Images { items: images }), ..Default::default() })
    }

    // --- DashScope TTS ---
    async fn complete_dashscope_tts(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let text = Self::extract_query(&request);
        if text.is_empty() {
            return Err(LlmError::Api { status: 400, message: "TTS requires text input".to_string() });
        }

        let voice = request.extra.get("voice").and_then(|v| v.as_str()).unwrap_or("Cherry").to_string();
        let body = serde_json::json!({ "model": request.model, "input": { "text": text, "voice": voice } });

        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        if let Some(code) = result.get("code").and_then(|c| c.as_str()) {
            if code != "Success" && code != "200" {
                let msg = result.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
                return Err(LlmError::Api { status: 400, message: format!("DashScope TTS error ({code}): {msg}") });
            }
        }

        let audio_url = result
            .pointer("/output/audio")
            .or_else(|| result.pointer("/output/results/0/url"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| LlmError::Parse("No audio URL in DashScope TTS response".to_string()))?
            .to_string();

        // Download audio bytes
        let audio_resp = self
            .client
            .get(&audio_url)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("Audio download failed: {e}")))?;
        let data = audio_resp
            .bytes()
            .await
            .map_err(|e| LlmError::Http(format!("Audio download read failed: {e}")))?;

        let duration_ms = {
            let word_count = text.split_whitespace().count() as u64;
            (word_count * 400).max(500)
        };

        Ok(CompletionResponse {
            media: Some(MediaOutput::Audio {
                data: data.to_vec(),
                format: "mp3".to_string(),
                duration_ms,
            }),
            ..Default::default()
        })
    }

    // --- DashScope Video ---
    async fn complete_dashscope_video(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = Self::extract_prompt(&request);
        if prompt.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Video generation requires a prompt".to_string() });
        }

        let extra = &request.extra;
        let extra_input = extra.get("input").and_then(|v| v.as_object());
        let extra_params = extra.get("parameters").and_then(|v| v.as_object());
        let resolution = extra_params.and_then(|p| p.get("resolution")).and_then(|v| v.as_str()).unwrap_or("720P");
        let duration = extra_params.and_then(|p| p.get("duration")).and_then(|v| v.as_u64()).unwrap_or(5);

        let mut input = serde_json::json!({ "prompt": prompt });
        if let Some(img_url) = extra_input.and_then(|i| i.get("img_url")).and_then(|v| v.as_str()) {
            input["img_url"] = serde_json::Value::String(img_url.to_string());
        }

        let body = serde_json::json!({
            "model": request.model,
            "input": input,
            "parameters": { "resolution": resolution, "duration": duration }
        });

        // Submit async task
        let resp = self.send_request(&self.base_url, &body, &[("X-DashScope-Async", "enable")]).await?;
        let submit_result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        let task_id = submit_result
            .pointer("/output/task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LlmError::Parse("No task_id in DashScope video response".to_string()))?;

        let poll_url = format!("https://dashscope.aliyuncs.com/api/v1/tasks/{task_id}");

        let result = self.poll_until_complete(&poll_url, |v| {
            let status = v.pointer("/output/task_status").and_then(|v| v.as_str()).unwrap_or("");
            match status {
                "SUCCEEDED" => PollStatus::Completed(v.clone()),
                "FAILED" => {
                    let msg = v.pointer("/output/message").and_then(|v| v.as_str()).unwrap_or("Unknown error");
                    PollStatus::Failed(msg.to_string())
                }
                _ => PollStatus::Pending,
            }
        }).await?;

        let video_url = result
            .pointer("/output/video_url")
            .or_else(|| result.pointer("/output/results/0/url"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| LlmError::Parse("No video URL in completed task".to_string()))?
            .to_string();
        let cover_url = result.pointer("/output/cover_url").and_then(|v| v.as_str()).map(String::from);

        Ok(CompletionResponse {
            media: Some(MediaOutput::Video { url: video_url, cover_url }),
            ..Default::default()
        })
    }

    // --- Kling ---
    async fn complete_kling(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = Self::extract_prompt(&request);
        if prompt.is_empty() {
            return Err(LlmError::Api { status: 400, message: "Kling requires a prompt".to_string() });
        }

        let extra = &request.extra;
        let mut body = serde_json::json!({ "model": request.model, "prompt": prompt });
        // Merge all extra params into body
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                body[k] = v.clone();
            }
        }

        // Submit async task
        let resp = self.send_request(&self.base_url, &body, &[]).await?;
        let submit_result: serde_json::Value = resp.json().await.map_err(|e| LlmError::Parse(e.to_string()))?;

        if let Some(code) = submit_result.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                let msg = submit_result.get("message").and_then(|m| m.as_str()).unwrap_or("Unknown error");
                return Err(LlmError::Api { status: 400, message: msg.to_string() });
            }
        }

        let task_id = submit_result
            .pointer("/data/task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LlmError::Parse("No task_id in Kling response".to_string()))?;

        let poll_url = format!("{}/{task_id}", self.base_url);

        let result = self.poll_until_complete(&poll_url, |v| {
            let status = v.pointer("/data/task_status").and_then(|v| v.as_str()).unwrap_or("");
            match status {
                "succeed" => PollStatus::Completed(v.clone()),
                "failed" => {
                    let msg = v.pointer("/data/task_status_msg").and_then(|v| v.as_str()).unwrap_or("Unknown error");
                    PollStatus::Failed(msg.to_string())
                }
                _ => PollStatus::Pending,
            }
        }).await?;

        // Parse result - can be video or images
        let task_result = result.pointer("/data/task_result").and_then(|v| v.as_array());
        if let Some(items) = task_result {
            // Check for video
            if let Some(url) = items.first().and_then(|i| i.get("url")).and_then(|u| u.as_str()) {
                let cover_url = items.first().and_then(|i| i.get("cover_url")).and_then(|u| u.as_str()).map(String::from);
                return Ok(CompletionResponse {
                    media: Some(MediaOutput::Video { url: url.to_string(), cover_url }),
                    ..Default::default()
                });
            }
            // Check for images
            if let Some(images_arr) = items.first().and_then(|i| i.get("images")).and_then(|v| v.as_array()) {
                let mut images = Vec::new();
                for img in images_arr {
                    let url = img.get("url").and_then(|u| u.as_str()).map(String::from);
                    let b64 = img.get("b64_json").and_then(|b| b.as_str()).unwrap_or("").to_string();
                    images.push(GeneratedImage { data_base64: b64, url });
                }
                return Ok(CompletionResponse { media: Some(MediaOutput::Images { items: images }), ..Default::default() });
            }
        }

        Err(LlmError::Parse("No video/images in Kling task result".to_string()))
    }
}

// ===========================================================================
// OpenAI format (typed structs + complex retry)
// ===========================================================================

#[derive(Debug, Serialize)]
struct OaiRequest {
    model: String,
    messages: Vec<OaiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
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
    content: Option<String>,
    tool_calls: Option<Vec<OaiToolCall>>,
    reasoning_content: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OaiUsage { prompt_tokens: u64, completion_tokens: u64 }

fn uses_completion_tokens(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("gpt-5") || m.starts_with("gpt5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
}

fn rejects_temperature(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
        || m.starts_with("gpt-5-mini") || m.starts_with("gpt5-mini")
        || m.contains("-reasoning")
}

fn temperature_must_be_one(model: &str) -> bool {
    let m = model.to_lowercase();
    m.starts_with("kimi-k2") || m == "kimi-k2.5" || m == "kimi-k2.5-0711"
}

fn needs_reasoning_content(model: &str, base_url: &str) -> bool {
    let m = model.to_lowercase();
    base_url.contains("moonshot") || m.contains("kimi") || m.contains("reasoner") || m.contains("deepseek")
}

fn extract_reasoning_text(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(""),
        serde_json::Value::Object(obj) => obj.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        _ => String::new(),
    }
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

        let kimi = needs_reasoning_content(&request.model, &self.base_url);

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
                        reasoning_content: if kimi {
                            Some(if reasoning_text.is_empty() { String::new() } else { reasoning_text })
                        } else {
                            None
                        },
                    });
                }
                _ => {}
            }
        }

        oai_messages
    }

    fn build_oai_request(&self, request: &CompletionRequest) -> OaiRequest {
        let messages = self.build_oai_messages(request);
        let model = &request.model;

        let (max_tokens, max_completion_tokens) = if uses_completion_tokens(model) {
            (None, if request.max_tokens > 0 { Some(request.max_tokens) } else { None })
        } else {
            (if request.max_tokens > 0 { Some(request.max_tokens) } else { None }, None)
        };

        let temperature = if rejects_temperature(model) {
            None
        } else if temperature_must_be_one(model) {
            Some(1.0)
        } else if request.temperature > 0.0 {
            Some(request.temperature)
        } else {
            None
        };

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

        let reasoning_effort = None;

        OaiRequest {
            model: model.clone(),
            messages,
            max_tokens,
            max_completion_tokens,
            temperature,
            tools,
            tool_choice,
            stream: false,
            stream_options: None,
            reasoning_effort,
        }
    }

    async fn complete_openai(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut oai_request = self.build_oai_request(&request);
        let resp = self.send_openai_with_retry(&mut oai_request).await?;

        let body = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;
        let oai_response: OaiResponse = serde_json::from_str(&body).map_err(|e| LlmError::Parse(e.to_string()))?;

        let choice = oai_response.choices.into_iter().next()
            .ok_or_else(|| LlmError::Parse("No choices in response".to_string()))?;

        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(ref reasoning) = choice.message.reasoning_content {
            let text = extract_reasoning_text(reasoning);
            if !text.is_empty() {
                debug!(len = text.len(), "Captured reasoning_content from response");
                content.push(ContentBlock::Thinking { thinking: text });
            }
        }

        if let Some(text) = choice.message.content {
            if !text.is_empty() {
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
        }

        let has_text = content.iter().any(|b| matches!(b, ContentBlock::Text { .. }));
        let has_thinking = content.iter().any(|b| matches!(b, ContentBlock::Thinking { .. }));
        if has_thinking && !has_text && choice.message.tool_calls.is_none() {
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

        Ok(CompletionResponse { content, stop_reason, tool_calls, usage, media: None })
    }

    /// OpenAI-specific retry with request body mutation.
    async fn send_openai_with_retry(&self, oai_request: &mut OaiRequest) -> Result<reqwest::Response, LlmError> {
        let max_retries: u8 = 3;
        for attempt in 0..=max_retries {
            let url = self.base_url.clone();
            debug!(url = %url, attempt, "Sending OpenAI API request");

            let mut builder = self.client
                .post(&url)
                .header("content-type", "application/json")
                .json(&*oai_request);
            builder = self.apply_auth(builder);

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
                warn!(status, ?problem_tools, "Provider rejected tool arguments schema");
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

            // Groq tool_use_failed recovery
            if status == 400 && body.contains("tool_use_failed") {
                if let Some(_response) = parse_groq_failed_tool_call(&body) {
                    warn!("Recovered tool call from Groq failed_generation");
                    // Return a fake response by wrapping in OaiResponse format
                    // Actually we need to return the CompletionResponse directly — but this
                    // method returns reqwest::Response. Handle Groq recovery at the caller level.
                    // For now, just retry.
                }
                if attempt < max_retries {
                    let retry_ms = (attempt as u64 + 1) * 1500;
                    warn!(status, attempt, retry_ms, "tool_use_failed, retrying");
                    tokio::time::sleep(std::time::Duration::from_millis(retry_ms)).await;
                    continue;
                }
            }

            // Strip temperature for reasoning models
            if status == 400 && body.contains("temperature") && body.contains("unsupported_parameter")
                && oai_request.temperature.is_some() && attempt < max_retries
            {
                warn!(model = %oai_request.model, "Stripping temperature for this model");
                oai_request.temperature = None;
                continue;
            }

            // Switch max_tokens -> max_completion_tokens
            if status == 400 && body.contains("max_tokens")
                && (body.contains("unsupported_parameter") || body.contains("max_completion_tokens"))
                && oai_request.max_tokens.is_some() && attempt < max_retries
            {
                let val = oai_request.max_tokens.unwrap();
                warn!(model = %oai_request.model, "Switching to max_completion_tokens");
                oai_request.max_tokens = None;
                oai_request.max_completion_tokens = Some(val);
                continue;
            }

            // Auto-cap max_tokens
            if status == 400 && body.contains("max_tokens") && attempt < max_retries {
                let current = oai_request.max_tokens.or(oai_request.max_completion_tokens).unwrap_or(4096);
                let cap = extract_max_tokens_limit(&body).unwrap_or(current / 2);
                warn!(old = current, new = cap, "Auto-capping max_tokens to model limit");
                if oai_request.max_completion_tokens.is_some() {
                    oai_request.max_completion_tokens = Some(cap);
                } else {
                    oai_request.max_tokens = Some(cap);
                }
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

// ---------------------------------------------------------------------------
// Anthropic format
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct ApiRequest {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ApiMessage { role: String, content: ApiContent }

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum ApiContent {
    Text(String),
    Blocks(Vec<ApiContentBlock>),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ApiContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: ApiImageSource },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(rename = "tool_result")]
    ToolResult { tool_use_id: String, content: String, #[serde(skip_serializing_if = "std::ops::Not::not")] is_error: bool },
}

#[derive(Debug, Serialize)]
struct ApiImageSource { #[serde(rename = "type")] source_type: String, media_type: String, data: String }

#[derive(Debug, Serialize)]
struct ApiTool { name: String, description: String, input_schema: serde_json::Value }

#[derive(Debug, Deserialize)]
struct ApiResponse { content: Vec<ResponseContentBlock>, stop_reason: String, usage: ApiUsage }

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ResponseContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

#[derive(Debug, Deserialize)]
struct ApiUsage { input_tokens: u64, output_tokens: u64 }

fn convert_anthropic_message(msg: &Message) -> ApiMessage {
    let role = match msg.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        _ => "user",
    };

    let content = match &msg.content {
        MessageContent::Text(t) => ApiContent::Text(t.clone()),
        MessageContent::Blocks(blocks) => {
            let api_blocks: Vec<ApiContentBlock> = blocks.iter().filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(ApiContentBlock::Text { text: text.clone() }),
                ContentBlock::Image { data, media_type, .. } => Some(ApiContentBlock::Image {
                    source: ApiImageSource {
                        source_type: "base64".to_string(),
                        media_type: media_type.clone(),
                        data: data.clone(),
                    },
                }),
                ContentBlock::ToolUse { id, name, input, .. } => Some(ApiContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                }),
                ContentBlock::ToolResult { tool_use_id, content: tc, is_error, .. } => Some(ApiContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: tc.clone(),
                    is_error: *is_error,
                }),
                _ => None,
            }).collect();
            ApiContent::Blocks(api_blocks)
        }
    };

    ApiMessage { role: role.to_string(), content }
}

fn convert_anthropic_response(api: ApiResponse) -> CompletionResponse {
    let mut content = Vec::new();
    let mut tool_calls = Vec::new();

    for block in api.content {
        match block {
            ResponseContentBlock::Text { text } => {
                content.push(ContentBlock::Text { text, provider_metadata: None });
            }
            ResponseContentBlock::ToolUse { id, name, input } => {
                content.push(ContentBlock::ToolUse { id: id.clone(), name: name.clone(), input: input.clone(), provider_metadata: None });
                tool_calls.push(ToolCall { id, name, input });
            }
            ResponseContentBlock::Thinking { thinking } => {
                content.push(ContentBlock::Thinking { thinking });
            }
        }
    }

    let stop_reason = match api.stop_reason.as_str() {
        "end_turn" | "stop_sequence" => StopReason::EndTurn,
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => if !tool_calls.is_empty() { StopReason::ToolUse } else { StopReason::EndTurn },
    };

    let usage = TokenUsage {
        input_tokens: api.usage.input_tokens,
        output_tokens: api.usage.output_tokens,
    };

    CompletionResponse { content, stop_reason, tool_calls, usage, media: None }
}

impl UnifiedHttpDriver {
    async fn complete_anthropic(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let system = request.system.clone().or_else(|| {
            request.messages.iter().find_map(|m| {
                if m.role == Role::System {
                    match &m.content { MessageContent::Text(t) => Some(t.clone()), _ => None }
                } else { None }
            })
        });

        let api_messages: Vec<ApiMessage> = request.messages.iter()
            .filter(|m| m.role != Role::System)
            .map(convert_anthropic_message)
            .collect();

        let api_tools: Vec<ApiTool> = request.tools.iter().map(|t| ApiTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        }).collect();

        let api_request = ApiRequest {
            model: request.model.clone(),
            max_tokens: if request.max_tokens > 0 { request.max_tokens } else { 8192 },
            system,
            messages: api_messages,
            tools: api_tools,
            temperature: if request.temperature > 0.0 { Some(request.temperature) } else { None },
            stream: false,
        };

        let resp = self.send_request(&self.base_url, &api_request, &[]).await?;
        let body = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;

        let api_response: ApiResponse = serde_json::from_str(&body)
            .map_err(|e| LlmError::Parse(e.to_string()))?;

        Ok(convert_anthropic_response(api_response))
    }
}

// ===========================================================================
// Gemini format
// ===========================================================================

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GenerationConfig>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum GeminiPart {
    Thought { text: String, thought: bool, #[serde(skip_serializing_if = "Option::is_none")] thought_signature: Option<String> },
    Text { text: String, #[serde(skip_serializing_if = "Option::is_none")] thought_signature: Option<String> },
    InlineData { inline_data: GeminiInlineData },
    FunctionCall { function_call: GeminiFunctionCallData, #[serde(skip_serializing_if = "Option::is_none")] thought_signature: Option<String> },
    FunctionResponse { function_response: GeminiFunctionResponseData },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiInlineData { mime_type: String, data: String }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionCallData { name: String, args: serde_json::Value }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionResponseData { name: String, response: serde_json::Value }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig { function_declarations: Vec<GeminiFunctionDeclaration> }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiFunctionDeclaration { name: String, description: String, parameters: serde_json::Value }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig { #[serde(skip_serializing_if = "Option::is_none")] temperature: Option<f32>, #[serde(skip_serializing_if = "Option::is_none")] max_output_tokens: Option<u32> }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate { content: Option<GeminiResponseContent>, finish_reason: Option<String> }

#[derive(Debug, Deserialize)]
struct GeminiResponseContent { parts: Vec<serde_json::Value> }

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata { prompt_token_count: Option<u64>, candidates_token_count: Option<u64> }

impl UnifiedHttpDriver {
    fn convert_gemini_messages(messages: &[Message], system: &Option<String>) -> (Option<GeminiContent>, Vec<GeminiContent>) {
        let system_instruction = system.as_ref().filter(|s| !s.is_empty()).map(|s| GeminiContent {
            role: None,
            parts: vec![GeminiPart::Text { text: s.clone(), thought_signature: None }],
        });

        let mut contents = Vec::new();
        for msg in messages {
            if msg.role == Role::System { continue; }
            let role = match msg.role {
                Role::User => Some("user".to_string()),
                Role::Assistant => Some("model".to_string()),
                _ => None,
            };

            let parts = match &msg.content {
                MessageContent::Text(t) => vec![GeminiPart::Text { text: t.clone(), thought_signature: None }],
                MessageContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
                    ContentBlock::Text { text, provider_metadata, .. } => {
                        let sig = provider_metadata.as_ref().and_then(|m| m.get("thought_signature")).and_then(|v| v.as_str()).map(String::from);
                        Some(GeminiPart::Text { text: text.clone(), thought_signature: sig })
                    }
                    ContentBlock::Thinking { thinking } => {
                        Some(GeminiPart::Thought { text: thinking.clone(), thought: true, thought_signature: None })
                    }
                    ContentBlock::Image { data, media_type, .. } => Some(GeminiPart::InlineData {
                        inline_data: GeminiInlineData { mime_type: media_type.clone(), data: data.clone() },
                    }),
                    ContentBlock::ToolUse { id: _, name, input, provider_metadata, .. } => {
                        let sig = provider_metadata.as_ref().and_then(|m| m.get("thought_signature")).and_then(|v| v.as_str()).map(String::from);
                        let schema = types::tool::normalize_schema_for_provider(input, "gemini");
                        Some(GeminiPart::FunctionCall { function_call: GeminiFunctionCallData { name: name.clone(), args: schema }, thought_signature: sig })
                    }
                    ContentBlock::ToolResult { tool_name, content: tc, .. } => Some(GeminiPart::FunctionResponse {
                        function_response: GeminiFunctionResponseData {
                            name: tool_name.clone(),
                            response: serde_json::json!({ "result": tc }),
                        },
                    }),
                    _ => None,
                }).collect(),
            };

            if !parts.is_empty() {
                contents.push(GeminiContent { role, parts });
            }
        }

        (system_instruction, contents)
    }

    fn convert_gemini_response(resp: GeminiResponse) -> CompletionResponse {
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        if let Some(candidate) = resp.candidates.first() {
            if let Some(c) = &candidate.content {
                for part_val in &c.parts {
                    // Thought part
                    if part_val.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                        if let Some(text) = part_val.get("text").and_then(|v| v.as_str()) {
                            content.push(ContentBlock::Thinking { thinking: text.to_string() });
                        }
                    }
                    // Text part
                    else if let Some(text) = part_val.get("text").and_then(|v| v.as_str()) {
                        let sig = part_val.get("thoughtSignature").and_then(|v| v.as_str()).map(String::from);
                        let pm = sig.map(|s| serde_json::json!({ "thought_signature": s }));
                        content.push(ContentBlock::Text { text: text.to_string(), provider_metadata: pm });
                    }
                    // Function call
                    else if let Some(fc) = part_val.get("functionCall") {
                        let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let args = fc.get("args").cloned().unwrap_or(serde_json::json!({}));
                        let id = format!("call_{}", uuid::Uuid::new_v4().simple());
                        let sig = part_val.get("thoughtSignature").and_then(|v| v.as_str()).map(String::from);
                        let pm = sig.map(|s| serde_json::json!({ "thought_signature": s }));
                        content.push(ContentBlock::ToolUse { id: id.clone(), name: name.clone(), input: args.clone(), provider_metadata: pm });
                        tool_calls.push(ToolCall { id, name, input: args });
                    }
                }
            }
        }

        let has_tool_calls = !tool_calls.is_empty();
        let finish_reason = resp.candidates.first().and_then(|c| c.finish_reason.clone());
        let stop_reason = if has_tool_calls {
            StopReason::ToolUse
        } else {
            match finish_reason.as_deref() {
                Some("STOP") => StopReason::EndTurn,
                Some("MAX_TOKENS") | Some("SAFETY") => StopReason::MaxTokens,
                _ => StopReason::EndTurn,
            }
        };

        let usage = resp.usage_metadata.map(|u| TokenUsage {
            input_tokens: u.prompt_token_count.unwrap_or(0),
            output_tokens: u.candidates_token_count.unwrap_or(0),
        }).unwrap_or_default();

        CompletionResponse { content, stop_reason, tool_calls, usage, media: None }
    }

    async fn complete_gemini(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let (system_instruction, contents) = Self::convert_gemini_messages(&request.messages, &request.system);

        let tools: Vec<GeminiToolConfig> = if request.tools.is_empty() { vec![] } else {
            vec![GeminiToolConfig {
                function_declarations: request.tools.iter().map(|t| {
                    let schema = types::tool::normalize_schema_for_provider(&t.input_schema, "gemini");
                    GeminiFunctionDeclaration { name: t.name.clone(), description: t.description.clone(), parameters: schema }
                }).collect(),
            }]
        };

        let generation_config = GenerationConfig {
            temperature: if request.temperature > 0.0 { Some(request.temperature) } else { None },
            max_output_tokens: if request.max_tokens > 0 { Some(request.max_tokens) } else { None },
        };

        let gemini_request = GeminiRequest {
            contents,
            system_instruction,
            tools,
            generation_config: Some(generation_config),
        };

        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url, request.model
        );

        let resp = self.send_request(&url, &gemini_request, &[("x-goog-api-key", self.api_key.as_str())]).await?;
        let body = resp.text().await.map_err(|e| LlmError::Http(e.to_string()))?;

        let gemini_response: GeminiResponse = serde_json::from_str(&body)
            .map_err(|e| LlmError::Parse(e.to_string()))?;

        Ok(Self::convert_gemini_response(gemini_response))
    }
}

// ===========================================================================
// Shared helper functions
// ===========================================================================

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
            format!("{}...", &trimmed[..200])
        } else {
            trimmed.to_string()
        }
    } else {
        "Thinking complete.".to_string()
    }
}

/// Parse Groq `tool_use_failed` error body to recover the tool call.
fn parse_groq_failed_tool_call(body: &str) -> Option<CompletionResponse> {
    let json_body: serde_json::Value = serde_json::from_str(body).ok()?;
    let failed = json_body
        .pointer("/error/failed_generation")
        .and_then(|v| v.as_str())?;

    let mut content = Vec::new();
    let mut tool_calls = Vec::new();
    let mut remaining = failed;

    while let Some(start) = remaining.find("<function=") {
        remaining = &remaining[start + 10..];
        let end = remaining.find("</function>")?;
        let mut call_content = &remaining[..end];
        remaining = &remaining[end + 11..];

        call_content = call_content.strip_suffix('>').unwrap_or(call_content);

        let (name, args) = if let Some(brace_pos) = call_content.find('{') {
            let name = call_content[..brace_pos].trim();
            let args = &call_content[brace_pos..];
            (name, args)
        } else {
            (call_content.trim(), "{}")
        };

        let input: serde_json::Value = serde_json::from_str(args).unwrap_or_else(|_| {
            serde_json::json!({ "raw": args })
        });
        let id = format!("groq_recovered_{}", tool_calls.len());
        content.push(ContentBlock::ToolUse { id: id.clone(), name: name.to_string(), input: input.clone(), provider_metadata: None });
        tool_calls.push(ToolCall { id, name: name.to_string(), input });
    }

    if tool_calls.is_empty() {
        if !failed.trim().is_empty() {
            return Some(CompletionResponse {
                content: vec![ContentBlock::Text { text: failed.to_string(), provider_metadata: None }],
                stop_reason: StopReason::EndTurn,
                tool_calls: vec![],
                usage: TokenUsage::default(),
                media: None,
            });
        }
        return None;
    }

    Some(CompletionResponse {
        content,
        stop_reason: StopReason::ToolUse,
        tool_calls,
        usage: TokenUsage::default(),
        media: None,
    })
}

/// Extract max_tokens limit from error body.
fn extract_max_tokens_limit(body: &str) -> Option<u32> {
    // Find "max_tokens" followed by a number
    let idx = body.find("max_tokens")?;
    let after = &body[idx + "max_tokens".len()..];
    // Find first digit sequence
    let start = after.find(|c: char| c.is_ascii_digit())?;
    let digits: String = after[start..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

// ===========================================================================
// SSE Streaming implementations
// ===========================================================================

impl UnifiedHttpDriver {
    // --- OpenAI streaming ---

    async fn stream_openai(
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
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| LlmError::Http(e.to_string()))?;
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

    // --- Anthropic streaming ---

    async fn stream_anthropic(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        let system = request.system.clone().or_else(|| {
            request.messages.iter().find_map(|m| {
                if m.role == Role::System {
                    match &m.content { MessageContent::Text(t) => Some(t.clone()), _ => None }
                } else { None }
            })
        });

        let api_messages: Vec<ApiMessage> = request.messages.iter()
            .filter(|m| m.role != Role::System)
            .map(convert_anthropic_message)
            .collect();

        let api_tools: Vec<ApiTool> = request.tools.iter().map(|t| ApiTool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        }).collect();

        let api_request = ApiRequest {
            model: request.model.clone(),
            max_tokens: if request.max_tokens > 0 { request.max_tokens } else { 8192 },
            system,
            messages: api_messages,
            tools: api_tools,
            temperature: if request.temperature > 0.0 { Some(request.temperature) } else { None },
            stream: true,
        };

        let resp = self.send_request(&self.base_url, &api_request, &[]).await?;

        enum BlockAccum {
            Text(String),
            Thinking(String),
            ToolUse { id: String, name: String, input_json: String },
        }

        let mut buffer = String::new();
        let mut blocks: Vec<BlockAccum> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut stop_reason = StopReason::EndTurn;

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| LlmError::Http(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                let mut event_type = String::new();
                let mut event_data = String::new();
                for line in event_block.lines() {
                    if let Some(t) = line.strip_prefix("event:") { event_type = t.trim().to_string(); }
                    if let Some(d) = line.strip_prefix("data:") { event_data = d.trim().to_string(); }
                }
                if event_data.is_empty() { continue; }

                let json: serde_json::Value = match serde_json::from_str(&event_data) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                match event_type.as_str() {
                    "message_start" => {
                        if let Some(input) = json["message"]["usage"]["input_tokens"].as_u64() {
                            usage.input_tokens = input;
                        }
                    }
                    "content_block_start" => {
                        let idx = json["index"].as_u64().unwrap_or(0) as usize;
                        let block_type = json["content_block"]["type"].as_str().unwrap_or("");
                        match block_type {
                            "text" => {
                                while blocks.len() <= idx { blocks.push(BlockAccum::Text(String::new())); }
                                blocks[idx] = BlockAccum::Text(String::new());
                            }
                            "tool_use" => {
                                let id = json["content_block"]["id"].as_str().unwrap_or_default().to_string();
                                let name = json["content_block"]["name"].as_str().unwrap_or_default().to_string();
                                while blocks.len() <= idx { blocks.push(BlockAccum::Text(String::new())); }
                                blocks[idx] = BlockAccum::ToolUse { id: id.clone(), name: name.clone(), input_json: String::new() };
                                let _ = tx.send(StreamEvent::ToolUseStart { id, name }).await;
                            }
                            "thinking" => {
                                while blocks.len() <= idx { blocks.push(BlockAccum::Text(String::new())); }
                                blocks[idx] = BlockAccum::Thinking(String::new());
                            }
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let idx = json["index"].as_u64().unwrap_or(0) as usize;
                        let delta_type = json["delta"]["type"].as_str().unwrap_or("");
                        if idx >= blocks.len() { continue; }
                        match delta_type {
                            "text_delta" => {
                                let text = json["delta"]["text"].as_str().unwrap_or("");
                                if !text.is_empty() {
                                    if let BlockAccum::Text(ref mut s) = blocks[idx] { s.push_str(text); }
                                    let _ = tx.send(StreamEvent::TextDelta { text: text.to_string() }).await;
                                }
                            }
                            "input_json_delta" => {
                                let partial = json["delta"]["partial_json"].as_str().unwrap_or("");
                                if !partial.is_empty() {
                                    if let BlockAccum::ToolUse { ref mut input_json, .. } = blocks[idx] { input_json.push_str(partial); }
                                    let _ = tx.send(StreamEvent::ToolInputDelta { text: partial.to_string() }).await;
                                }
                            }
                            "thinking_delta" => {
                                let thinking = json["delta"]["thinking"].as_str().unwrap_or("");
                                if !thinking.is_empty() {
                                    if let BlockAccum::Thinking(ref mut s) = blocks[idx] { s.push_str(thinking); }
                                    let _ = tx.send(StreamEvent::ThinkingDelta { text: thinking.to_string() }).await;
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let idx = json["index"].as_u64().unwrap_or(0) as usize;
                        if idx < blocks.len() {
                            if let BlockAccum::ToolUse { ref id, ref name, ref input_json } = blocks[idx] {
                                let input: serde_json::Value = serde_json::from_str(input_json)
                                    .unwrap_or(serde_json::Value::Object(Default::default()));
                                let _ = tx.send(StreamEvent::ToolUseEnd {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                }).await;
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(sr) = json["delta"]["stop_reason"].as_str() {
                            stop_reason = match sr {
                                "end_turn" => StopReason::EndTurn,
                                "tool_use" => StopReason::ToolUse,
                                "max_tokens" => StopReason::MaxTokens,
                                "stop_sequence" => StopReason::StopSequence,
                                _ => StopReason::EndTurn,
                            };
                        }
                        if let Some(ot) = json["usage"]["output_tokens"].as_u64() {
                            usage.output_tokens = ot;
                        }
                    }
                    "message_stop" | "ping" => {}
                    _ => {}
                }
            }
        }

        // Build response from accumulated blocks
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();
        for block in blocks {
            match block {
                BlockAccum::Text(text) if !text.is_empty() => {
                    content.push(ContentBlock::Text { text, provider_metadata: None });
                }
                BlockAccum::Thinking(thinking) if !thinking.is_empty() => {
                    content.push(ContentBlock::Thinking { thinking });
                }
                BlockAccum::ToolUse { id, name, input_json } => {
                    let input: serde_json::Value = serde_json::from_str(&input_json)
                        .unwrap_or(serde_json::Value::Object(Default::default()));
                    content.push(ContentBlock::ToolUse { id: id.clone(), name: name.clone(), input: input.clone(), provider_metadata: None });
                    tool_calls.push(ToolCall { id, name, input });
                }
                _ => {}
            }
        }

        let response = CompletionResponse { content, stop_reason, tool_calls, usage, media: None };
        let _ = tx.send(StreamEvent::ContentComplete { stop_reason: response.stop_reason, usage: response.usage }).await;
        Ok(response)
    }

    // --- Gemini streaming ---

    async fn stream_gemini(
        &self,
        request: CompletionRequest,
        tx: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> Result<CompletionResponse, LlmError> {
        let (system_instruction, contents) = Self::convert_gemini_messages(&request.messages, &request.system);

        let tools: Vec<GeminiToolConfig> = if request.tools.is_empty() { vec![] } else {
            vec![GeminiToolConfig {
                function_declarations: request.tools.iter().map(|t| {
                    let schema = types::tool::normalize_schema_for_provider(&t.input_schema, "gemini");
                    GeminiFunctionDeclaration { name: t.name.clone(), description: t.description.clone(), parameters: schema }
                }).collect(),
            }]
        };

        let generation_config = GenerationConfig {
            temperature: if request.temperature > 0.0 { Some(request.temperature) } else { None },
            max_output_tokens: if request.max_tokens > 0 { Some(request.max_tokens) } else { None },
        };

        let gemini_request = GeminiRequest {
            contents,
            system_instruction,
            tools,
            generation_config: Some(generation_config),
        };

        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.base_url, request.model
        );

        let resp = self.send_request(&url, &gemini_request, &[("x-goog-api-key", self.api_key.as_str())]).await?;

        let mut buffer = String::new();
        let mut text_content = String::new();
        let mut fn_calls: Vec<(String, serde_json::Value, Option<String>)> = Vec::new();
        let mut usage = TokenUsage::default();
        let mut finish_reason: Option<String> = None;
        let mut thought_signature: Option<String> = None;

        let mut byte_stream = resp.bytes_stream();
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| LlmError::Http(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(pos) = buffer.find("\n\n") {
                let event_block = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                for line in event_block.lines() {
                    let data = match line.strip_prefix("data:") {
                        Some(d) => d.trim_start(),
                        None => continue,
                    };

                    let json: serde_json::Value = match serde_json::from_str(data) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Extract usage
                    if let Some(um) = json.get("usageMetadata") {
                        if let Some(pt) = um["promptTokenCount"].as_u64() { usage.input_tokens = pt; }
                        if let Some(ct) = um["candidatesTokenCount"].as_u64() { usage.output_tokens = ct; }
                    }

                    // Extract finish reason
                    if let Some(fr) = json["candidates"][0]["finishReason"].as_str() {
                        finish_reason = Some(fr.to_string());
                    }

                    // Process parts
                    let parts = match json["candidates"][0]["content"]["parts"].as_array() {
                        Some(p) => p,
                        None => continue,
                    };

                    for part in parts {
                        // Thinking part
                        if part.get("thought").and_then(|v| v.as_bool()).unwrap_or(false) {
                            if let Some(text) = part["text"].as_str() {
                                if !text.is_empty() {
                                    let _ = tx.send(StreamEvent::ThinkingDelta { text: text.to_string() }).await;
                                }
                            }
                            continue;
                        }

                        // Function call (Gemini sends complete, not incremental)
                        if let Some(fc) = part.get("functionCall") {
                            let name = fc["name"].as_str().unwrap_or_default().to_string();
                            let args = fc.get("args").cloned().unwrap_or(serde_json::Value::Object(Default::default()));
                            let sig = part["thoughtSignature"].as_str().map(|s| s.to_string());
                            if let Some(ref s) = sig { thought_signature = Some(s.clone()); }

                            let id = format!("tool_{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]);
                            let _ = tx.send(StreamEvent::ToolUseStart { id: id.clone(), name: name.clone() }).await;
                            let args_str = serde_json::to_string(&args).unwrap_or_default();
                            let _ = tx.send(StreamEvent::ToolInputDelta { text: args_str }).await;
                            let _ = tx.send(StreamEvent::ToolUseEnd { id: id.clone(), name: name.clone(), input: args.clone() }).await;
                            fn_calls.push((name, args, sig));
                            continue;
                        }

                        // Text part
                        if let Some(text) = part["text"].as_str() {
                            if !text.is_empty() {
                                text_content.push_str(text);
                                if let Some(sig) = part["thoughtSignature"].as_str() {
                                    thought_signature = Some(sig.to_string());
                                }
                                let _ = tx.send(StreamEvent::TextDelta { text: text.to_string() }).await;
                            }
                        }
                    }
                }
            }
        }

        // Build response
        let mut content = Vec::new();
        let mut tool_calls = Vec::new();

        if !text_content.is_empty() {
            let pm = thought_signature.as_ref().map(|sig| {
                serde_json::json!({ "thought_signature": sig })
            });
            content.push(ContentBlock::Text {
                text: text_content,
                provider_metadata: pm,
            });
        }

        for (name, args, sig) in &fn_calls {
            let id = format!("tool_{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]);
            let pm = sig.as_ref().map(|s| serde_json::json!({ "thought_signature": s }));
            content.push(ContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: args.clone(),
                provider_metadata: pm,
            });
            tool_calls.push(ToolCall { id, name: name.clone(), input: args.clone() });
        }

        let stop_reason = match finish_reason.as_deref() {
            Some("STOP") => StopReason::EndTurn,
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some("SAFETY") => StopReason::EndTurn,
            _ => if !fn_calls.is_empty() { StopReason::ToolUse } else { StopReason::EndTurn },
        };

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
    fn test_uses_completion_tokens() {
        assert!(uses_completion_tokens("gpt-5-turbo"));
        assert!(uses_completion_tokens("o1-preview"));
        assert!(!uses_completion_tokens("gpt-4o"));
    }

    #[test]
    fn test_rejects_temperature() {
        assert!(rejects_temperature("o3-mini"));
        assert!(!rejects_temperature("gpt-4o"));
    }

    #[test]
    fn test_temperature_must_be_one() {
        assert!(temperature_must_be_one("kimi-k2.5"));
        assert!(!temperature_must_be_one("gpt-4o"));
    }

    #[test]
    fn test_mime_to_audio_format() {
        assert_eq!(mime_to_audio_format("audio/mpeg"), "mp3");
        assert_eq!(mime_to_audio_format("audio/wav"), "wav");
        assert_eq!(mime_to_audio_format("audio/unknown"), "mp3");
    }
}
