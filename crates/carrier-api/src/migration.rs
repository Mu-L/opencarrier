//! One-time migration: `plugins/{platform}/bot/<uuid>/bot.toml` → `{platform}-sessions/<id>.json`
//!
//! Runs at startup before channel registration. Idempotent — skips if
//! session files already exist.

use std::path::Path;
use tracing::{info, warn};

pub fn migrate_bot_toml_to_sessions(home_dir: &Path) {
    migrate_platform(home_dir, "wecom");
    migrate_platform(home_dir, "feishu");
    migrate_platform(home_dir, "dingtalk");
}

fn migrate_platform(home_dir: &Path, platform: &str) {
    let plugin_dir = home_dir.join("plugins").join(platform).join("bot");
    if !plugin_dir.exists() {
        return;
    }

    let session_dir = home_dir.join(format!("{platform}-sessions"));
    let entries = match std::fs::read_dir(&plugin_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut migrated = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let bot_toml = path.join("bot.toml");
        if !bot_toml.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&bot_toml) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %bot_toml.display(), "Migration: failed to read: {e}");
                continue;
            }
        };

        let mut val: toml::Value = match content.parse() {
            Ok(v) => v,
            Err(e) => {
                warn!(path = %bot_toml.display(), "Migration: failed to parse: {e}");
                continue;
            }
        };

        // Extract name for the session file name
        let name = val
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if name.is_empty() {
            warn!(path = %bot_toml.display(), "Migration: skipping bot with empty name");
            continue;
        }

        // Convert to JSON session file
        let json_val = match serde_json::to_value(&val) {
            Ok(v) => v,
            Err(e) => {
                warn!(path = %bot_toml.display(), "Migration: failed to convert to JSON: {e}");
                continue;
            }
        };

        // Determine session filename based on platform ID
        let session_filename = match platform {
            "wecom" => {
                // For smartbot mode, use bot_id; for app/kf, use name
                let mode = json_val.get("mode").and_then(|v| v.as_str()).unwrap_or("app");
                if mode == "smartbot" {
                    json_val
                        .get("bot_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&name)
                        .to_string()
                } else {
                    name.clone()
                }
            }
            "feishu" => json_val
                .get("app_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&name)
                .to_string(),
            "dingtalk" => json_val
                .get("app_key")
                .and_then(|v| v.as_str())
                .unwrap_or(&name)
                .to_string(),
            _ => name.clone(),
        };

        // Write session file
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            warn!(dir = %session_dir.display(), "Migration: failed to create session dir: {e}");
            continue;
        }

        let session_path = session_dir.join(format!("{session_filename}.json"));
        if session_path.exists() {
            // Already migrated
            continue;
        }

        let json_str = match serde_json::to_string_pretty(&json_val) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %bot_toml.display(), "Migration: failed to serialize JSON: {e}");
                continue;
            }
        };

        if let Err(e) = std::fs::write(&session_path, json_str) {
            warn!(path = %session_path.display(), "Migration: failed to write: {e}");
            continue;
        }

        info!(
            platform = %platform,
            name = %name,
            session_file = %session_path.display(),
            "Migrated bot.toml → session file"
        );
        migrated += 1;
    }

    if migrated > 0 {
        info!(
            platform = %platform,
            count = migrated,
            "Migration complete: converted bot.toml files to session files"
        );
    }
}
