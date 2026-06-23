//! Tool module framework.
//!
//! Each tool category implements `ToolModule` and provides both definitions
//! (for LLM tool schemas) and execution (the actual logic).

pub mod a2a;
pub mod agent;
pub mod agent_mgmt;
pub mod browser;
pub mod collaboration;
pub mod filesystem;
pub mod knowledge;
pub mod kv;
pub mod media;
pub mod memory;
pub mod misc;
pub mod scheduling;
pub mod shell;
pub mod sqlite;
pub mod toolset;
pub mod training;
pub mod web_fetch;
pub mod web_search;

use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Shared AginBrowser helpers (used by browser.rs and web_search.rs)
// ---------------------------------------------------------------------------

/// Default AginBrowser endpoint. Override via `AGINXBROWSER_URL` env var.
pub(crate) const AGINXBROWSER_DEFAULT_URL: &str = "http://127.0.0.1:8089";

/// Default timeout for AginBrowser HTTP requests (seconds).
pub(crate) const AGINXBROWSER_TIMEOUT_SECS: u64 = 60;

/// Read the AginBrowser URL from `AGINXBROWSER_URL` env var.
/// Returns `None` if not set or empty (e.g. web_search disables itself).
pub(crate) fn aginxbrowser_url_opt() -> Option<String> {
    std::env::var("AGINXBROWSER_URL").ok().filter(|s| !s.is_empty())
}

/// Read the AginBrowser URL from `AGINXBROWSER_URL` env var.
/// Returns the default URL if not set (e.g. browser_* tools are always enabled).
pub(crate) fn aginxbrowser_url() -> String {
    aginxbrowser_url_opt().unwrap_or_else(|| AGINXBROWSER_DEFAULT_URL.to_string())
}

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

    /// Return the permission level for a tool in this module.
    ///
    /// Default: `Dangerous` (fail-safe — unknown tools require maximum trust).
    fn permission_level(&self, _tool_name: &str) -> PermissionLevel {
        PermissionLevel::Dangerous
    }

    /// Return the maximum result size in chars for a tool in this module.
    ///
    /// Default: `None` (no per-tool limit — dynamic context truncation applies).
    fn max_result_size_chars(&self, _tool_name: &str) -> Option<usize> {
        None
    }
}

/// All built-in tool modules in dispatch order.
pub fn builtin_modules(cli_exec_config: types::config::CliExecConfig) -> Vec<Box<dyn ToolModule>> {
    let mut modules: Vec<Box<dyn ToolModule>> = vec![
        Box::new(filesystem::FilesystemTools),
        Box::new(sqlite::SqliteTools),
        Box::new(shell::ShellTools),
        Box::new(browser::BrowserTools),
        Box::new(web_fetch::WebFetchModule),
        Box::new(web_search::WebSearchTools),
        Box::new(misc::MiscTools),
        Box::new(toolset::ToolSearchTools),
        Box::new(knowledge::KnowledgeTools),
        Box::new(kv::KvTools),
        Box::new(media::MediaTools),
        Box::new(memory::MemoryTools),
        Box::new(agent::DelegationTools),
        Box::new(agent_mgmt::AgentMgmtTools),
        Box::new(training::TrainingTools),
        Box::new(scheduling::SchedulingTools),
        Box::new(collaboration::CollaborationTools),
        Box::new(a2a::A2aTools),
    ];
    // Only register cli_exec if there are whitelisted commands configured.
    if !cli_exec_config.commands.is_empty() {
        modules.push(Box::new(shell::CliExecTools::new(cli_exec_config)));
    }
    modules
}

// ---------------------------------------------------------------------------
// Shared kernel helpers (used by multiple tool modules)
// ---------------------------------------------------------------------------

/// Require a kernel handle, returning an error if none is available.
pub(crate) fn require_kernel(
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<&Arc<dyn KernelHandle>, String> {
    kernel.ok_or_else(|| {
        "Kernel handle not available. Inter-agent tools require a running kernel.".to_string()
    })
}

/// Check that the inter-agent call depth has not exceeded the maximum.
pub(crate) fn check_call_depth() -> Result<(), String> {
    let current = crate::tool_runner::AGENT_CALL_DEPTH
        .try_with(|d| d.get())
        .unwrap_or(0);
    if current >= crate::tool_runner::MAX_AGENT_CALL_DEPTH {
        Err(format!(
            "Agent call depth exceeded (max {}). Use the task queue instead.",
            crate::tool_runner::MAX_AGENT_CALL_DEPTH
        ))
    } else {
        Ok(())
    }
}

/// Resolve a target clone's workspace root via kernel.
pub(crate) fn resolve_target_workspace(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<PathBuf, String> {
    let kh = kernel.ok_or("train_* tools require kernel access")?;
    let target = input["target"]
        .as_str()
        .ok_or("Missing 'target' parameter (target clone name)")?;

    let target_workspace = kh
        .resolve_agent_workspace(target)
        .ok_or_else(|| format!("Agent '{}' not found or has no workspace", target))?;

    let path = PathBuf::from(&target_workspace);
    if !path.exists() {
        return Err(format!(
            "Workspace for '{}' does not exist: {}",
            target, target_workspace
        ));
    }
    Ok(path)
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
