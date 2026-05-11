//! Filesystem tools: file_read, file_write, file_list, file_convert.

use crate::tool_context::ToolContext;
use async_trait::async_trait;
use carrier_types::tool::ToolDefinition;
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
                description: "Write content to a file. Paths are relative to the agent workspace.".to_string(),
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
            "file_read" => Some(tool_file_read(input, ctx.workspace_root, ctx.sender_id, ctx.agent_name).await),
            "file_write" => Some(tool_file_write(input, ctx.workspace_root, ctx.sender_id, ctx.agent_name).await),
            "file_list" => Some(tool_file_list(input, ctx.workspace_root, ctx.sender_id, ctx.agent_name).await),
            "file_convert" => {
                Some(tool_file_convert(input, ctx.workspace_root, ctx.sender_id, ctx.agent_name).await)
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Private tool implementations
// ---------------------------------------------------------------------------

async fn tool_file_read(
    input: &Value,
    workspace_root: Option<&Path>,
    sender_id: Option<&str>,
    agent_name: Option<&str>,
) -> Result<String, String> {
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    let resolved = super::resolve_file_path_for_read(raw_path, workspace_root, sender_id, agent_name)?;
    tokio::fs::read_to_string(&resolved)
        .await
        .map_err(|e| format!("Failed to read file: {e}"))
}

async fn tool_file_write(
    input: &Value,
    workspace_root: Option<&Path>,
    sender_id: Option<&str>,
    agent_name: Option<&str>,
) -> Result<String, String> {
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    let resolved = if let Some(root) = workspace_root {
        crate::workspace_sandbox::resolve_sandbox_path_for_write(raw_path, root, sender_id, agent_name)?
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

async fn tool_file_list(
    input: &Value,
    workspace_root: Option<&Path>,
    sender_id: Option<&str>,
    agent_name: Option<&str>,
) -> Result<String, String> {
    let raw_path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    let resolved = super::resolve_file_path_for_read(raw_path, workspace_root, sender_id, agent_name)?;
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

async fn tool_file_convert(
    input: &Value,
    workspace_root: Option<&Path>,
    sender_id: Option<&str>,
    agent_name: Option<&str>,
) -> Result<String, String> {
    let raw_input_path = input["input_path"]
        .as_str()
        .ok_or("Missing 'input_path' parameter")?;
    let output_format = input["output_format"]
        .as_str()
        .ok_or("Missing 'output_format' parameter")?;
    let raw_output_path = input["output_path"].as_str();

    let input_path = super::resolve_file_path(raw_input_path, workspace_root)?;
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
        if let Some(root) = workspace_root {
            crate::workspace_sandbox::resolve_sandbox_path_for_write(op, root, sender_id, agent_name)?
        } else {
            let _ = super::validate_path(op)?;
            PathBuf::from(op)
        }
    } else {
        let input_stem = input_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("converted");
        let sender = sender_id.unwrap_or("unknown");
        let output_dir = if let Some(root) = workspace_root {
            root.join("senders").join(sender).join(agent_name.unwrap_or("unknown")).join("output")
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
