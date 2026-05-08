//! Knowledge and skill management tool module.
//!
//! Provides tools for reading, writing, linting, healing, importing, and
//! extracting knowledge files, managing skills, evaluating clone quality,
//! applying patches, and saving session summaries.

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use opencarrier_types::tool::ToolDefinition;
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
                description: "Read a specific knowledge file from the agent's knowledge base. Only files in data/knowledge/ are accessible.".to_string(),
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
                description: "Add a new knowledge entry to the clone's knowledge base.".to_string(),
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
                description: "Rebuild the knowledge index file (MEMORY.md) by scanning all knowledge files in data/knowledge/. Use after manually adding or removing knowledge files.".to_string(),
                input_schema: serde_json::json!({"type": "object", "properties": {}}),
            },
            ToolDefinition {
                name: "skill_create".to_string(),
                description: "Create a new skill file in the workspace skills/ directory. Skills define reusable workflows with steps and tool requirements.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Skill name (used as filename)"},
                        "when_to_use": {"type": "string", "description": "Brief description of when to activate this skill"},
                        "body": {"type": "string", "description": "The skill content: workflow steps, instructions, and examples (markdown)"},
                        "allowed_tools": {"type": "string", "description": "Comma-separated list of tools this skill needs (optional)"},
                    },
                    "required": ["name", "body"],
                }),
            },
            ToolDefinition {
                name: "skill_update".to_string(),
                description: "Update the body of an existing skill. Preserves the skill's frontmatter (name, when_to_use, allowed_tools).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Skill name to update"},
                        "body": {"type": "string", "description": "New skill body content (replaces existing)"},
                    },
                    "required": ["name", "body"],
                }),
            },
            ToolDefinition {
                name: "skill_load".to_string(),
                description: "Load the full content of a skill by name. Returns the complete skill file including frontmatter and body.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Skill name to load"},
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
            "skill_create" => Some(tool_skill_create(input, ctx.workspace_root).await),
            "skill_update" => Some(tool_skill_update(input, ctx.workspace_root).await),
            "skill_load" => Some(tool_skill_load(input, ctx.workspace_root).await),
            "session_summarize" => Some(
                tool_session_summarize(input, ctx.kernel, ctx.caller_agent_id, ctx.sender_id).await,
            ),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Knowledge tools (safe access to data/knowledge/)
// ---------------------------------------------------------------------------

