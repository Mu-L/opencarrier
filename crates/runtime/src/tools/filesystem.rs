//! Filesystem tools: file_read, file_write, file_list, file_convert.

use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Module struct
// ---------------------------------------------------------------------------

pub struct FilesystemTools;

// ---------------------------------------------------------------------------
// ToolModule implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl super::ToolModule for FilesystemTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "file_read".to_string(),
                description: "Read the contents of a file. Paths are relative to the agent workspace.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "The file path to read" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "file_write".to_string(),
                description: "Write content to a file. Use 'output/' prefix for user-specific task outputs (articles, reports, drafts, generated content). Use 'memory/' prefix for user-specific private notes. Paths are sandboxed per-user automatically.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "The file path to write to" },
                        "content": { "type": "string", "description": "The content to write" }
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolDefinition {
                name: "file_list".to_string(),
                description: "List files in a directory. Paths are relative to the agent workspace.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "The directory path to list" }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "file_convert".to_string(),
                description: "Convert a document between formats using Pandoc. Supported formats: markdown, html, docx, pdf, rst, latex, etc.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "input_path": { "type": "string", "description": "Path to the input file" },
                        "output_format": { "type": "string", "description": "Target format (e.g. 'pdf', 'docx', 'html')" },
                        "output_path": { "type": "string", "description": "Optional output path. Auto-generated if not provided." }
                    },
                    "required": ["input_path", "output_format"]
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
            "file_read" => Some(tool_file_read(input, ctx).await),
            "file_write" => Some(tool_file_write(input, ctx).await),
            "file_list" => Some(tool_file_list(input, ctx).await),
            "file_convert" => Some(tool_file_convert(input, ctx).await),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> types::tool::PermissionLevel {
        match tool_name {
            "file_read" | "file_list" | "file_convert" => types::tool::PermissionLevel::ReadOnly,
            "file_write" => types::tool::PermissionLevel::Write,
            _ => types::tool::PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Private tool implementations
// ---------------------------------------------------------------------------

/// Detect common binary file types from magic bytes.
/// Returns a human-readable kind (e.g. "PNG 图片") so we can tell the LLM
/// to use image_analyze instead of file_read.
fn detect_binary_kind(header: &[u8]) -> Option<&'static str> {
    if header.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        Some("PNG 图片")
    } else if header.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("JPEG 图片")
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        Some("GIF 图片")
    } else if header.starts_with(b"RIFF") && header.len() > 11 && &header[8..12] == b"WEBP" {
        Some("WebP 图片")
    } else if header.len() > 4 && &header[4..8] == b"ftyp" {
        Some("视频文件")
    } else if header.starts_with(&[0x25, 0x50, 0x44, 0x46]) {
        Some("PDF 文档")
    } else if header.starts_with(&[0x50, 0x4B, 0x03, 0x04]) {
        Some("ZIP 压缩包")
    } else {
        None
    }
}

/// Resolve output/memory (and catch-all) paths to the top-level senders directory.
///
/// Returns `None` if the path is a workspace-internal path (knowledge/, flows/, etc.)
/// that should be handled by the sandbox instead.
fn resolve_user_data_path(
    raw_path: &str,
    home_dir: &Path,
    sender_id: &str,
    owner_id: Option<&str>,
    agent_name: &str,
) -> Option<Result<PathBuf, String>> {
    // Absolute paths — delegate to the workspace sandbox, which strips the
    // workspace_root prefix and canonicalizes.  We MUST NOT strip the leading
    // slash ourselves (that would turn "/home/…/output/file.md" into
    // "home/…/output/file.md" and join it under the sender's output dir,
    // creating a malformed nested path).
    let normalized = raw_path.replace('\\', "/");
    if normalized.starts_with('/') {
        return None;
    }
    let rel = normalized.trim_start_matches('/');

    // Determine subdirectory and rest-of-path from the user's input
    let (subdir, rest) = if rel.starts_with("output/") || rel == "output" {
        let rest = rel.strip_prefix("output").unwrap_or("");
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        ("output", rest)
    } else if rel.starts_with("memory/") || rel == "memory" {
        let rest = rel.strip_prefix("memory").unwrap_or("");
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        ("memory", rest)
    } else if crate::workspace_sandbox::is_internal_path(rel) {
        // Internal paths go through sandbox
        return None;
    } else {
        // Catch-all: non-internal paths go to output/
        ("output", rel)
    };

    // Validate no path traversal
    if let Err(e) = super::validate_path(rel) {
        return Some(Err(e));
    }

    let oid = owner_id.unwrap_or(sender_id);
    let base = types::config::sender_data_dir(home_dir, oid, agent_name, Some(sender_id));
    let target = if rest.is_empty() {
        base.join(subdir)
    } else {
        base.join(subdir).join(rest)
    };

    Some(Ok(target))
}

