//! Knowledge and skill management tool module.
//!
//! Provides tools for reading, writing, linting, healing, importing, and
//! extracting knowledge files, managing skills, evaluating clone quality,
//! applying patches, and saving session summaries.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use types::error::{CarrierError, CarrierResult};
use types::tool::ToolDefinition;

/// Knowledge, skill, patch, evaluation, and session tools.
pub struct KnowledgeTools;

#[async_trait]
impl ToolModule for KnowledgeTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "knowledge_list".to_string(),
                description: "List available knowledge files in the agent's knowledge base. Returns filenames with descriptions.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "knowledge_read".to_string(),
                description: "Read a specific knowledge file from the agent's knowledge base. Only files in knowledge/ are accessible.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filename": { "type": "string", "description": "The knowledge file name (e.g., 'refund-policy.md')" }
                    },
                    "required": ["filename"]
                }),
            },
            ToolDefinition {
                name: "apply_patch".to_string(),
                description: "Apply a multi-hunk diff patch to add, update, move, or delete files. Use this for targeted edits instead of full file overwrites.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "string",
                            "description": "The patch in *** Begin Patch / *** End Patch format. Use *** Add File:, *** Update File:, *** Delete File: markers. Hunks use @@ headers with space (context), - (remove), + (add) prefixed lines."
                        }
                    },
                    "required": ["patch"]
                }),
            },
            ToolDefinition {
                name: "knowledge_lint".to_string(),
                description: "Check the health of the clone's knowledge base. Reports missing frontmatter, empty files, placeholder content, and other issues.".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "knowledge_heal".to_string(),
                description: "Automatically fix knowledge base issues: remove empty files, rebuild MEMORY.md index, add missing frontmatter templates.".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "knowledge_add".to_string(),
                description: "Save a long-term knowledge entry that ALL users share (e.g. policies, reference docs, how-to guides, facts). Do NOT use this for user-specific content like article drafts, reports, outlines, or task outputs — use file_write with an output/ path for those.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Short title for the knowledge entry"},
                        "content": {"type": "string", "description": "The knowledge content (markdown)"},
                    },
                    "required": ["title", "content"],
                }),
            },
            ToolDefinition {
                name: "knowledge_update".to_string(),
                description: "Update an EXISTING knowledge file in place (full content replacement). Use this to correct or supersede statements in old knowledge files instead of piling up new entries. First knowledge_read the file, edit, then pass the complete new content (must keep the --- frontmatter with name/description). Filename is fuzzy-matched like knowledge_read.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filename": {"type": "string", "description": "Knowledge file to update (e.g. 'business-info.md'; fuzzy matched)"},
                        "content": {"type": "string", "description": "Complete new file content, starting with the --- frontmatter block"},
                    },
                    "required": ["filename", "content"],
                }),
            },
            ToolDefinition {
                name: "knowledge_remove".to_string(),
                description: "Remove a knowledge entry by filename (supports fuzzy matching).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "filename": {"type": "string", "description": "Filename or title to remove (fuzzy matched)"},
                    },
                    "required": ["filename"],
                }),
            },
            ToolDefinition {
                name: "knowledge_import".to_string(),
                description: "Import data into the clone's knowledge base. Supports FAQ (CSV/TSV), chat logs (JSON), and document text.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "data": {"type": "string", "description": "Raw data content to import"},
                        "data_type": {"type": "string", "description": "Data format: 'faq', 'chat', 'document', or 'auto' (default: auto)"},
                    },
                    "required": ["data"],
                }),
            },
            ToolDefinition {
                name: "clone_evaluate".to_string(),
                description: "Evaluate the clone's quality with deterministic metrics. Returns a score (0-100) based on identity completeness, knowledge richness, skills, and knowledge quality.".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "knowledge_extract".to_string(),
                description: "Extract new knowledge from a conversation and save it to the knowledge base. Uses dual-layer format with timeline tracking and rebuilds MEMORY.md index. Use when you discover facts, rules, or preferences worth remembering.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "description": "Short title for the knowledge (English or pinyin preferred, used as filename)"},
                        "content": {"type": "string", "description": "The knowledge content to save (markdown)"},
                    },
                    "required": ["title", "content"],
                }),
            },
            ToolDefinition {
                name: "knowledge_index".to_string(),
                description: "Rebuild the knowledge index file (MEMORY.md) by scanning all knowledge files in knowledge/. Use after manually adding or removing knowledge files.".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "flow_create".to_string(),
                description: "Create a new flow in the workspace flows/ directory. Flows are tool prescriptions: frontmatter tools: are auto-injected when the flow matches; body is the hard workflow. Prefer declaring concrete tool names in tools (not tool_search).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Flow name (used as filename)"},
                        "description": {"type": "string", "description": "Brief description of when to activate this flow"},
                        "tools": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Tool names this flow needs (e.g. [\"file_read\", \"file_write\", \"web_search\"]). Injected automatically when the flow matches — do not rely on tool_search for these."
                        },
                        "toolsets": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Deprecated alias for tools. Prefer tools."
                        },
                        "body": {"type": "string", "description": "The flow content: hard rules, workflow steps, instructions (markdown)"},
                    },
                    "required": ["name", "body"],
                }),
            },
            ToolDefinition {
                name: "flow_update".to_string(),
                description: "Update an existing PRIVATE flow (body and/or tools: frontmatter) to固化 a proven tool path so next runs inject it without tool_search. Only workspace-private flows can be updated; shared system flows are READ-ONLY (no copy-on-write) — request a human to update shared flows, or use flow_create for a clone-specific variant.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Flow name to update"},
                        "body": {"type": "string", "description": "New flow body (replaces existing body; omit to keep body)"},
                        "tools": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Replace frontmatter tools: list with these names (proven tools to inject next time)"
                        },
                        "description": {"type": "string", "description": "Optional new frontmatter description"},
                    },
                    "required": ["name"],
                }),
            },
            ToolDefinition {
                name: "flow_load".to_string(),
                description: "Load the full content of a flow by name. Returns the complete flow file including frontmatter and body.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Flow name to load"},
                    },
                    "required": ["name"],
                }),
            },
            ToolDefinition {
                name: "session_summarize".to_string(),
                description: "Save a summary of the current conversation for future recall. Use after long or important conversations to preserve key points, decisions, and outcomes.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "summary": {"type": "string", "description": "Key points, decisions, and outcomes from this conversation"},
                    },
                    "required": ["summary"],
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<CarrierResult<String>> {
        match name {
            "knowledge_list" => Some(tool_knowledge_list(ctx.workspace_root).await),
            "knowledge_read" => Some(tool_knowledge_read(input, ctx.workspace_root).await),
            "apply_patch" => Some(tool_apply_patch(input, ctx.workspace_root, ctx).await),
            "knowledge_lint" => Some(tool_knowledge_lint(ctx.workspace_root).await),
            "knowledge_heal" => Some(tool_knowledge_heal(ctx.workspace_root).await),
            "knowledge_add" => Some(tool_knowledge_add(input, ctx.workspace_root).await),
            "knowledge_update" => Some(tool_knowledge_update(input, ctx.workspace_root).await),
            "knowledge_remove" => Some(tool_knowledge_remove(input, ctx.workspace_root).await),
            "knowledge_import" => Some(tool_knowledge_import(input, ctx.workspace_root).await),
            "clone_evaluate" => Some(tool_clone_evaluate(ctx.workspace_root).await),
            "knowledge_extract" => Some(tool_knowledge_extract(input, ctx.workspace_root).await),
            "knowledge_index" => Some(tool_knowledge_index(ctx.workspace_root).await),
            "flow_create" => Some(tool_flow_create(input, ctx.workspace_root).await),
            "flow_update" => Some(tool_flow_update(input, ctx.workspace_root).await),
            "flow_load" => Some(tool_flow_load(input, ctx.workspace_root).await),
            "session_summarize" => Some(
                tool_session_summarize(input, ctx.memory, ctx.caller_agent_id, ctx.sender_id).await,
            ),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> types::tool::PermissionLevel {
        match tool_name {
            "knowledge_list" | "knowledge_read" | "session_summarize" | "flow_load"
            | "clone_evaluate" => types::tool::PermissionLevel::None,
            "knowledge_lint" | "knowledge_index" | "knowledge_extract" | "train_read"
            | "train_list" | "train_evaluate" | "user_profile" => {
                types::tool::PermissionLevel::ReadOnly
            }
            "knowledge_add" | "knowledge_update" | "knowledge_remove" | "knowledge_import"
            | "knowledge_heal" | "flow_create" | "flow_update" | "apply_patch" | "train_write" => {
                types::tool::PermissionLevel::Write
            }
            _ => types::tool::PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Knowledge tools (safe access to knowledge/)
// ---------------------------------------------------------------------------

pub(crate) async fn tool_knowledge_list(workspace_root: Option<&Path>) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_list requires a workspace root".to_string(),
    ))?;
    let knowledge_dir = root.join("knowledge");

    if !knowledge_dir.exists() {
        return Ok("No knowledge files found (knowledge/ does not exist).".to_string());
    }

    let mut entries = tokio::fs::read_dir(&knowledge_dir)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to read knowledge directory: {e}")))?;

    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to read entry: {e}")))?
    {
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            let name = entry.file_name().to_string_lossy().to_string();
            // Try to extract title from frontmatter
            let title = tokio::fs::read_to_string(&path)
                .await
                .ok()
                .and_then(|content| extract_knowledge_title(&content));
            match title {
                Some(t) => files.push(format!("- {} ({})", t, name)),
                None => files.push(format!("- {}", name)),
            }
        }
    }

    files.sort();
    if files.is_empty() {
        Ok("No knowledge files found.".to_string())
    } else {
        Ok(format!(
            "Knowledge files ({}):\n{}",
            files.len(),
            files.join("\n")
        ))
    }
}