pub(crate) async fn tool_knowledge_list(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_list requires a workspace root")?;
    let knowledge_dir = root.join("data/knowledge");

    if !knowledge_dir.exists() {
        return Ok("No knowledge files found (data/knowledge/ does not exist).".to_string());
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

    let path = root.join("data/knowledge").join(filename);

    if !path.exists() {
        return Err(format!("Knowledge file not found: {}", filename));
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
    let report = opencarrier_lifecycle::health::check_health(root);
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
    let report = opencarrier_lifecycle::health::check_health(root);
    let fixes = opencarrier_lifecycle::health::auto_fix(root, &report);
    Ok(format!("Fixed {} issue(s).", fixes))
}

/// Core logic for adding a knowledge file. Shared by tool and train versions.
pub(crate) async fn knowledge_add_core(
    root: &Path,
    title: &str,
    content: &str,
    source_label: &str,
) -> Result<String, String> {
    let filename = opencarrier_lifecycle::evolution::sanitize_filename(title);
    let knowledge_dir = root.join("data/knowledge");
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
    let _ = opencarrier_lifecycle::version::record_version(
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
) -> Result<(Vec<String>, opencarrier_lifecycle::parsers::ParseQuality), String> {
    let result = opencarrier_lifecycle::parsers::parse_import_data(data, data_type)
        .map_err(|e| format!("Parse failed: {e}"))?;
    let knowledge_dir = root.join("data/knowledge");
    tokio::fs::create_dir_all(&knowledge_dir)
        .await
        .map_err(|e| format!("Failed to create knowledge dir: {e}"))?;
    let mut saved = Vec::new();
    for entry in &result.entries {
        let filename = opencarrier_lifecycle::evolution::sanitize_filename(&entry.title);
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

    let candidate = opencarrier_lifecycle::evolution::KnowledgeCandidate {
        title: title.to_string(),
        content: content.to_string(),
    };
    let analysis = opencarrier_lifecycle::evolution::EvolutionAnalysis {
        knowledge: vec![candidate],
        gaps: vec![],
        trivial: false,
    };
    let saved = opencarrier_lifecycle::evolution::apply_evolution(root, &analysis);
    match saved.len() {
        0 => Ok("No knowledge extracted (nothing new to save).".to_string()),
        n => Ok(format!(
            "Extracted {n} knowledge item(s) and updated index."
        )),
    }
}

async fn tool_knowledge_index(workspace_root: Option<&Path>) -> Result<String, String> {
    let root = workspace_root.ok_or("knowledge_index requires a workspace root")?;
    opencarrier_lifecycle::evolution::update_memory_index(root)
        .map_err(|e| format!("Failed to rebuild index: {e}"))?;
    Ok("Knowledge index (MEMORY.md) rebuilt successfully.".to_string())
}

async fn tool_skill_create(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("skill_create requires a workspace root")?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;
    let when_to_use = input["when_to_use"].as_str().unwrap_or("");
    let body = input["body"].as_str().ok_or("Missing 'body' parameter")?;
    let allowed_tools = input["allowed_tools"].as_str().unwrap_or("");

    let skills_dir = root.join("skills");
    tokio::fs::create_dir_all(&skills_dir)
        .await
        .map_err(|e| format!("Failed to create skills dir: {e}"))?;

    let filename = opencarrier_lifecycle::evolution::sanitize_filename(name);
    let path = skills_dir.join(format!("{filename}.md"));

    if path.exists() {
        return Err(format!(
            "Skill '{name}' already exists. Use skill_update to modify it."
        ));
    }

    let mut frontmatter = format!("---\nname: {name}\n");
    if !when_to_use.is_empty() {
        frontmatter.push_str(&format!("when_to_use: {when_to_use}\n"));
    }
    if !allowed_tools.is_empty() {
        frontmatter.push_str(&format!("allowed_tools: {allowed_tools}\n"));
    }
    frontmatter.push_str("---\n");

    let full = format!("{frontmatter}\n{body}");
    tokio::fs::write(&path, &full)
        .await
        .map_err(|e| format!("Failed to write skill: {e}"))?;

    Ok(format!("Skill '{name}' created successfully."))
}

async fn tool_skill_update(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("skill_update requires a workspace root")?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;
    let body = input["body"].as_str().ok_or("Missing 'body' parameter")?;

    let skills_dir = root.join("skills");
    let filename = opencarrier_lifecycle::evolution::sanitize_filename(name);
    let flat_path = skills_dir.join(format!("{filename}.md"));
    let dir_path = skills_dir.join(&filename).join("SKILL.md");

    let target = if flat_path.exists() {
        flat_path
    } else if dir_path.exists() {
        dir_path
    } else {
        return Err(format!("Skill '{name}' not found."));
    };

    let existing = tokio::fs::read_to_string(&target)
        .await
        .map_err(|e| format!("Failed to read skill: {e}"))?;

    let updated = if let Some(rest) = existing.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let fm_end = end + 6;
            format!("{}\n{}", &existing[..fm_end].trim_end(), body)
        } else {
            body.to_string()
        }
    } else {
        body.to_string()
    };

    tokio::fs::write(&target, &updated)
        .await
        .map_err(|e| format!("Failed to write skill: {e}"))?;

    Ok(format!("Skill '{name}' updated successfully."))
}

async fn tool_skill_load(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
) -> Result<String, String> {
    let root = workspace_root.ok_or("skill_load requires a workspace root")?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;

    let skills_dir = root.join("skills");
    let filename = opencarrier_lifecycle::evolution::sanitize_filename(name);
    let flat_path = skills_dir.join(format!("{filename}.md"));
    let dir_path = skills_dir.join(&filename).join("SKILL.md");

    if flat_path.exists() {
        return tokio::fs::read_to_string(&flat_path)
            .await
            .map_err(|e| format!("Failed to read skill: {e}"));
    }
    if dir_path.exists() {
        return tokio::fs::read_to_string(&dir_path)
            .await
            .map_err(|e| format!("Failed to read skill: {e}"));
    }

    // Fuzzy match
    if skills_dir.exists() {
        let mut entries = tokio::fs::read_dir(&skills_dir)
            .await
            .map_err(|e| format!("Failed to read skills dir: {e}"))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| format!("Read error: {e}"))?
        {
            let entry_name = entry.file_name().to_string_lossy().to_string();
            if entry_name.to_lowercase().contains(&name.to_lowercase()) {
                if entry_name.ends_with(".md") {
                    return tokio::fs::read_to_string(entry.path())
                        .await
                        .map_err(|e| format!("Failed to read skill: {e}"));
                } else if entry.path().is_dir() {
                    let skill_md = entry.path().join("SKILL.md");
                    if skill_md.exists() {
                        return tokio::fs::read_to_string(&skill_md)
                            .await
                            .map_err(|e| format!("Failed to read skill: {e}"));
                    }
                }
            }
        }
    }

    Err(format!("Skill '{name}' not found."))
}

async fn tool_session_summarize(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let kh = kernel.ok_or("session_summarize requires kernel access")?;
    let agent_id = caller_agent_id.ok_or("session_summarize requires caller agent ID")?;
    let sid = sender_id.unwrap_or("");
    let summary = input["summary"]
        .as_str()
        .ok_or("Missing 'summary' parameter")?;

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let key = format!("session_summary:{date}");

    kh.memory_store(
        agent_id,
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
    let knowledge_dir = root.join("data/knowledge");
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
    let _ = opencarrier_lifecycle::version::record_version(
        root,
        "delete",
        &name,
        before.as_deref(),
        None,
        "tool",
    );
    let _ = opencarrier_lifecycle::evolution::update_memory_index(root);
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
    let metrics = opencarrier_lifecycle::evaluate::compute_deterministic_metrics(root);
    Ok(format!(
        "Quality Score: {}/100 ({})\nKnowledge: {} files, {} bytes\nSkills: {}\nIdentity: SOUL={}, SP={}, MEMORY={}",
        metrics.score,
        metrics.grade,
        metrics.knowledge_files,
        metrics.knowledge_total_bytes,
        metrics.skill_count,
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