async fn tool_file_read(input: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;

    let resolved = if let (Some(hd), Some(sid), Some(an)) = (ctx.home_dir, ctx.sender_id, ctx.agent_name) {
        match resolve_user_data_path(raw_path, hd, sid, ctx.owner_id, an) {
            Some(Ok(path)) => path,
            Some(Err(e)) => return Err(e),
            None => {
                // Internal path — go through sandbox
                super::resolve_file_path_for_read(raw_path, ctx.workspace_root, ctx.sender_id, ctx.agent_name)?
            }
        }
    } else {
        super::resolve_file_path_for_read(raw_path, ctx.workspace_root, ctx.sender_id, ctx.agent_name)?
    };

    tracing::info!(raw_path, resolved = %resolved.display(), "file_read resolved path");

    // Friendly error: detect binary files (images, etc.) before reading.
    // file_read only handles text; binary files should use image_analyze etc.
    if let Ok(metadata) = tokio::fs::metadata(&resolved).await {
        if metadata.is_file() {
            // Check magic bytes to detect common binary formats
            if let Ok(header) = tokio::fs::read(&resolved).await {
                let kind = detect_binary_kind(&header);
                if let Some(kind) = kind {
                    return Err(format!(
                        "文件 '{raw_path}' 是二进制文件（{kind}），file_read 只能读取文本文件。\
                         如果是图片，请用 image_analyze 工具分析；如果是其他二进制文件，\
                         请直接使用它的路径/URL，不需要读取内容。"
                    ));
                }
            }
        }
    }

    tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| {
            // Friendly message for UTF-8 decode failures on text files
            if e.to_string().contains("stream did not contain valid UTF-8")
                || e.to_string().contains("invalid utf-8")
            {
                format!(
                    "文件 '{raw_path}' 包含非 UTF-8 内容（可能是二进制文件）。\
                     file_read 只能读文本。如果是图片，请用 image_analyze；\
                     如果是文档，请确认文件格式或使用对应的解析工具。"
                )
            } else {
                format!("Failed to read file: {e}")
            }
        })
}

async fn tool_file_write(input: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;

    let resolved = if let (Some(hd), Some(sid), Some(an)) = (ctx.home_dir, ctx.sender_id, ctx.agent_name) {
        match resolve_user_data_path(raw_path, hd, sid, ctx.owner_id, an) {
            Some(Ok(path)) => path,
            Some(Err(e)) => return Err(e),
            None => {
                // Internal path — go through sandbox
                if let Some(root) = ctx.workspace_root {
                    crate::workspace_sandbox::resolve_sandbox_path_for_write(raw_path, root, ctx.sender_id, ctx.agent_name, ctx.is_clone_admin)?
                } else {
                    let _ = super::validate_path(raw_path)?;
                    PathBuf::from(raw_path)
                }
            }
        }
    } else if let Some(root) = ctx.workspace_root {
        crate::workspace_sandbox::resolve_sandbox_path_for_write(raw_path, root, ctx.sender_id, ctx.agent_name, ctx.is_clone_admin)?
    } else {
        let _ = super::validate_path(raw_path)?;
        PathBuf::from(raw_path)
    };

    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;
    if let Some(parent) = resolved.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directories: {e}"))?;
    }
    tokio::fs::write(&resolved, content)
        .await
        .map_err(|e| format!("Failed to write file: {e}"))?;
    Ok(format!(
        "Successfully wrote {} bytes to {}",
        content.len(),
        resolved.display()
    ))
}