pub(crate) async fn tool_knowledge_read(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let filename = input["filename"]
        .as_str()
        .ok_or(CarrierError::InvalidInput(
            "Missing 'filename' parameter".to_string(),
        ))?;
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_read requires a workspace root".to_string(),
    ))?;

    // Security: validate filename (no path traversal)
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err(CarrierError::InvalidInput(
            "Invalid filename: path separators and '..' are forbidden".to_string(),
        ));
    }
    if !filename.ends_with(".md") {
        return Err(CarrierError::InvalidInput(
            "Only .md knowledge files can be read".to_string(),
        ));
    }

    let path = root.join("knowledge").join(filename);

    if !path.exists() {
        // List available files so the LLM can correct the filename
        let knowledge_dir = root.join("knowledge");
        let available: Vec<String> = std::fs::read_dir(&knowledge_dir)
            .map(|entries| {
                let mut names: Vec<String> = entries
                    .filter_map(|e| e.ok())
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.ends_with(".md") {
                            Some(name)
                        } else {
                            None
                        }
                    })
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default();
        if available.is_empty() {
            return Ok(format!(
                "Knowledge file '{}' not found. No knowledge files exist yet.",
                filename
            ));
        }
        return Ok(format!(
            "Knowledge file '{}' not found. Available files: {}",
            filename,
            available.join(", ")
        ));
    }

    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to read knowledge file: {e}")))
}

