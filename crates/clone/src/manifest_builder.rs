//! Build AgentManifest from an extracted v3 workspace.
//!
//! Replaces the old `convert_to_manifest(CloneData)` — instead of converting
//! from an in-memory struct, this reads the workspace files directly.

use std::path::Path;

use anyhow::Result;
use types::agent::{
    AgentManifest, CloneSource, ManifestCapabilities, ModelConfig, ResourceQuota,
};
use tracing::debug;

use crate::loader::TemplateManifest;

/// Build an `AgentManifest` from an extracted v3 workspace directory.
///
/// Reads `template.json`, `profile.md`, and scans `skills/` and `knowledge/`
/// to construct the manifest needed for `spawn_agent`.
pub fn build_manifest_from_workspace(
    workspace: &Path,
    name: &str,
    hub_template_id: Option<String>,
) -> Result<AgentManifest> {
    // 1. Read template.json
    let template = read_template_json(workspace);

    // 2. Read profile.md for description
    let description = read_profile_description(workspace);

    // 3. Scan skills/ directory
    let (skill_names, all_tools) = scan_skills(workspace);

    // 4. Collect knowledge file names
    let knowledge_files = collect_knowledge_files(workspace);

    // 5. Build tools list with evolution tools
    let mut tools = all_tools;
    let evolution_tools: &[&str] = &[
        "knowledge_add",
        "knowledge_list",
        "knowledge_read",
        "knowledge_lint",
        "knowledge_extract",
        "knowledge_index",
        "skill_create",
        "skill_update",
        "skill_load",
        "session_summarize",
        "file_read",
        "file_write",
        "file_list",
        "user_profile",
    ];
    for tool in evolution_tools {
        let t = tool.to_string();
        if !tools.contains(&t) {
            tools.push(t);
        }
    }

    // Default tools for chat clones
    if tools.len() == evolution_tools.len() {
        tools.push("web_fetch".into());
        tools.push("web_search".into());
    }

    tools.sort();
    tools.dedup();

    // 5.5 Derive auto_load_toolsets from the tool list
    let auto_load_toolsets = derive_auto_load_toolsets(&tools, &template);

    // 6. Build CloneSource
    let clone_source = CloneSource {
        template_name: name.to_string(),
        template_author: template
            .as_ref()
            .map(|t| t.author.clone())
            .unwrap_or_default(),
        installed_at: chrono::Utc::now().timestamp().to_string(),
        agx_version: template
            .as_ref()
            .map(|t| t.version.clone())
            .unwrap_or_else(|| "1".to_string()),
        hub_template_id,
        auto_upgrade: false,
    };

    // 7. Assemble manifest
    let manifest = AgentManifest {
        name: name.to_string(),
        display_name: template
            .as_ref()
            .map(|t| t.display_name.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_default(),
        version: template
            .as_ref()
            .map(|t| t.version.clone())
            .unwrap_or_else(|| "0.1.0".to_string()),
        description: if description.is_empty() {
            template
                .as_ref()
                .map(|t| t.description.clone())
                .unwrap_or_default()
        } else {
            description
        },
        author: template
            .as_ref()
            .map(|t| t.author.clone())
            .unwrap_or_default(),
        module: "builtin:chat".to_string(),
        schedule: types::agent::ScheduleMode::default(),
        model: ModelConfig {
            max_tokens: 8192,
            temperature: 0.7,
            system_prompt: String::new(), // clone prompts built dynamically from workspace
            modality: "chat".to_string(),
        },
        resources: ResourceQuota::default(),
        priority: types::agent::Priority::default(),
        capabilities: ManifestCapabilities {
            tools,
            network: vec!["*".to_string()],
            memory_read: vec!["*".to_string()],
            memory_write: vec!["self.*".to_string()],
            ..Default::default()
        },
        skills: skill_names,
        tags: template
            .as_ref()
            .map(|t| t.tags.clone())
            .unwrap_or_default(),
        clone_source: Some(clone_source),
        knowledge_files,
        plugins: template
            .as_ref()
            .map(|t| t.plugins.clone())
            .unwrap_or_default(),
        mcp_servers: template
            .as_ref()
            .map(|t| t.mcp_servers.clone())
            .unwrap_or_default(),
        auto_load_toolsets,
        generate_identity_files: false, // .agx already has identity files
        ..Default::default()
    };

    debug!(
        "Built manifest for '{}' from workspace: {} skills, {} knowledge files, {} tools",
        name,
        manifest.skills.len(),
        manifest.knowledge_files.len(),
        manifest.capabilities.tools.len(),
    );

    Ok(manifest)
}

fn read_template_json(workspace: &Path) -> Option<TemplateManifest> {
    let path = workspace.join("template.json");
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn read_profile_description(workspace: &Path) -> String {
    let content = match std::fs::read_to_string(workspace.join("profile.md")) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let frontmatter = &content[3..3 + end];
            for line in frontmatter.lines() {
                let trimmed = line.trim();
                if let Some(val) = trimmed.strip_prefix("description:") {
                    return val.trim().trim_matches('"').to_string();
                }
            }
        }
    }

    String::new()
}

