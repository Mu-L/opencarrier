//! Kling video/image generation driver via KlingAI API.
//!
//! Uses JWT authentication (access_key + secret_key) and async task polling.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmError, LlmDriver};
use async_trait::async_trait;
use opencarrier_types::media::MediaOutput;
use opencarrier_types::message::MessageContent;

/// Maximum time to wait for Kling generation (5 minutes).
const MAX_POLL_DURATION_SECS: u64 = 300;
/// Interval between status polls.
const POLL_INTERVAL_SECS: u64 = 5;

/// Kling driver with JWT authentication.
pub struct KlingDriver {
    access_key: String,
    secret_key: String,
    base_url: String,
}

impl KlingDriver {
    pub fn new(access_key: String, secret_key: String, base_url: String) -> Self {
        Self {
            access_key,
            secret_key,
            base_url,
        }
    }

    /// Generate JWT token for Kling API authentication.
    fn generate_jwt(&self) -> Result<String, LlmError> {
        let header = serde_json::json!({
            "alg": "HS256",
            "typ": "JWT"
        });

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let payload = serde_json::json!({
            "iss": self.access_key,
            "exp": now + 1800,
            "nbf": now - 5
        });

        let header_b64 = base64_url_encode(&serde_json::to_vec(&header).unwrap_or_default());
        let payload_b64 = base64_url_encode(&serde_json::to_vec(&payload).unwrap_or_default());

        let message = format!("{header_b64}.{payload_b64}");

        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        type HmacSha256 = Hmac<Sha256>;

        let mut mac = HmacSha256::new_from_slice(self.secret_key.as_bytes())
            .map_err(|e| LlmError::Config(format!("HMAC init failed: {e}")))?;
        mac.update(message.as_bytes());
        let sig = mac.finalize().into_bytes();
        let sig_b64 = base64_url_encode(&sig);

        Ok(format!("{message}.{sig_b64}"))
    }
}

fn base64_url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

#[async_trait]
impl LlmDriver for KlingDriver {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let prompt = request
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
            .unwrap_or_default();

        if prompt.is_empty() {
            return Err(LlmError::Api {
                status: 400,
                message: "Kling generation requires a prompt in messages".to_string(),
            });
        }

        let token = self.generate_jwt()?;

        // Build request body — merge prompt + extra params
        let mut body = serde_json::json!({
            "model": request.model,
            "prompt": prompt,
        });

        // Merge extra params (e.g., image_url, duration, etc.)
        if let Some(obj) = request.extra.as_object() {
            for (k, v) in obj {
                body[k] = v.clone();
            }
        }

        let client = reqwest::Client::new();

        // Submit task
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("Kling submit failed: {e}")))?;

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
            .map_err(|e| LlmError::Parse(format!("Failed to parse Kling response: {e}")))?;

        // Kling returns: { code: 0, data: { task_id: "...", task_status: "submitted" } }
        let code = result.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            let msg = result
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown error");
            return Err(LlmError::Api {
                status: 400,
                message: format!("Kling error ({code}): {msg}"),
            });
        }

        let task_id = result
            .pointer("/data/task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LlmError::Parse("No task_id in Kling response".to_string()))?
            .to_string();

        // Build poll URL from base_url
        // For video: base_url = .../videos/omni-video → poll at .../videos/omni-video/{task_id}
        // For image: base_url = .../images/omni-image → poll at .../images/omni-image/{task_id}
        let poll_url = format!("{}/{}", self.base_url, task_id);

        // Poll for completion
        let start = std::time::Instant::now();
        loop {
            if start.elapsed().as_secs() > MAX_POLL_DURATION_SECS {
                return Err(LlmError::Http(format!(
                    "Kling generation timed out after {MAX_POLL_DURATION_SECS}s (task_id: {task_id})"
                )));
            }

            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            // Re-generate JWT (might expire during long poll)
            let token = self.generate_jwt()?;

            let poll_resp = client
                .get(&poll_url)
                .header("Authorization", format!("Bearer {token}"))
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| LlmError::Http(format!("Kling task poll failed: {e}")))?;

            if !poll_resp.status().is_success() {
                continue;
            }

            let poll_result: serde_json::Value = poll_resp.json().await.map_err(|e| {
                LlmError::Parse(format!("Failed to parse Kling task status: {e}"))
            })?;

            let task_status = poll_result
                .pointer("/data/task_status")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN");

            match task_status {
                "succeed" => {
                    // Extract results
                    if let Some(results) =
                        poll_result
                            .pointer("/data/task_result")
                            .and_then(|r| r.as_array())
                    {
                        // Video result
                        if let Some(video) = results.first().and_then(|r| r.get("url")) {
                            let url = video.as_str().unwrap_or_default().to_string();
                            let cover_url = results
                                .first()
                                .and_then(|r| r.get("cover_url"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            return Ok(CompletionResponse {
                                media: Some(MediaOutput::Video { url, cover_url }),
                                ..Default::default()
                            });
                        }

                        // Image result
                        if let Some(images) = results
                            .first()
                            .and_then(|r| r.get("images"))
                            .and_then(|v| v.as_array())
                        {
                            let items: Vec<opencarrier_types::media::GeneratedImage> = images
                                .iter()
                                .filter_map(|img| {
                                    let url = img.get("url").and_then(|u| u.as_str()).map(|s| s.to_string());
                                    let b64 = img.get("b64_json").and_then(|b| b.as_str()).unwrap_or("").to_string();
                                    if url.is_some() || !b64.is_empty() {
                                        Some(opencarrier_types::media::GeneratedImage {
                                            data_base64: b64,
                                            url,
                                        })
                                    } else {
                                        None
                                    }
                                })
                                .collect();

                            if !items.is_empty() {
                                return Ok(CompletionResponse {
                                    media: Some(MediaOutput::Images { items }),
                                    ..Default::default()
                                });
                            }
                        }
                    }

                    return Err(LlmError::Parse(
                        "No usable result in completed Kling task".to_string(),
                    ));
                }
                "failed" => {
                    let msg = poll_result
                        .pointer("/data/task_status_msg")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error");
                    return Err(LlmError::Api {
                        status: 500,
                        message: format!("Kling generation failed: {msg}"),
                    });
                }
                _ => {
                    tracing::debug!(task_id, task_status, "Kling task still running");
                }
            }
        }
    }
}
