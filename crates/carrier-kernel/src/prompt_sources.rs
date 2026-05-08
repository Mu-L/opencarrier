//! Prompt source helpers — read workspace files for system prompt injection.
//!
//! All functions are pure: they take `&Path` and operate only on the filesystem.
//! No kernel state is accessed.

use std::path::Path;

/// Read an identity file from the workspace, with path-traversal protection.
/// Capped at 32KB.
pub fn read_identity_file(workspace: &Path, filename: &str) -> Option<String> {
    const MAX_IDENTITY_FILE_BYTES: usize = 32_768; // 32KB cap
    let path = workspace.join(filename);
    // Security: ensure path stays inside workspace
    match path.canonicalize() {
        Ok(canonical) => {
            if let Ok(ws_canonical) = workspace.canonicalize() {
                if !canonical.starts_with(&ws_canonical) {
                    return None; // path traversal attempt
                }
            }
        }
        Err(_) => return None, // file doesn't exist
    }
    let content = std::fs::read_to_string(&path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    if content.len() > MAX_IDENTITY_FILE_BYTES {
        Some(carrier_types::truncate_str(&content, MAX_IDENTITY_FILE_BYTES).to_string())
    } else {
        Some(content)
    }
}

/// Read user profile for multi-tenancy context injection.
/// Returns a short summary string suitable for the system prompt.
pub fn read_user_profile_summary(workspace: &Path, sender_id: &str) -> Option<String> {
    // SECURITY: sanitize sender_id to prevent path traversal
    if sender_id.contains('/')
        || sender_id.contains('\\')
        || sender_id.contains("..")
        || sender_id.is_empty()
    {
        return None;
    }
    let profile_path = workspace.join("users").join(sender_id).join("profile.json");
    if !profile_path.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&profile_path).ok()?;
    let profile: serde_json::Value = serde_json::from_str(&content).ok()?;

    let mut parts = Vec::new();
    if let Some(name) = profile["display_name"].as_str() {
        parts.push(format!("Name: {}", name));
    }
    if let Some(count) = profile["conversation_count"].as_u64() {
        if count > 0 {
            parts.push(format!("Previous conversations: {}", count));
        }
    }
    if let Some(prefs) = profile["preferences"].as_object() {
        if !prefs.is_empty() {
            parts.push(format!(
                "Preferences: {}",
                serde_json::to_string(prefs).unwrap_or_default()
            ));
        }
    }
    if let Some(patterns) = profile["interaction_patterns"].as_object() {
        if !patterns.is_empty() {
            parts.push(format!(
                "Interaction patterns: {}",
                serde_json::to_string(patterns).unwrap_or_default()
            ));
        }
    }
    if let Some(notes) = profile["notes"].as_str() {
        if !notes.is_empty() {
            parts.push(format!("Notes: {}", notes));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Update user profile after a conversation (touch last_seen, increment count).
pub fn touch_user_profile(workspace: &Path, sender_id: &str) {
    // SECURITY: sanitize sender_id to prevent path traversal
    if sender_id.contains('/')
        || sender_id.contains('\\')
        || sender_id.contains("..")
        || sender_id.is_empty()
    {
        return;
    }
    let profile_path = workspace.join("users").join(sender_id).join("profile.json");
    let mut profile: serde_json::Value = if profile_path.exists() {
        std::fs::read_to_string(&profile_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({
            "sender_id": sender_id,
            "first_seen": chrono::Utc::now().to_rfc3339(),
        })
    };

    profile["sender_id"] = serde_json::Value::String(sender_id.to_string());
    profile["last_seen"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
    let count = profile["conversation_count"].as_u64().unwrap_or(0);
    profile["conversation_count"] = serde_json::Value::Number((count + 1).into());

    if let Some(parent) = profile_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(output) = serde_json::to_string_pretty(&profile) {
        let _ = std::fs::write(&profile_path, output);
    }
}

/// Read clone skill catalog from workspace/skills/ directory.
/// Returns a short summary of all skills: "1. **{name}** — {when_to_use}"
pub fn read_skills_catalog(workspace: &Path) -> Option<String> {
    let skills_dir = workspace.join("skills");
    if !skills_dir.is_dir() {
        return None;
    }

    let mut entries: Vec<(String, String)> = Vec::new();

    let dir_iter = match std::fs::read_dir(&skills_dir) {
        Ok(iter) => iter,
        Err(_) => return None,
    };

    for entry in dir_iter.flatten() {
        let path = entry.path();

        // Directory format: skills/<name>/SKILL.md
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                if let Some((name, when_to_use)) = parse_skill_frontmatter(&skill_md) {
                    entries.push((name, when_to_use));
                }
            }
        }
        // Flat format: skills/<name>.md
        else if path.extension().is_some_and(|ext| ext == "md") {
            if let Some((name, when_to_use)) = parse_skill_frontmatter(&path) {
                entries.push((name, when_to_use));
            }
        }
    }

    if entries.is_empty() {
        return None;
    }

    let catalog: String = entries
        .iter()
        .enumerate()
        .map(|(i, (name, when_to_use))| {
            if when_to_use.is_empty() {
                format!("{}. **{}**", i + 1, name)
            } else {
                format!("{}. **{}** — {}", i + 1, name, when_to_use)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    Some(catalog)
}

/// Read all knowledge files from workspace/data/knowledge/ directory.
///
/// Returns a concatenated string of all knowledge file contents (compiled truth
/// section only, not timeline). Capped at ~6KB to avoid context overflow.
pub fn read_knowledge_content(workspace: &Path) -> Option<String> {
    const MAX_KNOWLEDGE_TOTAL_BYTES: usize = 6144; // 6KB cap
    let knowledge_dir = workspace.join("data/knowledge");
    if !knowledge_dir.is_dir() {
        return None;
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    let mut total_bytes = 0;

    if let Ok(dir_iter) = std::fs::read_dir(&knowledge_dir) {
        let mut files: Vec<_> = dir_iter
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
            .collect();
        files.sort_by_key(|e| e.file_name());

        for entry in files {
            let path = entry.path();
            let name = path.file_stem()?.to_string_lossy().to_string();
            if let Ok(content) = std::fs::read_to_string(&path) {
                // Extract compiled truth only (skip timeline for context injection)
                let compiled = if content.contains("\n---\n") {
                    // Dual-layer format: take content before the second --- separator
                    let (truth, _timeline) =
                        carrier_lifecycle::evolution::split_dual_layer(&content);
                    truth
                } else {
                    content.clone()
                };
                let trimmed = compiled.trim();
                if !trimmed.is_empty() {
                    total_bytes += trimmed.len();
                    if total_bytes > MAX_KNOWLEDGE_TOTAL_BYTES {
                        break; // Stop adding files once we hit the cap
                    }
                    entries.push((name, trimmed.to_string()));
                }
            }
        }
    }

    if entries.is_empty() {
        return None;
    }

    let result: String = entries
        .iter()
        .map(|(name, content)| format!("### {name}\n{content}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    Some(result)
}

/// Read all style samples from workspace/style/ directory.
/// Returns a concatenated summary of style files.
pub fn read_style_samples(workspace: &Path) -> Option<String> {
    let style_dir = workspace.join("style");
    if !style_dir.is_dir() {
        return None;
    }

    let dir_iter = match std::fs::read_dir(&style_dir) {
        Ok(iter) => iter,
        Err(_) => return None,
    };

    let mut parts: Vec<String> = Vec::new();
    for entry in dir_iter.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                // Enforce 32KB cap per style file (same as identity files)
                let capped = if trimmed.len() > 32_768 {
                    &trimmed[..32_768]
                } else {
                    trimmed
                };
                let name = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_str()
                    .unwrap_or("unknown");
                parts.push(format!("### {}\n{}", name, capped));
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Read sub-agent definitions from workspace/agents/ directory.
/// Returns formatted agent name + prompt for each agent.
pub fn read_agents_directory(workspace: &Path) -> Option<String> {
    let agents_dir = workspace.join("agents");
    if !agents_dir.is_dir() {
        return None;
    }

    let mut entries: Vec<_> = std::fs::read_dir(&agents_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    let mut parts: Vec<String> = Vec::new();
    for entry in &entries {
        let content = std::fs::read_to_string(entry.path()).unwrap_or_default();
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        let name = entry
            .path()
            .file_stem()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("unknown")
            .to_string();
        // Extract body (skip frontmatter)
        let body = if let Some(rest) = trimmed.strip_prefix("---") {
            if let Some(end) = rest.find("---") {
                trimmed[3 + end + 3..].trim()
            } else {
                trimmed
            }
        } else {
            trimmed
        };
        parts.push(format!("### {}\n{}", name, body));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Read full skill prompts from workspace/skills/ directory.
/// Returns formatted skill body + allowed_tools for each skill.
pub fn read_workspace_skills_prompts(workspace: &Path) -> Option<String> {
    let skills_dir = workspace.join("skills");
    if !skills_dir.is_dir() {
        return None;
    }

    let dir_iter = match std::fs::read_dir(&skills_dir) {
        Ok(iter) => iter,
        Err(_) => return None,
    };

    let mut parts: Vec<String> = Vec::new();
    for entry in dir_iter.flatten() {
        let path = entry.path();

        // Directory format: skills/<name>/SKILL.md
        let skill_path = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().is_some_and(|ext| ext == "md") {
            path.clone()
        } else {
            continue;
        };

        if !skill_path.exists() {
            continue;
        }

        let content = std::fs::read_to_string(&skill_path).unwrap_or_default();
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse frontmatter
        let (name, allowed_tools, body) = parse_skill_full(trimmed);
        let mut section = format!("### {}\n", name);
        if !allowed_tools.is_empty() {
            section.push_str(&format!("可用工具: {}\n", allowed_tools));
        }
        section.push_str(body);
        parts.push(section);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Parse a skill .md file to extract name, allowed_tools, and body.
pub fn parse_skill_full(content: &str) -> (String, String, &str) {
    let mut name = String::new();
    let mut allowed_tools = String::new();

    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let frontmatter = &rest[..end];
            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("name:") {
                    name = val.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(val) = line.strip_prefix("allowed_tools:") {
                    allowed_tools = val.trim().to_string();
                }
            }
            let body = rest[end + 3..].trim();
            return (name, allowed_tools, body);
        }
    }

    // No frontmatter
    (String::new(), String::new(), content)
}

/// Parse YAML frontmatter from a skill .md file to extract name and when_to_use.
pub fn parse_skill_frontmatter(path: &Path) -> Option<(String, String)> {
    let content = std::fs::read_to_string(path).ok()?;
    let content = content.trim();

    // Must start with ---
    if !content.starts_with("---") {
        // No frontmatter — use filename as name
        let name = path.file_stem()?.to_str()?.to_string();
        return Some((name, String::new()));
    }

    let rest = &content[3..];
    let end = rest.find("---")?;
    let frontmatter = &rest[..end];

    let mut name = String::new();
    let mut when_to_use = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(val) = line.strip_prefix("when_to_use:") {
            when_to_use = val.trim().trim_matches('"').trim_matches('\'').to_string();
        }
    }

    if name.is_empty() {
        name = path.parent()?.file_name()?.to_str()?.to_string();
    }

    Some((name, when_to_use))
}