/// Extract `name` from YAML frontmatter of a knowledge file.
fn extract_knowledge_title(content: &str) -> Option<String> {
    let content = content.strip_prefix("---")?;
    let end = content.find("---")?;
    let frontmatter = &content[..end];

    for line in frontmatter.lines() {
        if let Some(value) = line.strip_prefix("name:") {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Patch tool
// ---------------------------------------------------------------------------

async fn tool_apply_patch(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
    ctx: &ToolContext<'_>,
) -> CarrierResult<String> {
    let patch_str = input["patch"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'patch' parameter".to_string(),
    ))?;
    let root = workspace_root.ok_or(CarrierError::Internal(
        "apply_patch requires a workspace root".to_string(),
    ))?;
    let mut ops = crate::apply_patch::parse_patch(patch_str)?;

    // Route user-data paths (output/, memory/, input/, catch-all) through the
    // same sender-scoped resolver as file_read/file_write, so a file written by
    // file_write can be read/patched/deleted by apply_patch in the same turn.
    // Without this, file_write lands in senders/<sender>/output/... while
    // apply_patch looks in <workspace>/output/... - a path fork the agent
    // cannot reconcile (2026-08-21 86bus stuck-turn incident).
    for op in &mut ops {
        let (path, move_to): (&mut String, Option<&mut String>) = match op {
            crate::apply_patch::PatchOp::AddFile { path, .. } => (path, None),
            crate::apply_patch::PatchOp::UpdateFile { path, move_to, .. } => {
                (path, move_to.as_mut())
            }
            crate::apply_patch::PatchOp::DeleteFile { path } => (path, None),
        };
        for p in std::iter::once(path).chain(move_to.into_iter()) {
            if p.contains('\u{FFFD}') {
                return Err(CarrierError::InvalidInput(format!(
                    "路径 '{p}' 含损坏字符（U+FFFD），无法寻址。请用干净的文件名（中文名或 ASCII 名）重写补丁。"
                )));
            }
            let normalized = p.replace('\\', "/");
            if normalized == "input" || normalized.starts_with("input/") {
                return Err(CarrierError::InvalidInput(
                    "input/ 是用户发来的文件收件箱（只读），请改用 output/ 前缀写文件。".to_string(),
                ));
            }
            if let (Some(hd), Some(sid), Some(an)) =
                (ctx.home_dir, ctx.sender_id, ctx.agent_name)
            {
                if let Some(res) =
                    super::filesystem::resolve_user_data_path(p, hd, sid, ctx.owner_id, an)
                {
                    *p = res?.to_string_lossy().into_owned();
                }
            }
        }
    }

    let result = crate::apply_patch::apply_patch(&ops, root).await;
    if result.is_ok() {
        Ok(result.summary())
    } else {
        Err(CarrierError::Internal(format!(
            "Patch partially applied: {}. Errors: {}",
            result.summary(),
            result.errors.join("; ")
        )))
    }
}

// ---------------------------------------------------------------------------
// Lifecycle system tools (clone knowledge management)
// ---------------------------------------------------------------------------

pub(crate) async fn tool_knowledge_lint(workspace_root: Option<&Path>) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_lint requires a workspace root".to_string(),
    ))?;
    let report = lifecycle::health::check_health(root);
    if report.issues.is_empty() {
        Ok("All knowledge files are healthy.".to_string())
    } else {
        let mut out = format!("Found {} issue(s):\n", report.issues.len());
        for issue in &report.issues {
            out.push_str(&format!(
                "- [{:?}] {}: {}\n",
                issue.severity, issue.filename, issue.message
            ));
        }
        Ok(out)
    }
}

pub(crate) async fn tool_knowledge_heal(workspace_root: Option<&Path>) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_heal requires a workspace root".to_string(),
    ))?;
    let report = lifecycle::health::check_health(root);
    let fixes = lifecycle::health::auto_fix(root, &report);
    Ok(format!("Fixed {} issue(s).", fixes))
}

/// Core logic for adding a knowledge file. Shared by tool and train versions.
pub(crate) async fn knowledge_add_core(
    root: &Path,
    title: &str,
    content: &str,
    source_label: &str,
) -> CarrierResult<String> {
    let filename = lifecycle::evolution::sanitize_filename(title);
    let knowledge_dir = root.join("knowledge");
    tokio::fs::create_dir_all(&knowledge_dir)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to create knowledge dir: {e}")))?;
    let path = knowledge_dir.join(format!("{filename}.md"));
    let full = format!(
        "---\nname: {}\ndescription: {}\nconfidence: EXTRACTED\n---\n{}\n---\n",
        title, title, content
    );
    tokio::fs::write(&path, &full)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to write knowledge file: {e}")))?;
    let _ = lifecycle::version::record_version(
        root,
        "create",
        &format!("{filename}.md"),
        None,
        Some(&full),
        source_label,
    );
    Ok(filename)
}

