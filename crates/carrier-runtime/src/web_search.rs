//! Multi-provider web search engine.
//!
//! Two search modes:
//! - **Free search** (`search_free`): Bing → 360 → Sogou, zero-config HTML scraping.
//!   Always available, no API keys needed. Works in China.
//! - **Brain search** (`search_brain`): `brain.complete("search", request)`.
//!   Used when brain is configured with a search modality (Tavily, Brave, Perplexity, etc.).
//!
//! The public `search()` method tries brain first, then falls back to free search.

use crate::llm_driver::Brain;
use crate::web_cache::WebCache;
use crate::web_content::wrap_external_content;
use crate::web_fetch::WebFetchEngine;
use std::sync::Arc;
use tracing::{debug, warn};

/// Multi-provider web search engine (free search only — brain path uses Brain trait).
pub struct WebSearchEngine {
    client: reqwest::Client,
    cache: Arc<WebCache>,
}

/// Context that bundles search engine, fetch engine, and optional brain for tool execution.
pub struct WebToolsContext {
    pub search: WebSearchEngine,
    pub fetch: WebFetchEngine,
    pub brain: Option<Arc<dyn Brain>>,
}

impl WebToolsContext {
    /// Perform a web search: free search (Bing/360/Sogou) first, brain as fallback.
    pub async fn search(&self, query: &str, max_results: usize) -> Result<String, String> {
        // Check cache first
        let cache_key = format!("search:{}:{}", query, max_results);
        if let Some(cached) = self.search.cache.get(&cache_key) {
            debug!(query, "Search cache hit");
            return Ok(cached);
        }

        // Free search (Bing → 360 → Sogou) is real-time web scraping — always prefer it
        let result = match self.search.search_free(query, max_results).await {
            Ok(r) => Ok(r),
            Err(e) => {
                warn!("Free search failed, trying brain search: {e}");
                // Fallback to brain if configured
                if let Some(brain) = &self.brain {
                    self.search_brain(brain, query, max_results).await
                } else {
                    Err(e)
                }
            }
        };

        if let Ok(ref content) = result {
            self.search.cache.put(cache_key, content.clone());
        }

        result
    }

    /// Search via brain's search modality (API-key providers managed by brain).
    async fn search_brain(
        &self,
        brain: &Arc<dyn Brain>,
        query: &str,
        max_results: usize,
    ) -> Result<String, String> {
        use crate::llm_driver::CompletionRequest;
        use carrier_types::message::Message;

        let request = CompletionRequest {
            model: String::new(),
            messages: vec![Message::user(query)],
            tools: vec![],
            max_tokens: (max_results as u32) * 500,
            temperature: 0.0,
            system: Some(format!(
                "You are a search assistant. Return search results for the query. \
                 Format as numbered list with title, URL, and description. \
                 Return up to {max_results} results."
            )),
            thinking: None,
            extra: Default::default(),
        };

        let response = brain
            .complete("search", request)
            .await
            .map_err(|e| format!("Brain search error: {e}"))?;

        let text = response.text();
        if text.is_empty() {
            return Err("Brain search returned empty response".to_string());
        }

        Ok(wrap_external_content("brain-search", &text))
    }
}

impl WebSearchEngine {
    /// Create a new search engine with a shared cache.
    pub fn new(cache: Arc<WebCache>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .unwrap_or_default();
        Self { client, cache }
    }

    /// Free search: Bing → 360 → Sogou fallback chain (zero-config, no API keys).
    pub async fn search_free(&self, query: &str, max_results: usize) -> Result<String, String> {
        // Bing first (most reliable in China)
        debug!(query, "Free search: trying Bing");
        match self.search_bing(query, max_results).await {
            Ok(result) => return Ok(result),
            Err(e) => warn!("Bing search failed, trying 360: {e}"),
        }

        // 360 Search (popular in China)
        debug!(query, "Free search: trying 360");
        match self.search_360(query, max_results).await {
            Ok(result) => return Ok(result),
            Err(e) => warn!("360 search failed, trying Sogou: {e}"),
        }

        // Sogou last resort
        debug!(query, "Free search: trying Sogou");
        self.search_sogou(query, max_results).await
    }

