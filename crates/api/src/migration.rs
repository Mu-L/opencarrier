//! Migrations:
//! 1. `plugins/{platform}/bot/<uuid>/bot.toml` → `{platform}-sessions/<id>.json` (legacy)
//! 2. `{platform}-sessions/<id>.json` → `senders/{sender_id}/session.json` (current)
//! 3. `senders/{owner}/{agent}/` → `workspaces/{agent}/{owner}/` (completed)
//! 4. `workspaces/{agent}/{sender}/` → `workspaces/{agent}/senders/{sender}/` (current)

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

/// Migrate per-sender data from `workspaces/{agent}/{sender}/` to `workspaces/{agent}/senders/{sender}/`.
///
/// This also cleans up two legacy path issues:
/// - Old `senders/{sender}/{agent}/` nesting (extra agent name layer) is flattened into `senders/{sender}/`.
/// - Double-nested `workspaces/{agent}/workspaces/{agent}/{sender}/` directories are merged in.
///
/// Idempotent — skips if `.sender-senders-migrated` marker exists.
pub fn migrate_sender_data_to_senders_dir(home_dir: &Path) {
    let marker = home_dir.join("workspaces").join(".sender-senders-migrated");
    if marker.exists() {
        return;
    }

    let workspaces_dir = home_dir.join("workspaces");
    if !workspaces_dir.exists() {
        return;
    }

    let agent_data_indicators = [
        "sessions",
        "output",
        "input",
        "memory",
        "knowledge",
        "profile.json",
    ];

    let mut migrated = 0usize;
    let workspace_entries = match std::fs::read_dir(&workspaces_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for agent_entry in workspace_entries.flatten() {
        if !agent_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let agent_name = agent_entry.file_name().to_string_lossy().to_string();
        let agent_dir = agent_entry.path();

        // Skip if already has senders/ subdirectory and no flat sender dirs
        let senders_dir = agent_dir.join("senders");
        let has_senders_dir = senders_dir.exists();

        // 1. Move flat sender dirs (e.g. workspaces/{agent}/o9cq80-xxx/) → senders/{sender}/
        let entries = match std::fs::read_dir(&agent_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let dirname = entry.file_name().to_string_lossy().to_string();

            // Skip non-sender directories
            if matches!(dirname.as_str(), "skills" | "knowledge" | "sessions" | "logs" | "history" | "data" | "senders" | "workspaces" | "output" | "input" | "memory" | "agents" | ".lifecycle" | "style" | "test" | "test-img" | "test-pipeline" | "test-quick" | "test-sender" | "test2" | "test3" | "test4" | "test5" | "articles") {
                continue;
            }

            // Check if this looks like a sender data dir (has profile.json, sessions/, output/, etc.)
            let entry_path = entry.path();
            let is_sender_data = agent_data_indicators.iter().any(|indicator| {
                entry_path.join(indicator).exists()
            });

            if !is_sender_data {
                continue;
            }

            // Create senders/ if needed
            if !has_senders_dir {
                if let Err(e) = std::fs::create_dir_all(&senders_dir) {
                    warn!(dir = %senders_dir.display(), "Migration: failed to create senders dir: {e}");
                    continue;
                }
            }

            // Move workspaces/{agent}/{sender}/ → workspaces/{agent}/senders/{sender}/
            let target = senders_dir.join(&dirname);
            if target.exists() {
                // Target exists — merge contents
                move_dir_contents(&entry_path, &target);
                let _ = std::fs::remove_dir(&entry_path);
            } else {
                if let Err(e) = std::fs::rename(&entry_path, &target) {
                    warn!(src = %entry_path.display(), dst = %target.display(), "Migration: rename failed: {e}");
                    continue;
                }
            }

            info!(agent = %agent_name, sender = %dirname, "Migrated workspaces/{{agent}}/{{sender}} -> workspaces/{{agent}}/senders/{{sender}}");
            migrated += 1;
        }

        // 2. Flatten old senders/{sender}/{agent}/ → senders/{sender}/
        if senders_dir.exists() {
            let sender_entries = match std::fs::read_dir(&senders_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for sender_entry in sender_entries.flatten() {
                if !sender_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let sender_path = sender_entry.path();

                // Check for nested agent dir: senders/{sender}/{agent}/
                let nested_agent = sender_path.join(&agent_name);
                if nested_agent.exists() && nested_agent.is_dir() {
                    move_dir_contents(&nested_agent, &sender_path);
                    let _ = std::fs::remove_dir(&nested_agent);
                    info!(agent = %agent_name, "Flattened senders/{{sender}}/{{agent}} -> senders/{{sender}}");
                    migrated += 1;
                }
            }
        }

        // 3. Clean up double-nested workspaces/{agent}/workspaces/{agent}/{sender}/
        let double_nested = agent_dir.join("workspaces").join(&agent_name);
        if double_nested.exists() && double_nested.is_dir() {
            if !senders_dir.exists() {
                let _ = std::fs::create_dir_all(&senders_dir);
            }
            let nested_entries = match std::fs::read_dir(&double_nested) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for nested_entry in nested_entries.flatten() {
                if !nested_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let nested_name = nested_entry.file_name().to_string_lossy().to_string();
                let target = senders_dir.join(&nested_name);
                if target.exists() {
                    move_dir_contents(&nested_entry.path(), &target);
                } else {
                    let _ = std::fs::rename(nested_entry.path(), &target);
                }
                migrated += 1;
            }
            // Remove the now-empty double-nested dir tree
            let _ = std::fs::remove_dir_all(agent_dir.join("workspaces"));
            info!(agent = %agent_name, "Cleaned up double-nested workspaces/ dir");
        }
    }

    if migrated > 0 {
        info!(
            count = migrated,
            "Migration complete: moved sender data to senders/ subdirectories"
        );
    }

    // Write marker
    if let Err(e) = std::fs::write(&marker, "") {
        warn!(path = %marker.display(), "Migration: failed to write marker: {e}");
    }
}

/// Migrate agent-per-sender data from `senders/{owner}/{agent}/` to `workspaces/{agent}/{owner}/`.
///
/// Sender personal data (`senders/{sender_id}/session.json`, `config.json`) is NOT moved.
/// Only subdirectories that contain agent data (sessions/, output/, input/, memory/,
/// knowledge/, profile.json) are migrated.
///
/// Idempotent — skips if `.sender-data-migrated` marker exists or target already exists.
pub fn migrate_sender_data_to_workspaces(home_dir: &Path) {
    let marker = home_dir.join("senders").join(".sender-data-migrated");
    if marker.exists() {
        return;
    }

    let senders_dir = home_dir.join("senders");
    if !senders_dir.exists() {
        return;
    }

    // Known agent data subdirectories/files that indicate an agent data directory
    let agent_data_indicators = [
        "sessions",
        "output",
        "input",
        "memory",
        "knowledge",
        "profile.json",
    ];

    let mut migrated = 0usize;
    let owner_entries = match std::fs::read_dir(&senders_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for owner_entry in owner_entries.flatten() {
        if !owner_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let owner_id = owner_entry.file_name().to_string_lossy().to_string();
        let owner_dir = owner_entry.path();

        // Scan subdirectories of senders/{owner_id}/
        let agent_entries = match std::fs::read_dir(&owner_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for agent_entry in agent_entries.flatten() {
            if !agent_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let agent_name = agent_entry.file_name().to_string_lossy().to_string();
            let agent_dir = agent_entry.path();

            // Check if this looks like agent data (has known subdirs)
            let is_agent_data = agent_data_indicators.iter().any(|indicator| {
                agent_dir.join(indicator).exists()
            });

            if !is_agent_data {
                continue;
            }

            // Target: workspaces/{agent_name}/{owner_id}/
            let target_dir = home_dir
                .join("workspaces")
                .join(&agent_name)
                .join(&owner_id);

            // Skip if target already exists (partial migration)
            if target_dir.exists() {
                // Move remaining items from source to target
                move_dir_contents(&agent_dir, &target_dir);
                // Try to remove source dir if empty
                let _ = std::fs::remove_dir(&agent_dir);
                migrated += 1;
                continue;
            }

            if let Err(e) = std::fs::create_dir_all(&target_dir) {
                warn!(
                    dir = %target_dir.display(),
                    "Migration: failed to create target dir: {e}"
                );
                continue;
            }

            move_dir_contents(&agent_dir, &target_dir);

            // Try to remove source dir if empty
            let _ = std::fs::remove_dir(&agent_dir);

            info!(
                owner = %owner_id,
                agent = %agent_name,
                "Migrated senders/{{owner}}/{{agent}} -> workspaces/{{agent}}/{{owner}}"
            );
            migrated += 1;
        }
    }

    if migrated > 0 {
        info!(
            count = migrated,
            "Migration complete: moved sender agent data to workspaces/"
        );
    }

    // Write marker to prevent re-running
    if let Err(e) = std::fs::write(&marker, "") {
        warn!(path = %marker.display(), "Migration: failed to write marker: {e}");
    }
}

/// Move all contents from `src_dir` into `dst_dir`, preserving subdirectory structure.
fn move_dir_contents(src_dir: &Path, dst_dir: &Path) {
    let entries = match std::fs::read_dir(src_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst_dir.join(&file_name);

        if src_path.is_dir() {
            // Create target subdirectory if needed
            if !dst_path.exists() {
                if let Err(e) = std::fs::create_dir_all(&dst_path) {
                    warn!(path = %dst_path.display(), "Migration: failed to create dir: {e}");
                    continue;
                }
            }
            // Recursively move contents
            move_dir_contents(&src_path, &dst_path);
            // Remove now-empty source dir
            let _ = std::fs::remove_dir(&src_path);
        } else {
            // Skip if target already exists
            if dst_path.exists() {
                continue;
            }
            if let Err(e) = std::fs::rename(&src_path, &dst_path) {
                // rename may fail across filesystems; fall back to copy+delete
                if let Err(e2) = std::fs::copy(&src_path, &dst_path) {
                    warn!(
                        src = %src_path.display(),
                        dst = %dst_path.display(),
                        "Migration: copy failed: {e2} (rename also failed: {e})"
                    );
                    continue;
                }
                let _ = std::fs::remove_file(&src_path);
            }
        }
    }
}