/// Core logic for importing knowledge entries. Shared by tool and train versions.
pub(crate) async fn knowledge_import_core(
    root: &Path,
    data: &str,
    data_type: &str,
) -> CarrierResult<(Vec<String>, lifecycle::parsers::ParseQuality)> {
    let result = lifecycle::parsers::parse_import_data(data, data_type)
        .map_err(|e| CarrierError::Serialization(format!("Parse failed: {e}")))?;
    let knowledge_dir = root.join("knowledge");
    tokio::fs::create_dir_all(&knowledge_dir)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to create knowledge dir: {e}")))?;
    let mut saved = Vec::new();
    for entry in &result.entries {
        let filename = lifecycle::evolution::sanitize_filename(&entry.title);
        let path = knowledge_dir.join(format!("{filename}.md"));
        let full = format!(
            "---\nname: {}\ndescription: {}\nconfidence: INFERRED\n---\n{}\n---\n",
            entry.title, entry.title, entry.content
        );
        tokio::fs::write(&path, &full)
            .await
            .map_err(|e| CarrierError::Internal(format!("Failed to write {}: {e}", filename)))?;
        saved.push(filename);
    }
    Ok((saved, result.quality))
}

async fn tool_knowledge_add(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_add requires a workspace root".to_string(),
    ))?;
    let title = input["title"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'title' parameter".to_string(),
    ))?;
    let content = input["content"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'content' parameter".to_string(),
    ))?;

    // Reject content that looks like credentials/secrets — these belong in kv_set
    let content_lower = content.to_lowercase();
    let sensitive_patterns = [
        "app_secret",
        "app_id",
        "api_key",
        "apikey",
        "secret_key",
        "access_token",
        "private_key",
    ];
    let matched = sensitive_patterns
        .iter()
        .find(|p| content_lower.contains(*p));
    if let Some(pattern) = matched {
        return Err(CarrierError::InvalidInput(format!(
            "Rejected: content contains '{pattern}' which looks like credentials/secrets. \
             Use kv_set to store private data in your personal key-value store instead of knowledge_add."
        )));
    }

    let filename = knowledge_add_core(root, title, content, "tool").await?;
    Ok(format!("Knowledge added: {filename}.md"))
}

/// In-place update of an existing knowledge file. Fuzzy-matches the target
/// (same matcher as knowledge_remove), validates the replacement keeps a
/// frontmatter block, rejects credential-looking content (same patterns as
/// knowledge_add), and records a "update" version entry with before/after so
/// self-evolution stays auditable.
async fn tool_knowledge_update(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_update requires a workspace root".to_string(),
    ))?;
    let filename = input["filename"]
        .as_str()
        .ok_or(CarrierError::InvalidInput(
            "Missing 'filename' parameter".to_string(),
        ))?;
    let content = input["content"]
        .as_str()
        .ok_or(CarrierError::InvalidInput(
            "Missing 'content' parameter".to_string(),
        ))?;

    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err(CarrierError::InvalidInput(
            "Invalid filename: path separators and '..' are forbidden".to_string(),
        ));
    }

    // The replacement must be a complete knowledge file: frontmatter fence at
    // the top plus a closing fence. Structured error so the agent can self-heal.
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") || trimmed[3..].find("\n---").is_none() {
        return Err(CarrierError::InvalidInput(
            "content 必须是完整知识文件：以 --- frontmatter 开头（name/description 等），并有闭合的 ---。先 knowledge_read 原文件，在原内容上修改后整体传回。".to_string()
        ));
    }

    // Same credential guard as knowledge_add — updates must not leak secrets
    // into the shared knowledge base either.
    let content_lower = content.to_lowercase();
    let sensitive_patterns = [
        "app_secret",
        "app_id",
        "api_key",
        "apikey",
        "secret_key",
        "access_token",
        "private_key",
    ];
    if let Some(pattern) = sensitive_patterns
        .iter()
        .find(|p| content_lower.contains(*p))
    {
        return Err(CarrierError::InvalidInput(format!(
            "Rejected: content contains '{pattern}' which looks like credentials/secrets. \
             Use kv_set to store private data in your personal key-value store instead of knowledge_update."
        )));
    }

    let knowledge_dir = root.join("knowledge");
    let target = find_knowledge_file(&knowledge_dir, filename)?;
    let name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let before = tokio::fs::read_to_string(&target)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to read existing file: {e}")))?;

    tokio::fs::write(&target, content)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to write knowledge file: {e}")))?;

    let _ = lifecycle::version::record_version(
        root,
        "update",
        &name,
        Some(before.as_str()),
        Some(content),
        "tool",
    );
    let _ = lifecycle::evolution::update_memory_index(root);

    Ok(format!(
        "Knowledge updated in place: {name} (version recorded, index rebuilt)"
    ))
}

