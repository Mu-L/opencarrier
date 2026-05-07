//! DashScope video generation driver — WanXiang Video via Alibaba DashScope API.
//!
//! Uses async task pattern: submit → poll until complete → return video URL.

use crate::llm_driver::{CompletionRequest, CompletionResponse, LlmError, LlmDriver};
use async_trait::async_trait;
use opencarrier_types::media::MediaOutput;
use opencarrier_types::message::MessageContent;

/// Maximum time to wait for video generation (5 minutes).
const MAX_POLL_DURATION_SECS: u64 = 300;
/// Interval between status polls.
const POLL_INTERVAL_SECS: u64 = 5;

/// DashScope video generation driver.
///
/// Submits an async video generation task and polls until completion.
/// Returns `MediaOutput::Video` with the download URL.
pub struct DashScopeVideoDriver {
    api_key: String,
    base_url: String,
}

impl DashScopeVideoDriver {
    pub fn new(api_key: String, base_url: String) -> Self {
        Self { api_key, base_url }
    }
}

#[async_trait]
impl LlmDriver for DashScopeVideoDriver {
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
                message: "Video generation requires a prompt in messages".to_string(),
            });
        }

        let extra = &request.extra;
        let resolution = extra
            .get("resolution")
            .and_then(|v| v.as_str())
            .unwrap_or("720P");
        let duration = extra
            .get("duration")
            .and_then(|v| v.as_u64())
            .unwrap_or(5);
        let img_url = extra.get("img_url").and_then(|v| v.as_str());

        let mut input = serde_json::json!({ "prompt": prompt });
        if let Some(url) = img_url {
            input["img_url"] = serde_json::json!(url);
        }

        let body = serde_json::json!({
            "model": request.model,
            "input": input,
            "parameters": {
                "resolution": resolution,
                "duration": duration
            }
        });

        let client = reqwest::Client::new();

        // Submit async task
        let response = client
            .post(&self.base_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .header("X-DashScope-Async", "enable")
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| LlmError::Http(format!("DashScope video submit failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let err = response.text().await.unwrap_or_default();
            return Err(LlmError::Api {
                status,
                message: crate::str_utils::safe_truncate_str(&err, 500).to_string(),
            });
        }

        let result: serde_json::Value = response.json().await.map_err(|e| {
            LlmError::Parse(format!("Failed to parse DashScope video submit response: {e}"))
        })?;

        let task_id = result
            .pointer("/output/task_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LlmError::Parse("No task_id in DashScope video response".to_string()))?
            .to_string();

        // Poll for completion
        let task_url = format!(
            "https://dashscope.aliyuncs.com/api/v1/tasks/{}",
            task_id
        );

        let start = std::time::Instant::now();
        loop {
            if start.elapsed().as_secs() > MAX_POLL_DURATION_SECS {
                return Err(LlmError::Http(format!(
                    "Video generation timed out after {MAX_POLL_DURATION_SECS}s (task_id: {task_id})"
                )));
            }

            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;

            let poll_resp = client
                .get(&task_url)
                .header("Authorization", format!("Bearer {}", self.api_key))
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| LlmError::Http(format!("Video task poll failed: {e}")))?;

            if !poll_resp.status().is_success() {
                continue;
            }

            let poll_result: serde_json::Value = poll_resp.json().await.map_err(|e| {
                LlmError::Parse(format!("Failed to parse video task status: {e}"))
            })?;

            let task_status = poll_result
                .pointer("/output/task_status")
                .and_then(|v| v.as_str())
                .unwrap_or("UNKNOWN");

            match task_status {
                "SUCCEEDED" => {
                    let video_url = poll_result
                        .pointer("/output/video_url")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            poll_result
                                .pointer("/output/results/0/url")
                                .and_then(|v| v.as_str())
                        })
                        .ok_or_else(|| {
                            LlmError::Parse("No video URL in completed task".to_string())
                        })?
                        .to_string();

                    let cover_url = poll_result
                        .pointer("/output/cover_url")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());

                    return Ok(CompletionResponse {
                        media: Some(MediaOutput::Video {
                            url: video_url,
                            cover_url,
                        }),
                        ..Default::default()
                    });
                }
                "FAILED" => {
                    let msg = poll_result
                        .pointer("/output/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error");
                    return Err(LlmError::Api {
                        status: 500,
                        message: format!("Video generation failed: {msg}"),
                    });
                }
                _ => {
                    // PENDING / RUNNING — keep polling
                    tracing::debug!(task_id, task_status, "Video task still running");
                }
            }
        }
    }
}