/// Scan workspace/skills/ to collect skill names and union of allowed_tools.
fn scan_skills(workspace: &Path) -> (Vec<String>, Vec<String>) {
    let skills_dir = workspace.join("skills");
    if !skills_dir.is_dir() {
        return (Vec::new(), Vec::new());
    }

    let mut skill_names = Vec::new();
    let mut all_tools = Vec::new();

    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return (skill_names, all_tools);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_md = path.join("SKILL.md");
        if !skill_md.exists() {
            continue;
        }

        if let Ok(content) = std::fs::read_to_string(&skill_md) {
            let (name, allowed_tools) = parse_skill_frontmatter_simple(&content);
            skill_names.push(name);
            all_tools.extend(allowed_tools);
        }
    }

    (skill_names, all_tools)
}

/// Parse skill frontmatter to extract name and allowed_tools.
fn parse_skill_frontmatter_simple(content: &str) -> (String, Vec<String>) {
    let mut name = String::new();
    let mut allowed_tools = Vec::new();

    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let frontmatter = &rest[..end];
            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("name:") {
                    name = val.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(val) = line.strip_prefix("allowed_tools:") {
                    allowed_tools = crate::loader::parse_string_array(val.trim());
                }
            }
        }
    }

    (name, allowed_tools)
}

/// Collect knowledge file names from workspace/knowledge/.
fn collect_knowledge_files(workspace: &Path) -> Vec<String> {
    let knowledge_dir = workspace.join("knowledge");
    if !knowledge_dir.is_dir() {
        return Vec::new();
    }

    let mut files = Vec::new();
    collect_knowledge_recursive(&knowledge_dir, &knowledge_dir, &mut files);
    files.sort();
    files
}

fn collect_knowledge_recursive(base: &Path, current: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_knowledge_recursive(base, &path, files);
        } else if path.extension().map(|e| e == "md").unwrap_or(false) {
            if let Ok(rel) = path.strip_prefix(base) {
                files.push(rel.to_string_lossy().to_string());
            }
        }
    }
}

/// Map a tool name to its toolset. Returns None for core tools (always visible).
fn tool_to_toolset(name: &str) -> Option<&'static str> {
    match name {
        "session_summarize" | "tool_search" => None,
        n if n.starts_with("file_") || n == "apply_patch" => Some("filesystem"),
        "shell_exec" => Some("shell"),
        n if n.starts_with("knowledge_") || n.starts_with("skill_") || n == "clone_evaluate" => Some("knowledge"),
        n if n.starts_with("media_") || n.starts_with("image_") || n == "text_to_speech" || n == "speech_to_text" => Some("media"),
        n if n.starts_with("web_") => Some("web"),
        n if n.starts_with("agent_") || n.starts_with("train_") => Some("agent"),
        n if n.starts_with("location_") || n.starts_with("system_") || n == "user_profile" => Some("misc"),
        n if n.starts_with("process_") => Some("process"),
        _ => Some("misc"),
    }
}

/// Derive auto_load_toolsets from the capabilities.tools list and MCP servers.
fn derive_auto_load_toolsets(tools: &[String], template: &Option<TemplateManifest>) -> Vec<String> {
    let mut toolsets = std::collections::HashSet::new();

    // Map each declared tool to its toolset
    for tool in tools {
        if let Some(ts) = tool_to_toolset(tool) {
            toolsets.insert(ts.to_string());
        }
    }

    // MCP servers are also toolsets
    if let Some(t) = template {
        for server in &t.mcp_servers {
            toolsets.insert(server.clone());
        }
    }

    // All agents need knowledge tools to read their own knowledge base
    toolsets.insert("knowledge".to_string());

    let mut result: Vec<String> = toolsets.into_iter().collect();
    result.sort();
    result
}
