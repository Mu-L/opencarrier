//! searxng-mcp — SearXNG search MCP Server.
//!
//! Provides a `web_search` tool that queries a SearXNG instance
//! and returns structured search results in Markdown format.
//!
//! Configuration via environment variables:
//! - `SEARXNG_URL`: SearXNG instance URL (default: http://localhost:8888)
//! - `SEARXNG_USERNAME` / `SEARXNG_PASSWORD`: optional Basic Auth

use anyhow::Result;
use mcp_common::json::truncate_result;
use reqwest::header::HeaderMap;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, transport::stdio as stdio_transport, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;
use std::sync::Arc;
use tracing::info;

// ---------------------------------------------------------------------------
// SearXNG HTTP client
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SearXngClient {
    base_url: String,
    http: reqwest::Client,
    auth_header: Option<String>,
}

impl SearXngClient {
    fn new(base_url: &str, username: Option<&str>, password: Option<&str>) -> Self {
        let auth_header = match (username, password) {
            (Some(u), Some(p)) => {
                let creds = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, format!("{u}:{p}"));
                Some(format!("Basic {creds}"))
            }
            _ => None,
        };
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            auth_header,
        }
    }

    async fn search(&self, params: &SearchParams) -> Result<Vec<SearchResult>, String> {
        let url = format!("{}/search", self.base_url);

        let mut req = self.http.get(&url).query(&[
            ("q", params.query.as_str()),
            ("format", "json"),
            ("language", &params.language),
        ]);

        if let Some(ref time_range) = params.time_range {
            req = req.query(&[("time_range", time_range.as_str())]);
        }
        if let Some(ref categories) = params.categories {
            for cat in categories {
                req = req.query(&[("categories", cat.as_str())]);
            }
        }
        if let Some(ref engines) = params.engines {
            for eng in engines {
                req = req.query(&[("engines", eng.as_str())]);
            }
        }
        req = req.query(&[
            ("safesearch", &params.safesearch.to_string()),
            ("pageno", &params.pageno.to_string()),
        ]);

        if let Some(ref auth) = self.auth_header {
            let mut headers = HeaderMap::new();
            if let Ok(name) = "Authorization".parse::<reqwest::header::HeaderName>() {
                if let Ok(val) = auth.parse::<reqwest::header::HeaderValue>() {
                    headers.insert(name, val);
                }
            }
            req = req.headers(headers);
        }

        let resp = req.send().await.map_err(|e| format!("SearXNG request failed: {e}"))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let truncated: String = text.chars().take(500).collect();
            return Err(format!("SearXNG HTTP {status}: {truncated}"));
        }

        let data: SearXngResponse = resp.json().await.map_err(|e| format!("SearXNG parse error: {e}"))?;

        Ok(data.results.into_iter().take(params.max_results).map(|r| SearchResult {
            title: r.title,
            url: r.url,
            content: r.content,
            engine: r.engine,
            published_date: r.publishedDate,
        }).collect())
    }
}

#[derive(Debug, Deserialize)]
struct SearXngResponse {
    results: Vec<SearXngResult>,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct SearXngResult {
    title: String,
    url: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    engine: String,
    #[serde(default)]
    publishedDate: Option<String>,
}

#[derive(Debug, Clone)]
struct SearchResult {
    title: String,
    url: String,
    content: String,
    engine: String,
    published_date: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP tool parameters
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchParams {
    #[schemars(description = "Search query")]
    query: String,
    #[schemars(description = "Language code for results (e.g. 'en', 'zh')", default = "default_language")]
    #[serde(default = "default_language")]
    language: String,
    #[schemars(description = "Time range: 'day', 'week', 'month', 'year'")]
    #[serde(default)]
    time_range: Option<String>,
    #[schemars(description = "Search categories (e.g. ['general', 'news'])")]
    #[serde(default)]
    categories: Option<Vec<String>>,
    #[schemars(description = "Specific search engines to use (default: google, bing)")]
    #[serde(default = "default_engines")]
    engines: Option<Vec<String>>,
    #[schemars(description = "Safe search level: 0 (off), 1 (moderate), 2 (strict)", default = "default_safesearch")]
    #[serde(default = "default_safesearch")]
    safesearch: u8,
    #[schemars(description = "Result page number (minimum 1)", default = "default_pageno")]
    #[serde(default = "default_pageno")]
    pageno: usize,
    #[schemars(description = "Maximum number of results (1-50)", default = "default_max_results")]
    #[serde(default = "default_max_results")]
    max_results: usize,
}

fn default_language() -> String { "zh".to_string() }
fn default_safesearch() -> u8 { 1 }
fn default_pageno() -> usize { 1 }
fn default_max_results() -> usize { 10 }
fn default_engines() -> Option<Vec<String>> { Some(vec!["google".into(), "bing".into()]) }

// ---------------------------------------------------------------------------
// MCP server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct SearXngServer {
    client: Arc<SearXngClient>,
}

#[tool_router(server_handler)]
impl SearXngServer {
    #[tool(description = "Search the web using SearXNG (a privacy-respecting meta search engine). Returns results with title, URL, and content snippet.")]
    async fn web_search(&self, Parameters(params): Parameters<SearchParams>) -> String {
        info!("web_search: query={:?} language={}", params.query, params.language);

        let max_results = params.max_results.clamp(1, 50);
        let mut params = params;
        params.max_results = max_results;

        match self.client.search(&params).await {
            Ok(results) => {
                if results.is_empty() {
                    return "No results found.".to_string();
                }
                let mut output = String::new();
                for r in &results {
                    output.push_str(&format!("### {}\n", r.title));
                    output.push_str(&format!("{}\n", r.url));
                    if !r.content.is_empty() {
                        output.push_str(&format!("{}\n", r.content));
                    }
                    if let Some(ref date) = r.published_date {
                        output.push_str(&format!("Published: {}\n", date));
                    }
                    output.push_str("---\n");
                }
                output.push_str(&format!("\n{} results from engine(s): {}\n",
                    results.len(),
                    results.iter().map(|r| r.engine.as_str()).collect::<std::collections::HashSet<_>>().into_iter().collect::<Vec<_>>().join(", ")
                ));
                truncate_result(output, 30_000)
            }
            Err(e) => format!("Search error: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "searxng_mcp=info".into()),
        )
        .init();

    let base_url = std::env::var("SEARXNG_URL")
        .unwrap_or_else(|_| "http://localhost:8888".to_string());
    let username = std::env::var("SEARXNG_USERNAME").ok();
    let password = std::env::var("SEARXNG_PASSWORD").ok();

    info!("Starting SearXNG MCP server, endpoint={}", base_url);

    let client = SearXngClient::new(&base_url, username.as_deref(), password.as_deref());
    let server = SearXngServer {
        client: Arc::new(client),
    };
    let service = server.serve(stdio_transport()).await?;
    service.waiting().await?;

    Ok(())
}
