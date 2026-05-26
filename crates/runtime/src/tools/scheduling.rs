//! Scheduling and cron tools: schedule_create, schedule_list, schedule_delete,
//! cron_create, cron_list, cron_cancel.

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::sync::Arc;

const SCHEDULES_KEY: &str = "__carrier_schedules";

// ---------------------------------------------------------------------------
// Schedule parsing helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Scheduling tools
// ---------------------------------------------------------------------------

async fn tool_schedule_create(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_create")?;
    tracing::debug!(agent_id, ?input, "cron_create called");
    kh.cron_create(agent_id, owner_id, input.clone()).await
}

async fn tool_cron_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    owner_id: Option<&str>,
) -> Result<String, String> {
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
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
// ToolModule implementation
// ---------------------------------------------------------------------------

/// Scheduling and cron tools.
pub struct SchedulingTools;

#[async_trait]
impl ToolModule for SchedulingTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
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
        let owner_id = ctx.owner_id;

        match name {
            // Scheduling tools
            "schedule_create" => Some(tool_schedule_create(input, kernel, caller_agent_id).await),
            "schedule_list" => Some(tool_schedule_list(kernel, caller_agent_id).await),
            "schedule_delete" => Some(tool_schedule_delete(input, kernel, caller_agent_id).await),

            // Cron scheduling tools
            "cron_create" => Some(tool_cron_create(input, kernel, caller_agent_id, owner_id).await),
            "cron_list" => Some(tool_cron_list(kernel, caller_agent_id, owner_id).await),
            "cron_cancel" => Some(tool_cron_cancel(input, kernel, caller_agent_id, owner_id).await),

            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "schedule_list" | "cron_list" => PermissionLevel::None,
            "schedule_create" | "schedule_delete" | "cron_create" | "cron_cancel" => PermissionLevel::Write,
            _ => PermissionLevel::Dangerous,
        }
    }
}
