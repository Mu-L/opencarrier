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

    // 3. Scan skills/ directory — collect names and tools
    let scan_result = scan_skills(workspace);
    let skill_names = scan_result.names;
    let skill_tools = scan_result.tools;

    // 4. Collect knowledge file names
    let knowledge_files = collect_knowledge_files(workspace);

    // 5. Build tools list from skill-declared tools + evolution defaults
    // Core tools (file_read, tool_search, etc.) are auto-loaded at runtime and
    // should NOT be listed in capabilities.tools. MCP tools (mcp_*) are loaded
    // separately via mcp_servers config. So we only collect non-core builtins.
    let core_tools: &[&str] = &[
        "session_summarize", "tool_search", "skill_load",
        "knowledge_read", "knowledge_list",
        "file_read", "file_list",
        "cron_create", "cron_list", "cron_cancel",
        "memory_tree", "task_plan",
    ];

    let evolution_tools: &[&str] = &[
        "knowledge_add", "knowledge_list", "knowledge_read",
        "knowledge_lint", "knowledge_extract", "knowledge_index",
        "skill_create", "skill_update", "skill_load",
        "session_summarize", "file_read", "file_write",
        "file_list", "user_profile",
    ];

    let mut tools: Vec<String> = Vec::new();

    // Add evolution defaults (needed for self-evolution)
    for tool in evolution_tools {
        let t = tool.to_string();
        if !tools.contains(&t) {
            tools.push(t);
        }
    }

    // Add tools declared in skills (non-core builtins only, skip MCP tools)
    for tool in &skill_tools {
        if tool.starts_with("mcp_") { continue; }
        if core_tools.contains(&tool.as_str()) { continue; }
        if !tools.contains(tool) {
            tools.push(tool.clone());
        }
    }

    tools.sort();
    tools.dedup();

    // 5.5 Derive mcp_servers: merge template.json + skill-declared MCP tool prefixes
    let mut mcp_servers: Vec<String> = template
        .as_ref()
        .map(|t| t.mcp_servers.clone())
        .unwrap_or_default();
    for tool in &skill_tools {
        if let Some(server) = extract_mcp_server(tool) {
            if !mcp_servers.contains(&server) {
                mcp_servers.push(server);
            }
        }
    }
    mcp_servers.sort();

    // 5.6 Derive auto_load_toolsets from tools and mcp_servers
    let auto_load_toolsets = derive_auto_load_toolsets(&tools, &mcp_servers);

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
            network: vec![],
            memory_read: vec!["self.*".to_string()],
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
        mcp_servers,
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

/// Scan workspace/skills/ to collect skill names and all declared tools.
struct SkillScanResult {
    names: Vec<String>,
    tools: Vec<String>,
}

fn scan_skills(workspace: &Path) -> SkillScanResult {
    let skills_dir = workspace.join("skills");
    if !skills_dir.is_dir() {
        return SkillScanResult {
            names: Vec::new(),
            tools: Vec::new(),
        };
    }

    let mut names = Vec::new();
    let mut tools: Vec<String> = Vec::new();

    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return SkillScanResult { names, tools };
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
            if let Some(name) = parse_skill_name(&content) {
                names.push(name);
            }
            tools.extend(parse_skill_tools(&content));
        }
    }

    tools.sort();
    tools.dedup();

    SkillScanResult { names, tools }
}

/// Parse skill frontmatter to extract name.
fn parse_skill_name(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("---")?;
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            let name = val.trim().trim_matches('"').trim_matches('\'').to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

/// Parse skill frontmatter to extract tools list (builtin + MCP).
fn parse_skill_tools(content: &str) -> Vec<String> {
    let Some(rest) = content.strip_prefix("---") else { return Vec::new() };
    let Some(end) = rest.find("---") else { return Vec::new() };
    let frontmatter = &rest[..end];
    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("tools:") {
            return parse_yaml_string_list(val.trim());
        }
        if let Some(val) = line.strip_prefix("allowed_tools:") {
            return parse_yaml_string_list(val.trim());
        }
    }
    Vec::new()
}

/// Parse a YAML string list like `["a", "b"]` or `- a\n- b`.
fn parse_yaml_string_list(input: &str) -> Vec<String> {
    let input = input.trim();
    if input.starts_with('[') {
        // Inline array: ["a", "b"]
        let inner = input.trim_start_matches('[').trim_end_matches(']');
        inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else if input.starts_with('-') || input.is_empty() {
        // YAML list or empty
        input
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix('-')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
            })
            .collect()
    } else {
        Vec::new()
    }
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
fn derive_auto_load_toolsets(tools: &[String], mcp_servers: &[String]) -> Vec<String> {
    let mut toolsets = std::collections::HashSet::new();

    // Map each declared tool to its toolset
    for tool in tools {
        if let Some(ts) = tool_to_toolset(tool) {
            toolsets.insert(ts.to_string());
        }
    }

    // MCP servers are also toolsets
    for server in mcp_servers {
        toolsets.insert(server.clone());
    }

    // All agents need knowledge tools to read their own knowledge base
    toolsets.insert("knowledge".to_string());

    let mut result: Vec<String> = toolsets.into_iter().collect();
    result.sort();
    result
}

/// Extract an MCP server name from a tool name like `mcp_{server}_{tool}`.
/// Returns None if the tool name doesn't match the MCP prefix pattern.
fn extract_mcp_server(tool_name: &str) -> Option<String> {
    let rest = tool_name.strip_prefix("mcp_")?;
    let underscore_pos = rest.find('_')?;
    let server = &rest[..underscore_pos];
    if server.is_empty() {
        return None;
    }
    Some(server.to_string())
}
