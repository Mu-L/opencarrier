//! Tool module framework.
//!
//! Each tool category implements `ToolModule` and provides both definitions
//! (for LLM tool schemas) and execution (the actual logic).

pub mod agent;
pub mod filesystem;
pub mod knowledge;
pub mod media;
pub mod misc;
pub mod shell;
pub mod toolset;

use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// A category of related tools.
///
/// Modules are tried in order; the first one returning `Some` handles the tool.
#[async_trait]
pub trait ToolModule: Send + Sync {
    /// Tool definitions exposed to the LLM.
    fn definitions(&self) -> Vec<ToolDefinition>;

    /// Try to execute a tool by name.
    ///
    /// Returns `Some(Ok(content))` if handled successfully,
    /// `Some(Err(message))` if handled but failed,
    /// `None` if this module doesn't handle the tool.
    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>>;
}

/// All built-in tool modules in dispatch order.
pub fn builtin_modules() -> Vec<Box<dyn ToolModule>> {
    vec![
        Box::new(filesystem::FilesystemTools),
        Box::new(shell::ShellTools),
        Box::new(misc::MiscTools),
        Box::new(toolset::ToolSearchTools),
        Box::new(knowledge::KnowledgeTools),
        Box::new(media::MediaTools),
        Box::new(agent::AgentTools),
    ]
}

// ---------------------------------------------------------------------------
// Shared path validation utilities (used by multiple tool modules)
// ---------------------------------------------------------------------------

/// Reject path traversal attempts and absolute paths.
pub fn validate_path(path: &str) -> Result<&str, String> {
    for component in std::path::Path::new(path).components() {
        match component {
            std::path::Component::ParentDir => {
                return Err("Path traversal denied: '..' components are forbidden".to_string());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err("Absolute paths are forbidden".to_string());
            }
            _ => {}
        }
    }
    Ok(path)
}

/// Sanitize a string before using it as a single path component.
pub fn sanitize_path_component(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("Empty path component".to_string());
    }
    if name.contains('/') || name.contains('\\') || name == ".." || name.contains("..") {
        return Err(format!("Invalid path component: {:?}", name));
    }
    for component in std::path::Path::new(name).components() {
        match component {
            std::path::Component::ParentDir => {
                return Err(format!("Path traversal denied in component: {:?}", name));
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return Err(format!("Absolute path denied in component: {:?}", name));
            }
            _ => {}
        }
    }
    Ok(name)
}

/// Validate a clone name: only lowercase alphanumeric and hyphens allowed.
pub fn validate_clone_name(name: &str) -> Result<&str, String> {
    if name.is_empty() {
        return Err("Clone name cannot be empty".to_string());
    }
    if name.len() > 64 {
        return Err("Clone name too long (max 64 characters)".to_string());
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("Clone name cannot start or end with a hyphen".to_string());
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(
            "Clone name must contain only lowercase letters, digits, and hyphens (e.g. 'customer-support')".to_string()
        );
    }
    Ok(name)
}

/// Validate a file path key inside clone files map — no traversal, no absolute paths.
pub fn validate_clone_file_path(path: &str) -> Result<&str, String> {
    if path.is_empty() {
        return Err("File path cannot be empty".to_string());
    }
    if path.starts_with('/') || path.starts_with("..") {
        return Err(format!(
            "Invalid file path '{}': must be relative and not escape the archive",
            path
        ));
    }
    validate_path(path)
}

/// Resolve a file path through the workspace sandbox (if available) or legacy validation.
pub fn resolve_file_path(raw_path: &str, workspace_root: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(root) = workspace_root {
        crate::workspace_sandbox::resolve_sandbox_path(raw_path, root)
    } else {
        let _ = validate_path(raw_path)?;
        Ok(PathBuf::from(raw_path))
    }
}

/// Resolve a file read path through the workspace sandbox with sender_id-aware rewriting.
pub fn resolve_file_path_for_read(
    raw_path: &str,
    workspace_root: Option<&Path>,
    sender_id: Option<&str>,
    agent_name: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(root) = workspace_root {
        crate::workspace_sandbox::resolve_sandbox_path_for_read(raw_path, root, sender_id, agent_name)
    } else {
        let _ = validate_path(raw_path)?;
        Ok(PathBuf::from(raw_path))
    }
}
