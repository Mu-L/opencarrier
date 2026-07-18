//! Knowledge and skill management tool module.
//!
//! Provides tools for reading, writing, linting, healing, importing, and
//! extracting knowledge files, managing skills, evaluating clone quality,
//! applying patches, and saving session summaries.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
                description: "Update an existing flow after a successful discovery path. Can replace body and/or tools: frontmatter so next runs inject the proven tools without tool_search. Shared system flows are copy-on-write into the workspace private flows/.".to_string(),
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
    ) -> Option<Result<String, String>> {
        match name {
            "knowledge_list" => Some(tool_knowledge_list(ctx.workspace_root).await),
            "knowledge_read" => Some(tool_knowledge_read(input, ctx.workspace_root).await),
            "apply_patch" => Some(tool_apply_patch(input, ctx.workspace_root).await),
            "knowledge_lint" => Some(tool_knowledge_lint(ctx.workspace_root).await),
            "knowledge_heal" => Some(tool_knowledge_heal(ctx.workspace_root).await),
            "knowledge_add" => Some(tool_knowledge_add(input, ctx.workspace_root).await),
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
            "knowledge_list" | "knowledge_read" | "session_summarize"
            | "flow_load" | "clone_evaluate" => types::tool::PermissionLevel::None,
            "knowledge_lint" | "knowledge_index" | "knowledge_extract"
            | "train_read" | "train_list"
            | "train_evaluate" | "user_profile" => types::tool::PermissionLevel::ReadOnly,
            "knowledge_add" | "knowledge_remove" | "knowledge_import"
            | "knowledge_heal" | "flow_create" | "flow_update"
            | "apply_patch" | "train_write" => types::tool::PermissionLevel::Write,
            _ => types::tool::PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// Knowledge tools (safe access to knowledge/)
// ---------------------------------------------------------------------------

pub(crate) async fn tool_knowledge_list(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_list requires a workspace root")?;
    let knowledge_dir = root.join("knowledge");

    if !knowledge_dir.exists() {
        return Ok("No knowledge files found (knowledge/ does not exist).".to_string());
    }

    let mut entries = tokio::fs::read_dir(&knowledge_dir)
        .await
        .map_err(|e| format!("Failed to read knowledge directory: {e}"))?;

    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("Failed to read entry: {e}"))?
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
) -> Result<String, String> {
    let filename = input["filename"]
        .as_str()
        .ok_or("Missing 'filename' parameter")?;
    let root = workspace_root.ok_or("knowledge_read requires a workspace root")?;

    // Security: validate filename (no path traversal)
    if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
        return Err("Invalid filename: path separators and '..' are forbidden".to_string());
    }
    if !filename.ends_with(".md") {
        return Err("Only .md knowledge files can be read".to_string());
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
                        if name.ends_with(".md") { Some(name) } else { None }
                    })
                    .collect();
                names.sort();
                names
            })
            .unwrap_or_default();
        if available.is_empty() {
            return Ok(format!("Knowledge file '{}' not found. No knowledge files exist yet.", filename));
        }
        return Ok(format!(
            "Knowledge file '{}' not found. Available files: {}",
            filename,
            available.join(", ")
        ));
    }

    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read knowledge file: {e}"))
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
) -> Result<String, String> {
    let patch_str = input["patch"].as_str().ok_or("Missing 'patch' parameter")?;
    let root = workspace_root.ok_or("apply_patch requires a workspace root")?;
    let ops = crate::apply_patch::parse_patch(patch_str)?;
    let result = crate::apply_patch::apply_patch(&ops, root).await;
    if result.is_ok() {
        Ok(result.summary())
    } else {
        Err(format!(
            "Patch partially applied: {}. Errors: {}",
            result.summary(),
            result.errors.join("; ")
        ))
    }
}

// ---------------------------------------------------------------------------
// Lifecycle system tools (clone knowledge management)
// ---------------------------------------------------------------------------

pub(crate) async fn tool_knowledge_lint(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_lint requires a workspace root")?;
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

pub(crate) async fn tool_knowledge_heal(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_heal requires a workspace root")?;
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
) -> Result<String, String> {
    let filename = lifecycle::evolution::sanitize_filename(title);
    let knowledge_dir = root.join("knowledge");
    tokio::fs::create_dir_all(&knowledge_dir)
        .await
        .map_err(|e| format!("Failed to create knowledge dir: {e}"))?;
    let path = knowledge_dir.join(format!("{filename}.md"));
    let full = format!(
        "---\nname: {}\ndescription: {}\nconfidence: EXTRACTED\n---\n{}\n---\n",
        title, title, content
    );
    tokio::fs::write(&path, &full)
        .await
        .map_err(|e| format!("Failed to write knowledge file: {e}"))?;
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
) -> Result<(Vec<String>, lifecycle::parsers::ParseQuality), String> {
    let result = lifecycle::parsers::parse_import_data(data, data_type)
        .map_err(|e| format!("Parse failed: {e}"))?;
    let knowledge_dir = root.join("knowledge");
    tokio::fs::create_dir_all(&knowledge_dir)
        .await
        .map_err(|e| format!("Failed to create knowledge dir: {e}"))?;
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
            .map_err(|e| format!("Failed to write {}: {e}", filename))?;
        saved.push(filename);
    }
    Ok((saved, result.quality))
}

