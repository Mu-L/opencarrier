//! api_tool_register — built-in tool for runtime API tool registration.
//!
//! Agent calls this with a TOML definition. The tool is parsed, validated,
//! written to the workspace's api_tools.toml, and registered in the global
//! registry so it's immediately available to all agents on the next turn.

use crate::tools::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::api_tool::{ApiToolDef, ApiToolsConfig};
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;
use std::sync::RwLock;

/// Global registry of dynamically registered API tools.
/// Written by api_tool_register, read by DeclarativeApiModule + messaging.rs.
static DYNAMIC_TOOLS: once_cell::sync::Lazy<RwLock<Vec<ApiToolDef>>> =
    once_cell::sync::Lazy::new(|| RwLock::new(Vec::new()));

/// Get all dynamically registered API tools (for inclusion in builtin_modules).
pub fn dynamic_tools() -> Vec<ApiToolDef> {
    DYNAMIC_TOOLS.read().map(|t| t.clone()).unwrap_or_default()
}

pub struct ApiToolRegisterModule;

#[async_trait]
impl ToolModule for ApiToolRegisterModule {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "api_tool_register".to_string(),
            description: "Register a new API tool from a TOML definition. \
                The tool becomes immediately available to all agents. \
                Provide a single [[tool]] block in TOML format."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "definition": {
                        "type": "string",
                        "description": "TOML definition of the API tool. \
                            Must be a valid [[tool]] block with at least name, description, url. \
                            Example: [[tool]]\\nname = \"weather\"\\ndescription = \"查询天气\"\\nurl = \"https://api.weather.com/v1\"\\nmethod = \"GET\"\\n..."
                    },
                    "global": {
                        "type": "boolean",
                        "default": false,
                        "description": "true = register globally (all agents), false = workspace-only"
                    }
                },
                "required": ["definition"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "api_tool_register" {
            return None;
        }

        let definition = match input["definition"].as_str() {
            Some(s) => s,
            None => return Some(Err("Missing 'definition' parameter (TOML string)".to_string())),
        };

        let global = input["global"].as_bool().unwrap_or(false);

        // Parse and validate the TOML definition
        let config: ApiToolsConfig = match toml::from_str(definition) {
            Ok(c) => c,
            Err(e) => return Some(Err(format!("Invalid TOML: {e}"))),
        };

        if config.tool.is_empty() {
            return Some(Err("No [[tool]] block found in definition".to_string()));
        }

        let tool_def = &config.tool[0];

        // Basic validation
        if tool_def.name.is_empty() {
            return Some(Err("Tool name is required".to_string()));
        }
        if tool_def.url.is_empty() {
            return Some(Err("Tool url is required".to_string()));
        }
        if !tool_def.url.starts_with("https://") && !tool_def.url.starts_with("http://") {
            return Some(Err("Tool url must start with http:// or https://".to_string()));
        }

        let tool_name = tool_def.name.clone();

        // Write to api_tools.toml (workspace or global)
        let write_result = if global {
            write_to_global_toml(&config.tool[0])
        } else {
            write_to_workspace_toml(&config.tool[0], ctx.workspace_root)
        };

        if let Err(e) = write_result {
            return Some(Err(format!("Failed to write api_tools.toml: {e}")));
        }

        // Register in the dynamic registry so it's available immediately
        {
            let mut tools = match DYNAMIC_TOOLS.write() {
            Ok(t) => t,
            Err(e) => return Some(Err(format!("Registry lock: {e}"))),
        };
            // Remove existing tool with same name
            tools.retain(|t| t.name != tool_name);
            tools.push(config.tool[0].clone());
        }

        Some(Ok(format!(
            "✅ API tool '{}' registered successfully. It will be available on the next agent turn. (scope: {})",
            tool_name,
            if global { "global" } else { "workspace" }
        )))
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "api_tool_register" => PermissionLevel::Write,
            _ => PermissionLevel::Dangerous,
        }
    }
}

