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
/// Returns a short summary of all skills: "1. **{name}** — {description}"
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
                if let Some((name, description)) = parse_skill_frontmatter(&skill_md) {
                    entries.push((name, description));
                }
            }
        }
        // Flat format: skills/<name>.md
        else if path.extension().is_some_and(|ext| ext == "md") {
            if let Some((name, description)) = parse_skill_frontmatter(&path) {
                entries.push((name, description));
            }
        }
    }

    if entries.is_empty() {
        return None;
    }

    let catalog: String = entries
        .iter()
        .enumerate()
        .map(|(i, (name, description))| {
            if description.is_empty() {
                format!("{}. **{}**", i + 1, name)
            } else {
                format!("{}. **{}** — {}", i + 1, name, description)
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
/// Returns formatted skill body for each skill.
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
        let (name, _, _, _, body) = parse_skill_full(trimmed);
        let section = format!("### {}\n{}", name, body);
        parts.push(section);
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

/// Parse a skill .md file to extract name, description, max_iterations, and body.
/// The `tools:` field in frontmatter is no longer parsed — tool guidance is
/// provided via the skill body's natural language instructions.
pub fn parse_skill_full(content: &str) -> (String, String, Option<u32>, Vec<String>, &str) {
    let mut name = String::new();
    let mut description = String::new();
    let mut max_iterations: Option<u32> = None;
    let mut tools: Vec<String> = Vec::new();
    let mut in_tools_list = false;

    if let Some(rest) = content.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            let frontmatter = &rest[..end];
            for line in frontmatter.lines() {
                let trimmed = line.trim();
                // Detect new key — ends multi-line tools list
                if trimmed.starts_with('-') && in_tools_list {
                    let item = trimmed.strip_prefix('-').unwrap().trim().trim_matches('"').trim_matches('\'');
                    if !item.is_empty() {
                        tools.push(item.to_string());
                    }
                    continue;
                }
                in_tools_list = false;
                if let Some(val) = trimmed.strip_prefix("name:") {
                    name = val.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(val) = trimmed.strip_prefix("description:") {
                    description = val.trim().trim_matches('"').trim_matches('\'').to_string();
                } else if let Some(val) = trimmed.strip_prefix("max_iterations:") {
                    max_iterations = val.trim().parse().ok();
                } else if let Some(val) = trimmed.strip_prefix("tools:") {
                    let inline = val.trim();
                    if inline.starts_with('[') {
                        let inner = inline.trim_start_matches('[').trim_end_matches(']');
                        tools = inner.split(',')
                            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                    } else if inline.is_empty() {
                        // Multi-line list format: tools:\n  - foo\n  - bar
                        in_tools_list = true;
                    }
                }
            }
            let body = rest[end + 3..].trim();
            return (name, description, max_iterations, tools, body);
        }
    }

    (String::new(), String::new(), None, Vec::new(), content)
}

/// Result of automatic skill matching against a user message.
pub struct SkillMatch {
    /// Skill name.
    pub name: String,
    /// Full skill body (instructions after frontmatter).
    pub body: String,
    /// Override max_iterations for the agent loop (from skill frontmatter).
    pub max_iterations: Option<u32>,
    /// Tools declared in the skill frontmatter (e.g., ["sqlite_query", "web_fetch"]).
    pub tools: Vec<String>,
}

/// Classify which skill (if any) matches the user message using an LLM.
pub async fn classify_skill_with_llm(
    message: &str,
    workspace: &std::path::Path,
    brain: &std::sync::Arc<dyn runtime::llm_driver::Brain>,
) -> Option<SkillMatch> {
    let skills_dir = workspace.join("skills");
    if !skills_dir.is_dir() {
        return None;
    }

    // Collect all skill summaries (name + description)
    let mut skill_summaries: Vec<(String, String)> = Vec::new();
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

        let (name, description, _, _, _) = parse_skill_full(trimmed);
        if description.is_empty() {
            continue;
        }

        skill_summaries.push((name, description));
    }

    if skill_summaries.is_empty() {
        return None;
    }

    // Build classification prompt
    let mut prompt = String::from(
        "You are a skill classifier. Given a user message and available skills, respond with ONLY the best-matching skill name or \"none\".\n\nAvailable skills:\n",
    );
    for (name, description) in &skill_summaries {
        prompt.push_str(&format!("- {}: {}\n", name, description));
    }
    prompt.push_str(&format!("\nUser message: {}\n\nSkill name:", message));

    // Call LLM for classification
    let request = runtime::llm_driver::CompletionRequest {
        model: String::new(),
        messages: vec![types::message::Message {
            role: types::message::Role::User,
            content: types::message::MessageContent::Text(prompt),
        }],
        tools: Vec::new(),
        max_tokens: 50,
        temperature: 0.0,
        system: None,
        thinking: None,
        extra: Default::default(),
    };

    let response = match brain.complete("fast", request).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Skill classification LLM call failed: {}", e);
            return None;
        }
    };

    let raw = response.text().trim().to_lowercase();
    if raw == "none" || raw.is_empty() {
        return None;
    }

    // Clean up common LLM artifacts (quotes, markdown, newlines)
    let skill_name = raw
        .trim_matches('"')
        .trim_matches('\'')
        .trim_matches('`')
        .lines()
        .next()
        .unwrap_or(&raw)
        .trim()
        .to_string();

    if skill_name.is_empty() {
        return None;
    }

    // Find matching skill (exact or case-insensitive)
    let matched = skill_summaries
        .iter()
        .find(|(name, _)| name.to_lowercase() == skill_name)
        .or_else(|| {
            skill_summaries.iter().find(|(name, _)| {
                name.to_lowercase().contains(&skill_name)
                    || skill_name.contains(&name.to_lowercase())
            })
        });

    let matched_name = match matched {
        Some((name, _)) => name.clone(),
        None => {
            tracing::warn!(
                skill_name = %skill_name,
                available = ?skill_summaries.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
                "LLM returned unknown skill name"
            );
            return None;
        }
    };

    // Load full skill content
    let skill_file = skills_dir.join(&matched_name).join("SKILL.md");
    let content = std::fs::read_to_string(&skill_file).ok()?;
    let (name, _description, max_iterations, tools, body) = parse_skill_full(&content);

    tracing::info!(
        skill = %name,
        tools = ?tools,
        "Skill classified by LLM"
    );

    Some(SkillMatch {
        name,
        body: body.to_string(),
        max_iterations,
        tools,
    })
}