async fn tool_knowledge_add(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_add requires a workspace root")?;
    let title = input["title"].as_str().ok_or("Missing 'title' parameter")?;
    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;

    // Reject content that looks like credentials/secrets — these belong in kv_set
    let content_lower = content.to_lowercase();
    let sensitive_patterns = ["app_secret", "app_id", "api_key", "apikey", "secret_key", "access_token", "private_key"];
    let matched = sensitive_patterns.iter().find(|p| content_lower.contains(*p));
    if let Some(pattern) = matched {
        return Err(format!(
            "Rejected: content contains '{pattern}' which looks like credentials/secrets. \
             Use kv_set to store private data in your personal key-value store instead of knowledge_add."
        ));
    }

    let filename = knowledge_add_core(root, title, content, "tool").await?;
    Ok(format!("Knowledge added: {filename}.md"))
}

async fn tool_knowledge_extract(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_extract requires a workspace root")?;
    let title = input["title"].as_str().ok_or("Missing 'title' parameter")?;
    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;

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

async fn tool_knowledge_index(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_index requires a workspace root")?;
    lifecycle::evolution::update_memory_index(root)
        .map_err(|e| format!("Failed to rebuild index: {e}"))?;
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
) -> Result<String, String> {
    let root = workspace_root.ok_or("flow_create requires a workspace root")?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;
    let description = input["description"].as_str().unwrap_or("");
    let body = input["body"].as_str().ok_or("Missing 'body' parameter")?;
    // Prefer `tools`; accept legacy `toolsets` as alias (same list of tool names).
    let mut tools = parse_string_list(input, "tools");
    if tools.is_empty() {
        tools = parse_string_list(input, "toolsets");
    }

    let flows_dir = root.join("flows");
    tokio::fs::create_dir_all(&flows_dir)
        .await
        .map_err(|e| format!("Failed to create flows dir: {e}"))?;

    let filename = lifecycle::evolution::sanitize_filename(name);
    let path = flows_dir.join(format!("{filename}.md"));

    if path.exists() {
        return Err(format!(
            "Flow '{name}' already exists. Use flow_update to modify it."
        ));
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
        .map_err(|e| format!("Failed to write flow: {e}"))?;

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
) -> Result<String, String> {
    let root = workspace_root.ok_or("flow_update requires a workspace root")?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;
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
        return Err(
            "flow_update requires at least one of: body, tools, description (non-empty)".to_string(),
        );
    }

    let private_flows = root.join("flows");
    let shared_flows = types::config::home_dir().join("flows");
    let filename = lifecycle::evolution::sanitize_filename(name);

    // Prefer private workspace flow; fall back to shared (copy-on-write).
    let private_path = find_flow_path(&private_flows, name).await;
    let shared_path = find_flow_path(&shared_flows, name).await;

    let (source_path, cow_to_private) = match (&private_path, &shared_path) {
        (Some(p), _) => (p.clone(), false),
        (None, Some(p)) => (p.clone(), true),
        (None, None) => {
            return Err(format!(
                "Flow '{name}' not found in workspace or shared flows."
            ));
        }
    };

    let existing = tokio::fs::read_to_string(&source_path)
        .await
        .map_err(|e| format!("Failed to read flow: {e}"))?;

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

    // Write target: private path if exists; else COW into workspace/flows/{name}.md
    let target = if cow_to_private {
        tokio::fs::create_dir_all(&private_flows)
            .await
            .map_err(|e| format!("Failed to create private flows dir: {e}"))?;
        // Prefer dir format for new private overlays of shared dir-based flows
        if source_path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "flow.md" || n == "SKILL.md")
        {
            let dir = private_flows.join(&filename);
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| format!("Failed to create flow dir: {e}"))?;
            dir.join("flow.md")
        } else {
            private_flows.join(format!("{filename}.md"))
        }
    } else {
        source_path
    };

    tokio::fs::write(&target, &updated)
        .await
        .map_err(|e| format!("Failed to write flow: {e}"))?;

    let mut notes = Vec::new();
    if cow_to_private {
        notes.push("private overlay created (shared flow left unchanged)".to_string());
    }
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
) -> Result<String, String> {
    let root = workspace_root.ok_or("flow_load requires a workspace root")?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;

    // Search private flows first (workspace/flows), then fall back to
    // shared system flows (~/.opencarrier/flows). Private wins on collision.
    let dirs = [
        root.join("flows"),
        types::config::home_dir().join("flows"),
    ];
    for flows_dir in dirs {
        if let Some(path) = find_flow_path(&flows_dir, name).await {
            return tokio::fs::read_to_string(&path)
                .await
                .map_err(|e| format!("Failed to read flow: {e}"));
        }
    }

    Err(format!("Flow '{name}' not found."))
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
) -> Result<String, String> {
    let mem = memory.ok_or("session_summarize requires memory access")?;
    let agent_id = caller_agent_id.ok_or("session_summarize requires caller agent ID")?;
    let sid = sender_id.unwrap_or("");
    let summary = input["summary"]
        .as_str()
        .ok_or("Missing 'summary' parameter")?;

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let key = format!("session_summary:{date}");

    mem.kv_set(
        agent_id,
        sid,
        sid,
        &key,
        serde_json::Value::String(summary.to_string()),
    )
    .map_err(|e| format!("Failed to store summary: {e}"))?;

    Ok(format!("Session summary stored for {date}."))
}

