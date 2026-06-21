//! Built-in web_fetch tool — fetches URLs with SSRF protection, AginBrowser
//! fallback for risk-controlled sites, HTML→Markdown conversion, and caching.
//!
//! Migrated from the Phase 2 match arm in tool_runner.rs to the ToolModule
//! framework, consistent with browser.rs and web_search.rs.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use std::collections::HashSet;
use tracing::warn;
use types::taint::{TaintLabel, TaintSink, TaintedValue};
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct WebFetchModule;

#[async_trait]
impl ToolModule for WebFetchModule {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a URL with SSRF protection. Supports GET/POST/PUT/PATCH/DELETE. \
                For GET, HTML is converted to Markdown. For other methods, returns raw response body."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to fetch (http/https only)" },
                    "method": { "type": "string", "enum": ["GET","POST","PUT","PATCH","DELETE"], "description": "HTTP method (default: GET)" },
                    "headers": { "type": "object", "description": "Custom HTTP headers as key-value pairs" },
                    "body": { "type": "string", "description": "Request body for POST/PUT/PATCH" }
                },
                "required": ["url"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "web_fetch" {
            return None;
        }

        let url = input["url"].as_str().unwrap_or("");

        // Taint check — block URLs containing API keys/tokens/secrets
        if let Some(violation) = check_taint_net_fetch(url) {
            return Some(Err(format!("Taint violation: {violation}")));
        }

        let engine = match ctx.fetch_engine {
            Some(e) => e,
            None => return Some(Err("Web fetch not available".to_string())),
        };

        let method = input["method"].as_str().unwrap_or("GET");
        let headers = input.get("headers").and_then(|v| v.as_object());
        let body = input["body"].as_str();

        Some(engine.fetch_with_options(url, method, headers, body).await)
    }

    fn permission_level(&self, _tool_name: &str) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }
}

/// Check if the URL contains taint violations (secrets in query parameters).
///
/// Blocks URLs that appear to contain API keys, tokens, or other secrets
/// in query parameters (potential data exfiltration). Implements TaintSink::net_fetch().
fn check_taint_net_fetch(url: &str) -> Option<String> {
    let exfil_patterns = [
        "api_key=",
        "apikey=",
        "token=",
        "secret=",
        "password=",
        "Authorization:",
    ];
    for pattern in &exfil_patterns {
        if url.to_lowercase().contains(&pattern.to_lowercase()) {
            let mut labels = HashSet::new();
            labels.insert(TaintLabel::Secret);
            let tainted = TaintedValue::new(url, labels, "llm_tool_call");
            if let Err(violation) = tainted.check_sink(&TaintSink::net_fetch()) {
                warn!(url = crate::str_utils::safe_truncate_str(url, 80), %violation, "Net fetch taint check failed");
                return Some(violation.to_string());
            }
        }
    }
    None
}
