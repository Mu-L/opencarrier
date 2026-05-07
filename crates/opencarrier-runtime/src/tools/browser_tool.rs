//! Browser automation tool module.

use super::ToolModule;
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use opencarrier_types::taint::{TaintLabel, TaintSink, TaintedValue};
use opencarrier_types::tool::ToolDefinition;
use serde_json::Value;
use std::collections::HashSet;
use tracing::warn;

/// Browser automation tools (navigate, click, type, screenshot, etc.).
pub struct BrowserTools;

/// Check if a URL should be blocked by taint tracking before network fetch.
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
                warn!(
                    url = crate::str_utils::safe_truncate_str(url, 80),
                    %violation,
                    "Net fetch taint check failed"
                );
                return Some(violation.to_string());
            }
        }
    }
    None
}

/// Resolve browser context and caller agent ID from the tool context.
fn require_browser<'a>(
    ctx: &'a ToolContext<'_>,
) -> Result<(&'a crate::browser::BrowserManager, &'a str), String> {
    let mgr = ctx.browser_ctx.ok_or(
        "Browser tools not available. Ensure Chrome/Chromium is installed.".to_string(),
    )?;
    let aid = ctx
        .caller_agent_id
        .ok_or("Missing caller agent identity".to_string())?;
    Ok((mgr, aid))
}

#[async_trait]
impl ToolModule for BrowserTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "browser_navigate".to_string(),
                description: "Navigate a browser to a URL. Returns the page title and readable content as markdown. Opens a persistent browser session.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "The URL to navigate to (http/https only)" }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_click".to_string(),
                description: "Click an element on the current browser page by CSS selector or visible text. Returns the resulting page state.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": { "type": "string", "description": "CSS selector (e.g., '#submit-btn', '.add-to-cart') or visible text to click" }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "browser_type".to_string(),
                description: "Type text into an input field on the current browser page.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": { "type": "string", "description": "CSS selector for the input field (e.g., 'input[name=\"email\"]', '#search-box')" },
                        "text": { "type": "string", "description": "The text to type into the field" }
                    },
                    "required": ["selector", "text"]
                }),
            },
            ToolDefinition {
                name: "browser_screenshot".to_string(),
                description: "Take a screenshot of the current browser page. Returns a base64-encoded PNG image.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_read_page".to_string(),
                description: "Read the current browser page content as structured markdown. Use after clicking or navigating to see the updated page.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_close".to_string(),
                description: "Close the browser session. The browser will also auto-close when the agent loop ends.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolDefinition {
                name: "browser_scroll".to_string(),
                description: "Scroll the browser page. Use this to see content below the fold or navigate long pages.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "direction": { "type": "string", "description": "Scroll direction: 'up', 'down', 'left', 'right' (default: 'down')" },
                        "amount": { "type": "integer", "description": "Pixels to scroll (default: 600)" }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_wait".to_string(),
                description: "Wait for a CSS selector to appear on the page. Useful for dynamic content that loads asynchronously.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "selector": { "type": "string", "description": "CSS selector to wait for" },
                        "timeout_ms": { "type": "integer", "description": "Max wait time in milliseconds (default: 5000, max: 30000)" }
                    },
                    "required": ["selector"]
                }),
            },
            ToolDefinition {
                name: "browser_run_js".to_string(),
                description: "Run JavaScript on the current browser page and return the result. For advanced interactions that other browser tools cannot handle.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "expression": { "type": "string", "description": "JavaScript expression to run in the page context" }
                    },
                    "required": ["expression"]
                }),
            },
            ToolDefinition {
                name: "browser_back".to_string(),
                description: "Go back to the previous page in browser history.".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
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
        match name {
            "browser_navigate" => {
                // Taint check on the URL before dispatching
                let url = input["url"].as_str().unwrap_or("");
                if let Some(violation) = check_taint_net_fetch(url) {
                    return Some(Err(format!("Taint violation: {violation}")));
                }
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_navigate(input, mgr, aid).await)
            }
            "browser_click" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_click(input, mgr, aid).await)
            }
            "browser_type" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_type(input, mgr, aid).await)
            }
            "browser_screenshot" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_screenshot(input, mgr, aid).await)
            }
            "browser_read_page" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_read_page(input, mgr, aid).await)
            }
            "browser_close" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_close(input, mgr, aid).await)
            }
            "browser_scroll" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_scroll(input, mgr, aid).await)
            }
            "browser_wait" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_wait(input, mgr, aid).await)
            }
            "browser_run_js" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_run_js(input, mgr, aid).await)
            }
            "browser_back" => {
                let (mgr, aid) = match require_browser(ctx) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                Some(crate::browser::tool_browser_back(input, mgr, aid).await)
            }
            _ => None,
        }
    }
}
