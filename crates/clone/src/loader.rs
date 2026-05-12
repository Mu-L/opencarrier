//! .agx archive helpers — shared parsing utilities for YAML frontmatter.
//!
//! The v2 `CloneData` / `load_agx` / `pack_agx` pipeline has been replaced
//! by the v3 "extract directly" flow in `extractor.rs`. This module retains
//! only the shared parsing utilities.

use std::collections::HashMap;

use serde::Deserialize;

/// Parsed template.json from the .agx archive.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct TemplateManifest {
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub exported_at: String,
    #[serde(default)]
    pub knowledge_version: u32,
    /// Required plugins for this clone.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// Required MCP servers for this clone.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
}

/// Format a string slice as `["a", "b"]` — safe for YAML frontmatter.
pub fn format_string_array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// Parse YAML frontmatter from markdown content.
/// Returns (key_value_map, body_after_frontmatter).
pub fn parse_frontmatter(content: &str) -> (HashMap<String, String>, String) {
    let mut map = HashMap::new();
    if !content.starts_with("---") {
        return (map, content.to_string());
    }

    let rest = &content[3..];
    let Some(end) = rest.find("---") else {
        return (map, content.to_string());
    };

    let frontmatter = &rest[..end];
    let body = &rest[end + 3..];

    // Simple key: value parsing (handles basic YAML)
    let mut current_key = String::new();
    let mut in_array = false;
    let mut array_val = String::new();

    for line in frontmatter.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if in_array {
            if trimmed.starts_with('-') || trimmed.starts_with('"') || trimmed.starts_with('[') {
                array_val.push_str(trimmed);
                array_val.push(' ');
            }
            if trimmed.ends_with(']')
                || (!trimmed.starts_with('-')
                    && !trimmed.starts_with('"')
                    && !trimmed.starts_with('[')
                    && !trimmed.starts_with(' '))
            {
                map.insert(current_key.clone(), array_val.trim().to_string());
                in_array = false;
            }
            continue;
        }

        if let Some(colon_pos) = trimmed.find(':') {
            let key = trimmed[..colon_pos].trim().to_string();
            let val = trimmed[colon_pos + 1..].trim().to_string();

            if val.is_empty() {
                // Might be an array on next lines
                current_key = key;
                in_array = true;
                array_val = String::new();
            } else {
                map.insert(key, val.trim_matches('"').to_string());
            }
        }
    }

    (map, body.to_string())
}

/// Parse a string like `["tool1", "tool2"]` or `["tool1","tool2"]` into a Vec.
pub fn parse_string_array(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.starts_with('[') {
        return vec![s.trim_matches('"').to_string()];
    }

    s.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|item| item.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Extract description from a TOML script file.
pub fn parse_toml_description(content: &str) -> String {
    for line in content.lines() {
        if let Some(val) = line.trim().strip_prefix("description") {
            if let Some(val) = val.trim_start_matches('=').trim().strip_prefix('"') {
                if let Some(val) = val.strip_suffix('"') {
                    return val.to_string();
                }
            }
        }
    }
    String::new()
}
