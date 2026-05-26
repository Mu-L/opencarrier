//! Agent, collaboration, scheduling, training, cron, A2A, and delegation tool
//! module.
//!
//! Groups together all tools that require kernel access and/or inter-agent
//! coordination. Extracted from `tool_runner.rs` as part of the modular
//! tool-module refactoring.

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Maximum inter-agent call depth (used by agent tools).
const MAX_AGENT_CALL_DEPTH: u32 = crate::tool_runner::MAX_AGENT_CALL_DEPTH;

// ---------------------------------------------------------------------------
// Path validation helpers — delegates to shared utilities in tools/mod.rs
// ---------------------------------------------------------------------------

fn validate_path(path: &str) -> Result<&str, String> {
    crate::tools::validate_path(path)
}
fn sanitize_path_component(name: &str) -> Result<&str, String> {
    crate::tools::sanitize_path_component(name)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn require_kernel(
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<&Arc<dyn KernelHandle>, String> {
    kernel.ok_or_else(|| {
        "Kernel handle not available. Inter-agent tools require a running kernel.".to_string()
    })
}

/// Resolve a target clone's workspace root via kernel.
fn resolve_target_workspace(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<PathBuf, String> {
    let kh = kernel.ok_or("train_* tools require kernel access")?;
    let target = input["target"]
        .as_str()
        .ok_or("Missing 'target' parameter (target clone name)")?;

    let target_workspace = kh
        .resolve_agent_workspace(target)
        .ok_or_else(|| format!("Agent '{}' not found or has no workspace", target))?;

    let path = PathBuf::from(&target_workspace);
    if !path.exists() {
        return Err(format!(
            "Workspace for '{}' does not exist: {}",
            target, target_workspace
        ));
    }
    Ok(path)
}

/// Parse a natural language schedule into a cron expression.
fn parse_schedule_to_cron(input: &str) -> Result<String, String> {
    let input = input.trim().to_lowercase();

    // If it already looks like a cron expression (5 space-separated fields), pass through
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.len() == 5
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_digit() || "*/,-".contains(c)))
    {
        return Ok(input);
    }

    // Natural language patterns
    if let Some(rest) = input.strip_prefix("every ") {
        if rest == "minute" || rest == "1 minute" {
            return Ok("* * * * *".to_string());
        }
        if let Some(mins) = rest.strip_suffix(" minutes") {
            let n: u32 = mins
                .trim()
                .parse()
                .map_err(|_| format!("Invalid number in '{input}'"))?;
            if n == 0 || n > 59 {
                return Err(format!("Minutes must be 1-59, got {n}"));
            }
            return Ok(format!("*/{n} * * * *"));
        }
        if rest == "hour" || rest == "1 hour" {
            return Ok("0 * * * *".to_string());
        }
        if let Some(hrs) = rest.strip_suffix(" hours") {
            let n: u32 = hrs
                .trim()
                .parse()
                .map_err(|_| format!("Invalid number in '{input}'"))?;
            if n == 0 || n > 23 {
                return Err(format!("Hours must be 1-23, got {n}"));
            }
            return Ok(format!("0 */{n} * * *"));
        }
        if rest == "day" || rest == "1 day" {
            return Ok("0 0 * * *".to_string());
        }
        if rest == "week" || rest == "1 week" {
            return Ok("0 0 * * 0".to_string());
        }
    }

    // "daily at Xam/pm"
    if let Some(time_str) = input.strip_prefix("daily at ") {
        let hour = parse_time_to_hour(time_str)?;
        return Ok(format!("0 {hour} * * *"));
    }

    // "weekdays at Xam/pm"
    if let Some(time_str) = input.strip_prefix("weekdays at ") {
        let hour = parse_time_to_hour(time_str)?;
        return Ok(format!("0 {hour} * * 1-5"));
    }

    // "weekends at Xam/pm"
    if let Some(time_str) = input.strip_prefix("weekends at ") {
        let hour = parse_time_to_hour(time_str)?;
        return Ok(format!("0 {hour} * * 0,6"));
    }

    // "hourly" / "daily" / "weekly" / "monthly"
    match input.as_str() {
        "hourly" => return Ok("0 * * * *".to_string()),
        "daily" => return Ok("0 0 * * *".to_string()),
        "weekly" => return Ok("0 0 * * 0".to_string()),
        "monthly" => return Ok("0 0 1 * *".to_string()),
        _ => {}
    }

    Err(format!(
        "Could not parse schedule '{input}'. Try: 'every 5 minutes', 'daily at 9am', 'weekdays at 6pm', or a cron expression like '0 */5 * * *'"
    ))
}

/// Parse a time string like "9am", "6pm", "14:00", "9:30am" into an hour (0-23).
fn parse_time_to_hour(s: &str) -> Result<u32, String> {
    let s = s.trim().to_lowercase();

    // Handle "9am", "6pm", "12pm", "12am"
    if let Some(h) = s.strip_suffix("am") {
        let hour: u32 = h.trim().parse().map_err(|_| format!("Invalid time: {s}"))?;
        return match hour {
            12 => Ok(0),
            1..=11 => Ok(hour),
            _ => Err(format!("Invalid hour: {hour}")),
        };
    }
    if let Some(h) = s.strip_suffix("pm") {
        let hour: u32 = h.trim().parse().map_err(|_| format!("Invalid time: {s}"))?;
        return match hour {
            12 => Ok(12),
            1..=11 => Ok(hour + 12),
            _ => Err(format!("Invalid hour: {hour}")),
        };
    }

    // Handle "14:00" or "9:30"
    if let Some((h, _m)) = s.split_once(':') {
        let hour: u32 = h.trim().parse().map_err(|_| format!("Invalid time: {s}"))?;
        if hour > 23 {
            return Err(format!("Hour must be 0-23, got {hour}"));
        }
        return Ok(hour);
    }

    // Plain number
    let hour: u32 = s.parse().map_err(|_| format!("Invalid time: {s}"))?;
    if hour > 23 {
        return Err(format!("Hour must be 0-23, got {hour}"));
    }
    Ok(hour)
}

const SCHEDULES_KEY: &str = "__carrier_schedules";

// ---------------------------------------------------------------------------
// Cross-workspace training tools (for trainer agents)
// ---------------------------------------------------------------------------

async fn tool_train_read(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    validate_path(path)?;
    let full_path = target_root.join(path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| format!("Failed to read file: {e}"))
}

async fn tool_train_write(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    validate_path(path)?;
    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;
    let full_path = target_root.join(path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directories: {e}"))?;
    }
    tokio::fs::write(&full_path, content)
        .await
        .map_err(|e| format!("Failed to write file: {e}"))?;
    Ok(format!(
        "Successfully wrote {} bytes to {}",
        content.len(),
        path
    ))
}

async fn tool_train_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let sub_path = input["path"].as_str().unwrap_or(".");
    validate_path(sub_path)?;
    let full_path = target_root.join(sub_path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    let mut entries = tokio::fs::read_dir(&full_path)
        .await
        .map_err(|e| format!("Failed to list directory: {e}"))?;
    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("Failed to read entry: {e}"))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata().await;
        let suffix = match metadata {
            Ok(m) if m.is_dir() => "/",
            _ => "",
        };
        files.push(format!("{name}{suffix}"));
    }
    files.sort();
    Ok(files.join("\n"))
}

