//! KV store tools: kv_get, kv_set, kv_list.
//!
//! Provide native agents with read/write access to the system KV store.
//! Data is isolated by (agent_id, owner_id, user_id).

use crate::memory_handle::MemoryHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::sync::Arc;

pub struct KvTools;

#[async_trait]
impl super::ToolModule for KvTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "kv_get".to_string(),
                description: "Retrieve a value from your private key-value store by key. Your data is isolated per-agent and per-user.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "The key to look up"
                        }
                    },
                    "required": ["key"]
                }),
            },
            ToolDefinition {
                name: "kv_set".to_string(),
                description: "Store a key-value pair in your private key-value store. Overwrites any existing value for the same key. Your data is isolated per-agent and per-user.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "The key to store under"
                        },
                        "value": {
                            "description": "The value to store (any JSON type)"
                        }
                    },
                    "required": ["key", "value"]
                }),
            },
            ToolDefinition {
                name: "kv_list".to_string(),
                description: "List all keys in your private key-value store, optionally filtered by prefix. Your data is isolated per-agent and per-user.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "prefix": {
                            "type": "string",
                            "description": "Optional prefix to filter keys (e.g. 'entity.' to list only entity keys)"
                        }
                    }
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
        let memory = match ctx.memory {
            Some(m) => m,
            None => return Some(Err("kv tools: memory not available".to_string())),
        };

        match name {
            "kv_get" => Some(handle_kv_get(input, memory, ctx).await),
            "kv_set" => Some(handle_kv_set(input, memory, ctx).await),
            "kv_list" => Some(handle_kv_list(input, memory, ctx).await),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "kv_get" | "kv_list" => PermissionLevel::None,
            "kv_set" => PermissionLevel::Write,
            _ => PermissionLevel::Dangerous,
        }
    }
}

async fn handle_kv_get(
    input: &Value,
    memory: &Arc<dyn MemoryHandle>,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let key = input["key"].as_str().ok_or("Missing 'key' parameter")?;
    let agent_id = ctx.caller_agent_id.ok_or("No agent context")?;
    let owner_id = ctx.owner_id.unwrap_or("");
    let user_id = ctx.sender_id.unwrap_or("");

    match memory.kv_get(agent_id, owner_id, user_id, key)? {
        Some(val) => Ok(serde_json::to_string_pretty(&val)
            .unwrap_or_else(|_| val.to_string())),
        None => Ok(format!("No value found for key '{key}'.")),
    }
}

async fn handle_kv_set(
    input: &Value,
    memory: &Arc<dyn MemoryHandle>,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let key = input["key"].as_str().ok_or("Missing 'key' parameter")?;
    let value = input.get("value").cloned().ok_or("Missing 'value' parameter")?;
    let agent_id = ctx.caller_agent_id.ok_or("No agent context")?;
    let owner_id = ctx.owner_id.unwrap_or("");
    let user_id = ctx.sender_id.unwrap_or("");

    memory.kv_set(agent_id, owner_id, user_id, key, value)?;
    Ok(format!("Stored value for key '{key}'."))
}

async fn handle_kv_list(
    input: &Value,
    memory: &Arc<dyn MemoryHandle>,
    ctx: &ToolContext<'_>,
) -> Result<String, String> {
    let prefix = input["prefix"].as_str();
    let agent_id = ctx.caller_agent_id.ok_or("No agent context")?;
    let owner_id = ctx.owner_id.unwrap_or("");
    let user_id = ctx.sender_id.unwrap_or("");

    let pairs = memory.kv_list(agent_id, owner_id, user_id)?;
    let filtered: Vec<_> = if let Some(p) = prefix {
        pairs.into_iter().filter(|(k, _)| k.starts_with(p)).collect()
    } else {
        pairs
    };

    if filtered.is_empty() {
        Ok("No keys found.".to_string())
    } else {
        let lines: Vec<String> = filtered
            .iter()
            .map(|(k, v)| format!("- {}: {}", k, v))
            .collect();
        Ok(lines.join("\n"))
    }
}
