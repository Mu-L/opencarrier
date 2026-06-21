//! Browser automation tools — powered by AginxBrowser HTTP API.
//!
//! Replaces the old browser-mcp (standalone MCP server) with direct HTTP calls
//! to a local AginxBrowser instance (default: http://127.0.0.1:8089).
//!
//! Supported tools:
//! - browser_navigate: fetch a page and return content (markdown/html/text)
//! - browser_click: click an element via JS element.click()
//! - browser_evaluate: run arbitrary JavaScript on the page
//!
//! Legacy tools (browser_type, browser_scroll, browser_wait, browser_back,
//! browser_screenshot, browser_close) are emulated via browser_evaluate where
//! possible, or return an error explaining the limitation.

use super::ToolModule;
use super::{aginxbrowser_url, AGINXBROWSER_TIMEOUT_SECS};
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct BrowserTools;

#[async_trait]
impl ToolModule for BrowserTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "browser_navigate".to_string(),
                description: "Open a URL in the browser and return the page content. \
Supports markdown, html, or text output. Use CSS selectors to extract specific regions. \
Set use_proxy=true for foreign sites that may be blocked."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Target URL to open"
                        },
                        "format": {
                            "type": "string",
                            "enum": ["markdown", "html", "text"],
                            "description": "Output format. Default: markdown"
                        },
                        "selector": {
                            "type": "string",
                            "description": "Optional CSS selector to extract a specific region"
                        },
                        "wait_secs": {
                            "type": "integer",
                            "description": "Seconds to wait after page load for JS rendering"
                        },
                        "use_proxy": {
                            "type": "boolean",
                            "description": "Route through proxy for foreign sites. Default: false"
                        }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_click".to_string(),
                description: "Click an element on the page using JS element.click(). \
Returns the page text after clicking."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Page URL (will navigate first)"
                        },
                        "selector": {
                            "type": "string",
                            "description": "CSS selector of the element to click"
                        },
                        "wait_secs": {
                            "type": "integer",
                            "description": "Seconds to wait after page load before clicking"
                        }
                    },
                    "required": ["url", "selector"]
                }),
            },
            ToolDefinition {
                name: "browser_evaluate".to_string(),
                description: "Run arbitrary JavaScript on the page and return the result. \
Useful for scrolling, extracting data, filling forms, or any custom interaction. \
The script can be an expression or an async IIFE."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Page URL (will navigate first)"
                        },
                        "script": {
                            "type": "string",
                            "description": "JavaScript expression or IIFE to execute"
                        },
                        "wait_secs": {
                            "type": "integer",
                            "description": "Seconds to wait after page load before executing"
                        }
                    },
                    "required": ["url", "script"]
                }),
            },
            // Legacy tool aliases — emulated via evaluate or return helpful error
            ToolDefinition {
                name: "browser_type".to_string(),
                description: "Type text into an input field (emulated via JS).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "selector": { "type": "string", "description": "CSS selector of the input" },
                        "text": { "type": "string", "description": "Text to type" }
                    },
                    "required": ["url", "selector", "text"]
                }),
            },
            ToolDefinition {
                name: "browser_scroll".to_string(),
                description: "Scroll the page (emulated via JS).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "direction": { "type": "string", "enum": ["up", "down"], "description": "Scroll direction" },
                        "amount": { "type": "integer", "description": "Pixels to scroll. Default: 500" }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_back".to_string(),
                description: "Go back to the previous page (emulated via JS history.back()).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Current page URL (for context)" }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_screenshot".to_string(),
                description: "Capture a screenshot. NOTE: AginxBrowser does not support screenshots. \
Use browser_navigate to extract page content instead."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" }
                    }
                }),
            },
            ToolDefinition {
                name: "browser_read_page".to_string(),
                description: "Extract page content as text. Alias for browser_navigate with format=text."
                    .to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "selector": { "type": "string" }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_wait".to_string(),
                description: "Wait for a condition or element (emulated via JS).".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "selector": { "type": "string", "description": "CSS selector to wait for" },
                        "timeout_ms": { "type": "integer", "description": "Max wait time in ms. Default: 5000" }
                    },
                    "required": ["url"]
                }),
            },
            ToolDefinition {
                name: "browser_close".to_string(),
                description: "Close the browser session. NOTE: AginxBrowser is stateless; this is a no-op."
                    .to_string(),
                input_schema: serde_json::json!({ "type": "object", "properties": {} }),
            },
        ]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        match name {
            "browser_navigate" | "browser_read_page" => Some(browser_navigate(input).await),
            "browser_click" => Some(browser_click(input).await),
            "browser_evaluate" => Some(browser_evaluate(input).await),
            "browser_type" => Some(browser_type(input).await),
            "browser_scroll" => Some(browser_scroll(input).await),
            "browser_back" => Some(browser_back(input).await),
            "browser_screenshot" => Some(browser_screenshot(input).await),
            "browser_wait" => Some(browser_wait(input).await),
            "browser_close" => Some(Ok("Browser session closed (AginxBrowser is stateless).".to_string())),
            _ => None,
        }
    }

    fn permission_level(&self, tool_name: &str) -> PermissionLevel {
        match tool_name {
            "browser_navigate" | "browser_read_page" | "browser_click" | "browser_evaluate"
            | "browser_type" | "browser_scroll" | "browser_back" | "browser_wait"
            | "browser_screenshot" | "browser_close" => PermissionLevel::ReadOnly,
            _ => PermissionLevel::Dangerous,
        }
    }
}