async fn tool_train_evaluate(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_clone_evaluate(Some(&target_root)).await
}

// ---------------------------------------------------------------------------
// User profile tool (multi-tenancy)
// ---------------------------------------------------------------------------

async fn tool_user_profile(
    input: &serde_json::Value,
    home_dir: Option<&Path>,
    agent_name: Option<&str>,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let sender = sender_id.ok_or("user_profile requires a sender context (sender_id). This tool is only available when a user identity is provided.")?;
    let hd = home_dir.ok_or("user_profile requires home_dir")?;
    let an = agent_name.ok_or("user_profile requires agent_name")?;
    let oid = sanitize_path_component(owner_id.unwrap_or(sender))?;
    let sender = sanitize_path_component(sender)?;

    let action = input["action"].as_str().unwrap_or("read");
    let profile_path = types::config::sender_data_dir(hd, oid, an, Some(sender)).join("profile.json");

    match action {
        "read" => {
            if profile_path.exists() {
                let content = tokio::fs::read_to_string(&profile_path)
                    .await
                    .map_err(|e| format!("Failed to read profile: {e}"))?;
                Ok(content)
            } else {
                // Return empty profile template
                let template = serde_json::json!({
                    "sender_id": sender,
                    "display_name": null,
                    "preferences": {},
                    "interaction_patterns": {},
                    "notes": null,
                    "conversation_count": 0,
                    "first_seen": null,
                    "last_seen": null,
                });
                Ok(serde_json::to_string_pretty(&template).unwrap_or_else(|_| "{}".to_string()))
            }
        }
        "update" => {
            // Load existing profile or create new
            let mut profile: serde_json::Value = if profile_path.exists() {
                let content = tokio::fs::read_to_string(&profile_path)
                    .await
                    .map_err(|e| format!("Failed to read profile: {e}"))?;
                serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
            } else {
                serde_json::json!({
                    "sender_id": sender,
                    "conversation_count": 0,
                    "first_seen": chrono::Utc::now().to_rfc3339(),
                })
            };

            // Ensure sender_id is set
            profile["sender_id"] = serde_json::Value::String(sender.to_string());
            profile["last_seen"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());

            // Merge updates
            if let Some(updates) = input.get("updates").and_then(|u| u.as_object()) {
                for (key, value) in updates {
                    // Only allow known safe keys
                    match key.as_str() {
                        "display_name" | "preferences" | "interaction_patterns" | "notes" => {
                            profile[key] = value.clone();
                        }
                        _ => {} // ignore unknown keys
                    }
                }
            }

            // Ensure directory exists
            if let Some(parent) = profile_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("Failed to create user directory: {e}"))?;
            }

            let output = serde_json::to_string_pretty(&profile)
                .map_err(|e| format!("Failed to serialize profile: {e}"))?;
            tokio::fs::write(&profile_path, &output)
                .await
                .map_err(|e| format!("Failed to write profile: {e}"))?;
            Ok(format!("Profile updated for user '{}'", sender))
        }
        _ => Err(format!(
            "Unknown action '{}'. Use 'read' or 'update'.",
            action
        )),
    }
}

