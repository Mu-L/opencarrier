//! Tool search meta-tool — searches the tool catalog and returns matching tools.

use crate::tool_context::ToolContext;
use crate::tools::ToolModule;
use async_trait::async_trait;
use serde_json::Value;
use types::tool::ToolDefinition;

pub struct ToolSearchTools;

#[async_trait]
impl ToolModule for ToolSearchTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "tool_search".to_string(),
            description: "Search the tool catalog for tools matching a natural language query. Only call this when you need a capability you do NOT currently have. Check your current tool list first — if a tool is already there, use it directly. Returns matching tool names and descriptions.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "What you want to do (e.g. 'send message', 'browse web', 'read file')"
                    }
                },
                "required": ["query"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "tool_search" {
            return None;
        }
        let query = input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let results = if let Some(kernel) = ctx.kernel {
            kernel.search_tools(query, 10, ctx.max_tool_level)
        } else {
            Vec::new()
        };

        if results.is_empty() {
            return Some(Ok("No tools found matching your query. All available tools are already loaded.".to_string()));
        }

        let mut out = format!("Found {} tool(s) matching \"{}\":\n\n", results.len(), query);
        for (ts_name, def) in &results {
            let desc_preview = if def.description.len() > 200 {
                format!("{}...", &def.description[..197])
            } else {
                def.description.clone()
            };
            out.push_str(&format!("## {} (from {})\n{}\n\n", def.name, ts_name, desc_preview));
            // Include input_schema so LLM knows how to call the tool
            if !def.input_schema.is_null() {
                out.push_str(&format!("Parameters: {}\n\n", serde_json::to_string(&def.input_schema).unwrap_or_default()));
            }
        }
        out.push_str("You can call any of these tools directly. Use the tool name and follow the parameter schema.");

        Some(Ok(out))
    }

    fn permission_level(&self, tool_name: &str) -> types::tool::PermissionLevel {
        if tool_name == "tool_search" {
            types::tool::PermissionLevel::None
        } else {
            types::tool::PermissionLevel::Dangerous
        }
    }
}
