//! Built-in web search tool — powered by AginxBrowser /search endpoint.
//!
//! Replaces the old searxng-mcp (standalone MCP server) with a direct HTTP call
//! to the local AginxBrowser instance (default: http://127.0.0.1:8089).
//!
//! AginxBrowser /search provides native search aggregation (baidu, sogou, etc.) and optionally
//! auto-fetches full content for top N results (one-step "search → read").
//!
//! Enabled via `AGINXBROWSER_URL` env var (same switch as web_fetch and browser_*).
//! Not set → tool returns "Search not available".

use super::ToolModule;
use super::{aginxbrowser_url_opt, AGINXBROWSER_TIMEOUT_SECS};
use crate::tool_context::ToolContext;
use async_trait::async_trait;
use types::tool::{PermissionLevel, ToolDefinition};
use serde_json::Value;

pub struct WebSearchTools;

#[async_trait]
impl ToolModule for WebSearchTools {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web using AginxBrowser (native search aggregation). \
                Returns results with title, URL, and snippet. \
                Set fetch_top>0 to auto-fetch full content for top N results (one-step search+read)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "q": {
                        "type": "string",
                        "description": "Search query"
                    },
                    "fetch_top": {
                        "type": "integer",
                        "description": "Auto-fetch full content for top N results. Default: 0 (snippet only, fast)"
                    },
                    "categories": {
                        "type": "string",
                        "description": "Search category: general, news, images, etc. Default: general"
                    },
                    "language": {
                        "type": "string",
                        "description": "Language code. Default: zh-CN"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Max number of results. Default: 10"
                    },
                    "max_chars_per": {
                        "type": "integer",
                        "description": "Truncate each fetched content to N chars. Default: 4000. 0 = no limit"
                    },
                },
                "required": ["q"]
            }),
        }]
    }

    async fn execute(
        &self,
        name: &str,
        input: &Value,
        _ctx: &ToolContext<'_>,
    ) -> Option<Result<String, String>> {
        if name != "web_search" {
            return None;
        }

        let base = match aginxbrowser_url_opt() {
            Some(u) => u,
            None => return Some(Err("Search not available: AGINXBROWSER_URL not set".into())),
        };

        Some(do_search(&base, input).await)
    }

    fn permission_level(&self, _tool_name: &str) -> PermissionLevel {
        PermissionLevel::ReadOnly
    }
}

/// POST AginxBrowser /search and format results as Markdown.
async fn do_search(base_url: &str, input: &Value) -> Result<String, String> {
    let q = input["q"]
        .as_str()
        .ok_or("Missing required parameter: q")?;

    let mut body = serde_json::json!({
        "q": q,
    });

    // Optional parameters — only include if provided
    if let Some(v) = input["fetch_top"].as_u64() {
        body["fetch_top"] = v.into();
    }
    if let Some(v) = input["categories"].as_str() {
        body["categories"] = v.into();
    }
    if let Some(v) = input["language"].as_str() {
        body["language"] = v.into();
    }
    if let Some(v) = input["max_results"].as_u64() {
        body["max_results"] = v.into();
    }
    if let Some(v) = input["max_chars_per"].as_u64() {
        body["max_chars_per"] = v.into();
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(AGINXBROWSER_TIMEOUT_SECS + 30)) // search+fetch needs more time
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let url = format!("{}/search", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("AginxBrowser search request failed: {e}"))?;

    let status = resp.status();
    if status.as_u16() == 503 {
        return Err("Search backend unavailable. AginxBrowser /search returned 503.".into());
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        let truncated: String = text.chars().take(500).collect();
        return Err(format!("AginxBrowser search error ({}): {}", status, truncated));
    }

    let data: Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse search response: {e}"))?;

    let results = data["results"]
        .as_array()
        .ok_or("Malformed search response: missing results array")?;

    if results.is_empty() {
        return Ok("No results found.".to_string());
    }

    let mut output = String::new();
    for (i, r) in results.iter().enumerate() {
        let title = r["title"].as_str().unwrap_or("(untitled)");
        let link = r["url"].as_str().unwrap_or("");
        let snippet = r["snippet"].as_str().or_else(|| r["content"].as_str()).unwrap_or("");

        output.push_str(&format!("### {}. {}\n", i + 1, title));
        output.push_str(&format!("{}\n", link));
        if !snippet.is_empty() {
            output.push_str(&format!("{}\n", snippet));
        }

        // Full content (only present when fetch_top > 0 and index < fetch_top)
        if let Some(content) = r["content"].as_str() {
            if !content.is_empty() {
                output.push_str(&format!("\n**Full content:**\n{}\n", content));
                if r["content_truncated"].as_bool().unwrap_or(false) {
                    output.push_str("(content truncated)\n");
                }
            }
        }
        if let Some(err) = r["fetch_error"].as_str() {
            if !err.is_empty() {
                output.push_str(&format!("⚠️ Fetch error: {}\n", err));
            }
        }

        output.push_str("---\n");
    }

    let total = data["number_of_results"]
        .as_u64()
        .map(|n| format!("{} total results", n))
        .unwrap_or_default();

    let backend = data["search_backend"]
        .as_str()
        .unwrap_or("unknown");

    output.push_str(&format!(
        "\n{} shown, {}. Backend: {}\n",
        results.len(),
        total,
        backend
    ));

    // Truncate very long outputs (char-boundary-safe to avoid panic on multi-byte UTF-8)
    if output.len() > 60_000 {
        let truncated = crate::str_utils::safe_truncate_str(&output, 50_000);
        output = format!("{}... [truncated, {} total chars]", truncated, output.len());
    }

    Ok(output)
}