// ---------------------------------------------------------------------------
// Inter-agent tools
// ---------------------------------------------------------------------------

async fn tool_agent_send(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    let message = input["message"]
        .as_str()
        .ok_or("Missing 'message' parameter")?;

    // Check + increment inter-agent call depth
    let current_depth = crate::tool_runner::AGENT_CALL_DEPTH
        .try_with(|d| d.get())
        .unwrap_or(0);
    if current_depth >= MAX_AGENT_CALL_DEPTH {
        return Err(format!(
            "Inter-agent call depth exceeded (max {}). \
             A->B->C chain is too deep. Use the task queue instead.",
            MAX_AGENT_CALL_DEPTH
        ));
    }

    crate::tool_runner::AGENT_CALL_DEPTH
        .scope(std::cell::Cell::new(current_depth + 1), async {
            kh.send_to_agent(agent_id, message, sender_id, None, caller_agent_id, owner_id, None)
                .await
        })
        .await
}

async fn tool_agent_spawn(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    parent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let manifest_toml = input["manifest_toml"]
        .as_str()
        .ok_or("Missing 'manifest_toml' parameter")?;
    let (id, name) = kh.spawn_agent(manifest_toml, parent_id).await?;
    Ok(format!(
        "Agent spawned successfully.\n  ID: {id}\n  Name: {name}"
    ))
}

fn tool_agent_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agents = kh.list_agents();
    if agents.is_empty() {
        return Ok("No agents currently running.".to_string());
    }
    let mut output = format!("Running agents ({}):\n", agents.len());
    for a in &agents {
        output.push_str(&format!(
            "  - {} (id: {}, state: {}, modality: {}, model: {})\n",
            a.name, a.id, a.state, a.modality, a.model
        ));
    }
    Ok(output)
}

fn tool_agent_kill(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let target_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    kh.kill_agent(target_id)?;
    Ok(format!("Agent {target_id} killed successfully."))
}

fn tool_agent_restart(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let target_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    kh.restart_agent(target_id)?;
    Ok(format!("Agent {target_id} restarted successfully."))
}

// ---------------------------------------------------------------------------
// Collaboration tools
// ---------------------------------------------------------------------------

fn tool_agent_find(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let query = input["query"].as_str().ok_or("Missing 'query' parameter")?;
    let agents = kh.find_agents(query);
    if agents.is_empty() {
        return Ok(format!("No agents found matching '{query}'."));
    }
    let result: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "name": a.name,
                "state": a.state,
                "description": a.description,
                "tags": a.tags,
                "tools": a.tools,
                "model": format!("{}:{}", a.modality, a.model),
            })
        })
        .collect();
    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

async fn tool_task_post(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let title = input["title"].as_str().ok_or("Missing 'title' parameter")?;
    let description = input["description"]
        .as_str()
        .ok_or("Missing 'description' parameter")?;
    let assigned_to = input["assigned_to"].as_str();
    let task_id = kh
        .task_post(title, description, assigned_to, caller_agent_id)
        .await?;
    Ok(format!("Task created with ID: {task_id}"))
}