async fn tool_knowledge_extract(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_extract requires a workspace root".to_string(),
    ))?;
    let title = input["title"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'title' parameter".to_string(),
    ))?;
    let content = input["content"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'content' parameter".to_string(),
    ))?;

    let candidate = lifecycle::evolution::KnowledgeCandidate {
        title: title.to_string(),
        content: content.to_string(),
        scope: "shared".to_string(),
    };
    let analysis = lifecycle::evolution::EvolutionAnalysis {
        knowledge: vec![candidate],
        gaps: vec![],
        trivial: false,
    };
    let saved = lifecycle::evolution::apply_evolution(root, &analysis, None, None, None);
    match saved.len() {
        0 => Ok("No knowledge extracted (nothing new to save).".to_string()),
        n => Ok(format!(
            "Extracted {n} knowledge item(s) and updated index."
        )),
    }
}

async fn tool_knowledge_index(workspace_root: Option<&Path>) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_index requires a workspace root".to_string(),
    ))?;
    lifecycle::evolution::update_memory_index(root)
        .map_err(|e| CarrierError::Internal(format!("Failed to rebuild index: {e}")))?;
    Ok("Knowledge index (MEMORY.md) rebuilt successfully.".to_string())
}

fn parse_string_list(input: &serde_json::Value, key: &str) -> Vec<String> {
    input[key]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Format a YAML frontmatter `tools:` list block.
fn format_tools_yaml(tools: &[String]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = String::from("tools:\n");
    for t in tools {
        out.push_str(&format!("  - {t}\n"));
    }
    out
}

/// Split a flow file into (frontmatter_inner, body). frontmatter_inner excludes the `---` fences.
fn split_flow_file(content: &str) -> (Option<String>, String) {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---") {
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        if let Some(end) = rest.find("\n---") {
            let fm = rest[..end].to_string();
            let after = &rest[end + 4..]; // skip \n---
            let body = after.strip_prefix('\n').unwrap_or(after).to_string();
            return (Some(fm), body);
        }
    }
    (None, content.to_string())
}

/// Replace or insert `tools:` in YAML frontmatter text (without fences).
fn upsert_frontmatter_tools(fm: &str, tools: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut skipping_tools_list = false;
    let mut tools_written = false;
    for line in fm.lines() {
        let trimmed = line.trim();
        if skipping_tools_list {
            // Continue skipping multi-line list items under tools:
            if trimmed.starts_with('-') || trimmed.is_empty() {
                continue;
            }
            // Also skip inline tools: [...] was a single line already consumed
            skipping_tools_list = false;
        }
        if trimmed.starts_with("tools:") {
            if !tools_written {
                lines.push(format_tools_yaml(tools).trim_end().to_string());
                tools_written = true;
            }
            // Skip old tools value: either same-line list or following `-` lines
            if trimmed == "tools:" || trimmed.ends_with(':') {
                skipping_tools_list = true;
            }
            continue;
        }
        // Drop deprecated toolsets so tools: is the single source of truth
        if trimmed.starts_with("toolsets:") {
            if trimmed == "toolsets:" || trimmed.ends_with(':') && !trimmed.contains('[') {
                skipping_tools_list = true;
            }
            continue;
        }
        lines.push(line.to_string());
    }
    if !tools_written && !tools.is_empty() {
        lines.push(format_tools_yaml(tools).trim_end().to_string());
    }
    lines.join("\n")
}

fn upsert_frontmatter_description(fm: &str, description: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut written = false;
    for line in fm.lines() {
        if line.trim().starts_with("description:") {
            lines.push(format!("description: {description}"));
            written = true;
        } else {
            lines.push(line.to_string());
        }
    }
    if !written {
        lines.push(format!("description: {description}"));
    }
    lines.join("\n")
}

async fn tool_flow_create(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "flow_create requires a workspace root".to_string(),
    ))?;
    let name = input["name"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'name' parameter".to_string(),
    ))?;
    let description = input["description"].as_str().unwrap_or("");
    let body = input["body"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'body' parameter".to_string(),
    ))?;
    // Prefer `tools`; accept legacy `toolsets` as alias (same list of tool names).
    let mut tools = parse_string_list(input, "tools");
    if tools.is_empty() {
        tools = parse_string_list(input, "toolsets");
    }

    let flows_dir = root.join("flows");
    tokio::fs::create_dir_all(&flows_dir)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to create flows dir: {e}")))?;

    let filename = lifecycle::evolution::sanitize_filename(name);
    let path = flows_dir.join(format!("{filename}.md"));

    if path.exists() {
        return Err(CarrierError::InvalidInput(format!(
            "Flow '{name}' already exists. Use flow_update to modify it."
        )));
    }

    let mut frontmatter = format!("---\nname: {name}\n");
    if !description.is_empty() {
        frontmatter.push_str(&format!("description: {description}\n"));
    }
    if !tools.is_empty() {
        frontmatter.push_str(&format_tools_yaml(&tools));
    }
    frontmatter.push_str("---\n");

    let full = format!("{frontmatter}\n{body}");
    tokio::fs::write(&path, &full)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to write flow: {e}")))?;

    Ok(format!(
        "Flow '{name}' created successfully{}.",
        if tools.is_empty() {
            String::new()
        } else {
            format!(" with tools: [{}]", tools.join(", "))
        }
    ))
}

