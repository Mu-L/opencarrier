//! DashScope TTS driver — text-to-speech via Alibaba DashScope API.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmDriver, LlmError};
use async_trait::async_trait;
use types::media::MediaOutput;
use types::message::MessageContent;

/// DashScope TTS driver.
///
/// Converts `CompletionRequest` (text in last message, voice in `extra`) into
/// DashScope's multimodal-generation format and returns audio bytes.
pub struct DashScopeTtsDriver {
    api_key: String,
    base_url: String,
}

impl DashScopeTtsDriver {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { api_key, base_url }
    }
}

/// Extract text from the last message in a CompletionRequest.
fn extract_text(request: &CompletionRequest) -> String {
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
impl LlmDriver for DashScopeTtsDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let text = extract_text(&request);

        if text.is_empty() {
            return Err(LlmError::Api {
                status: 400,
                message: "TTS requires non-empty text in messages".to_string(),
            });
        }

        let voice = request
            .extra
            .get("voice")
            .and_then(|v| v.as_str())
            .unwrap_or("Cherry");

        let body = serde_json::json!({
            "model": request.model,
            "input": {
                "text": text,
                "voice": voice
            }
        });

        let client = reqwest::Client::new();
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("DashScope TTS request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response
            .json()
            .await
            .map_err(|e| LlmError::Parse(format!("Failed to parse DashScope TTS response: {e}")))?;

        if let Some(code) = result.get("code").and_then(|c| c.as_str()) {
            if code != "Success" && code != "200" {
                let msg = result
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("Unknown error");
                return Err(LlmError::Api {
                    status: 400,
                    message: format!("DashScope TTS error ({code}): {msg}"),
                });
            }
        }

        let audio_url = result
            .pointer("/output/audio")
            .and_then(|v| v.as_str())
            .or_else(|| {
                result
                    .pointer("/output/results/0/url")
                    .and_then(|v| v.as_str())
            });

        if let Some(url) = audio_url {
            let audio_resp = client
                .get(url)
                .timeout(std::time::Duration::from_secs(30))
                .send()
                .await
                .map_err(|e| LlmError::Http(format!("Failed to download TTS audio: {e}")))?;

            let audio_data = audio_resp
                .bytes()
                .await
                .map_err(|e| LlmError::Http(format!("Failed to read TTS audio: {e}")))?;

            let word_count = text.split_whitespace().count() as u64;
            let duration_ms = (word_count * 400).max(500);

            Ok(CompletionResponse {
                media: Some(MediaOutput::Audio {
                    data: audio_data.to_vec(),
                    format: "mp3".to_string(),
                    duration_ms,
                }),
                ..Default::default()
            })
        } else {
            Err(LlmError::Parse(
                "No audio URL in DashScope TTS response".to_string(),
            ))
        }
    }
}
