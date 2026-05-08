//! DashScope image generation driver — WanXiang via Alibaba DashScope API.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use opencarrier_types::media::{GeneratedImage, MediaOutput};
use opencarrier_types::message::MessageContent;

/// DashScope image generation driver.
pub struct DashScopeImageDriver {
    api_key: String,
    base_url: String,
}

impl DashScopeImageDriver {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { api_key, base_url }
    }
}

fn extract_prompt(request: &CompletionRequest) -> String {
    request
        .messages
        .last()
        .map(|m| match &m.content {
            MessageContent::Text(t) => t.clone(),
            MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    opencarrier_types::message::ContentBlock::Text { text, .. } => {
                        Some(text.as_str())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        })
        .unwrap_or_default()
}

#[async_trait]
impl LlmDriver for DashScopeImageDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = extract_prompt(&request);

        if prompt.is_empty() {
            return Err(LlmError::Api {
                status: 400,
                message: "Image generation requires a prompt in messages".to_string(),
            });
        }

        let size = request
            .extra
            .get("size")
            .and_then(|v| v.as_str())
            .unwrap_or("1280*1280");
        let n = request.extra.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        let body = serde_json::json!({
            "model": request.model,
            "input": {
                "messages": [{
                    "role": "user",
                    "content": [{ "text": prompt }]
                }]
            },
            "parameters": {
                "prompt_extend": true,
                "watermark": false,
                "n": n,
                "size": size
            }
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("DashScope image request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response.json().await.map_err(|e| {
            LlmError::Parse(format!("Failed to parse DashScope image response: {e}"))
        })?;

        if let Some(code) = result.get("code").and_then(|c| c.as_str()) {
            if code != "Success" && code != "200" {
                let msg = result
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                return Err(LlmError::Api {
                    status: 400,
                    message: format!("DashScope image error ({code}): {msg}"),
                });
            }
        }

        let mut images = Vec::new();

        if let Some(results) = result.pointer("/output/results").and_then(|r| r.as_array()) {
            for item in results {
                let url = item
                    .get("url")
                    .and_then(|u| u.as_str())
                    .map(|s| s.to_string());
                let b64 = item
                    .get("b64_image")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string();

                images.push(GeneratedImage {
                    data_base64: b64,
                    url,
                });
            }
        }

        if images.is_empty() {
            return Err(LlmError::Parse(
                "No images in DashScope response".to_string(),
            ));
        }

        Ok(CompletionResponse {
            media: Some(MediaOutput::Images { items: images }),
            ..Default::default()
        })
    }
}