async fn tool_flow_update(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "flow_update requires a workspace root".to_string(),
    ))?;
    let name = input["name"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'name' parameter".to_string(),
    ))?;
    let new_body = input["body"].as_str().filter(|s| !s.is_empty());
    let new_tools = {
        let t = parse_string_list(input, "tools");
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    };
    let new_description = input["description"].as_str().filter(|s| !s.is_empty());

    if new_body.is_none() && new_tools.is_none() && new_description.is_none() {
        return Err(CarrierError::InvalidInput(
            "flow_update requires at least one of: body, tools, description (non-empty)"
                .to_string(),
        ));
    }

    // Only workspace flows are updateable — system-shared flows are abolished
    // ("全进分身"), every flow lives in the clone's workspace/flows/.
    let private_flows = root.join("flows");
    let private_path = find_flow_path(&private_flows, name).await;

    let source_path = match &private_path {
        Some(p) => p.clone(),
        None => {
            return Err(CarrierError::InvalidInput(format!(
                "Flow '{name}' not found in workspace flows."
            )));
        }
    };

    let existing = tokio::fs::read_to_string(&source_path)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to read flow: {e}")))?;

    let (fm_opt, old_body) = split_flow_file(&existing);
    let mut fm = fm_opt.unwrap_or_else(|| format!("name: {name}"));
    if let Some(desc) = new_description {
        fm = upsert_frontmatter_description(&fm, desc);
    }
    if let Some(ref tools) = new_tools {
        fm = upsert_frontmatter_tools(&fm, tools);
    }
    let body = new_body.unwrap_or(old_body.as_str());
    let updated = format!("---\n{}\n---\n\n{}", fm.trim(), body.trim_start());

    // Write in place at the private path (shared flows are refused above, so
    // source_path is always a private workspace flow).
    let target = source_path;

    tokio::fs::write(&target, &updated)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to write flow: {e}")))?;

    let mut notes = Vec::new();
    if let Some(ref tools) = new_tools {
        notes.push(format!("tools=[{}]", tools.join(", ")));
    }
    if new_body.is_some() {
        notes.push("body updated".to_string());
    }
    if new_description.is_some() {
        notes.push("description updated".to_string());
    }

    Ok(format!(
        "Flow '{name}' updated successfully ({}).",
        notes.join("; ")
    ))
}

async fn tool_flow_load(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "flow_load requires a workspace root".to_string(),
    ))?;
    let name = input["name"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'name' parameter".to_string(),
    ))?;

    // Only the clone's own workspace flows are loadable — system-shared flows
    // (~/.opencarrier/flows/) are no longer scanned ("全进分身").
    let dirs = [root.join("flows")];
    for flows_dir in dirs {
        if let Some(path) = find_flow_path(&flows_dir, name).await {
            return tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| CarrierError::Internal(format!("Failed to read flow: {e}")));
        }
    }

    Err(CarrierError::InvalidInput(format!(
        "Flow '{name}' not found."
    )))
}

/// Locate a flow file by name within a flows directory.
///
/// Tries exact flat (`{name}.md`), exact directory (`{name}/flow.md`, falling
/// back to legacy `{name}/SKILL.md`), then a case-insensitive fuzzy match on
/// entry names. Returns the path if found.
async fn find_flow_path(flows_dir: &Path, name: &str) -> Option<PathBuf> {
    if !flows_dir.is_dir() {
        return None;
    }
    let filename = lifecycle::evolution::sanitize_filename(name);
    let flat_path = flows_dir.join(format!("{filename}.md"));
    if flat_path.exists() {
        return Some(flat_path);
    }
    let dir = flows_dir.join(&filename);
    let dir_flow = dir.join("flow.md");
    if dir_flow.exists() {
        return Some(dir_flow);
    }
    let dir_skill = dir.join("SKILL.md");
    if dir_skill.exists() {
        return Some(dir_skill);
    }

    // Fuzzy match on entry names
    let mut entries = tokio::fs::read_dir(flows_dir).await.ok()?;
    while let Some(entry) = entries.next_entry().await.ok()? {
        let entry_name = entry.file_name().to_string_lossy().to_string();
        if !entry_name.to_lowercase().contains(&name.to_lowercase()) {
            continue;
        }
        if entry_name.ends_with(".md") {
            return Some(entry.path());
        }
        if entry.path().is_dir() {
            let flow_md = entry.path().join("flow.md");
            if flow_md.exists() {
                return Some(flow_md);
            }
            let skill_md = entry.path().join("SKILL.md");
            if skill_md.exists() {
                return Some(skill_md);
            }
        }
    }
    None
}

async fn tool_session_summarize(
    input: &serde_json::Value,
    memory: Option<&Arc<dyn crate::memory_handle::MemoryHandle>>,
    caller_agent_id: Option<&str>,
    sender_id: Option<&str>,
) -> CarrierResult<String> {
    let mem = memory.ok_or(CarrierError::Internal(
        "session_summarize requires memory access".to_string(),
    ))?;
    let agent_id = caller_agent_id.ok_or(CarrierError::Internal(
        "session_summarize requires caller agent ID".to_string(),
    ))?;
    let sid = sender_id.unwrap_or("");
    let summary = input["summary"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'summary' parameter".to_string(),
    ))?;

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let key = format!("session_summary:{date}");

    mem.kv_set(
        agent_id,
        sid,
        sid,
        &key,
        serde_json::Value::String(summary.to_string()),
    )?;

    Ok(format!("Session summary stored for {date}."))
}