fn tool_task_plan(input: &serde_json::Value) -> Result<String, String> {
    let title = input["title"].as_str().ok_or("Missing 'title' parameter")?;
    let steps = input["steps"].as_array().ok_or("Missing 'steps' parameter")?;
    if steps.is_empty() {
        return Err("Steps array must not be empty".to_string());
    }
    let ids: Vec<&str> = steps.iter().filter_map(|s| s["id"].as_str()).collect();
    if ids.len() != steps.len() {
        return Err("All steps must have an 'id' field".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    for &id in &ids {
        if !seen.insert(id) {
            return Err(format!("Duplicate step id: '{}'", id));
        }
    }
    for step in steps {
        let id = step["id"].as_str().ok_or("Step missing 'id'")?;
        if step["prompt"].as_str().is_none() {
            return Err(format!("Step '{}' missing 'prompt'", id));
        }
        if let Some(deps) = step["depends_on"].as_array() {
            for dep in deps {
                let dep_str = dep.as_str().unwrap_or("");
                if !ids.contains(&dep_str) {
                    return Err(format!("Step '{}' depends_on unknown step '{}'", id, dep_str));
                }
            }
        }
    }
    Ok(format!("Plan '{}' accepted with {} steps. Execution will begin now.", title, steps.len()))
}

async fn tool_task_claim(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    match kh.task_claim(agent_id).await? {
        Some(task) => {
            serde_json::to_string_pretty(&task).map_err(|e| format!("Serialize error: {e}"))
        }
        None => Ok("No tasks available.".to_string()),
    }
}

async fn tool_task_complete(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let task_id = input["task_id"]
        .as_str()
        .ok_or("Missing 'task_id' parameter")?;
    let result = input["result"]
        .as_str()
        .ok_or("Missing 'result' parameter")?;
    kh.task_complete(task_id, result).await?;
    Ok(format!("Task {task_id} marked as completed."))
}

async fn tool_task_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let status = input["status"].as_str();
    let tasks = kh.task_list(status).await?;
    if tasks.is_empty() {
        return Ok("No tasks found.".to_string());
    }
    serde_json::to_string_pretty(&tasks).map_err(|e| format!("Serialize error: {e}"))
}

async fn tool_event_publish(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let event_type = input["event_type"]
        .as_str()
        .ok_or("Missing 'event_type' parameter")?;
    let payload = input
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    kh.publish_event(event_type, payload).await?;
    Ok(format!("Event '{event_type}' published successfully."))
}

// ---------------------------------------------------------------------------
// Scheduling tools
// ---------------------------------------------------------------------------

async fn tool_schedule_create(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for schedule_create")?;
    let description = input["description"]
        .as_str()
        .ok_or("Missing 'description' parameter")?;
    let schedule_str = input["schedule"]
        .as_str()
        .ok_or("Missing 'schedule' parameter")?;
    let agent = input["agent"].as_str().unwrap_or("");

    let cron_expr = parse_schedule_to_cron(schedule_str)?;
    let schedule_id = uuid::Uuid::new_v4().to_string();

    let entry = serde_json::json!({
        "id": schedule_id,
        "description": description,
        "schedule_input": schedule_str,
        "cron": cron_expr,
        "agent": agent,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "enabled": true,
    });

    // Load existing schedules from agent's memory
    let mut schedules: Vec<serde_json::Value> = match kh.system_kv_recall(aid, "", "", SCHEDULES_KEY)? {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    schedules.push(entry);
    kh.system_kv_store(aid, "", "", SCHEDULES_KEY, serde_json::Value::Array(schedules))?;

    Ok(format!(
        "Schedule created:\n  ID: {schedule_id}\n  Description: {description}\n  Cron: {cron_expr}\n  Original: {schedule_str}"
    ))
}

async fn tool_schedule_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for schedule_list")?;

    let schedules: Vec<serde_json::Value> = match kh.system_kv_recall(aid, "", "", SCHEDULES_KEY)? {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    if schedules.is_empty() {
        return Ok("No scheduled tasks.".to_string());
    }

    let mut output = format!("Scheduled tasks ({}):\n\n", schedules.len());
    for s in &schedules {
        let enabled = s["enabled"].as_bool().unwrap_or(true);
        let status = if enabled { "active" } else { "paused" };
        output.push_str(&format!(
            "  [{status}] {} — {}\n    Cron: {} | Agent: {}\n    Created: {}\n\n",
            s["id"].as_str().unwrap_or("?"),
            s["description"].as_str().unwrap_or("?"),
            s["cron"].as_str().unwrap_or("?"),
            s["agent"].as_str().unwrap_or("(self)"),
            s["created_at"].as_str().unwrap_or("?"),
        ));
    }

    Ok(output)
}

async fn tool_schedule_delete(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for schedule_delete")?;
    let id = input["id"].as_str().ok_or("Missing 'id' parameter")?;

    let mut schedules: Vec<serde_json::Value> = match kh.system_kv_recall(aid, "", "", SCHEDULES_KEY)? {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    let before = schedules.len();
    schedules.retain(|s| s["id"].as_str() != Some(id));

    if schedules.len() == before {
        return Err(format!("Schedule '{id}' not found."));
    }

    kh.system_kv_store(aid, "", "", SCHEDULES_KEY, serde_json::Value::Array(schedules))?;
    Ok(format!("Schedule '{id}' deleted."))
}

// ---------------------------------------------------------------------------
// Cron scheduling tools (delegated to kernel via KernelHandle trait)
// ---------------------------------------------------------------------------

async fn tool_cron_create(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    owner_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_create")?;
    tracing::debug!(agent_id, ?input, "cron_create called");
    kh.cron_create(agent_id, owner_id, input.clone()).await
}

async fn tool_cron_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    owner_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_list")?;
    let jobs = kh.cron_list(agent_id, owner_id).await?;
    serde_json::to_string_pretty(&jobs).map_err(|e| format!("Failed to serialize cron jobs: {e}"))
}

async fn tool_cron_cancel(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    owner_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_cancel")?;
    let job_id = input["job_id"]
        .as_str()
        .ok_or("Missing 'job_id' parameter")?;
    // Ownership check: verify this job belongs to the caller
    let jobs = kh.cron_list(agent_id, owner_id).await?;
    let owned = jobs
        .iter()
        .any(|j| j.get("id").and_then(|v| v.as_str()) == Some(job_id));
    if !owned {
        return Err("Cron job not found or does not belong to you".to_string());
    }
    kh.cron_cancel(job_id).await?;
    Ok(format!("Cron job '{job_id}' cancelled."))
}

// ---------------------------------------------------------------------------
// A2A outbound tools (cross-instance agent communication)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Subagent delegation tools (delegate_{name})
// ---------------------------------------------------------------------------

async fn tool_delegate_subagent(
    subagent_name: &str,
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    owner_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let message = input["message"]
        .as_str()
        .ok_or("Missing 'message' parameter")?;
    let aid = caller_agent_id.ok_or("delegate_* requires caller_agent_id")?;

    // Check + increment inter-agent call depth
    let current_depth = crate::tool_runner::AGENT_CALL_DEPTH
        .try_with(|d| d.get())
        .unwrap_or(0);
    if current_depth >= MAX_AGENT_CALL_DEPTH {
        return Err(format!(
            "Subagent delegation depth exceeded (max {}). The agent call chain is too deep.",
            MAX_AGENT_CALL_DEPTH
        ));
    }

    tracing::info!(
        subagent = %subagent_name,
        depth = current_depth + 1,
        "Delegating to subagent"
    );

    // Route through kernel: send to self with channel_type hint for subagent
    // The kernel will see the same agent_id and apply subagent tool filtering
    let subagent_channel = format!("subagent:{}", subagent_name);

    crate::tool_runner::AGENT_CALL_DEPTH
        .scope(std::cell::Cell::new(current_depth + 1), async {
            kh.send_to_agent(aid, message, sender_id, None, caller_agent_id, owner_id, Some(&subagent_channel))
                .await
        })
        .await
}

/// Discover an external A2A agent by fetching its agent card.
async fn tool_a2a_discover(input: &serde_json::Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;

    // SSRF protection: block private/metadata IPs
    if types::ssrf::check_ssrf(url).is_err() {
        return Err("SSRF blocked: URL resolves to a private or metadata address".to_string());
    }

    let client = crate::a2a::A2aClient::new();
    let card = client.discover(url).await?;

    serde_json::to_string_pretty(&card).map_err(|e| format!("Serialization error: {e}"))
}

/// Send a task to an external A2A agent.
async fn tool_a2a_send(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let message = input["message"]
        .as_str()
        .ok_or("Missing 'message' parameter")?;

    // Resolve agent URL: either directly provided or looked up by name
    let url = if let Some(url) = input["agent_url"].as_str() {
        // SSRF protection
        if types::ssrf::check_ssrf(url).is_err() {
            return Err("SSRF blocked: URL resolves to a private or metadata address".to_string());
        }
        url.to_string()
    } else if let Some(name) = input["agent_name"].as_str() {
        kh.get_a2a_agent_url(name)
            .ok_or_else(|| format!("No known A2A agent with name '{name}'. Use a2a_discover first or provide agent_url directly."))?
    } else {
        return Err("Missing 'agent_url' or 'agent_name' parameter".to_string());
    };

    let session_id = input["session_id"].as_str();
    let client = crate::a2a::A2aClient::new();
    let task = client.send_task(&url, message, session_id).await?;

    serde_json::to_string_pretty(&task).map_err(|e| format!("Serialization error: {e}"))
}

// ---------------------------------------------------------------------------
// ToolModule implementation
// ---------------------------------------------------------------------------

/// Agent, collaboration, scheduling, training, cron, A2A, and delegation tools.
pub struct AgentTools;

#[async_trait]
impl ToolModule for AgentTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            // --- Cross-workspace training tools (for trainer agents) ---
            ToolDefinition {
                name: "train_read".to_string(),
                description: "Read a file from a target clone's workspace. Used by trainer agents to inspect other clones.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone to read from"},
                        "path": {"type": "string", "description": "File path relative to the target clone's workspace root"},
                    },
                    "required": ["target", "path"],
                }),
            },
            ToolDefinition {
                name: "train_write".to_string(),
                description: "Write a file to a target clone's workspace. Can modify any file including SOUL.md, system_prompt.md, agent.toml, and skills. Used by trainer agents to train other clones.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone to write to"},
                        "path": {"type": "string", "description": "File path relative to the target clone's workspace root"},
                        "content": {"type": "string", "description": "File content to write"},
                    },
                    "required": ["target", "path", "content"],
                }),
            },
            ToolDefinition {
                name: "train_list".to_string(),
                description: "List files in a target clone's workspace directory. Used by trainer agents to explore other clones.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone"},
                        "path": {"type": "string", "description": "Directory path relative to the target clone's workspace root (default: '.')"},
                    },
                    "required": ["target"],
                }),
            },
            ToolDefinition {
                name: "train_evaluate".to_string(),
                description: "Evaluate a target clone's quality with deterministic metrics. Returns score (0-100), knowledge stats, skill count, and identity completeness.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {"type": "string", "description": "Name of the target clone to evaluate"},
                    },
                    "required": ["target"],
                }),
            },
            // --- User profile tool (multi-tenancy) ---
            ToolDefinition {
                name: "user_profile".to_string(),
                description: "Read or update the current user's profile. The profile stores preferences, habits, and interaction patterns between this clone and a specific user. Requires a sender context (sender_id).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["read", "update"], "description": "Read the profile or update it with new key-value pairs"},
                        "updates": {"type": "object", "description": "Key-value pairs to merge into the profile (only for action=update). Supported keys: display_name, preferences (object), interaction_patterns (object), notes (string)"},
                    },
                    "required": ["action"],
                }),
            },
            // --- Inter-agent tools ---
            ToolDefinition {
                name: "agent_send".to_string(),
                description: "Send a message to another agent and receive their response. Accepts UUID or agent name. Use agent_find first to discover agents.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "The target agent's UUID or name" },
                        "message": { "type": "string", "description": "The message to send to the agent" }
                    },
                    "required": ["agent_id", "message"]
                }),
            },
            ToolDefinition {
                name: "agent_spawn".to_string(),
                description: "Spawn a new agent from a TOML manifest. Returns the new agent's ID and name.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "manifest_toml": {
                            "type": "string",
                            "description": "The agent manifest in TOML format (must include name, module, [model], and [capabilities])"
                        }
                    },
                    "required": ["manifest_toml"]
                }),
            },
            ToolDefinition {
                name: "agent_list".to_string(),
                description: "List all currently running agents with their IDs, names, states, and models.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "agent_kill".to_string(),
                description: "Kill (terminate) another agent by its ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "The agent's UUID to kill" }
                    },
                    "required": ["agent_id"]
                }),
            },
            ToolDefinition {
                name: "agent_restart".to_string(),
                description: "Restart another agent by its ID. Cancels any running task and resets state to Running. Useful after modifying an agent's configuration to apply changes.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_id": { "type": "string", "description": "The target agent's UUID or name" }
                    },
                    "required": ["agent_id"]
                }),
            },
            // --- Collaboration tools ---
            ToolDefinition {
                name: "agent_find".to_string(),
                description: "Discover agents by name, tag, tool, or description. Use to find specialists before delegating work.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search query (matches agent name, tags, tools, description)" }
                    },
                    "required": ["query"]
                }),
            },
            ToolDefinition {
                name: "task_post".to_string(),
                description: "Post a task to the shared task queue for another agent to pick up.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Short task title" },
                        "description": { "type": "string", "description": "Detailed task description" },
                        "assigned_to": { "type": "string", "description": "Agent name or ID to assign the task to (optional)" }
                    },
                    "required": ["title", "description"]
                }),
            },
            ToolDefinition {
                name: "task_claim".to_string(),
                description: "Claim the next available task from the task queue assigned to you or unassigned.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "task_complete".to_string(),
                description: "Mark a previously claimed task as completed with a result.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": { "type": "string", "description": "The task ID to complete" },
                        "result": { "type": "string", "description": "The result or outcome of the task" }
                    },
                    "required": ["task_id", "result"]
                }),
            },
            ToolDefinition {
                name: "task_list".to_string(),
                description: "List tasks in the shared queue, optionally filtered by status (pending, in_progress, completed).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "status": { "type": "string", "description": "Filter by status: pending, in_progress, completed (optional)" }
                    }
                }),
            },
            ToolDefinition {
                name: "event_publish".to_string(),
                description: "Publish a custom event that can trigger proactive agents. Use to broadcast signals to the agent fleet.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "event_type": { "type": "string", "description": "Type identifier for the event (e.g., 'code_review_requested')" },
                        "payload": { "type": "object", "description": "JSON payload data for the event" }
                    },
                    "required": ["event_type"]
                }),
            },
            // --- Scheduling tools ---
            ToolDefinition {
                name: "schedule_create".to_string(),
                description: "Schedule a recurring task using natural language or cron syntax. Examples: 'every 5 minutes', 'daily at 9am', 'weekdays at 6pm', '0 */5 * * *'.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "description": { "type": "string", "description": "What this schedule does (e.g., 'Check for new emails')" },
                        "schedule": { "type": "string", "description": "Natural language or cron expression (e.g., 'every 5 minutes', 'daily at 9am', '0 */5 * * *')" },
                        "agent": { "type": "string", "description": "Agent name or ID to run this task (optional, defaults to self)" }
                    },
                    "required": ["description", "schedule"]
                }),
            },
            ToolDefinition {
                name: "schedule_list".to_string(),
                description: "List all scheduled tasks with their IDs, descriptions, schedules, and next run times.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "schedule_delete".to_string(),
                description: "Remove a scheduled task by its ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "The schedule ID to remove" }
                    },
                    "required": ["id"]
                }),
            },
            // --- Cron scheduling tools ---
            ToolDefinition {
                name: "cron_create".to_string(),
                description: "Create a scheduled/cron job. Supports one-shot (at), recurring (every N seconds), and cron expressions. Max 50 jobs per agent.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Job name (max 128 chars, alphanumeric + spaces/hyphens/underscores)" },
                        "schedule": {
                            "type": "object",
                            "description": "Schedule: {\"kind\":\"at\",\"at\":\"2025-01-01T00:00:00Z\"} or {\"kind\":\"every\",\"every_secs\":300} or {\"kind\":\"cron\",\"expr\":\"0 8 * * *\"}. Cron expressions default to server local timezone; pass {\"kind\":\"cron\",\"expr\":\"...\",\"tz\":\"UTC\"} or any IANA tz (e.g. \"Asia/Shanghai\") to override."
                        },
                        "action": {
                            "type": "object",
                            "description": "Action: {\"kind\":\"system_event\",\"text\":\"...\"} or {\"kind\":\"agent_turn\",\"message\":\"...\",\"timeout_secs\":300}"
                        },
                        "delivery": {
                            "type": "object",
                            "description": "Delivery target: {\"kind\":\"none\"} or {\"kind\":\"channel\",\"channel\":\"telegram\"} or {\"kind\":\"last_channel\"}"
                        },
                        "one_shot": { "type": "boolean", "description": "If true, auto-delete after execution. Default: false" }
                    },
                    "required": ["name", "schedule", "action"]
                }),
            },
            ToolDefinition {
                name: "cron_list".to_string(),
                description: "List all scheduled/cron jobs for the current agent.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "cron_cancel".to_string(),
                description: "Cancel a scheduled/cron job by its ID.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "job_id": { "type": "string", "description": "The UUID of the cron job to cancel" }
                    },
                    "required": ["job_id"]
                }),
            },
            // --- A2A outbound tools ---
            ToolDefinition {
                name: "a2a_discover".to_string(),
                description: "Discover an external A2A agent by fetching its agent card from a URL. Returns the agent's name, description, skills, and supported protocols.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Base URL of the remote OpenCarrier/A2A-compatible agent (e.g., 'https://agent.example.com')" }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "a2a_send".to_string(),
                description: "Send a task/message to an external A2A agent and get the response. Use agent_name to send to a previously discovered agent, or agent_url for direct addressing.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "The task/message to send to the remote agent" },
                        "agent_url": { "type": "string", "description": "Direct URL of the remote agent's A2A endpoint" },
                        "agent_name": { "type": "string", "description": "Name of a previously discovered A2A agent (looked up from kernel)" },
                        "session_id": { "type": "string", "description": "Optional session ID for multi-turn conversations" }
                    },
                    "required": ["message"]
                }),
            },
            ToolDefinition {
                name: "task_plan".to_string(),
                description: "Split a complex task into ordered steps with dependencies. Each step runs as an independent agent turn (up to 15 iterations). Use this when the task is too complex for a single turn — e.g. multi-stage workflows like research -> write -> format -> publish. Steps without dependencies run in parallel; steps with depends_on wait for those steps to complete first. Previous step outputs are injected into the step's prompt automatically.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Short title for the overall plan"
                        },
                        "steps": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": { "type": "string", "description": "Unique step identifier (e.g. 'research', 'write', 'publish')" },
                                    "prompt": { "type": "string", "description": "What to do in this step — detailed instructions" },
                                    "depends_on": {
                                        "type": "array",
                                        "items": { "type": "string" },
                                        "description": "IDs of steps that must complete before this step starts. Empty = run immediately."
                                    }
                                },
                                "required": ["id", "prompt"]
                            },
                            "description": "Ordered list of steps. Each step gets its own agent turn."
                        }
                    },
                    "required": ["title", "steps"]
                }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        let kernel = ctx.kernel;
        let caller_agent_id = ctx.caller_agent_id;
        let _workspace_root = ctx.workspace_root;
        let sender_id = ctx.sender_id;
        let owner_id = ctx.owner_id;

        match name {
            // Cross-workspace training tools (for trainer agents)
            "train_read" => Some(tool_train_read(input, kernel, caller_agent_id).await),
            "train_write" => Some(tool_train_write(input, kernel, caller_agent_id).await),
            "train_list" => Some(tool_train_list(input, kernel, caller_agent_id).await),
            "train_evaluate" => Some(tool_train_evaluate(input, kernel, caller_agent_id).await),

            // User profile
            "user_profile" => Some(tool_user_profile(input, ctx.home_dir, ctx.agent_name, owner_id, sender_id).await),

            // Clone management tools

            // Inter-agent tools (require kernel handle)
            "agent_send" => Some(tool_agent_send(input, kernel, caller_agent_id, owner_id, sender_id).await),
            "agent_spawn" => Some(tool_agent_spawn(input, kernel, caller_agent_id).await),
            "agent_list" => Some(tool_agent_list(kernel, caller_agent_id)),
            "agent_kill" => Some(tool_agent_kill(input, kernel, caller_agent_id)),
            "agent_restart" => Some(tool_agent_restart(input, kernel, caller_agent_id)),

            // Collaboration tools
            "agent_find" => Some(tool_agent_find(input, kernel, caller_agent_id)),
            "task_post" => Some(tool_task_post(input, kernel, caller_agent_id).await),
            "task_claim" => Some(tool_task_claim(kernel, caller_agent_id).await),
            "task_complete" => Some(tool_task_complete(input, kernel, caller_agent_id).await),
            "task_list" => Some(tool_task_list(input, kernel, caller_agent_id).await),
            "task_plan" => Some(tool_task_plan(input)),
            "event_publish" => Some(tool_event_publish(input, kernel, caller_agent_id).await),

            // Scheduling tools
            "schedule_create" => Some(tool_schedule_create(input, kernel, caller_agent_id).await),
            "schedule_list" => Some(tool_schedule_list(kernel, caller_agent_id).await),
            "schedule_delete" => Some(tool_schedule_delete(input, kernel, caller_agent_id).await),

            // Cron scheduling tools
            "cron_create" => Some(tool_cron_create(input, kernel, caller_agent_id, owner_id).await),
            "cron_list" => Some(tool_cron_list(kernel, caller_agent_id, owner_id).await),
            "cron_cancel" => Some(tool_cron_cancel(input, kernel, caller_agent_id, owner_id).await),

            // A2A outbound tools (cross-instance agent communication)
            "a2a_discover" => Some(tool_a2a_discover(input).await),
            "a2a_send" => Some(tool_a2a_send(input, kernel).await),

            // Subagent delegation (delegate_{name})
            name if name.starts_with("delegate_") => {
                let subagent_name = &name["delegate_".len()..];
                Some(tool_delegate_subagent(
                    subagent_name, input, kernel, caller_agent_id, owner_id, sender_id,
                ).await)
            }

            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> types::tool::PermissionLevel {
        match tool_name {
            "agent_find" | "agent_list"
            | "train_read" | "train_list"
            | "train_evaluate" | "user_profile"
            | "task_list" | "schedule_list" | "cron_list"
            | "a2a_discover" => types::tool::PermissionLevel::None,
            "task_post" | "task_claim" | "task_complete" | "task_plan"
            | "event_publish" | "schedule_create" | "schedule_delete"
            | "train_write"
            | "cron_create" | "cron_cancel" => types::tool::PermissionLevel::Write,
            "agent_send" | "agent_spawn" | "agent_restart"
            | "a2a_send" => types::tool::PermissionLevel::Execute,
            name if name.starts_with("delegate_") => types::tool::PermissionLevel::Execute,
            "agent_kill" => types::tool::PermissionLevel::Dangerous,
            _ => types::tool::PermissionLevel::Dangerous,
        }
    }
}
