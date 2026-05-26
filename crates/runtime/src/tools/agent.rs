//! Delegation and user profile tool module.
//!
//! Handles `delegate_*` wildcard tools (subagent delegation) and the
//! `user_profile` tool. All other agent tools have been split into
//! domain-specific modules: agent_mgmt, training, scheduling,
//! collaboration, a2a.

use super::ToolModule;
use crate::kernel_handle::KernelHandle;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::ToolDefinition;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

/// Maximum inter-agent call depth (used by delegation tools).
const MAX_AGENT_CALL_DEPTH: u32 = crate::tool_runner::MAX_AGENT_CALL_DEPTH;

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
    let oid = crate::tools::sanitize_path_component(owner_id.unwrap_or(sender))?;
    let sender = crate::tools::sanitize_path_component(sender)?;

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
    let kh = crate::tools::require_kernel(kernel)?;
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

// ---------------------------------------------------------------------------
// ToolModule implementation
// ---------------------------------------------------------------------------

/// Delegation (delegate_*) and user_profile tools.
pub struct DelegationTools;

#[async_trait]
impl ToolModule for DelegationTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
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
            // User profile
            "user_profile" => Some(tool_user_profile(input, ctx.home_dir, ctx.agent_name, owner_id, sender_id).await),

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
            "user_profile" => types::tool::PermissionLevel::None,
            name if name.starts_with("delegate_") => types::tool::PermissionLevel::Execute,
            _ => types::tool::PermissionLevel::Dangerous,
        }
    }
}
