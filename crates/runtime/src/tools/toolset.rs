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
            kernel.search_tools(query, 5)
        } else {
            Vec::new()
        };

        if results.is_empty() {
            return Some(Ok("No tools found matching your query. All available tools are already loaded.".to_string()));
        }

        let allowed: std::collections::HashSet<&str> = ctx
            .allowed_tools
            .map(|a| a.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();

        let mut already = Vec::new();
        let mut discovered = Vec::new();

        for (ts_name, def) in &results {
            let desc_preview = if def.description.len() > 120 {
                format!("{}...", &def.description[..117])
            } else {
                def.description.clone()
            };
            let entry = format!("- {} (from {}): {}", def.name, ts_name, desc_preview);
            if allowed.contains(def.name.as_str()) {
                already.push(entry);
            } else {
                discovered.push(entry);
            }
        }

        let mut out = String::new();
        if already.is_empty() && !discovered.is_empty() {
            out.push_str(&format!("Found {} new tool(s) matching \"{}\":\n\n", discovered.len(), query));
            for line in &discovered {
                out.push_str(line);
                out.push('\n');
            }
            out.push_str("\nThese tools will be available in your next response.");
        } else if !already.is_empty() && discovered.is_empty() {
            out.push_str(&format!("All {} matching tool(s) for \"{}\" are already in your tool list:\n\n", already.len(), query));
            for line in &already {
                out.push_str(line);
                out.push('\n');
            }
            out.push_str("\nDo NOT call tool_search again for this. Use these tools directly.");
        } else {
            out.push_str(&format!("Found {} tool(s) matching \"{}\":\n\n", results.len(), query));
            if !already.is_empty() {
                out.push_str("Already available:\n");
                for line in &already {
                    out.push_str(line);
                    out.push('\n');
                }
                out.push('\n');
            }
            if !discovered.is_empty() {
                out.push_str("Newly discovered (will be activated next turn):\n");
                for line in &discovered {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }

        Some(Ok(out))
    }
}
