//! MiniMax image generation driver — supports image-01 and image-01-live models.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use carrier_types::media::{GeneratedImage, MediaOutput};
use carrier_types::message::MessageContent;

pub struct MiniMaxImageDriver {
    api_key: String,
    base_url: String,
}

impl MiniMaxImageDriver {
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
                    carrier_types::message::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""),
        })
        .unwrap_or_default()
}

#[async_trait]
impl LlmDriver for MiniMaxImageDriver {
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
            "response_format": "url",
        });

        if let Some(ar) = request.extra.get("aspect_ratio").and_then(|v| v.as_str()) {
            body["aspect_ratio"] = serde_json::Value::String(ar.to_string());
        }

        if let Some(po) = request.extra.get("prompt_optimizer").and_then(|v| v.as_bool()) {
            body["prompt_optimizer"] = serde_json::Value::Bool(po);
        }

        if let Some(seed) = request.extra.get("seed").and_then(|v| v.as_i64()) {
            body["seed"] = serde_json::Value::Number(serde_json::Number::from(seed));
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
            .map_err(|e| LlmError::Http(format!("MiniMax image generation request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response.json().await.map_err(|e| {
            LlmError::Parse(format!("Failed to parse MiniMax image generation response: {e}"))
        })?;

        let mut images = Vec::new();

        // MiniMax returns data.image_urls (when response_format=url)
        // or data.image_base64 (when response_format=base64)
        if let Some(data) = result.get("data") {
            if let Some(urls) = data.get("image_urls").and_then(|u| u.as_array()) {
                for url_val in urls {
                    if let Some(url) = url_val.as_str() {
                        images.push(GeneratedImage {
                            data_base64: String::new(),
                            url: Some(url.to_string()),
                        });
                    }
                }
            }
            if let Some(b64s) = data.get("image_base64").and_then(|b| b.as_array()) {
                for b64_val in b64s {
                    if let Some(b64) = b64_val.as_str() {
                        images.push(GeneratedImage {
                            data_base64: b64.to_string(),
                            url: None,
                        });
                    }
                }
            }
        }

        // Fallback: also check OpenAI-style data[].url/data[].b64_json
        if images.is_empty() {
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
        }

        if images.is_empty() {
            return Err(LlmError::Parse("No images in MiniMax response".to_string()));
        }

        Ok(CompletionResponse {
            media: Some(MediaOutput::Images { items: images }),
            ..Default::default()
        })
    }
}
