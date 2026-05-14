//! MiniMax search API driver — POST /v1/coding_plan/search
//!
//! Returns structured search results (title, link, snippet, date).

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use types::message::{ContentBlock, StopReason, TokenUsage};

pub struct MiniMaxSearchDriver {
    api_key: String,
    base_url: String,
}

impl MiniMaxSearchDriver {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { api_key, base_url }
    }
}

fn extract_query(request: &CompletionRequest) -> String {
    request
        .messages
        .last()
        .map(|m| m.content.text_content())
        .unwrap_or_default()
}

#[async_trait]
impl LlmDriver for MiniMaxSearchDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let query = extract_query(&request);
        if query.is_empty() {
            return Err(LlmError::Api {
                status: 400,
                message: "Search query is required".to_string(),
            });
        }

        let body = serde_json::json!({
            "q": query,
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("MiniMax search request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response.json().await.map_err(|e| {
            LlmError::Parse(format!("Failed to parse MiniMax search response: {e}"))
        })?;

        // MiniMax returns: { "organic": [{ "title": "...", "link": "...", "snippet": "...", "date": "..." }] }
        let mut output = format!("Search results for '{}':\n\n", query);
        let mut count = 0u32;
        let max = if request.max_tokens > 0 {
            request.max_tokens.min(20)
        } else {
            10
        };

        if let Some(organic) = result.get("organic").and_then(|o| o.as_array()) {
            for item in organic {
                if count >= max {
                    break;
                }
                let title = item
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                let link = item
                    .get("link")
                    .and_then(|l| l.as_str())
                    .unwrap_or("");
                let snippet = item
                    .get("snippet")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                let date = item
                    .get("date")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");

                if title.is_empty() && link.is_empty() {
                    continue;
                }

                count += 1;
                output.push_str(&format!(
                    "{}. {}\n   URL: {}\n   {}{}\n\n",
                    count,
                    title,
                    link,
                    snippet,
                    if date.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", date)
                    }
                ));
            }
        }

        if count == 0 {
            return Err(LlmError::Api {
                status: 200,
                message: format!("No results found for '{query}' (MiniMax search)"),
            });
        }

        Ok(CompletionResponse {
            content: vec![ContentBlock::Text {
                text: output,
                provider_metadata: None,
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 0,
                output_tokens: count as u64,
            },
            tool_calls: vec![],
            media: None,
        })
    }
}
