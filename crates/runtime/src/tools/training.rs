//! Cross-workspace training tools (for trainer agents).

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Cross-workspace training tools (for trainer agents)
// ---------------------------------------------------------------------------

async fn tool_train_read(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = crate::tools::resolve_target_workspace(input, kernel)?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    crate::tools::validate_path(path)?;
    let full_path = target_root.join(path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| format!("Failed to read file: {e}"))
}

async fn tool_train_write(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = crate::tools::resolve_target_workspace(input, kernel)?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    crate::tools::validate_path(path)?;
    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;
    let full_path = target_root.join(path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directories: {e}"))?;
    }
    tokio::fs::write(&full_path, content)
        .await
        .map_err(|e| format!("Failed to write file: {e}"))?;
    Ok(format!(
        "Successfully wrote {} bytes to {}",
        content.len(),
        path
    ))
}

async fn tool_train_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = crate::tools::resolve_target_workspace(input, kernel)?;
    let sub_path = input["path"].as_str().unwrap_or(".");
    crate::tools::validate_path(sub_path)?;
    let full_path = target_root.join(sub_path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    let mut entries = tokio::fs::read_dir(&full_path)
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

async fn tool_train_evaluate(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = crate::tools::resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_clone_evaluate(Some(&target_root)).await
}

// ---------------------------------------------------------------------------
// ToolModule implementation
// ---------------------------------------------------------------------------

/// Cross-workspace training tools (for trainer agents).
pub struct TrainingTools;

#[async_trait]
impl ToolModule for TrainingTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "train_read".to_string(),
                description: "Read a file from a target clone's workspace. Used by trainer agents to inspect other clones.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone to read from"},
                        "path": {"type": "string", "description": "File path relative to the target clone's workspace root"},
                    },
                    "required": ["target", "path"],
                }),
            },
            ToolDefinition {
                name: "train_write".to_string(),
                description: "Write a file to a target clone's workspace. Can modify any file including SOUL.md, system_prompt.md, agent.toml, and skills. Used by trainer agents to train other clones.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone to write to"},
                        "path": {"type": "string", "description": "File path relative to the target clone's workspace root"},
                        "content": {"type": "string", "description": "File content to write"},
                    },
                    "required": ["target", "path", "content"],
                }),
            },
            ToolDefinition {
                name: "train_list".to_string(),
                description: "List files in a target clone's workspace directory. Used by trainer agents to explore other clones.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone"},
                        "path": {"type": "string", "description": "Directory path relative to the target clone's workspace root (default: '.')"},
                    },
                    "required": ["target"],
                }),
            },
            ToolDefinition {
                name: "train_evaluate".to_string(),
                description: "Evaluate a target clone's quality with deterministic metrics. Returns score (0-100), knowledge stats, skill count, and identity completeness.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone to evaluate"},
                    },
                    "required": ["target"],
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
        let kernel = ctx.kernel;
        let caller_agent_id = ctx.caller_agent_id;

        match name {
            "train_read" => Some(tool_train_read(input, kernel, caller_agent_id).await),
            "train_write" => Some(tool_train_write(input, kernel, caller_agent_id).await),
            "train_list" => Some(tool_train_list(input, kernel, caller_agent_id).await),
            "train_evaluate" => Some(tool_train_evaluate(input, kernel, caller_agent_id).await),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "train_read" | "train_list" | "train_evaluate" => PermissionLevel::None,
            "train_write" => PermissionLevel::Write,
            _ => PermissionLevel::Dangerous,
        }
    }
}
