//! Agent management tools: send, spawn, list, kill, restart.

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::sync::Arc;

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
    let kh = crate::tools::require_kernel(kernel)?;
    let agent_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    let message = input["message"]
        .as_str()
        .ok_or("Missing 'message' parameter")?;

    // Check + increment inter-agent call depth
    crate::tools::check_call_depth()?;
    let current_depth = crate::tool_runner::AGENT_CALL_DEPTH
        .try_with(|d| d.get())
        .unwrap_or(0);

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
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
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
    let kh = crate::tools::require_kernel(kernel)?;
    let target_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    kh.restart_agent(target_id)?;
    Ok(format!("Agent {target_id} restarted successfully."))
}

// ---------------------------------------------------------------------------
// ToolModule implementation
// ---------------------------------------------------------------------------

/// Agent management tools: send, spawn, list, kill, restart.
pub struct AgentMgmtTools;

#[async_trait]
impl ToolModule for AgentMgmtTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
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
        let sender_id = ctx.sender_id;
        let owner_id = ctx.owner_id;

        match name {
            "agent_send" => Some(tool_agent_send(input, kernel, caller_agent_id, owner_id, sender_id).await),
            "agent_spawn" => Some(tool_agent_spawn(input, kernel, caller_agent_id).await),
            "agent_list" => Some(tool_agent_list(kernel, caller_agent_id)),
            "agent_kill" => Some(tool_agent_kill(input, kernel, caller_agent_id)),
            "agent_restart" => Some(tool_agent_restart(input, kernel, caller_agent_id)),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "agent_list" => PermissionLevel::None,
            "agent_send" | "agent_spawn" | "agent_restart" => PermissionLevel::Execute,
            "agent_kill" => PermissionLevel::Dangerous,
            _ => PermissionLevel::Dangerous,
        }
    }
}