async fn tool_knowledge_remove(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_remove requires a workspace root")?;
    let query = input["filename"]
        .as_str()
        .ok_or("Missing 'filename' parameter")?;
    let knowledge_dir = root.join("knowledge");
    let target = find_knowledge_file(&knowledge_dir, query)?;
    let before = tokio::fs::read_to_string(&target).await.ok();
    tokio::fs::remove_file(&target)
        .await
        .map_err(|e| format!("Failed to delete: {e}"))?;
    let name = target
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    let _ = lifecycle::version::record_version(
        root,
        "delete",
        &name,
        before.as_deref(),
        None,
        "tool",
    );
    let _ = lifecycle::evolution::update_memory_index(root);
    Ok(format!("Knowledge removed: {name}"))
}

async fn tool_knowledge_import(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_import requires a workspace root")?;
    let data = input["data"].as_str().ok_or("Missing 'data' parameter")?;
    let data_type = input["data_type"].as_str().unwrap_or("auto");
    let (saved, quality) = knowledge_import_core(root, data, data_type).await?;
    Ok(format!(
        "Imported {} entries as knowledge files. Quality: {:?}",
        saved.len(),
        quality
    ))
}

pub(crate) async fn tool_clone_evaluate(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("clone_evaluate requires a workspace root")?;
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
fn find_knowledge_file(knowledge_dir: &Path, query: &str) -> Result<PathBuf, String> {
    let entries = std::fs::read_dir(knowledge_dir).map_err(|e| e.to_string())?;
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

    Err(format!("No knowledge file matching '{}' found", query))
}

/// Append tool names to a skill .md file's `tools:` frontmatter field.
///
/// If a `tools:` line already exists in the frontmatter, new tools are merged
/// (duplicates removed). Otherwise, a new line is inserted after `name:`.
/// Uses atomic write (tmp + rename) for safety.
pub fn write_skill_tools(workspace: &Path, skill_name: &str, tools: &[String]) -> Result<(), String> {
    let flows_dir = workspace.join("flows");
    let filename = lifecycle::evolution::sanitize_filename(skill_name);
    let flat_path = flows_dir.join(format!("{filename}.md"));
    // Directory format: prefer canonical flow.md, fall back to legacy SKILL.md.
    let dir = flows_dir.join(&filename);
    let dir_flow = dir.join("flow.md");
    let dir_skill = dir.join("SKILL.md");

    let target = if flat_path.exists() {
        flat_path
    } else if dir_flow.exists() {
        dir_flow
    } else if dir_skill.exists() {
        dir_skill
    } else {
        return Err(format!("Flow '{skill_name}' not found"));
    };

    let content = std::fs::read_to_string(&target)
        .map_err(|e| format!("Failed to read flow: {e}"))?;

    if tools.is_empty() {
        return Ok(());
    }

    let updated = if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let fm = &rest[..end];
            let after_fm = &rest[end + 3..]; // skip closing ---

            if fm.contains("tools:") {
                // Merge into existing tools line
                let new_fm: String = fm.lines().map(|line| {
                    let trimmed = line.trim();
                    if trimmed.starts_with("tools:") {
                        // Parse existing list
                        if let Some(val) = trimmed.strip_prefix("tools:") {
                            let val = val.trim();
                            if val.starts_with('[') && val.ends_with(']') {
                                let inner = &val[1..val.len() - 1];
                                let mut existing: Vec<String> = if inner.is_empty() {
                                    Vec::new()
                                } else {
                                    inner.split(',').map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string()).filter(|s| !s.is_empty()).collect()
                                };
                                for t in tools {
                                    if !existing.contains(t) {
                                        existing.push(t.clone());
                                    }
                                }
                                let ts_str = existing.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
                                format!("tools: [{ts_str}]")
                            } else {
                                line.to_string()
                            }
                        } else {
                            line.to_string()
                        }
                    } else {
                        line.to_string()
                    }
                }).collect::<Vec<_>>().join("\n");
                format!("---\n{new_fm}---{after_fm}")
            } else {
                // Insert after name: line
                let mut new_fm = String::new();
                let mut inserted = false;
                for line in fm.lines() {
                    new_fm.push_str(line);
                    new_fm.push('\n');
                    if !inserted && line.trim().starts_with("name:") {
                        let ts_str = tools.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
                        new_fm.push_str(&format!("tools: [{ts_str}]"));
                        new_fm.push('\n');
                        inserted = true;
                    }
                }
                if !inserted {
                    let ts_str = tools.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ");
                    new_fm.push_str(&format!("tools: [{ts_str}]"));
                    new_fm.push('\n');
                }
                format!("---\n{new_fm}---{after_fm}")
            }
        } else {
            return Err("Invalid frontmatter: no closing ---".to_string());
        }
    } else {
        return Err("No frontmatter found in skill file".to_string());
    };

    // Atomic write
    let tmp_path = target.with_extension("tmp");
    std::fs::write(&tmp_path, &updated)
        .map_err(|e| format!("Failed to write temp file: {e}"))?;
    std::fs::rename(&tmp_path, &target)
        .map_err(|e| format!("Failed to rename temp file: {e}"))?;

    Ok(())
}

