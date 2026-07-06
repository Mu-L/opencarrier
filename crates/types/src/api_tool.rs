//! Declarative API tool types — TOML-driven HTTP tool definitions.
//!
//! Tools are defined in `api_tools.toml` (global or per-workspace) and
//! registered at startup as `ToolProvider` instances. No Rust code needed
//! for the common "call HTTP endpoint, extract JSON fields" pattern.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single tool definition from `api_tools.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToolDef {
    pub name: String,
    pub description: String,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    /// Env var name holding the API key (e.g. "AMAP_API_KEY").
    pub auth_env: Option<String>,
    /// Query param name for the API key (e.g. "key").
    pub auth_param: Option<String>,
    #[serde(default)]
    pub params: HashMap<String, ApiParamDef>,
    #[serde(default)]
    pub extract: HashMap<String, ApiExtractDef>,
    #[serde(default)]
    pub error_check: Option<ApiErrorCheck>,
    #[serde(default)]
    pub resolve: HashMap<String, ApiResolveDef>,
    #[serde(default)]
    pub cron: Option<ApiCronDef>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_method() -> String {
    "GET".to_string()
}

/// Input parameter definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiParamDef {
    #[serde(default)]
    pub required: bool,
    #[serde(default = "default_type_string")]
    pub r#type: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub default: Option<serde_json::Value>,
}

fn default_type_string() -> String {
    "string".to_string()
}

/// Output field extraction from JSON response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiExtractDef {
    /// Dot-path into response JSON (e.g. "route.paths[0].distance").
    pub path: Option<String>,
    /// Output type: "int", "float", "string".
    #[serde(default)]
    pub r#type: Option<String>,
    /// Built-in transform: "divide_1000_round1", "divide_60_round", etc.
    pub transform: Option<String>,
    /// If true, this field is derived from other extracted fields, not from API.
    #[serde(default)]
    pub derived: Option<bool>,
    /// For derived fields: which other extracted field to derive from.
    pub from: Option<String>,
    /// For derived tier mapping.
    pub tiers: Option<Vec<ApiTier>>,
}

/// Tier mapping for derived fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiTier {
    /// Less-than-or-equal threshold.
    pub le: Option<f64>,
    /// Output value when condition matches.
    pub value: String,
}

/// Error check: validate response before extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiErrorCheck {
    /// Field to check in response (e.g. "status").
    pub field: String,
    /// Expected value (e.g. "1" for Amap).
    pub expect: String,
}

/// Pre-request resolution: call another tool to resolve a parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResolveDef {
    /// Name of another api_tool to call.
    pub tool: String,
    /// Param name to pass to that tool.
    pub param: String,
    /// Field to extract from that tool's result.
    pub extract: String,
    /// Condition for when to resolve (e.g. "not_coordinates").
    pub condition: Option<String>,
}

/// Cron definition for periodic API calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCronDef {
    /// Cron expression (e.g. "0 */6 * * *").
    pub schedule: String,
    /// SQLite database path (relative to workspace).
    pub save_to: Option<String>,
    /// Table name for auto-creation.
    pub table: Option<String>,
}

/// Parsed api_tools.toml — array of tool definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToolsConfig {
    pub tool: Vec<ApiToolDef>,
}

impl ApiToolDef {
    /// Build the JSON Schema for this tool's parameters.
    pub fn input_schema_json(&self) -> String {
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();

        for (name, param) in &self.params {
            let mut prop = serde_json::json!({
                "type": match param.r#type.as_str() {
                    "int" | "integer" => "integer",
                    "float" | "number" => "number",
                    "bool" | "boolean" => "boolean",
                    _ => "string",
                }
            });
            if !param.description.is_empty() {
                prop["description"] = serde_json::Value::String(param.description.clone());
            }
            if let Some(ref default) = param.default {
                prop["default"] = default.clone();
            }
            properties.insert(name.clone(), prop);
            if param.required {
                required.push(serde_json::Value::String(name.clone()));
            }
        }

        // Add auth param if not already in params (some APIs include it in URL template)
        // Don't expose auth params to the LLM — they're injected automatically.

        let schema = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        });

        serde_json::to_string(&schema).unwrap_or_else(|_| "{}".to_string())
    }
}
