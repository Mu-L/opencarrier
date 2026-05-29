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

/// Resolve output/memory (and catch-all) paths to the top-level senders directory.
///
/// Returns `None` if the path is a workspace-internal path (knowledge/, skills/, etc.)
/// that should be handled by the sandbox instead.
fn resolve_user_data_path(
    raw_path: &str,
    home_dir: &Path,
    sender_id: &str,
    owner_id: Option<&str>,
    agent_name: &str,
) -> Option<Result<PathBuf, String>> {
    let normalized = raw_path.replace('\\', "/");
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
    tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| format!("Failed to read file: {e}"))
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
                    crate::workspace_sandbox::resolve_sandbox_path_for_write(raw_path, root, ctx.sender_id, ctx.agent_name)?
                } else {
                    let _ = super::validate_path(raw_path)?;
                    PathBuf::from(raw_path)
                }
            }
        }
    } else if let Some(root) = ctx.workspace_root {
        crate::workspace_sandbox::resolve_sandbox_path_for_write(raw_path, root, ctx.sender_id, ctx.agent_name)?
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

    let mut entries = tokio::fs::read_dir(&resolved)
        .await
        .map_err(|e| format!("Failed to list directory: {e}"))?;
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
    Ok(files.join("\n"))
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
                        crate::workspace_sandbox::resolve_sandbox_path_for_write(op, root, ctx.sender_id, ctx.agent_name)?
                    } else {
                        let _ = super::validate_path(op)?;
                        PathBuf::from(op)
                    }
                }
            }
        } else if let Some(root) = ctx.workspace_root {
            crate::workspace_sandbox::resolve_sandbox_path_for_write(op, root, ctx.sender_id, ctx.agent_name)?
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