/// Result of automatic subagent trigger matching against a user message.
pub struct SubagentMatch {
    /// Subagent name (forms the `delegate_{name}` tool).
    pub name: String,
    /// Description of the subagent.
    pub description: String,
    /// Max iterations for the subagent's agent loop.
    pub max_iterations: u32,
}

/// Match a user message against subagent trigger keywords.
///
/// Uses the same keyword extraction as skill matching. Returns the best
/// match (most keyword hits), or `None` if nothing matches.
pub fn match_subagent_for_message(message: &str, subagents: &[types::agent::SubagentConfig]) -> Option<SubagentMatch> {
    if subagents.is_empty() {
        return None;
    }

    let msg_lower = message.to_lowercase();
    let mut best: Option<(usize, &types::agent::SubagentConfig)> = None;

    for sa in subagents {
        let keywords = extract_keywords(&sa.trigger);
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

        if best.as_ref().is_none_or(|(c, _)| match_count > *c) {
            best = Some((match_count, sa));
        }
    }

    best.map(|(count, sa)| {
        tracing::info!(
            subagent = %sa.name,
            keyword_matches = count,
            "Subagent trigger matched for message"
        );
        SubagentMatch {
            name: sa.name.clone(),
            description: sa.description.clone(),
            max_iterations: sa.max_iterations,
        }
    })
}

/// Split description text into keywords by common delimiters, filtering stop words.
/// Also used by subagent trigger matching.
fn extract_keywords(text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "用户", "要求", "使用", "时", "当", "想要", "需要", "请", "帮", "帮我", "你",
        "可以", "时候", "以下", "情况", "或者", "或", "说",
    ];

    let mut keywords: Vec<String> = Vec::new();

    // Extract quoted terms (Chinese "" and English "") as standalone keywords
    // e.g. 用户说"排版" → "排版" is a keyword
    let quote_separators: &[char] = &['"', '"', '"'];
    for quoted in text.split(quote_separators) {
        let q = quoted.trim();
        if q.len() >= 2 && !STOP_WORDS.contains(&q) && !keywords.iter().any(|k| k == q) {
            keywords.push(q.to_string());
        }
    }

    // Split on punctuation and add remaining segments
    let punct_separators: &[char] = &['、', '，', '；', ',', ';', ' ', '\t', '。'];
    for segment in text.split(punct_separators) {
        let s = segment.trim();
        // Strip leading stop words
        let s = s.strip_prefix("当").unwrap_or(s)
            .strip_prefix("或").unwrap_or(s)
            .trim();
        if s.len() >= 2 && !STOP_WORDS.contains(&s) && !keywords.iter().any(|k| k == s) {
            keywords.push(s.to_string());
        }
    }

    keywords
}

/// Parse YAML frontmatter from a skill .md file to extract name and description.
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
    let mut description = String::new();

    for line in frontmatter.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("name:") {
            name = val.trim().trim_matches('"').trim_matches('\'').to_string();
        } else if let Some(val) = line.strip_prefix("description:") {
            description = val.trim().trim_matches('"').trim_matches('\'').to_string();
        }
    }

    if name.is_empty() {
        name = path.parent()?.file_name()?.to_str()?.to_string();
    }

    Some((name, description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_skill_full_inline_tools() {
        let content = "---\nname: test-skill\ndescription: test\ntools: [\"foo\", \"bar\"]\n---\nBody text";
        let (name, desc, max_iter, tools, body) = parse_skill_full(content);
        assert_eq!(name, "test-skill");
        assert_eq!(desc, "test");
        assert_eq!(max_iter, None);
        assert_eq!(tools, vec!["foo", "bar"]);
        assert_eq!(body, "Body text");
    }

    #[test]
    fn test_parse_skill_full_multiline_tools() {
        let content = "---\nname: test-skill\ndescription: test\ntools:\n  - mcp_searxng_web_search\n  - knowledge_add\n---\nBody text";
        let (name, desc, _max_iter, tools, body) = parse_skill_full(content);
        assert_eq!(name, "test-skill");
        assert_eq!(desc, "test");
        assert_eq!(tools, vec!["mcp_searxng_web_search", "knowledge_add"]);
        assert_eq!(body, "Body text");
    }

    #[test]
    fn test_parse_skill_full_no_tools() {
        let content = "---\nname: test-skill\ndescription: test\n---\nBody text";
        let (name, _, _, tools, _) = parse_skill_full(content);
        assert_eq!(name, "test-skill");
        assert!(tools.is_empty());
    }

    #[test]
    fn test_parse_skill_full_tools_stops_at_next_key() {
        let content = "---\nname: test-skill\ntools:\n  - foo\n  - bar\nversion: 2\n---\nBody";
        let (name, _, _, tools, _) = parse_skill_full(content);
        assert_eq!(name, "test-skill");
        assert_eq!(tools, vec!["foo", "bar"]);
    }
}