// ---------------------------------------------------------------------------
// HTTP client helpers
// ---------------------------------------------------------------------------

/// Shared AginBrowser HTTP request — POST to a given path, return JSON response.
async fn do_aginxbrowser_request(path: &str, req_body: Value) -> Result<Value, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(AGINXBROWSER_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let url = format!("{}/{}", aginxbrowser_url(), path);
    let resp = client
        .post(&url)
        .json(&req_body)
        .send()
        .await
        .map_err(|e| format!("AginxBrowser request failed: {e}"))?;

    let status = resp.status();
    let body = resp
        .json::<Value>()
        .await
        .map_err(|e| format!("Failed to parse AginxBrowser response: {e}"))?;

    if !status.is_success() {
        let err = body["error"].as_str().unwrap_or("Unknown error");
        return Err(format!("AginxBrowser error ({}): {}", status, err));
    }

    Ok(body)
}

async fn do_fetch_request(req_body: Value) -> Result<Value, String> {
    do_aginxbrowser_request("fetch", req_body).await
}

async fn do_click_request(req_body: Value) -> Result<Value, String> {
    do_aginxbrowser_request("click", req_body).await
}

async fn do_eval_request(req_body: Value) -> Result<Value, String> {
    do_aginxbrowser_request("eval", req_body).await
}

// ---------------------------------------------------------------------------
// Tool implementations
// ---------------------------------------------------------------------------

async fn browser_navigate(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;
    let format = input["format"].as_str().unwrap_or("markdown");
    let selector = input["selector"].as_str();
    let wait_secs = input["wait_secs"].as_u64();
    let use_proxy = input["use_proxy"].as_bool().unwrap_or(false);

    let mut req_body = serde_json::json!({
        "url": url,
        "format": format,
        "use_proxy": use_proxy,
    });
    if let Some(s) = selector {
        req_body["selector"] = s.into();
    }
    if let Some(w) = wait_secs {
        req_body["wait_secs"] = w.into();
    }

    let resp = do_fetch_request(req_body).await?;

    let title = resp["title"].as_str().unwrap_or("");
    let content = resp["content"].as_str().unwrap_or("");
    let final_url = resp["url"].as_str().unwrap_or(url);

    let result = if !title.is_empty() {
        format!("Title: {}\nURL: {}\n\n{}", title, final_url, content)
    } else {
        format!("URL: {}\n\n{}", final_url, content)
    };

    Ok(result)
}

async fn browser_click(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;
    let selector = input["selector"].as_str().ok_or("Missing 'selector' parameter")?;
    let wait_secs = input["wait_secs"].as_u64();

    let mut req_body = serde_json::json!({
        "url": url,
        "selector": selector,
    });
    if let Some(w) = wait_secs {
        req_body["wait_secs"] = w.into();
    }

    let resp = do_click_request(req_body).await?;

    let clicked = resp["clicked"].as_bool().unwrap_or(false);
    let text_after = resp["text_after"].as_str().unwrap_or("");
    let final_url = resp["url"].as_str().unwrap_or(url);

    let result = if clicked {
        format!("Clicked element '{}'.\nURL: {}\n\nPage text after click:\n{}", selector, final_url, text_after)
    } else {
        format!("Element '{}' not found on page.\nURL: {}", selector, final_url)
    };

    Ok(result)
}