    /// Search via Bing HTML (no API key needed, works in China).
    async fn search_bing(&self, query: &str, max_results: usize) -> Result<String, String> {
        debug!(query, "Searching via Bing HTML");

        let count = max_results.to_string();
        let resp = self
            .client
            .get("https://www.bing.com/search")
            .query(&[("q", query), ("count", count.as_str())])
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
            )
            .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
            .send()
            .await
            .map_err(|e| format!("Bing request failed: {e}"))?;

        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read Bing response: {e}"))?;

        let results = parse_bing_results(&body, max_results);

        if results.is_empty() {
            return Err(format!("No results found for '{query}' (Bing)."));
        }

        let mut output = format!("Search results for '{query}':\n\n");
        for (i, (title, url, snippet)) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {}\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }

        Ok(output)
    }

    /// Search via 360 Search HTML (popular in China, no API key needed).
    async fn search_360(&self, query: &str, max_results: usize) -> Result<String, String> {
        debug!(query, "Searching via 360 HTML");

        let count = max_results.to_string();
        let resp = self
            .client
            .get("https://www.so.com/s")
            .query(&[("q", query), ("count", count.as_str())])
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
            )
            .send()
            .await
            .map_err(|e| format!("360 request failed: {e}"))?;

        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read 360 response: {e}"))?;

        let results = parse_360_results(&body, max_results);

        if results.is_empty() {
            return Err(format!("No results found for '{query}' (360)."));
        }

        let mut output = format!("Search results for '{query}':\n\n");
        for (i, (title, url, snippet)) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {}\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }

        Ok(output)
    }

    /// Search via Sogou HTML (Chinese search engine, no API key needed).
    async fn search_sogou(&self, query: &str, max_results: usize) -> Result<String, String> {
        debug!(query, "Searching via Sogou HTML");

        let resp = self
            .client
            .get("https://www.sogou.com/web")
            .query(&[("query", query)])
            .header(
                "User-Agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36",
            )
            .send()
            .await
            .map_err(|e| format!("Sogou request failed: {e}"))?;

        let body = resp
            .text()
            .await
            .map_err(|e| format!("Failed to read Sogou response: {e}"))?;

        let results = parse_sogou_results(&body, max_results);

        if results.is_empty() {
            return Err(format!("No results found for '{query}' (Sogou)."));
        }

        let mut output = format!("Search results for '{query}':\n\n");
        for (i, (title, url, snippet)) in results.iter().enumerate() {
            output.push_str(&format!(
                "{}. {}\n   URL: {}\n   {}\n\n",
                i + 1,
                title,
                url,
                snippet
            ));
        }

        Ok(output)
    }
}

// ---------------------------------------------------------------------------
// Bing HTML parser
// ---------------------------------------------------------------------------

/// Parse Bing HTML search results into (title, url, snippet) tuples.
pub fn parse_bing_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();

    for block in html.split("<li class=\"b_algo\">") {
        if results.len() >= max {
            break;
        }

        let url = extract_between(block, "<a href=\"", "\"")
            .unwrap_or_default()
            .to_string();

        let title = if let Some(href_end) = block.find("\">") {
            let after = &block[href_end + 2..];
            extract_between(after, "", "</a>")
                .map(strip_html_tags)
                .unwrap_or_default()
        } else {
            String::new()
        };

        let snippet = extract_bing_snippet(block);

        if !title.is_empty() && !url.is_empty() && !url.starts_with("javascript:") {
            results.push((title, url, snippet));
        }
    }

    results
}

fn extract_bing_snippet(block: &str) -> String {
    if let Some(cap) = block.find("class=\"b_caption\"") {
        let after = &block[cap..];
        if let Some(p_start) = after.find("<p>") {
            let p_after = &after[p_start + 3..];
            if let Some(p_end) = p_after.find("</p>") {
                return strip_html_tags(&p_after[..p_end]);
            }
        }
    }

    if let Some(p_start) = block.find("<p>") {
        let after = &block[p_start + 3..];
        if let Some(p_end) = after.find("</p>") {
            return strip_html_tags(&after[..p_end]);
        }
    }

    String::new()
}

