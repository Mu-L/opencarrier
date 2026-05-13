//! Toolset meta-tool — activates a group of tools by name.

use crate::tool_context::ToolContext;
use crate::tools::ToolModule;
use async_trait::async_trait;
use serde_json::Value;
use types::tool::ToolDefinition;

pub struct ToolsetTools;

#[async_trait]
impl ToolModule for ToolsetTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "use_toolset".to_string(),
            description: "Activate a toolset to access its tools. Available toolsets are listed in your system prompt under Toolsets. Call this when you need tools from a specific category or MCP server that you don't currently have access to.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "toolset": {
                        "type": "string",
                        "description": "Name of the toolset to activate (e.g. feishu, browser, filesystem, knowledge)"
                    }
                },
                "required": ["toolset"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "use_toolset" {
            return None;
        }
        let toolset_name = input
            .get("toolset")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        Some(Ok(format!(
            "Activating toolset '{}'... The tools will be available in your next response.",
            toolset_name
        )))
    }
}