async fn browser_evaluate(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;
    let script = input["script"].as_str().ok_or("Missing 'script' parameter")?;
    let wait_secs = input["wait_secs"].as_u64();

    let mut req_body = serde_json::json!({
        "url": url,
        "script": script,
    });
    if let Some(w) = wait_secs {
        req_body["wait_secs"] = w.into();
    }

    let resp = do_eval_request(req_body).await?;

    let result = &resp["result"];
    let final_url = resp["url"].as_str().unwrap_or(url);

    let result_str = serde_json::to_string_pretty(result)
        .unwrap_or_else(|_| result.to_string());

    Ok(format!("URL: {}\n\nResult:\n{}", final_url, result_str))
}

// Legacy tool emulations via browser_evaluate

async fn browser_type(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;
    let selector = input["selector"].as_str().ok_or("Missing 'selector' parameter")?;
    let text = input["text"].as_str().ok_or("Missing 'text' parameter")?;

    let script = format!(
        r#"(function() {{
            var el = document.querySelector('{}');
            if (!el) return {{error: "Element not found"}};
            el.focus();
            el.value = '{}';
            el.dispatchEvent(new Event('input', {{bubbles: true}}));
            el.dispatchEvent(new Event('change', {{bubbles: true}}));
            return {{success: true, value: el.value}};
        }})()"#,
        selector.replace("'", "\\'"),
        text.replace("'", "\\'")
    );

    let req_body = serde_json::json!({
        "url": url,
        "script": script,
    });

    let resp = do_eval_request(req_body).await?;
    let result = &resp["result"];

    if result.get("error").is_some() {
        return Err(result["error"].as_str().unwrap_or("Type failed").to_string());
    }

    Ok(format!("Typed '{}' into '{}'. Result: {}", text, selector, result))
}

async fn browser_scroll(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;
    let direction = input["direction"].as_str().unwrap_or("down");
    let amount = input["amount"].as_u64().unwrap_or(500);

    let delta_y = if direction == "up" {
        -(amount as i64)
    } else {
        amount as i64
    };

    let script = format!(
        "window.scrollBy(0, {}); ({{scrollY: window.scrollY, scrollHeight: document.body.scrollHeight}})",
        delta_y
    );

    let req_body = serde_json::json!({
        "url": url,
        "script": script,
    });

    let resp = do_eval_request(req_body).await?;
    let result = &resp["result"];

    Ok(format!("Scrolled {} by {}px. Result: {}", direction, amount, result))
}

async fn browser_back(_input: &Value) -> Result<String, String> {
    Ok("browser_back: AginxBrowser is stateless and does not maintain navigation history. \
Use browser_navigate with the target URL instead.".to_string())
}

async fn browser_screenshot(_input: &Value) -> Result<String, String> {
    Err("Screenshots are not supported by AginxBrowser. \
AginxBrowser uses a lightweight engine without a layout/paint renderer. \
Use browser_navigate to extract page content as text/markdown instead.".to_string())
}

async fn browser_wait(input: &Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;
    let selector = input["selector"].as_str();
    let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(5000);

    let script = if let Some(sel) = selector {
        format!(
            r#"(async function() {{
                const start = Date.now();
                while (Date.now() - start < {}) {{
                    if (document.querySelector('{}')) return {{found: true}};
                    await new Promise(r => setTimeout(r, 200));
                }}
                return {{found: false, timeout: true}};
            }})()"#,
            timeout_ms,
            sel.replace("'", "\\'")
        )
    } else {
        format!(
            "(async function() {{ await new Promise(r => setTimeout(r, {})); return {{waited: true}}; }})()",
            timeout_ms
        )
    };

    let req_body = serde_json::json!({
        "url": url,
        "script": script,
    });

    let resp = do_eval_request(req_body).await?;
    let result = &resp["result"];

    Ok(format!("Wait result: {}", result))
}
