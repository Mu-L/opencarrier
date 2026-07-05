//! Data analysis tool — clone admins can query user analytics in-chat.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct DataAnalyzeTools;

#[async_trait]
impl ToolModule for DataAnalyzeTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "data_analyze".to_string(),
            description: "Query user analytics and usage data for this clone. \
                Only available to clone admins. Supports: \
                user_stats (total/active/new users), \
                user_lookup (per-user conversation details), \
                usage_analytics (token consumption/trends), \
                recent_conversations (latest session list).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query_type": {
                        "type": "string",
                        "enum": ["user_stats", "user_lookup", "usage_analytics", "recent_conversations"],
                        "description": "Type of analytics query to run"
                    },
                    "user_id": {
                        "type": "string",
                        "description": "Target user's sender_id (required for user_lookup)"
                    },
                    "days": {
                        "type": "integer",
                        "description": "Time window in days for active users and usage trends (default: 7)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results for recent_conversations (default: 10, max: 50)"
                    }
                },
                "required": ["query_type"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "data_analyze" {
            return None;
        }

        if !ctx.is_clone_admin {
            return Some(Err("data_analyze requires clone admin privileges.".to_string()));
        }

        let memory = ctx.memory?;
        let agent_id = ctx.caller_agent_id?;

        let query_type = input["query_type"].as_str()
            .unwrap_or("");

        let result = match query_type {
            "user_stats" => {
                let days = input["days"].as_u64().unwrap_or(7) as u32;
                memory.analytics_user_stats(agent_id, days)
            }
            "user_lookup" => {
                let user_id = match input["user_id"].as_str() {
                    Some(id) => id,
                    None => return Some(Err("user_id is required for user_lookup".to_string())),
                };
                memory.analytics_user_lookup(agent_id, user_id)
            }
            "usage_analytics" => {
                let days = input["days"].as_u64().unwrap_or(7) as u32;
                memory.analytics_usage(agent_id, days)
            }
            "recent_conversations" => {
                let limit = input["limit"].as_u64().unwrap_or(10).min(50) as u32;
                memory.analytics_recent_conversations(agent_id, limit)
            }
            other => Err(format!(
                "Unknown query_type '{}'. Valid: user_stats, user_lookup, usage_analytics, recent_conversations",
                other
            )),
        };

        Some(match result {
            Ok(val) => Ok(serde_json::to_string_pretty(&val).unwrap_or_else(|_| "Query succeeded but serialization failed".to_string())),
            Err(e) => Err(e),
        })
    }

    fn permission_level(&self, _tool_name: &str) -> PermissionLevel {
        PermissionLevel::None
    }
}
