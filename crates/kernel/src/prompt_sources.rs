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
        Some(types::truncate_str(&content, MAX_IDENTITY_FILE_BYTES).to_string())
    } else {
        Some(content)
    }
}

/// Read user profile for multi-tenancy context injection.
/// Returns a short summary string suitable for the system prompt.
pub fn read_user_profile_summary(home_dir: &Path, owner_id: &str, agent_name: &str, user_id: Option<&str>) -> Option<String> {
    // SECURITY: sanitize to prevent path traversal
    if owner_id.contains('/') || owner_id.contains('\\') || owner_id.contains("..") || owner_id.is_empty() {
        return None;
    }
    if let Some(uid) = user_id {
        if uid.contains('/') || uid.contains('\\') || uid.contains("..") || uid.is_empty() {
            return None;
        }
    }
    let profile_path = types::config::sender_data_dir(home_dir, owner_id, agent_name, user_id).join("profile.json");
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
pub fn touch_user_profile(home_dir: &Path, owner_id: &str, agent_name: &str, user_id: Option<&str>) {
    // SECURITY: sanitize to prevent path traversal
    if owner_id.contains('/') || owner_id.contains('\\') || owner_id.contains("..") || owner_id.is_empty() {
        return;
    }
    if let Some(uid) = user_id {
        if uid.contains('/') || uid.contains('\\') || uid.contains("..") || uid.is_empty() {
            return;
        }
    }
    let profile_path = types::config::sender_data_dir(home_dir, owner_id, agent_name, user_id).join("profile.json");
    let mut profile: serde_json::Value = if profile_path.exists() {
        std::fs::read_to_string(&profile_path)
            .ok()
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_else(|| serde_json::json!({}))
    } else {
        serde_json::json!({
            "sender_id": user_id.unwrap_or(owner_id),
            "first_seen": chrono::Utc::now().to_rfc3339(),
        })
    };

    profile["sender_id"] = serde_json::Value::String(user_id.unwrap_or(owner_id).to_string());
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

/// Read all knowledge files from workspace/knowledge/ directory and (if provided)
/// from the sender's private knowledge directory.
///
/// Returns a concatenated string of all knowledge file contents (compiled truth
/// section only, not timeline). Private knowledge overrides shared knowledge
/// with the same filename. Capped at ~6KB to avoid context overflow.
pub fn read_knowledge_content(
    workspace: &Path,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
    home_dir: Option<&Path>,
    agent_name: Option<&str>,
) -> Option<String> {
    const MAX_KNOWLEDGE_TOTAL_BYTES: usize = 6144; // 6KB cap
    let knowledge_dir = workspace.join("knowledge");

    // Collect shared knowledge
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut total_bytes = 0;

    if knowledge_dir.is_dir() {
        if let Some(shared) = read_knowledge_dir(&knowledge_dir, &mut total_bytes, MAX_KNOWLEDGE_TOTAL_BYTES) {
            entries.extend(shared);
        }
    }

    // Collect private knowledge (overrides shared with same filename)
    if let (Some(oid), Some(hd)) = (owner_id, home_dir) {
        let aname;
        let aname_ref: &str = match agent_name {
            Some(a) => a,
            None => {
                aname = workspace.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                &aname
            }
        };
        let private_dir = types::config::sender_data_dir(hd, oid, aname_ref, sender_id).join("knowledge");
        if private_dir.is_dir() {
            if let Some(private) = read_knowledge_dir(&private_dir, &mut total_bytes, MAX_KNOWLEDGE_TOTAL_BYTES) {
                // Private overrides shared: remove shared entries with same name
                let private_names: std::collections::HashSet<String> = private.iter().map(|(n, _)| n.clone()).collect();
                entries.retain(|(n, _)| !private_names.contains(n));
                entries.extend(private);
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

/// Read knowledge files from a single directory, returning (name, compiled_content) pairs.
fn read_knowledge_dir(knowledge_dir: &Path, total_bytes: &mut usize, max_bytes: usize) -> Option<Vec<(String, String)>> {
    let dir_iter = std::fs::read_dir(knowledge_dir).ok()?;
    let mut files: Vec<_> = dir_iter
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
        .collect();
    files.sort_by_key(|e| e.file_name());

    let mut entries: Vec<(String, String)> = Vec::new();
    for entry in files {
        let path = entry.path();
        let name = path.file_stem()?.to_string_lossy().to_string();
        if let Ok(content) = std::fs::read_to_string(&path) {
            let compiled = if content.contains("\n---\n") {
                let (truth, _timeline) = lifecycle::evolution::split_dual_layer(&content);
                truth
            } else {
                content.clone()
            };
            let trimmed = compiled.trim();
            if !trimmed.is_empty() {
                *total_bytes += trimmed.len();
                if *total_bytes > max_bytes {
                    break;
                }
                entries.push((name, trimmed.to_string()));
            }
        }
    }

    if entries.is_empty() { None } else { Some(entries) }
}

/// Read EVOLUTION.md rules (body text after YAML frontmatter).
/// The frontmatter is consumed by `EvolutionConfig` for system configuration;
/// only the rules text after the second `---` is injected into the prompt.
/// Capped at 32KB.
pub fn read_evolution_rules(workspace: &Path) -> Option<String> {
    const MAX_EVOLUTION_FILE_BYTES: usize = 32_768; // 32KB cap
    let path = workspace.join("EVOLUTION.md");
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
    // Strip YAML frontmatter (same pattern as read_agents_directory)
    let body = if let Some(rest) = content.trim().strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            content.trim()[3 + end + 3..].trim()
        } else {
            content.trim()
        }
    } else {
        content.trim()
    };
    if body.is_empty() {
        return None;
    }
    if body.len() > MAX_EVOLUTION_FILE_BYTES {
        Some(types::truncate_str(body, MAX_EVOLUTION_FILE_BYTES).to_string())
    } else {
        Some(body.to_string())
    }
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
        let (name, allowed_tools, _max_iterations, body) = parse_skill_full(trimmed);
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

/// Parse a skill .md file to extract name, allowed_tools, max_iterations, and body.
pub fn parse_skill_full(content: &str) -> (String, String, Option<u32>, &str) {
    let mut name = String::new();
    let mut allowed_tools = String::new();
    let mut max_iterations: Option<u32> = None;

    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let frontmatter = &rest[..end];
            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("name:") {
                    name = val.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(val) = line.strip_prefix("allowed_tools:") {
                    allowed_tools = val.trim().to_string();
                } else if let Some(val) = line.strip_prefix("max_iterations:") {
                    max_iterations = val.trim().parse().ok();
                }
            }
            let body = rest[end + 3..].trim();
            return (name, allowed_tools, max_iterations, body);
        }
    }

    // No frontmatter
    (String::new(), String::new(), None, content)
}

/// Result of automatic skill matching against a user message.
pub struct SkillMatch {
    /// Skill name.
    pub name: String,
    /// Full skill body (instructions after frontmatter).
    pub body: String,
    /// Tools declared in `allowed_tools` frontmatter.
    pub allowed_tools: Vec<String>,
    /// Override max_iterations for the agent loop (from skill frontmatter).
    pub max_iterations: Option<u32>,
}

/// Match a user message against available skills using keyword matching.
///
/// Extracts keywords from each skill's `when_to_use` frontmatter field and
/// checks if the user message contains them. Returns the best match (most
/// keyword hits), or `None` if nothing matches.
pub fn match_skill_for_message(message: &str, workspace: &Path) -> Option<SkillMatch> {
    let skills_dir = workspace.join("skills");
    if !skills_dir.is_dir() {
        return None;
    }

    let msg_lower = message.to_lowercase();
    let mut best: Option<(usize, SkillMatch)> = None;

    for entry in std::fs::read_dir(&skills_dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();

        let skill_path = if path.is_dir() {
            path.join("SKILL.md")
        } else if path.extension().is_some_and(|ext| ext == "md") {
            path
        } else {
            continue;
        };

        if !skill_path.exists() {
            continue;
        }

        let content = std::fs::read_to_string(&skill_path).ok()?;
        let trimmed = content.trim();

        let when_to_use = extract_when_to_use(trimmed);
        if when_to_use.is_empty() {
            continue;
        }

        let keywords = extract_keywords(&when_to_use);
        if keywords.is_empty() {
            continue;
        }

        let match_count = keywords
            .iter()
            .filter(|kw| msg_lower.contains(&kw.to_lowercase()))
            .count();

        if match_count == 0 {
            continue;
        }

        let (name, allowed_tools_str, max_iterations, body) = parse_skill_full(trimmed);
        let allowed_tools = parse_allowed_tools_list(&allowed_tools_str);

        if best.as_ref().is_none_or(|(c, _)| match_count > *c) {
            best = Some((
                match_count,
                SkillMatch {
                    name,
                    body: body.to_string(),
                    allowed_tools,
                    max_iterations,
                },
            ));
        }
    }

    if let Some((count, m)) = &best {
        tracing::info!(
            skill = %m.name,
            keyword_matches = count,
            keywords = ?extract_keywords(&extract_when_to_use(
                &std::fs::read_to_string(
                    workspace.join("skills").join(&m.name).join("SKILL.md")
                ).unwrap_or_default()
            )),
            "Skill auto-matched for message"
        );
    }

    best.map(|(_, m)| m)
}

/// Extract `when_to_use` value from YAML frontmatter.
fn extract_when_to_use(content: &str) -> String {
    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let frontmatter = &rest[..end];
            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(val) = line.strip_prefix("when_to_use:") {
                    return val.trim().trim_matches('"').trim_matches('\'').to_string();
                }
            }
        }
    }
    String::new()
}

/// Split `when_to_use` into keywords by common delimiters, filtering stop words.
fn extract_keywords(when_to_use: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "用户", "要求", "使用", "时", "当", "想要", "需要", "请", "帮", "帮我", "你",
        "可以", "时候", "以下", "情况",
    ];

    when_to_use
        .split(&['、', '，', '；', ',', ';', ' ', '\t', '。'][..])
        .map(|s| s.trim())
        .filter(|s| s.len() >= 2 && !STOP_WORDS.contains(s) && !s.chars().all(|c| c.is_whitespace()))
        .map(String::from)
        .collect()
}

/// Parse a bracket-or-comma-delimited list of tool names.
fn parse_allowed_tools_list(allowed_tools_str: &str) -> Vec<String> {
    let trimmed = allowed_tools_str.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Handle bracket format: [tool1, tool2]
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        return trimmed[1..trimmed.len() - 1]
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }

    // Fallback: comma-separated
    trimmed
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
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