async fn tool_knowledge_remove(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_remove requires a workspace root".to_string(),
    ))?;
    let query = input["filename"]
        .as_str()
        .ok_or(CarrierError::InvalidInput(
            "Missing 'filename' parameter".to_string(),
        ))?;
    let knowledge_dir = root.join("knowledge");
    let target = find_knowledge_file(&knowledge_dir, query)?;
    let before = tokio::fs::read_to_string(&target).await.ok();
    tokio::fs::remove_file(&target)
        .await
        .map_err(|e| CarrierError::Internal(format!("Failed to delete: {e}")))?;
    let name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let _ =
        lifecycle::version::record_version(root, "delete", &name, before.as_deref(), None, "tool");
    let _ = lifecycle::evolution::update_memory_index(root);
    Ok(format!("Knowledge removed: {name}"))
}

async fn tool_knowledge_import(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "knowledge_import requires a workspace root".to_string(),
    ))?;
    let data = input["data"].as_str().ok_or(CarrierError::InvalidInput(
        "Missing 'data' parameter".to_string(),
    ))?;
    let data_type = input["data_type"].as_str().unwrap_or("auto");
    let (saved, quality) = knowledge_import_core(root, data, data_type).await?;
    Ok(format!(
        "Imported {} entries as knowledge files. Quality: {:?}",
        saved.len(),
        quality
    ))
}

pub(crate) async fn tool_clone_evaluate(workspace_root: Option<&Path>) -> CarrierResult<String> {
    let root = workspace_root.ok_or(CarrierError::Internal(
        "clone_evaluate requires a workspace root".to_string(),
    ))?;
    let metrics = lifecycle::evaluate::compute_deterministic_metrics(root);
    Ok(format!(
        "Quality Score: {}/100 ({})\nKnowledge: {} files, {} bytes\nSkills: {}\nIdentity: SOUL={}, SP={}, MEMORY={}",
        metrics.score,
        metrics.grade,
        metrics.knowledge_files,
        metrics.knowledge_total_bytes,
        metrics.flow_count,
        metrics.has_soul,
        metrics.has_system_prompt,
        metrics.has_memory,
    ))
}

/// Fuzzy-match a knowledge file by name (exact -> prefix -> substring).
fn find_knowledge_file(knowledge_dir: &Path, query: &str) -> CarrierResult<PathBuf> {
    let entries = std::fs::read_dir(knowledge_dir)?;
    let query_lower = query.to_lowercase();
    let query_no_ext = query_lower.trim_end_matches(".md");

    let candidates: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|ext| ext == "md").unwrap_or(false))
        .map(|e| e.path())
        .collect();

    // Exact match
    if let Some(exact) = candidates.iter().find(|p| {
        p.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            == query_lower
            || p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase()
                == format!("{query_no_ext}.md")
    }) {
        return Ok(exact.clone());
    }

    // Prefix match
    if let Some(prefix) = candidates.iter().find(|p| {
        p.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .starts_with(query_no_ext)
    }) {
        return Ok(prefix.clone());
    }

    // Substring match
    if let Some(sub) = candidates.iter().find(|p| {
        p.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .contains(query_no_ext)
    }) {
        return Ok(sub.clone());
    }

    Err(CarrierError::InvalidInput(format!(
        "No knowledge file matching '{}' found",
        query
    )))
}

#[cfg(test)]
mod apply_patch_routing_tests {
    use super::*;
    use crate::tool_context::ToolContext;