/// Read the tools field from a flow .md file's frontmatter.
/// Returns an empty Vec if the flow doesn't exist or has no tools.
pub fn read_skill_tools(workspace: &Path, skill_name: &str) -> Vec<String> {
    let flows_dir = workspace.join("flows");
    let filename = lifecycle::evolution::sanitize_filename(skill_name);
    let flat_path = flows_dir.join(format!("{filename}.md"));
    // Directory format: prefer canonical flow.md, fall back to legacy SKILL.md.
    let dir = flows_dir.join(&filename);
    let dir_flow = dir.join("flow.md");
    let dir_skill = dir.join("SKILL.md");

    let target = if flat_path.exists() {
        flat_path
    } else if dir_flow.exists() {
        dir_flow
    } else if dir_skill.exists() {
        dir_skill
    } else {
        return Vec::new();
    };

    let content = match std::fs::read_to_string(&target) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let rest = match content.strip_prefix("---") {
        Some(r) => r,
        None => return Vec::new(),
    };
    let end = match rest.find("---") {
        Some(e) => e,
        None => return Vec::new(),
    };
    let fm = &rest[..end];

    for line in fm.lines() {
        let trimmed = line.trim();
        if let Some(value) = trimmed.strip_prefix("tools:") {
            let val = value.trim();
            if val.starts_with('[') && val.ends_with(']') {
                let inner = &val[1..val.len() - 1];
                if inner.is_empty() {
                    return Vec::new();
                }
                return inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            // Multi-line list: tools:\n  - foo\n  - bar — parsed by caller via full content
            break;
        }
    }
    // Multi-line tools: under frontmatter
    let mut tools = Vec::new();
    let mut in_tools = false;
    for line in fm.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("tools:") {
            in_tools = true;
            continue;
        }
        if in_tools {
            if let Some(item) = trimmed.strip_prefix('-') {
                let t = item.trim().trim_matches('"').trim_matches('\'').to_string();
                if !t.is_empty() {
                    tools.push(t);
                }
            } else if !trimmed.is_empty() {
                break;
            }
        }
    }
    tools
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

    #[tokio::test]
    async fn flow_update_tools_and_cow_from_shared() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        // Point OPENCARRIER_HOME at temp so shared flows resolve under it
        // SAFETY: test-only env mutation
        std::env::set_var("OPENCARRIER_HOME", home.path());
        let shared = home.path().join("flows");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(
            shared.join("demo.md"),
            "---\nname: demo\ntools:\n  - file_read\n---\n\nold body\n",
        )
        .unwrap();

        let input = serde_json::json!({
            "name": "demo",
            "tools": ["file_read", "file_write"],
            "body": "new hard rules\n"
        });
        let msg = tool_flow_update(&input, Some(tmp.path()))
            .await
            .expect("update");
        assert!(msg.contains("private overlay") || msg.contains("updated"));
        let private = tmp.path().join("flows/demo.md");
        assert!(private.exists(), "COW private flow");
        let content = std::fs::read_to_string(&private).unwrap();
        assert!(content.contains("file_write"));
        assert!(content.contains("new hard rules"));
        // Shared unchanged
        let shared_content = std::fs::read_to_string(shared.join("demo.md")).unwrap();
        assert!(shared_content.contains("old body"));
        std::env::remove_var("OPENCARRIER_HOME");
    }
}
