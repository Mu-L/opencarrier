//! OpenAI Images-compatible driver — supports Wan2.7, DALL-E, and any /v1/images/generations API.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use types::media::{GeneratedImage, MediaOutput};
use types::message::MessageContent;

pub struct OpenAIImagesDriver {
    api_key: String,
    base_url: String,
}

impl OpenAIImagesDriver {
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
                    types::message::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        })
        .unwrap_or_default()
}

#[async_trait]
impl LlmDriver for OpenAIImagesDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = extract_prompt(&request);
        if prompt.is_empty() {
            return Err(LlmError::Api {
                status: 400,
                message: "Image generation requires a prompt".to_string(),
            });
        }

        let n = request.extra.get("n").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

        let mut body = serde_json::json!({
            "model": request.model,
            "prompt": prompt,
            "n": n,
        });

        if let Some(size) = request.extra.get("size").and_then(|v| v.as_str()) {
            body["size"] = serde_json::Value::String(size.to_string());
        }

        let client = reqwest::Client::new();
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("Image generation request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response.json().await.map_err(|e| {
            LlmError::Parse(format!("Failed to parse image generation response: {e}"))
        })?;

        let mut images = Vec::new();

        if let Some(data) = result.get("data").and_then(|d| d.as_array()) {
            for item in data {
                let url = item.get("url").and_then(|u| u.as_str()).map(String::from);
                let b64 = item
                    .get("b64_json")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string();

                if url.is_none() && b64.is_empty() {
                    continue;
                }

                images.push(GeneratedImage {
                    data_base64: b64,
                    url,
                });
            }
        }

        if images.is_empty() {
            return Err(LlmError::Parse(
                "No images in response".to_string(),
            ));
        }

        Ok(CompletionResponse {
            media: Some(MediaOutput::Images { items: images }),
            ..Default::default()
        })
    }
}
