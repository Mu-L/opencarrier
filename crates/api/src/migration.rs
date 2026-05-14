//! Migrations:
//! 1. `plugins/{platform}/bot/<uuid>/bot.toml` → `{platform}-sessions/<id>.json` (legacy)
//! 2. `{platform}-sessions/<id>.json` → `senders/{sender_id}/session.json` (current)

use std::path::Path;
use tracing::{info, warn};

pub fn migrate_bot_toml_to_sessions(home_dir: &Path) {
    migrate_platform(home_dir, "wecom");
    migrate_platform(home_dir, "feishu");
    migrate_platform(home_dir, "dingtalk");
}

/// Migrate `{platform}-sessions/*.json` → `senders/{sender_id}/session.json`.
///
/// For each session file, extracts the sender_id (bot_id for wecom smartbot,
/// app_id for feishu, app_key for dingtalk, openid for weixin), creates
/// `senders/{sender_id}/session.json` with `channel` and `sender_key` fields added,
/// and removes the old file.
///
/// Idempotent — skips if the target already exists.
pub fn migrate_sessions_to_senders(home_dir: &Path) {
    migrate_sessions_platform(home_dir, "wecom");
    migrate_sessions_platform(home_dir, "feishu");
    migrate_sessions_platform(home_dir, "dingtalk");
    migrate_sessions_platform(home_dir, "weixin");
}

fn migrate_sessions_platform(home_dir: &Path, platform: &str) {
    let session_dir = home_dir.join(format!("{platform}-sessions"));
    if !session_dir.exists() {
        return;
    }

    let entries = match std::fs::read_dir(&session_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut migrated = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let mut json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Determine sender_id based on platform
        let sender_id = match platform {
            "wecom" => {
                let mode = json.get("mode").and_then(|v| v.as_str()).unwrap_or("app");
                if mode == "smartbot" {
                    json.get("bot_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    // app/kf: use name as sender_id
                    json.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string()
                }
            }
            "feishu" => json.get("app_id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            "dingtalk" => json.get("app_key").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            "weixin" => json.get("user_id")
                .or_else(|| json.get("bot_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            _ => continue,
        };

        if sender_id.is_empty() {
            continue;
        }

        // Add channel and sender_key fields
        let sender_key = match platform {
            "wecom" => {
                let mode = json.get("mode").and_then(|v| v.as_str()).unwrap_or("app");
                if mode == "smartbot" { "bot_id" } else { "name" }
            }
            "feishu" => "app_id",
            "dingtalk" => "app_key",
            "weixin" => "openid",
            _ => continue,
        };

        if let Some(obj) = json.as_object_mut() {
            obj.insert("channel".to_string(), serde_json::Value::String(platform.to_string()));
            obj.insert("sender_key".to_string(), serde_json::Value::String(sender_key.to_string()));
        }

        // Write to senders/{sender_id}/session.json
        let sender_dir = home_dir.join("senders").join(&sender_id);
        let target_path = sender_dir.join("session.json");

        if target_path.exists() {
            continue; // Already migrated
        }

        if let Err(e) = std::fs::create_dir_all(&sender_dir) {
            warn!(dir = %sender_dir.display(), "Migration: failed to create sender dir: {e}");
            continue;
        }

        let json_str = match serde_json::to_string_pretty(&json) {
            Ok(s) => s,
            Err(e) => {
                warn!("Migration: failed to serialize: {e}");
                continue;
            }
        };

        if let Err(e) = std::fs::write(&target_path, &json_str) {
            warn!(path = %target_path.display(), "Migration: failed to write: {e}");
            continue;
        }

        // Remove old file
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(path = %path.display(), "Migration: failed to remove old file: {e}");
        }

        info!(
            platform = %platform,
            sender_id = %sender_id,
            "Migrated {platform}-sessions → senders/{{sender_id}}/session.json"
        );
        migrated += 1;
    }

    if migrated > 0 {
        // Try to remove the now-empty session directory
        let _ = std::fs::remove_dir(&session_dir);
    }
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

        let val: toml::Value = match content.parse() {
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
