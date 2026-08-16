//! Shared parsing utilities for clone definition files.
//!
//! `TemplateManifest` parses a clone's `template.json` (name, author, plugins,
//! mcp_servers, ...); the `parse_*` helpers handle YAML frontmatter and simple
//! TOML/JSON-array extraction. Used by the hub install flow and the
//! manifest builder.

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
    /// Optional fallback flow loaded when the intent classifier returns NO
    /// match (bare-turn gap). Flows through manifest_builder into the
    /// generated agent.toml's `default_flow` field. See AgentManifest::default_flow.
    #[serde(default)]
    pub default_flow: Option<String>,
}

impl TemplateManifest {
    /// Lenient per-field extraction from a raw JSON value — the fallback when
    /// the strict struct parse fails.
    ///
    /// Real-world template.json files drift from this struct (notably
    /// `mcp_servers` as objects `[{name, required}]` where the struct wants
    /// `Vec<String>`); a whole-struct parse failure would silently drop
    /// display_name/description/plugins/mcp_servers/default_flow at install
    /// time. Same Value-based pattern as the kernel's
    /// `fill_presentation_from_template_json`.
    pub fn from_value_lenient(v: &serde_json::Value) -> TemplateManifest {
        let str_field = |key: &str| {
            v.get(key)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let str_array = |key: &str| -> Vec<String> {
            v.get(key)
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| item.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        TemplateManifest {
            version: str_field("version"),
            name: str_field("name"),
            display_name: str_field("display_name"),
            description: str_field("description"),
            author: str_field("author"),
            tags: str_array("tags"),
            exported_at: str_field("exported_at"),
            knowledge_version: v
                .get("knowledge_version")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32,
            plugins: str_array("plugins"),
            // Accept both the canonical `["srv"]` and the drifted object form
            // `[{name, required}]` — extract the server names.
            mcp_servers: v
                .get("mcp_servers")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|item| match item {
                            serde_json::Value::String(s) => Some(s.clone()),
                            serde_json::Value::Object(o) => o
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(str::to_string),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            default_flow: v
                .get("default_flow")
                .and_then(|x| x.as_str())
                .map(str::to_string),
        }
    }
}

/// Parse template.json content into a `TemplateManifest`, falling back to
/// lenient per-field extraction when the strict struct parse fails.
///
/// Single shared entry point for all template.json reads on the install path
/// (`manifest_builder::read_template_json`, kernel `clone_install_files`) —
/// keeps the drift fallback in one place instead of per-caller `.ok()` calls
/// that silently drop fields.
pub fn parse_template_manifest_lenient(content: &str) -> Option<TemplateManifest> {
    match serde_json::from_str::<TemplateManifest>(content) {
        Ok(t) => Some(t),
        Err(_) => serde_json::from_str::<serde_json::Value>(content)
            .ok()
            .map(|v| TemplateManifest::from_value_lenient(&v)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Real-world template.json shape (mcp_servers as OBJECTS
    /// `[{name, required}]`, exported_at as string number) must not defeat
    /// the manifest build — regression: before the lenient fallback this
    /// whole-struct parse failure silently dropped display_name/description/
    /// plugins/mcp_servers/default_flow from every installed manifest.
    #[test]
    fn lenient_fallback_survives_mcp_servers_object_drift() {
        let drifted = r#"{
            "version": "2",
            "name": "ai-writer",
            "display_name": "AI科技写手",
            "description": "AI科技公众号写手",
            "exported_at": "1745395200",
            "tags": ["writing"],
            "plugins": ["wechat-oa"],
            "mcp_servers": [{"name": "searxng", "required": true}],
            "default_flow": "start-writing"
        }"#;
        // Strict parse must fail on this shape (Vec<String> vs objects)…
        assert!(serde_json::from_str::<TemplateManifest>(drifted).is_err());
        // …and the lenient fallback must recover every field.
        let t = parse_template_manifest_lenient(drifted).expect("lenient parse");
        assert_eq!(t.display_name, "AI科技写手");
        assert_eq!(t.description, "AI科技公众号写手");
        assert_eq!(t.plugins, vec!["wechat-oa".to_string()]);
        assert_eq!(t.mcp_servers, vec!["searxng".to_string()]);
        assert_eq!(t.default_flow.as_deref(), Some("start-writing"));
        assert_eq!(t.tags, vec!["writing".to_string()]);
    }

    /// Canonical shape still parses through the strict path.
    #[test]
    fn canonical_shape_parses_strictly() {
        let canonical = r#"{
            "version": "2",
            "name": "x",
            "display_name": "X",
            "mcp_servers": ["searxng"]
        }"#;
        let t = parse_template_manifest_lenient(canonical).expect("parse");
        assert_eq!(t.mcp_servers, vec!["searxng".to_string()]);
        assert_eq!(t.display_name, "X");
    }

    /// Garbage input returns None, not a panic or a half-empty manifest.
    #[test]
    fn garbage_returns_none() {
        assert!(parse_template_manifest_lenient("not json").is_none());
        assert!(parse_template_manifest_lenient("").is_none());
    }
}