async fn tool_file_list(input: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;

    let resolved = if let (Some(hd), Some(sid), Some(an)) = (ctx.home_dir, ctx.sender_id, ctx.agent_name) {
        match resolve_user_data_path(raw_path, hd, sid, ctx.owner_id, an) {
            Some(Ok(path)) => path,
            Some(Err(e)) => return Err(e),
            None => {
                // Internal path — go through sandbox
                super::resolve_file_path_for_read(raw_path, ctx.workspace_root, ctx.sender_id, ctx.agent_name)?
            }
        }
    } else {
        super::resolve_file_path_for_read(raw_path, ctx.workspace_root, ctx.sender_id, ctx.agent_name)?
    };

    // For user-data paths (output/ memory/), treat missing directory as empty
    let is_user_data = raw_path.starts_with("output/") || raw_path == "output"
        || raw_path.starts_with("memory/") || raw_path == "memory";

    // Friendly error: if path points to a file (not a directory), tell the
    // LLM clearly instead of returning the cryptic OS "Not a directory" error.
    if let Ok(metadata) = tokio::fs::metadata(&resolved).await {
        if metadata.is_file() {
            return Err(format!(
                "路径 '{raw_path}' 是一个文件，不是目录。file_list 只能列出目录内容。\n\
                 修正方法：\n\
                 - 想读取这个文件内容 → 用 file_read(path=\"{raw_path}\")\n\
                 - 想列出它所在的目录 → 用 file_list 并去掉文件名（例如列出上级目录）"
            ));
        }
    }

    let read_dir_result = tokio::fs::read_dir(&resolved).await;
    let mut entries = match read_dir_result {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && is_user_data => {
            return Ok("(empty directory)".to_string());
        }
        Err(e) => return Err(format!("Failed to list directory: {e}")),
    };
    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("Failed to read entry: {e}"))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata().await;
        let suffix = match metadata {
            Ok(m) if m.is_dir() => "/",
            _ => "",
        };
        files.push(format!("{name}{suffix}"));
    }
    files.sort();
    if files.is_empty() {
        Ok("(empty directory)".to_string())
    } else {
        Ok(files.join("\n"))
    }
}

async fn tool_file_convert(input: &Value, ctx: &ToolContext<'_>) -> Result<String, String> {
    let raw_input_path = input["input_path"]
        .as_str()
        .ok_or("Missing 'input_path' parameter")?;
    let output_format = input["output_format"]
        .as_str()
        .ok_or("Missing 'output_format' parameter")?;
    let raw_output_path = input["output_path"].as_str();

    let input_path = super::resolve_file_path(raw_input_path, ctx.workspace_root)?;
    if !input_path.exists() {
        return Err(format!("Input file not found: {}", input_path.display()));
    }
    let metadata = std::fs::metadata(&input_path)
        .map_err(|e| format!("Cannot read input file metadata: {e}"))?;
    if metadata.len() > 50 * 1024 * 1024 {
        return Err(format!(
            "Input file too large: {} bytes (max 50MB)",
            metadata.len()
        ));
    }

    let output_path = if let Some(op) = raw_output_path {
        // User-specified output path — resolve through the same logic as file_write
        if let (Some(hd), Some(sid), Some(an)) = (ctx.home_dir, ctx.sender_id, ctx.agent_name) {
            match resolve_user_data_path(op, hd, sid, ctx.owner_id, an) {
                Some(Ok(path)) => path,
                Some(Err(e)) => return Err(e),
                None => {
                    if let Some(root) = ctx.workspace_root {
                        crate::workspace_sandbox::resolve_sandbox_path_for_write(op, root, ctx.sender_id, ctx.agent_name, ctx.is_clone_admin)?
                    } else {
                        let _ = super::validate_path(op)?;
                        PathBuf::from(op)
                    }
                }
            }
        } else if let Some(root) = ctx.workspace_root {
            crate::workspace_sandbox::resolve_sandbox_path_for_write(op, root, ctx.sender_id, ctx.agent_name, ctx.is_clone_admin)?
        } else {
            let _ = super::validate_path(op)?;
            PathBuf::from(op)
        }
    } else {
        // Auto-generated output path — use top-level senders directory
        let input_stem = input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("converted");
        let sender = ctx.sender_id.unwrap_or("unknown");
        let agent = ctx.agent_name.unwrap_or("unknown");
        let oid = ctx.owner_id.unwrap_or(sender);
        let output_dir = if let Some(hd) = ctx.home_dir {
            types::config::sender_data_dir(hd, oid, agent, Some(sender)).join("output")
        } else {
            PathBuf::from("output")
        };
        let _ = std::fs::create_dir_all(&output_dir);
        let filename = format!("{input_stem}.{output_format}");
        output_dir.join(filename)
    };

    let mut cmd = tokio::process::Command::new("pandoc");
    cmd.arg(&input_path)
        .arg("-t")
        .arg(output_format)
        .arg("-o")
        .arg(&output_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to run pandoc (is it installed?): {e}"))?;

    let output = tokio::time::timeout(std::time::Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| "Pandoc timed out after 60 seconds".to_string())
        .and_then(|r| r.map_err(|e| format!("Pandoc process error: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(format!("Pandoc conversion failed: {stderr}"));
    }

    if !output_path.exists() {
        return Err("Pandoc completed but no output file was produced".to_string());
    }

    let out_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    Ok(format!(
        "Successfully converted {} -> {}\nInput: {} ({} bytes)\nOutput: {} ({} bytes)",
        raw_input_path,
        output_format,
        input_path.display(),
        metadata.len(),
        output_path.display(),
        out_size,
    ))
}