/// Append a tool definition to the workspace's api_tools.toml.
fn write_to_workspace_toml(tool: &ApiToolDef, workspace_root: Option<&std::path::Path>) -> Result<(), String> {
    let ws = workspace_root.ok_or("No workspace root available")?;
    let toml_path = ws.join("api_tools.toml");

    let serialized = serialize_tool(tool);

    // Read existing content and append, or create new file
    let content = std::fs::read_to_string(&toml_path).unwrap_or_default();
    let new_content = if content.trim().is_empty() {
        serialized
    } else {
        format!("{}\n\n{}", content.trim_end(), serialized)
    };

    std::fs::write(&toml_path, new_content).map_err(|e| format!("Write error: {e}"))?;
    tracing::info!(path = %toml_path.display(), tool = %tool.name, "Registered API tool to workspace api_tools.toml");
    Ok(())
}

/// Append a tool definition to the global api_tools.toml (~/.opencarrier/).
fn write_to_global_toml(tool: &ApiToolDef) -> Result<(), String> {
    let home = types::config::home_dir();
    let toml_path = home.join("api_tools.toml");

    let serialized = serialize_tool(tool);

    let content = std::fs::read_to_string(&toml_path).unwrap_or_default();
    let new_content = if content.trim().is_empty() {
        serialized
    } else {
        format!("{}\n\n{}", content.trim_end(), serialized)
    };

    std::fs::write(&toml_path, new_content).map_err(|e| format!("Write error: {e}"))?;
    tracing::info!(path = %toml_path.display(), tool = %tool.name, "Registered API tool to global api_tools.toml");
    Ok(())
}

/// Serialize an ApiToolDef back to a TOML [[tool]] block.
fn serialize_tool(tool: &ApiToolDef) -> String {
    let mut out = String::new();
    out.push_str("[[tool]]\n");
    out.push_str(&format!("name = \"{}\"\n", tool.name));
    out.push_str(&format!("description = \"{}\"\n", tool.description.replace('"', "\\\"")));
    out.push_str(&format!("url = \"{}\"\n", tool.url));
    out.push_str(&format!("method = \"{}\"\n", tool.method));

    if let Some(ref auth_env) = tool.auth_env {
        out.push_str(&format!("auth_env = \"{}\"\n", auth_env));
    }
    if let Some(ref auth_param) = tool.auth_param {
        out.push_str(&format!("auth_param = \"{}\"\n", auth_param));
    }

    // Params
    if !tool.params.is_empty() {
        out.push_str("\n[tool.params]\n");
        for (name, param) in &tool.params {
            let mut parts = Vec::new();
            if param.required {
                parts.push("required = true".to_string());
            }
            parts.push(format!("type = \"{}\"", param.r#type));
            if !param.description.is_empty() {
                parts.push(format!("description = \"{}\"", param.description.replace('"', "\\\"")));
            }
            if let Some(ref default) = param.default {
                match default {
                    Value::String(s) => parts.push(format!("default = \"{}\"", s)),
                    Value::Number(n) => parts.push(format!("default = {}", n)),
                    Value::Bool(b) => parts.push(format!("default = {}", b)),
                    _ => {}
                }
            }
            out.push_str(&format!("{} = {{ {} }}\n", name, parts.join(", ")));
        }
    }

    // Extract
    if !tool.extract.is_empty() {
        out.push_str("\n[tool.extract]\n");
        for (name, def) in &tool.extract {
            let mut parts = Vec::new();
            if let Some(ref path) = def.path {
                parts.push(format!("path = \"{}\"", path));
            }
            if let Some(ref transform) = def.transform {
                parts.push(format!("transform = \"{}\"", transform));
            }
            if let Some(ref t) = def.r#type {
                parts.push(format!("type = \"{}\"", t));
            }
            if def.derived.unwrap_or(false) {
                parts.push("derived = true".to_string());
            }
            if let Some(ref from) = def.from {
                parts.push(format!("from = \"{}\"", from));
            }
            if let Some(ref tiers) = def.tiers {
                let tier_strs: Vec<String> = tiers.iter().map(|t| {
                    match t.le {
                        Some(le) => format!("{{ le = {}, value = \"{}\" }}", le, t.value),
                        None => format!("{{ value = \"{}\" }}", t.value),
                    }
                }).collect();
                parts.push(format!("tiers = [\n  {},\n]", tier_strs.join(",\n  ")));
            }
            out.push_str(&format!("{} = {{ {} }}\n", name, parts.join(", ")));
        }
    }

    // Error check
    if let Some(ref check) = tool.error_check {
        out.push_str("\n[tool.error_check]\n");
        out.push_str(&format!("field = \"{}\"\n", check.field));
        out.push_str(&format!("expect = \"{}\"\n", check.expect));
    }

    out
}
