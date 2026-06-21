//! Media, Docker, process, and canvas tool module.
//!
//! Provides image analysis, media understanding, TTS/STT, image generation,
//! Docker sandbox, persistent process management, and canvas presentation tools.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::config::ExecPolicy;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::Path;

/// Media, Docker, process, and canvas tools.
pub struct MediaTools;

#[async_trait]
impl ToolModule for MediaTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            // --- Image analysis tool ---
            ToolDefinition {
                name: "image_analyze".to_string(),
                description: "Analyze an image file — returns format, dimensions, file size, and a base64 preview. For vision-model analysis, include a prompt.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the image file" },
                        "prompt": { "type": "string", "description": "Optional prompt for vision analysis (e.g., 'Describe what you see')" }
                    },
                    "required": ["path"]
                }),
            },
            // --- Media understanding tools ---
            ToolDefinition {
                name: "media_describe".to_string(),
                description: "Describe an image using a vision-capable LLM. Auto-selects the best available provider (Anthropic, OpenAI, or Gemini). Returns a text description of the image content.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the image file (relative to workspace)" },
                        "prompt": { "type": "string", "description": "Optional prompt to guide the description (e.g., 'Extract all text from this image')" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "media_transcribe".to_string(),
                description: "Transcribe audio to text using speech-to-text. Auto-selects the best available provider (Groq Whisper or OpenAI Whisper). Returns the transcript.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the audio file (relative to workspace). Supported: mp3, wav, ogg, flac, m4a, webm." },
                        "language": { "type": "string", "description": "Optional ISO-639-1 language code (e.g., 'en', 'es', 'ja')" }
                    },
                    "required": ["path"]
                }),
            },
            // --- Image generation tool ---
            ToolDefinition {
                name: "image_generate".to_string(),
                description: "Generate images from a text prompt. Uses the configured image modality. Generated images are saved to the user's output directory. Response includes base64 data for the first image — pass it directly to upload tools.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "prompt": { "type": "string", "description": "Text description of the image to generate" },
                        "size": { "type": "string", "description": "Image size. Minimum 768x768 (589824 pixels). Common values: '1024x1024' (default), '1024x1792', '1792x1024', '768x768'. Smaller sizes will be auto-upscaled to 768x768." },
                        "count": { "type": "integer", "description": "Number of images to generate (1-4, default: 1)" },
                        "aspect_ratio": { "type": "string", "description": "Image aspect ratio: '1:1', '16:9', '4:3', '3:2', '2:3', '3:4', '9:16', '21:9'" },
                        "prompt_optimizer": { "type": "boolean", "description": "Whether to auto-optimize the prompt (default: false)" }
                    },
                    "required": ["prompt"]
                }),
            },
            // --- TTS/STT tools ---
            ToolDefinition {
                name: "text_to_speech".to_string(),
                description: "Convert text to speech audio. Auto-selects OpenAI or ElevenLabs. Saves audio to the user's output directory.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The text to convert to speech (max 4096 chars)" },
                        "voice": { "type": "string", "description": "Voice name: 'alloy', 'echo', 'fable', 'onyx', 'nova', 'shimmer' (default: 'alloy')" },
                        "format": { "type": "string", "description": "Output format: 'mp3', 'opus', 'aac', 'flac' (default: 'mp3')" }
                    },
                    "required": ["text"]
                }),
            },
            ToolDefinition {
                name: "speech_to_text".to_string(),
                description: "Transcribe audio to text using speech-to-text. Auto-selects Groq Whisper or OpenAI Whisper. Supported formats: mp3, wav, ogg, flac, m4a, webm.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Path to the audio file (relative to workspace)" },
                        "language": { "type": "string", "description": "Optional ISO-639-1 language code (e.g., 'en', 'es', 'ja')" }
                    },
                    "required": ["path"]
                }),
            },
            // --- Persistent process tools ---
            ToolDefinition {
                name: "process_start".to_string(),
                description: "Start a long-running process (REPL, server, watcher). Returns a process_id for subsequent poll/write/kill operations. Max 5 processes per agent.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "description": "The executable to run (e.g. 'python', 'node', 'npm')" },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Command-line arguments (e.g. ['-i'] for interactive Python)"
                        }
                    },
                    "required": ["command"]
                }),
            },
            ToolDefinition {
                name: "process_poll".to_string(),
                description: "Read accumulated stdout/stderr from a running process. Non-blocking: returns whatever output has buffered since the last poll.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "process_id": { "type": "string", "description": "The process ID returned by process_start" }
                    },
                    "required": ["process_id"]
                }),
            },
            ToolDefinition {
                name: "process_write".to_string(),
                description: "Write data to a running process's stdin. A newline is appended automatically if not present.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "process_id": { "type": "string", "description": "The process ID returned by process_start" },
                        "data": { "type": "string", "description": "The data to write to stdin" }
                    },
                    "required": ["process_id", "data"]
                }),
            },
            ToolDefinition {
                name: "process_kill".to_string(),
                description: "Terminate a running process and clean up its resources.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "process_id": { "type": "string", "description": "The process ID returned by process_start" }
                    },
                    "required": ["process_id"]
                }),
            },
            ToolDefinition {
                name: "process_list".to_string(),
                description: "List all running processes for the current agent, including their IDs, commands, uptime, and alive status.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            // --- Canvas / A2UI tool ---
            ToolDefinition {
                name: "canvas_present".to_string(),
                description: "Present an interactive HTML canvas to the user. The HTML is sanitized (no scripts, no event handlers) and saved to the workspace. The dashboard will render it in a panel. Use for rich data visualizations, formatted reports, or interactive UI.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "html": { "type": "string", "description": "The HTML content to present. Must not contain <script> tags, event handlers, or javascript: URLs." },
                        "title": { "type": "string", "description": "Optional title for the canvas panel" }
                    },
                    "required": ["html"]
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        match name {
            // Image analysis
            "image_analyze" => Some(tool_image_analyze(input).await),

            // Media understanding
            "media_describe" => Some(tool_media_describe(input, ctx.brain).await),
            "media_transcribe" => Some(tool_media_transcribe(input, ctx.brain).await),

            // Image generation
            "image_generate" => {
                Some(tool_image_generate(input, ctx.brain, ctx.home_dir, ctx.agent_name, ctx.owner_id, ctx.sender_id).await)
            }

            // TTS/STT
            "text_to_speech" => {
                Some(tool_text_to_speech(input, ctx.brain, ctx.home_dir, ctx.agent_name, ctx.owner_id, ctx.sender_id).await)
            }
            "speech_to_text" => {
                Some(tool_speech_to_text(input, ctx.brain, ctx.workspace_root).await)
            }

            // Persistent process tools
            "process_start" => {
                Some(tool_process_start(
                    input,
                    ctx.process_manager,
                    ctx.caller_agent_id,
                    ctx.exec_policy,
                    ctx.allowed_env_vars,
                ).await)
            }
            "process_poll" => {
                Some(tool_process_poll(input, ctx.process_manager, ctx.caller_agent_id).await)
            }
            "process_write" => {
                Some(tool_process_write(input, ctx.process_manager, ctx.caller_agent_id).await)
            }
            "process_kill" => {
                Some(tool_process_kill(input, ctx.process_manager, ctx.caller_agent_id).await)
            }
            "process_list" => {
                Some(tool_process_list(ctx.process_manager, ctx.caller_agent_id).await)
            }

            // Canvas / A2UI
            "canvas_present" => {
                Some(tool_canvas_present(input, ctx.workspace_root, ctx.home_dir, ctx.agent_name, ctx.owner_id, ctx.sender_id).await)
            }

            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> types::tool::PermissionLevel {
        match tool_name {
            "image_analyze" | "media_describe" | "media_transcribe"
            | "speech_to_text" => types::tool::PermissionLevel::ReadOnly,
            "image_generate" | "text_to_speech" | "canvas_present" => types::tool::PermissionLevel::Write,
            "process_start" | "process_poll"
            | "process_write" | "process_list" => types::tool::PermissionLevel::Execute,
            "process_kill" => types::tool::PermissionLevel::Dangerous,
            _ => types::tool::PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Image analysis tool
// ---------------------------------------------------------------------------

async fn tool_image_analyze(input: &serde_json::Value) -> Result<String, String> {
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    let prompt = input["prompt"].as_str().unwrap_or("");

    let data = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Failed to read image '{path}': {e}"))?;

    let file_size = data.len();

    // Detect image format from magic bytes
    let format = detect_image_format(&data);

    // Extract dimensions for common formats
    let dimensions = extract_image_dimensions(&data, &format);

    // Base64-encode (truncate for very large images in the response)
    let base64_preview = if file_size <= 512 * 1024 {
        // Under 512KB — include full base64
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(&data)
    } else {
        // Over 512KB — include first 64KB preview
        use base64::Engine;
        let preview_bytes = &data[..64 * 1024];
        format!(
            "{}... [truncated, {} total bytes]",
            base64::engine::general_purpose::STANDARD.encode(preview_bytes),
            file_size
        )
    };

    let mut result = serde_json::json!({
        "path": path,
        "format": format,
        "file_size_bytes": file_size,
        "file_size_human": format_file_size(file_size),
    });

    if let Some((w, h)) = dimensions {
        result["width"] = serde_json::json!(w);
        result["height"] = serde_json::json!(h);
    }

    if !prompt.is_empty() {
        result["prompt"] = serde_json::json!(prompt);
        result["note"] = serde_json::json!(
            "Vision analysis requires a vision-capable LLM. The base64 data is included for downstream processing."
        );
    }

    result["base64_preview"] = serde_json::json!(base64_preview);

    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

/// Detect image format from magic bytes.
fn detect_image_format(data: &[u8]) -> String {
    if data.len() < 4 {
        return "unknown".to_string();
    }
    if data.starts_with(b"\x89PNG") {
        "png".to_string()
    } else if data.starts_with(b"\xFF\xD8\xFF") {
        "jpeg".to_string()
    } else if data.starts_with(b"GIF8") {
        "gif".to_string()
    } else if data.starts_with(b"RIFF") && data.len() > 12 && &data[8..12] == b"WEBP" {
        "webp".to_string()
    } else if data.starts_with(b"BM") {
        "bmp".to_string()
    } else if data.starts_with(b"\x00\x00\x01\x00") {
        "ico".to_string()
    } else {
        "unknown".to_string()
    }
}

/// Extract image dimensions from common formats.
fn extract_image_dimensions(data: &[u8], format: &str) -> Option<(u32, u32)> {
    match format {
        "png" => {
            // PNG: IHDR chunk starts at byte 16, width at 16-19, height at 20-23
            if data.len() >= 24 {
                let w = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
                let h = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
                Some((w, h))
            } else {
                None
            }
        }
        "gif" => {
            // GIF: width at bytes 6-7, height at bytes 8-9 (little-endian)
            if data.len() >= 10 {
                let w = u16::from_le_bytes([data[6], data[7]]) as u32;
                let h = u16::from_le_bytes([data[8], data[9]]) as u32;
                Some((w, h))
            } else {
                None
            }
        }
        "bmp" => {
            // BMP: width at bytes 18-21, height at bytes 22-25 (little-endian)
            if data.len() >= 26 {
                let w = u32::from_le_bytes([data[18], data[19], data[20], data[21]]);
                let h = u32::from_le_bytes([data[22], data[23], data[24], data[25]]);
                Some((w, h))
            } else {
                None
            }
        }
        "jpeg" => {
            // JPEG: scan for SOF0 marker (0xFF 0xC0) to find dimensions
            extract_jpeg_dimensions(data)
        }
        _ => None,
    }
}

/// Extract JPEG dimensions by scanning for SOF markers.
fn extract_jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2; // Skip SOI marker
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        // SOF0-SOF3 markers contain dimensions
        if (0xC0..=0xC3).contains(&marker) && i + 9 < data.len() {
            let h = u16::from_be_bytes([data[i + 5], data[i + 6]]) as u32;
            let w = u16::from_be_bytes([data[i + 7], data[i + 8]]) as u32;
            return Some((w, h));
        }
        if i + 3 < data.len() {
            let seg_len = u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize;
            i += 2 + seg_len;
        } else {
            break;
        }
    }
    None
}

/// Format file size in human-readable form.
fn format_file_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

// ---------------------------------------------------------------------------
// Media understanding tools
// ---------------------------------------------------------------------------

/// Describe an image using a vision-capable LLM through the Brain fallback chain.
async fn tool_media_describe(
    input: &serde_json::Value,
    brain: Option<&std::sync::Arc<dyn crate::llm_driver::Brain>>,
) -> Result<String, String> {
    use base64::Engine;
    let brain = brain.ok_or("Brain not available. Check configuration.")?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    let prompt = input["prompt"]
        .as_str()
        .unwrap_or("Describe this image in detail.");
    // Allow /tmp/ paths for browser screenshots; validate relative paths normally
    if !path.starts_with("/tmp/") {
        let _ = crate::tools::validate_path(path)?;
    }

    // Read image file
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Failed to read image file: {e}"))?;

    // Detect MIME type from extension
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => return Err(format!("Unsupported image format: .{ext}")),
    };

    // Validate image size
    let max_bytes = 5 * 1024 * 1024; // 5 MB
    if data.len() > max_bytes {
        return Err(format!(
            "Image too large: {} bytes (max {} MB)",
            data.len(),
            max_bytes / (1024 * 1024)
        ));
    }

    let base64_data = base64::engine::general_purpose::STANDARD.encode(&data);

    // Build a CompletionRequest with the image for the vision modality
    let request = crate::llm_driver::CompletionRequest {
        model: String::new(), // brain sets this from the resolved endpoint
        messages: vec![types::message::Message {
            role: types::message::Role::User,
            content: types::message::MessageContent::Blocks(vec![
                types::message::ContentBlock::Image {
                    media_type: mime.to_string(),
                    data: base64_data,
                },
                types::message::ContentBlock::Text {
                    text: prompt.to_string(),
                    provider_metadata: None,
                },
            ]),
        }],
        tools: vec![],
        max_tokens: 1024,
        temperature: 0.3,
        system: None,
        thinking: None,
        extra: Default::default(),
    };

    let response = brain
        .complete("vision", request)
        .await
        .map_err(|e| format!("Vision LLM call failed: {e}"))?;

    let description = response.text();
    if description.is_empty() {
        return Err("Vision model returned empty response".into());
    }

    let result = serde_json::json!({
        "description": description,
        "usage": {
            "input_tokens": response.usage.input_tokens,
            "output_tokens": response.usage.output_tokens,
        },
    });
    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

/// Transcribe audio to text via the Brain's audio modality.
async fn tool_media_transcribe(
    input: &serde_json::Value,
    brain: Option<&std::sync::Arc<dyn crate::llm_driver::Brain>>,
) -> Result<String, String> {
    use base64::Engine;
    let brain = brain.ok_or("Brain not available. Ensure audio modality is configured.")?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    // Allow /tmp/ paths for browser screenshots; validate relative paths normally
    if !path.starts_with("/tmp/") {
        let _ = crate::tools::validate_path(path)?;
    }

    // Read audio file
    let data = tokio::fs::read(path)
        .await
        .map_err(|e| format!("Failed to read audio file: {e}"))?;

    // Detect MIME type from extension
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    let mime = match ext.as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        _ => return Err(format!("Unsupported audio format: .{ext}")),
    };

    let audio_block = types::message::ContentBlock::Audio {
        media_type: mime.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&data),
    };

    let request = crate::llm_driver::CompletionRequest {
        model: String::new(),
        messages: vec![types::message::Message {
            role: types::message::Role::User,
            content: types::message::MessageContent::Blocks(vec![audio_block]),
        }],
        tools: vec![],
        max_tokens: 4096,
        temperature: 0.0,
        system: None,
        thinking: None,
        extra: serde_json::Value::Object(serde_json::Map::new()),
    };

    let response = brain
        .complete("audio", request)
        .await
        .map_err(|e| format!("Audio transcription brain call failed: {e}"))?;

    let transcript = response.text();
    let result = serde_json::json!({
        "transcript": transcript,
        "provider": "brain",
    });
    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

// ---------------------------------------------------------------------------
// Image generation tool
// ---------------------------------------------------------------------------

/// Generate images from a text prompt via the Brain's image modality.
async fn tool_image_generate(
    input: &serde_json::Value,
    brain: Option<&std::sync::Arc<dyn crate::llm_driver::Brain>>,
    home_dir: Option<&Path>,
    agent_name: Option<&str>,
    _owner_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let brain = brain.ok_or("Brain not available. Ensure image modality is configured.")?;
    let prompt = input["prompt"]
        .as_str()
        .ok_or("Missing 'prompt' parameter")?;

    let model = input["model"].as_str().unwrap_or("dall-e-3");
    let mut size = input["size"].as_str().unwrap_or("1024x1024").to_string();

    // Enforce minimum pixel count for providers that require it (e.g. DashScope: 589824 = 768x768)
    const MIN_PIXELS: u32 = 589824;
    if let Some((w, h)) = size.split_once('x').and_then(|(w, h)| {
        let w = w.parse::<u32>().ok()?;
        let h = h.parse::<u32>().ok()?;
        Some((w, h))
    }) {
        if w.saturating_mul(h) < MIN_PIXELS {
            tracing::warn!(requested = %size, "Image size below provider minimum; upscaling to 1024x1024");
            size = "1024x1024".to_string();
        }
    }

    let quality = input["quality"].as_str().unwrap_or("hd");
    let count = input["count"].as_u64().unwrap_or(1).min(4) as u8;
    let include_base64 = input["include_base64"].as_bool().unwrap_or(false);

    let mut extra = serde_json::Map::new();
    extra.insert("model".to_string(), serde_json::json!(model));
    extra.insert("size".to_string(), serde_json::json!(size));
    extra.insert("quality".to_string(), serde_json::json!(quality));
    extra.insert("n".to_string(), serde_json::json!(count));
    if let Some(ar) = input["aspect_ratio"].as_str() {
        extra.insert("aspect_ratio".to_string(), serde_json::json!(ar));
    }
    if let Some(po) = input["prompt_optimizer"].as_bool() {
        extra.insert("prompt_optimizer".to_string(), serde_json::json!(po));
    }

    let request = crate::llm_driver::CompletionRequest {
        model: String::new(),
        messages: vec![types::message::Message {
            role: types::message::Role::User,
            content: types::message::MessageContent::Text(prompt.to_string()),
        }],
        tools: vec![],
        max_tokens: 0,
        temperature: 0.0,
        system: None,
        thinking: None,
        extra: serde_json::Value::Object(extra),
    };

    let response = brain
        .complete("image", request)
        .await
        .map_err(|e| {
            format!(
                "Image generation failed: {e}. \
                 Do NOT retry image_generate with the same prompt. \
                 Tell the user the image generation service is currently unavailable \
                 and suggest trying again later."
            )
        })?;

    let images = match response.media {
        Some(types::media::MediaOutput::Images { items }) => items,
        Some(types::media::MediaOutput::Image { data, format: _fmt }) => {
            vec![types::media::GeneratedImage {
                data_base64: {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.encode(&data)
                },
                url: None,
            }]
        }
        _ => return Err("Image generation returned no images".into()),
    };

    if images.is_empty() {
        return Err("Image generation returned empty image list".into());
    }

    // Save images to workspace output directory if available
    let saved_paths = if let (Some(hd), Some(an)) = (home_dir, agent_name) {
        // Use workspaces/{agent_name}/{sender}/output for per-sender isolation
        let sid = sender_id.unwrap_or("shared");
        let output_dir = types::config::sender_data_dir(hd, sid, an, None).join("output");
        tokio::fs::create_dir_all(&output_dir)
            .await
            .map_err(|e| format!("Failed to create output dir: {e}"))?;

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let mut paths = Vec::new();

        for (i, image) in images.iter().enumerate() {
            let filename = if images.len() == 1 {
                format!("image_{timestamp}.png")
            } else {
                format!("image_{timestamp}_{i}.png")
            };
            let path = output_dir.join(&filename);

            let decoded = if !image.data_base64.is_empty() {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(&image.data_base64)
                    .map_err(|e| format!("Failed to decode base64 image: {e}"))?
            } else if let Some(ref url) = image.url {
                // Download from URL (e.g. MiniMax returns temporary URLs)
                let resp = reqwest::Client::new()
                    .get(url)
                    .timeout(std::time::Duration::from_secs(60))
                    .send()
                    .await
                    .map_err(|e| format!("Failed to download image from URL: {e}"))?;
                resp.bytes()
                    .await
                    .map_err(|e| format!("Failed to read image response: {e}"))?
                    .to_vec()
            } else {
                return Err("Image has neither base64 data nor URL".into());
            };

            tokio::fs::write(&path, &decoded)
                .await
                .map_err(|e| format!("Failed to write image: {e}"))?;

            paths.push(output_dir.join(&filename).to_string_lossy().to_string());
        }
        paths
    } else {
        Vec::new()
    };

    // Also save to the uploads temp dir so the web UI can serve them via
    // GET /api/uploads/{file_id}. Each image gets a UUID filename.
    let mut image_urls: Vec<String> = Vec::new();
    let mut temp_paths: Vec<String> = Vec::new();
    {
        let upload_dir = std::env::temp_dir().join("carrier_uploads");
        let _ = std::fs::create_dir_all(&upload_dir);
        for image in &images {
            let file_id = uuid::Uuid::new_v4().to_string();
            let decoded = if !image.data_base64.is_empty() {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(&image.data_base64).ok()
            } else {
                None
            };
            // For URL-only images, they can be accessed directly — skip local upload
            if let Some(decoded) = decoded {
                let path = upload_dir.join(&file_id);
                if std::fs::write(&path, &decoded).is_ok() {
                    image_urls.push(format!("/api/uploads/{file_id}"));
                    // Return actual file path for MCP tools that need direct file access
                    temp_paths.push(path.to_string_lossy().to_string());
                }
            } else if let Some(ref url) = image.url {
                image_urls.push(url.clone());
            }
        }
    }

    // Include base64 of the first image so downstream tools (e.g. upload) can use it directly.
    // For URL-only providers (DashScope), download and encode to base64 on the fly.
    let base64_data = if let Some(first) = images.first() {
        if !first.data_base64.is_empty() {
            first.data_base64.clone()
        } else if let Some(ref url) = first.url {
            match reqwest::Client::new()
                .get(url)
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
            {
                Ok(resp) => match resp.bytes().await {
                    Ok(bytes) => {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Failed to read image bytes for base64 encoding");
                        String::new()
                    }
                },
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to download image for base64 encoding");
                    String::new()
                }
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };

    // Build response - only include base64 when explicitly requested
    // to avoid response truncation (25500 char limit)
    let mut response = serde_json::Map::new();
    response.insert("images_generated".into(), serde_json::json!(images.len()));
    response.insert("saved_to".into(), serde_json::json!(saved_paths));
    response.insert("image_urls".into(), serde_json::json!(image_urls));
    response.insert("temp_paths".into(), serde_json::json!(temp_paths));
    response.insert("provider".into(), serde_json::json!("brain"));

    // Only include base64 if explicitly requested (for small images or debugging)
    if include_base64 {
        response.insert("base64".into(), serde_json::json!(base64_data));
    } else {
        response.insert("base64".into(), serde_json::json!(null));
        response.insert("base64_truncated".into(), serde_json::json!(true));
        response.insert("note".into(), serde_json::json!("base64 omitted to avoid response truncation. Use include_base64=true if needed, or use saved_to/temp_paths paths directly."));
    }

    serde_json::to_string_pretty(&response).map_err(|e| format!("Serialize error: {e}"))
}

// ---------------------------------------------------------------------------
// TTS / STT tools
// ---------------------------------------------------------------------------

async fn tool_text_to_speech(
    input: &serde_json::Value,
    brain: Option<&std::sync::Arc<dyn crate::llm_driver::Brain>>,
    home_dir: Option<&Path>,
    agent_name: Option<&str>,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let brain = brain.ok_or("Brain not available. Ensure tts modality is configured.")?;
    let text = input["text"].as_str().ok_or("Missing 'text' parameter")?;
    let voice = input["voice"].as_str();
    let format = input["format"].as_str();

    let mut extra = serde_json::Map::new();
    if let Some(v) = voice {
        extra.insert("voice".to_string(), serde_json::json!(v));
    }
    if let Some(f) = format {
        extra.insert("format".to_string(), serde_json::json!(f));
    }

    let request = crate::llm_driver::CompletionRequest {
        model: String::new(),
        messages: vec![types::message::Message {
            role: types::message::Role::User,
            content: types::message::MessageContent::Text(text.to_string()),
        }],
        tools: vec![],
        max_tokens: 0,
        temperature: 0.0,
        system: None,
        thinking: None,
        extra: serde_json::Value::Object(extra),
    };

    let response = brain
        .complete("tts", request)
        .await
        .map_err(|e| format!("TTS brain call failed: {e}"))?;

    let media = response.media.ok_or("TTS returned no media")?;
    let (audio_data, format, duration_ms) = match media {
        types::media::MediaOutput::Audio {
            data,
            format,
            duration_ms,
        } => (data, format, duration_ms),
        _ => return Err("TTS returned non-audio media".into()),
    };

    // Save audio to per-sender output directory
    let saved_path = if let (Some(hd), Some(an)) = (home_dir, agent_name) {
        let sid = sender_id.ok_or("Cannot save audio: no sender context")?;
        let oid = owner_id.unwrap_or(sid);
        let rel_dir = types::config::sender_relative_path(oid, an, Some(sid), "output");
        let output_dir = hd.join(&rel_dir);
        tokio::fs::create_dir_all(&output_dir)
            .await
            .map_err(|e| format!("Failed to create output dir: {e}"))?;

        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let filename = format!("tts_{timestamp}.{format}");
        let path = output_dir.join(&filename);

        tokio::fs::write(&path, &audio_data)
            .await
            .map_err(|e| format!("Failed to write audio file: {e}"))?;

        Some(format!("{}/{}", rel_dir, filename))
    } else {
        None
    };

    let response = serde_json::json!({
        "saved_to": saved_path,
        "format": format,
        "provider": "brain",
        "duration_estimate_ms": duration_ms,
        "size_bytes": audio_data.len(),
    });

    serde_json::to_string_pretty(&response).map_err(|e| format!("Serialize error: {e}"))
}

async fn tool_speech_to_text(
    input: &serde_json::Value,
    brain: Option<&std::sync::Arc<dyn crate::llm_driver::Brain>>,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    use base64::Engine;
    let brain = brain.ok_or("Brain not available. Ensure audio modality is configured.")?;
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    let language = input["language"].as_str();

    let resolved = crate::tools::resolve_file_path(raw_path, workspace_root)?;

    // Read the audio file
    let data = tokio::fs::read(&resolved)
        .await
        .map_err(|e| format!("Failed to read audio file: {e}"))?;

    // Determine MIME type from extension
    let ext = resolved
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("mp3");
    let mime_type = match ext {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "webm" => "audio/webm",
        _ => "audio/mpeg",
    };

    let audio_block = types::message::ContentBlock::Audio {
        media_type: mime_type.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(&data),
    };

    let mut extra = serde_json::Map::new();
    if let Some(lang) = language {
        extra.insert("language".to_string(), serde_json::json!(lang));
    }

    let request = crate::llm_driver::CompletionRequest {
        model: String::new(),
        messages: vec![types::message::Message {
            role: types::message::Role::User,
            content: types::message::MessageContent::Blocks(vec![audio_block]),
        }],
        tools: vec![],
        max_tokens: 4096,
        temperature: 0.0,
        system: None,
        thinking: None,
        extra: serde_json::Value::Object(extra),
    };

    let response = brain
        .complete("audio", request)
        .await
        .map_err(|e| format!("Speech-to-text brain call failed: {e}"))?;

    let transcript = response.text();
    let result = serde_json::json!({
        "transcript": transcript,
        "provider": "brain",
    });

    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

// ---------------------------------------------------------------------------
// Docker sandbox tool
// ---------------------------------------------------------------------------
// Persistent process tools
// ---------------------------------------------------------------------------

/// Start a long-running process (REPL, server, watcher).
async fn tool_process_start(
    input: &serde_json::Value,
    pm: Option<&crate::process_manager::ProcessManager>,
    caller_agent_id: Option<&str>,
    exec_policy: Option<&ExecPolicy>,
    allowed_env_vars: Option<&[String]>,
) -> Result<String, String> {
    let pm = pm.ok_or("Process manager not available")?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    let command = input["command"]
        .as_str()
        .ok_or("Missing 'command' parameter")?;
    let args: Vec<String> = input["args"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let proc_id = pm
        .start(agent_id, command, &args, exec_policy, allowed_env_vars)
        .await?;
    Ok(serde_json::json!({
        "process_id": proc_id,
        "status": "started"
    })
    .to_string())
}

/// Read accumulated stdout/stderr from a process (non-blocking drain).
async fn tool_process_poll(
    input: &serde_json::Value,
    pm: Option<&crate::process_manager::ProcessManager>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let pm = pm.ok_or("Process manager not available")?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    let proc_id = input["process_id"]
        .as_str()
        .ok_or("Missing 'process_id' parameter")?;
    // Ownership: verify the process belongs to the caller
    if !pm.list(agent_id).iter().any(|p| p.id == proc_id) {
        return Err("Process not found or does not belong to you".to_string());
    }
    let (stdout, stderr) = pm.read(proc_id).await?;
    Ok(serde_json::json!({
        "stdout": stdout,
        "stderr": stderr,
    })
    .to_string())
}

/// Write data to a process's stdin.
async fn tool_process_write(
    input: &serde_json::Value,
    pm: Option<&crate::process_manager::ProcessManager>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let pm = pm.ok_or("Process manager not available")?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    let proc_id = input["process_id"]
        .as_str()
        .ok_or("Missing 'process_id' parameter")?;
    // Ownership: verify the process belongs to the caller
    if !pm.list(agent_id).iter().any(|p| p.id == proc_id) {
        return Err("Process not found or does not belong to you".to_string());
    }
    let data = input["data"].as_str().ok_or("Missing 'data' parameter")?;
    // Always append newline if not present (common expectation for REPLs)
    let data = if data.ends_with('\n') {
        data.to_string()
    } else {
        format!("{data}\n")
    };
    pm.write(proc_id, &data).await?;
    Ok(r#"{"status": "written"}"#.to_string())
}

/// Terminate a process.
async fn tool_process_kill(
    input: &serde_json::Value,
    pm: Option<&crate::process_manager::ProcessManager>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let pm = pm.ok_or("Process manager not available")?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    let proc_id = input["process_id"]
        .as_str()
        .ok_or("Missing 'process_id' parameter")?;
    // Ownership: verify the process belongs to the caller
    if !pm.list(agent_id).iter().any(|p| p.id == proc_id) {
        return Err("Process not found or does not belong to you".to_string());
    }
    pm.kill(proc_id).await?;
    Ok(r#"{"status": "killed"}"#.to_string())
}

/// List processes for the current agent.
async fn tool_process_list(
    pm: Option<&crate::process_manager::ProcessManager>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let pm = pm.ok_or("Process manager not available")?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    let procs = pm.list(agent_id);
    let list: Vec<serde_json::Value> = procs
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "command": p.command,
                "alive": p.alive,
                "uptime_secs": p.uptime_secs,
            })
        })
        .collect();
    Ok(serde_json::Value::Array(list).to_string())
}

// ---------------------------------------------------------------------------
// Canvas / A2UI tool
// ---------------------------------------------------------------------------

/// Sanitize HTML for canvas presentation.
///
/// SECURITY: Strips dangerous elements and attributes to prevent XSS:
/// - Rejects <script>, <iframe>, <object>, <embed>, <applet> tags
/// - Strips all on* event attributes (onclick, onload, onerror, etc.)
/// - Strips javascript:, data:text/html, vbscript: URLs
/// - Enforces size limit
fn sanitize_canvas_html(html: &str, max_bytes: usize) -> Result<String, String> {
    if html.is_empty() {
        return Err("Empty HTML content".to_string());
    }
    if html.len() > max_bytes {
        return Err(format!(
            "HTML too large: {} bytes (max {})",
            html.len(),
            max_bytes
        ));
    }

    let lower = html.to_lowercase();

    // Reject dangerous tags
    let dangerous_tags = [
        "<script", "</script", "<iframe", "</iframe", "<object", "</object", "<embed", "<applet",
        "</applet",
    ];
    for tag in &dangerous_tags {
        if lower.contains(tag) {
            return Err(format!("Forbidden HTML tag detected: {tag}"));
        }
    }

    // Reject event handler attributes (on*)
    // Match patterns like: onclick=, onload=, onerror=, onmouseover=, etc.
    static EVENT_PATTERN: std::sync::LazyLock<regex_lite::Regex> =
        std::sync::LazyLock::new(|| regex_lite::Regex::new(r"(?i)\bon[a-z]+\s*=").unwrap());
    if EVENT_PATTERN.is_match(html) {
        return Err(
            "Forbidden event handler attribute detected (on* attributes are not allowed)"
                .to_string(),
        );
    }

    // Reject dangerous URL schemes
    let dangerous_schemes = ["javascript:", "vbscript:", "data:text/html"];
    for scheme in &dangerous_schemes {
        if lower.contains(scheme) {
            return Err(format!("Forbidden URL scheme detected: {scheme}"));
        }
    }

    Ok(html.to_string())
}

/// Canvas presentation tool handler.
async fn tool_canvas_present(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
    home_dir: Option<&Path>,
    agent_name: Option<&str>,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let html = input["html"].as_str().ok_or("Missing 'html' parameter")?;
    let title = input["title"].as_str().unwrap_or("Canvas");

    // Use configured max from task-local (set by agent_loop from KernelConfig), or default 512KB.
    let max_bytes = crate::tool_runner::CANVAS_MAX_BYTES
        .try_with(|v| *v)
        .unwrap_or(512 * 1024);
    let sanitized = sanitize_canvas_html(html, max_bytes)?;

    // Generate canvas ID
    let canvas_id = uuid::Uuid::new_v4().to_string();

    // Save to per-sender output directory
    let (output_dir, rel_dir) = if let (Some(_root), Some(hd), Some(an)) = (workspace_root, home_dir, agent_name) {
        let sid = sender_id.ok_or("Cannot save canvas: no sender context")?;
        let oid = owner_id.unwrap_or(sid);
        let rel = types::config::sender_relative_path(oid, an, Some(sid), "output");
        (hd.join(&rel), rel)
    } else {
        return Err("Cannot save canvas: no workspace".into());
    };
    let _ = tokio::fs::create_dir_all(&output_dir).await;

    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let filename = format!(
        "canvas_{timestamp}_{}.html",
        crate::str_utils::safe_truncate_str(&canvas_id, 8)
    );
    let filepath = output_dir.join(&filename);

    // Write the full HTML document
    let full_html = format!(
        "<!DOCTYPE html>\n<html>\n<head><meta charset=\"utf-8\"><title>{title}</title></head>\n<body>\n{sanitized}\n</body>\n</html>"
    );
    tokio::fs::write(&filepath, &full_html)
        .await
        .map_err(|e| format!("Failed to save canvas: {e}"))?;

    let response = serde_json::json!({
        "canvas_id": canvas_id,
        "title": title,
        "saved_to": format!("{}/{}", rel_dir, filename),
        "size_bytes": full_html.len(),
    });

    serde_json::to_string_pretty(&response).map_err(|e| format!("Serialize error: {e}"))
}
