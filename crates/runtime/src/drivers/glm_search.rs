//! GLM (智谱) search API driver — POST /api/paas/v4/web_search
//!
//! Returns structured search results (title, content, link, refer, publish_date).

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use types::message::{ContentBlock, StopReason, TokenUsage};

pub struct GlmSearchDriver {
    api_key: String,
    base_url: String,
}

impl GlmSearchDriver {
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
impl LlmDriver for GlmSearchDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let query = extract_query(&request);
        if query.is_empty() {
            return Err(LlmError::Api {
                status: 400,
                message: "Search query is required".to_string(),
            });
        }

        let count = if request.max_tokens > 0 {
            request.max_tokens.min(20)
        } else {
            10
        };

        let mut body = serde_json::json!({
            "search_query": query,
            "search_engine": "search_std",
            "count": count,
        });

        if let Some(recency) = request.extra.get("search_recency_filter").and_then(|v| v.as_str())
        {
            body["search_recency_filter"] = serde_json::Value::String(recency.to_string());
        }

        let client = reqwest::Client::new();
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("GLM search request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response.json().await.map_err(|e| {
            LlmError::Parse(format!("Failed to parse GLM search response: {e}"))
        })?;

        // GLM returns: { "search_result": [{ "title": "...", "content": "...", "link": "...", "refer": "...", "publish_date": "..." }] }
        let mut output = format!("Search results for '{}':\n\n", query);
        let mut found = 0u32;

        if let Some(results) = result
            .get("search_result")
            .and_then(|r| r.as_array())
        {
            for item in results {
                if found >= count {
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
                let content = item
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let date = item
                    .get("publish_date")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");

                if title.is_empty() && link.is_empty() {
                    continue;
                }

                found += 1;
                output.push_str(&format!(
                    "{}. {}\n   URL: {}\n   {}{}\n\n",
                    found,
                    title,
                    link,
                    content,
                    if date.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", date)
                    }
                ));
            }
        }

        if found == 0 {
            return Err(LlmError::Api {
                status: 200,
                message: format!("No results found for '{query}' (GLM search)"),
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
                output_tokens: found as u64,
            },
            tool_calls: vec![],
            media: None,
        })
    }
}