// ---------------------------------------------------------------------------
// 360 Search HTML parser
// ---------------------------------------------------------------------------

/// Parse 360 Search HTML results into (title, url, snippet) tuples.
fn parse_360_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();

    // 360 results are in <li class="res-list"> blocks
    for block in html.split("<li class=\"res-list") {
        if results.len() >= max {
            break;
        }

        let url = extract_between(block, "<a href=\"", "\"")
            .unwrap_or_default()
            .to_string();

        let title = if let Some(href_end) = block.find("\">") {
            let after = &block[href_end + 2..];
            extract_between(after, "", "</a>")
                .map(strip_html_tags)
                .unwrap_or_default()
        } else {
            String::new()
        };

        // 360 snippets are in <p class="res-desc"> or class="res-desc-rich"
        let snippet = if let Some(desc_start) = block.find("class=\"res-desc") {
            let after = &block[desc_start..];
            if let Some(content_start) = after.find(">") {
                let content = &after[content_start + 1..];
                if let Some(end) = content.find("</p>") {
                    strip_html_tags(&content[..end])
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if !title.is_empty() && !url.is_empty() {
            results.push((title, url, snippet));
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Sogou HTML parser
// ---------------------------------------------------------------------------

/// Parse Sogou HTML results into (title, url, snippet) tuples.
fn parse_sogou_results(html: &str, max: usize) -> Vec<(String, String, String)> {
    let mut results = Vec::new();

    // Sogou results are in <div class="vrwrap"> or <div class="rb"> blocks
    for block in html.split("class=\"vrwrap\"") {
        if results.len() >= max {
            break;
        }

        let url = extract_between(block, "href=\"", "\"")
            .unwrap_or_default()
            .to_string();

        let title = extract_between(block, ">", "</a>")
            .map(strip_html_tags)
            .unwrap_or_default();

        // Sogou snippets are in <p class="str-text-info"> or <div class="str_info">
        let snippet = if let Some(snip_start) = block.find("class=\"str-text-info\"") {
            let after = &block[snip_start..];
            if let Some(content_start) = after.find(">") {
                let content = &after[content_start + 1..];
                if let Some(end) = content.find("</p>") {
                    strip_html_tags(&content[..end])
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        if !title.is_empty() && !url.is_empty() && !url.starts_with("javascript:") {
            results.push((title, url, snippet));
        }
    }

    results
}

// ---------------------------------------------------------------------------
// HTML utilities
// ---------------------------------------------------------------------------

/// Extract text between two delimiters.
pub fn extract_between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let start_idx = text.find(start)? + start.len();
    let remaining = &text[start_idx..];
    let end_idx = remaining.find(end)?;
    Some(&remaining[..end_idx])
}

/// Strip HTML tags from a string.
pub fn strip_html_tags(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }
    result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&#39;", "'")
}

/// Simple percent-decode for URLs.
pub fn urldecode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push_str(&hex);
            }
        } else if ch == '+' {
            result.push(' ');
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bing_parser_basic() {
        let html = r#"junk <li class="b_algo"><a href="https://example.com">Example</a><div class="b_caption"><p>A test snippet</p></div>"#;
        let results = parse_bing_results(html, 5);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "Example");
        assert_eq!(results[0].1, "https://example.com");
        assert_eq!(results[0].2, "A test snippet");
    }

    #[test]
    fn test_bing_parser_empty() {
        let results = parse_bing_results("<html><body>No results</body></html>", 5);
        assert!(results.is_empty());
    }

    #[test]
    fn test_strip_html_tags() {
        assert_eq!(strip_html_tags("<b>hello</b>"), "hello");
        assert_eq!(strip_html_tags("&amp; &lt; &gt;"), "& < >");
    }
}