    /// ToolContext with sender routing fields set, mirroring a sender-scoped
    /// chat turn (home_dir + sender_id + agent_name + workspace_root).
    fn sender_ctx<'a>(
        home: &'a std::path::Path,
        workspace: &'a std::path::Path,
    ) -> ToolContext<'a> {
        ToolContext {
            kernel: None,
            memory: None,
            caller_agent_id: None,
            mcp_connections: None,
            fetch_engine: None,
            allowed_env_vars: None,
            workspace_root: Some(workspace),
            brain: None,
            exec_policy: None,
            cli_exec_config: None,
            process_manager: None,
            sender_id: Some("u1"),
            owner_id: None,
            home_dir: Some(home),
            agent_name: Some("ag"),
            subagent_configs: None,
            channel_type: None,
            max_tool_level: types::tool::PermissionLevel::Write,
            is_clone_admin: false,
            external_url: None,
            flow_elevated_tools: None,
            flow_shell_allow: None,
            flow_deny_tools: None,
            flow_allowed_tools: None,
        }
    }

    #[tokio::test]
    async fn apply_patch_routes_output_to_sender_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let workspace = home.join("workspaces/ag");
        std::fs::create_dir_all(&workspace).unwrap();

        // AddFile via patch -> must land in senders/u1/output/, not workspace/output/
        let ctx = sender_ctx(home, &workspace);
        let add = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: output/p1/material.md\n+hello\n*** End Patch\n"
        });
        let out = tool_apply_patch(&add, Some(&workspace), &ctx).await.unwrap();
        assert!(out.contains("added"), "{out}");
        let sender_file = home.join("workspaces/ag/senders/u1/output/p1/material.md");
        assert!(sender_file.exists(), "file must land in sender output dir");
        assert!(!workspace.join("output/p1/material.md").exists());

        // DeleteFile via patch on the same logical path must find it there too
        // (2026-08-21 86bus incident: delete failed with "No such file").
        let del = serde_json::json!({
            "patch": "*** Begin Patch\n*** Delete File: output/p1/material.md\n*** End Patch\n"
        });
        tool_apply_patch(&del, Some(&workspace), &ctx).await.unwrap();
        assert!(!sender_file.exists(), "delete must remove the sender-scoped file");
    }

    #[tokio::test]
    async fn apply_patch_rejects_replacement_char_path() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let workspace = home.join("workspaces/ag");
        std::fs::create_dir_all(&workspace).unwrap();
        let ctx = sender_ctx(home, &workspace);
        let bad = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: output/\u{FFFD}\u{FFFD}材.md\n+x\n*** End Patch\n"
        });
        let err = tool_apply_patch(&bad, Some(&workspace), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("U+FFFD"), "{err}");
    }

    #[tokio::test]
    async fn apply_patch_rejects_input_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let workspace = home.join("workspaces/ag");
        std::fs::create_dir_all(&workspace).unwrap();
        let ctx = sender_ctx(home, &workspace);
        let bad = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: input/recvd.md\n+x\n*** End Patch\n"
        });
        let err = tool_apply_patch(&bad, Some(&workspace), &ctx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("只读"), "{err}");
    }

    #[tokio::test]
    async fn apply_patch_internal_paths_stay_in_workspace() {
        // knowledge/ is an internal path - must NOT be rerouted to the sender dir.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let workspace = home.join("workspaces/ag");
        std::fs::create_dir_all(workspace.join("knowledge")).unwrap();
        let ctx = sender_ctx(home, &workspace);
        let add = serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: knowledge/new.md\n+x\n*** End Patch\n"
        });
        tool_apply_patch(&add, Some(&workspace), &ctx).await.unwrap();
        assert!(workspace.join("knowledge/new.md").exists());
        assert!(!home.join("workspaces/ag/senders/u1/output/knowledge/new.md").exists());
    }
}

#[cfg(test)]
mod knowledge_update_tests {
    use super::*;

    fn input(filename: &str, content: &str) -> serde_json::Value {
        serde_json::json!({ "filename": filename, "content": content })
    }

    #[tokio::test]
    async fn update_replaces_in_place_with_fuzzy_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let saved = knowledge_add_core(root, "business-info", "old body", "test")
            .await
            .unwrap();
        assert_eq!(saved, "business-info");

        let new_content = "---\nname: business-info\ndescription: 86巴士业务\n---\nnew body\n";
        let out = tool_knowledge_update(&input("business", new_content), Some(root))
            .await
            .unwrap();
        assert!(out.contains("business-info.md"), "{out}");
        // Same path, content replaced in place
        let on_disk = std::fs::read_to_string(root.join("knowledge/business-info.md")).unwrap();
        assert!(on_disk.contains("new body"));
        assert!(!on_disk.contains("old body"));
    }

    #[tokio::test]
    async fn update_rejects_missing_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        knowledge_add_core(root, "policy", "old", "test").await.unwrap();
        let err = tool_knowledge_update(&input("policy.md", "just body, no frontmatter"), Some(root))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "{err}");
    }

    #[tokio::test]
    async fn update_rejects_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        knowledge_add_core(root, "policy", "old", "test").await.unwrap();
        let bad = "---\nname: p\n---\napp_secret = wx123\n";
        let err = tool_knowledge_update(&input("policy.md", bad), Some(root))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("app_secret"), "{err}");
    }

    #[tokio::test]
    async fn update_unknown_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("knowledge")).unwrap();
        let err = tool_knowledge_update(
            &input("nope.md", "---\nname: n\n---\nbody\n"),
            Some(root),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("No knowledge file"), "{err}");
    }

    #[tokio::test]
    async fn update_rejects_path_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let err = tool_knowledge_update(&input("../evil.md", "---\nname: e\n---\nx\n"), Some(root))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("forbidden"), "{err}");
    }
}

#[cfg(test)]
mod flow_evolution_tests {
    use super::*;

    #[test]
    fn upsert_tools_replaces_inline_list() {
        let fm = "name: article-writer\ntools: [\"file_read\"]\nversion: 7\n";
        let out = upsert_frontmatter_tools(fm, &["file_read".into(), "file_write".into()]);
        assert!(out.contains("file_write"));
        assert!(out.contains("file_read"));
        assert!(out.contains("version: 7"));
        assert!(!out.contains("tools: [\"file_read\"]"));
    }

    #[test]
    fn upsert_tools_replaces_multiline_list() {
        let fm = "name: x\ntools:\n  - file_read\n  - old_tool\nversion: 1\n";
        let out = upsert_frontmatter_tools(fm, &["file_write".into()]);
        assert!(out.contains("- file_write"));
        assert!(!out.contains("old_tool"));
        assert!(out.contains("version: 1"));
    }

    #[test]
    fn split_flow_preserves_body() {
        let content = "---\nname: x\n---\n\n# Body\n\nstep 1\n";
        let (fm, body) = split_flow_file(content);
        assert!(fm.unwrap().contains("name: x"));
        assert!(body.contains("# Body"));
    }
}
