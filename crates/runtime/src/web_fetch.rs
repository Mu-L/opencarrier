//! Enhanced web fetch with SSRF protection, HTML→Markdown extraction,
//! in-memory caching, and external content markers.
//!
//! Pipeline: SSRF check → cache lookup → HTTP GET → detect HTML →
//! html_to_markdown() → truncate → wrap_external_content() → cache → return

use crate::str_utils::safe_truncate_str;
use crate::web_cache::WebCache;
use crate::web_content::{html_to_markdown, wrap_external_content};
use types::config::WebFetchConfig;
use types::ssrf;
use std::sync::Arc;
use tracing::debug;

/// 外挂 AginxBrowser 需要兜底抓取的站点（JS 渲染或风控）。命中且 AginxBrowser 已启用时
/// 走浏览器，其余走 reqwest 直连。这是 web_fetch 的私有路由策略，不暴露为配置。
const AGINXBROWSER_HOSTS: &[&str] = &[
    "mp.weixin.qq.com", // 微信公众号文章（风控）
    "zhuanlan.zhihu.com", // 知乎专栏（JS 渲染）
    "search.jd.com", // 京东搜索（动态）
    "github.com", // GitHub（动态 + 需代理）
];

/// 读 AGINXBROWSER_URL。未设/空 → None（不启用外挂，纯 reqwest，行为等同改造前）。
/// 在 fetch 调用时读取（非构造时），故环境变量可动态生效，且无需把字段塞进 WebFetchConfig。
fn aginxbrowser_url() -> Option<String> {
    std::env::var("AGINXBROWSER_URL").ok().filter(|s| !s.is_empty())
}

/// 目标 URL 是否属于已知需要浏览器渲染/过风控的站点。
fn should_use_aginxbrowser(url: &str) -> bool {
    let host = ssrf::extract_host(url); // 返回 "host:port"
    AGINXBROWSER_HOSTS.iter().any(|h| host.contains(h)) // .contains 兼容带端口后缀
}

/// Enhanced web fetch engine with SSRF protection and readability extraction.
pub struct WebFetchEngine {
    config: WebFetchConfig,
    client: reqwest::Client,
    cache: Arc<WebCache>,
}

impl WebFetchEngine {
    /// Create a new fetch engine from config with a shared cache.
    pub fn new(config: WebFetchConfig, cache: Arc<WebCache>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .redirect(reqwest::redirect::Policy::none())
            .gzip(true)
            .deflate(true)
            .brotli(true)
            .build()
            .unwrap_or_default();
        Self {
            config,
            client,
            cache,
        }
    }

    /// Fetch a URL with full security pipeline (GET only, for backwards compat).
    pub async fn fetch(&self, url: &str) -> Result<String, String> {
        self.fetch_with_options(url, "GET", None, None).await
    }

    /// Fetch a URL with configurable HTTP method, headers, and body.
    pub async fn fetch_with_options(
        &self,
        url: &str,
        method: &str,
        headers: Option<&serde_json::Map<String, serde_json::Value>>,
        body: Option<&str>,
    ) -> Result<String, String> {
        let method_upper = method.to_uppercase();

        // Step 1: SSRF protection — BEFORE any network I/O
        ssrf::check_ssrf(url)?;

        // Step 2: Cache lookup (only for GET)
        let cache_key = format!("fetch:{}:{}", method_upper, url);
        if method_upper == "GET" {
            if let Some(cached) = self.cache.get(&cache_key) {
                debug!(url, "Fetch cache hit");
                return Ok(cached);
            }
        }

        // Step 2b: 外挂 AginxBrowser —— 命中风控站 + 已启用时走浏览器抓取，失败回退 reqwest。
        // 仅对 GET 生效；POST/PUT 等 API 调用永远走 reqwest。对 agent 完全透明。
        if method_upper == "GET" && should_use_aginxbrowser(url) && aginxbrowser_url().is_some() {
            if let Ok(content) = self.fetch_via_aginxbrowser(url).await {
                let truncated = if content.len() > self.config.max_chars {
                    format!(
                        "{}... [truncated, {} total chars]",
                        safe_truncate_str(&content, self.config.max_chars),
                        content.len()
                    )
                } else {
                    content
                };
                let result = format!(
                    "HTTP 200 (via AginxBrowser)\n\n{}",
                    wrap_external_content(url, &truncated)
                );
                self.cache.put(cache_key.clone(), result.clone());
                return Ok(result);
            }
            // AginxBrowser 失败/空内容 → 不 return，继续走下方 reqwest（降级）
            debug!(url, "AginxBrowser fetch failed or empty, falling back to reqwest");
        }

        // Step 3: Build request with configured method
        let mut req = match method_upper.as_str() {
            "POST" => self.client.post(url),
            "PUT" => self.client.put(url),
            "PATCH" => self.client.patch(url),
            "DELETE" => self.client.delete(url),
            _ => self.client.get(url),
        };
        req = req.header(
            "User-Agent",
            format!("Mozilla/5.0 (compatible; {})", crate::USER_AGENT),
        );

        // Add custom headers
        if let Some(hdrs) = headers {
            for (k, v) in hdrs {
                if let Some(val) = v.as_str() {
                    req = req.header(k.as_str(), val);
                }
            }
        }

        // Add body for non-GET methods
        if let Some(b) = body {
            // Auto-detect JSON body
            if b.trim_start().starts_with('{') || b.trim_start().starts_with('[') {
                req = req.header("Content-Type", "application/json");
            }
            req = req.body(b.to_string());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {e}"))?;

        // Step 3b: Handle redirects manually with SSRF validation on each hop
        let (final_resp, final_url) = self.follow_redirects(resp, url).await?;
        let status = final_resp.status();

        // Check response size
        if let Some(len) = final_resp.content_length() {
            if len > self.config.max_response_bytes as u64 {
                return Err(format!(
                    "Response too large: {} bytes (max {})",
                    len, self.config.max_response_bytes
                ));
            }
        }

        let content_type = final_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let resp_body = final_resp
            .text()
            .await
            .map_err(|e| format!("Failed to read response body: {e}"))?;

        // Step 4: For GET requests, detect HTML and convert to Markdown.
        // For non-GET (API calls), return raw body — don't mangle JSON/XML responses.
        let processed = if method_upper == "GET"
            && self.config.readability
            && is_html(&content_type, &resp_body)
        {
            let markdown = html_to_markdown(&resp_body);
            if markdown.trim().is_empty() {
                resp_body
            } else {
                markdown
            }
        } else {
            resp_body
        };

        // Step 5: Truncate (char-boundary-safe to avoid panics on multi-byte UTF-8)
        let truncated = if processed.len() > self.config.max_chars {
            format!(
                "{}... [truncated, {} total chars]",
                safe_truncate_str(&processed, self.config.max_chars),
                processed.len()
            )
        } else {
            processed
        };

        // Step 6: Wrap with external content markers
        let result = format!(
            "HTTP {status}\n\n{}",
            wrap_external_content(&final_url, &truncated)
        );

        // Step 7: Cache (only GET responses)
        if method_upper == "GET" {
            self.cache.put(cache_key, result.clone());
        }

        Ok(result)
    }

    /// 调外挂 AginxBrowser 的 /fetch，返回 markdown 正文。失败返回 Err（调用方回退 reqwest）。
    /// 请求格式对齐 browser.rs 的 do_fetch_request；响应解析 {content, title, url}。
    async fn fetch_via_aginxbrowser(&self, url: &str) -> Result<String, String> {
        let base = aginxbrowser_url().expect("caller guards aginxbrowser_url().is_some()");
        let body = serde_json::json!({
            "url": url,
            "format": "markdown",
            "wait_secs": 4, // 等 JS 渲染（微信/动态页必需）
        });
        let resp: serde_json::Value = self.client
            .post(format!("{}/fetch", base.trim_end_matches('/')))
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("AginxBrowser request failed: {e}"))?
            .json()
            .await
            .map_err(|e| format!("AginxBrowser parse failed: {e}"))?;
        resp.get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "AginxBrowser response missing/empty content".into())
    }
}

/// Detect if content is HTML based on Content-Type header or body sniffing.
fn is_html(content_type: &str, body: &str) -> bool {
    if content_type.contains("text/html") || content_type.contains("application/xhtml") {
        return true;
    }
    let trimmed = body.trim_start();
    trimmed.starts_with("<!DOCTYPE")
        || trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html")
}

impl WebFetchEngine {
    /// Follow HTTP redirects with SSRF validation on each hop.
    /// Limits redirect chain to 5 hops to prevent redirect loops.
    async fn follow_redirects(
        &self,
        mut resp: reqwest::Response,
        original_url: &str,
    ) -> Result<(reqwest::Response, String), String> {
        let mut current_url = original_url.to_string();
        let max_hops = 5;

        for _ in 0..max_hops {
            let status = resp.status().as_u16();
            if !(status == 301 || status == 302 || status == 303 || status == 307 || status == 308) {
                return Ok((resp, current_url));
            }

            let location = match resp.headers().get("location").and_then(|v| v.to_str().ok()) {
                Some(loc) => loc.to_string(),
                None => return Ok((resp, current_url)),
            };

            // Resolve relative URLs
            let next_url = if location.starts_with("http://") || location.starts_with("https://") {
                location
            } else if location.starts_with('/') {
                let base = ssrf::extract_host(&current_url);
                let scheme = if current_url.starts_with("https") { "https" } else { "http" };
                format!("{scheme}://{base}{location}")
            } else {
                format!("{current_url}/{location}")
            };

            ssrf::check_ssrf(&next_url)?;

            debug!(from = %current_url, to = %next_url, "Following redirect");

            let req = self.client.get(&next_url)
                .header("User-Agent", format!("Mozilla/5.0 (compatible; {})", crate::USER_AGENT));

            resp = req.send().await.map_err(|e| format!("Redirect request failed: {e}"))?;
            current_url = next_url;
        }

        Err("Too many redirects (max 5)".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::str_utils::safe_truncate_str;

    #[test]
    fn test_truncate_multibyte_no_panic() {
        // Simulate a gzip-decoded response containing multi-byte UTF-8
        // (Chinese, Japanese, emoji — common on international finance sites).
        // Old code: &s[..max] panics when max lands inside a multi-byte char.
        let content = "\u{4f60}\u{597d}\u{4e16}\u{754c}!"; // "你好世界!" = 13 bytes
                                                           // Truncate at byte 7 — lands inside the 3rd Chinese char (bytes 6..9).
                                                           // safe_truncate_str walks back to byte 6, returning "你好".
        let truncated = safe_truncate_str(content, 7);
        assert_eq!(truncated, "\u{4f60}\u{597d}");
        assert!(truncated.len() <= 7);
    }

    #[test]
    fn test_truncate_emoji_no_panic() {
        let content = "\u{1f4b0}\u{1f4c8}\u{1f4b9}"; // 💰📈💹 = 12 bytes
                                                     // Truncate at byte 5 — lands inside the 2nd emoji (bytes 4..8).
        let truncated = safe_truncate_str(content, 5);
        assert_eq!(truncated, "\u{1f4b0}"); // 4 bytes
    }

    #[test]
    fn test_ssrf_blocks_localhost() {
        assert!(ssrf::check_ssrf("http://localhost/admin").is_err());
        assert!(ssrf::check_ssrf("http://localhost:8080/api").is_err());
    }

    #[test]
    fn test_ssrf_blocks_private_ip() {
        use std::net::IpAddr;
        assert!(ssrf::is_private_ip(&"10.0.0.1".parse::<IpAddr>().unwrap()));
        assert!(ssrf::is_private_ip(&"172.16.0.1".parse::<IpAddr>().unwrap()));
        assert!(ssrf::is_private_ip(&"192.168.1.1".parse::<IpAddr>().unwrap()));
        assert!(ssrf::is_private_ip(&"169.254.169.254".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn test_ssrf_blocks_metadata() {
        assert!(ssrf::check_ssrf("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(ssrf::check_ssrf("http://metadata.google.internal/computeMetadata/v1/").is_err());
    }

    #[test]
    fn test_ssrf_allows_public() {
        assert!(!ssrf::is_private_ip(
            &"8.8.8.8".parse::<std::net::IpAddr>().unwrap()
        ));
        assert!(!ssrf::is_private_ip(
            &"1.1.1.1".parse::<std::net::IpAddr>().unwrap()
        ));
    }

    #[test]
    fn test_ssrf_blocks_non_http() {
        assert!(ssrf::check_ssrf("file:///etc/passwd").is_err());
        assert!(ssrf::check_ssrf("ftp://internal.corp/data").is_err());
        assert!(ssrf::check_ssrf("gopher://evil.com").is_err());
    }

    #[test]
    fn test_ssrf_blocks_cloud_metadata() {
        assert!(ssrf::check_ssrf("http://100.100.100.200/latest/meta-data/").is_err());
        assert!(ssrf::check_ssrf("http://192.0.0.192/metadata/instance").is_err());
    }

    #[test]
    fn test_ssrf_blocks_zero_ip() {
        assert!(ssrf::check_ssrf("http://0.0.0.0/").is_err());
    }

    #[test]
    fn test_ssrf_blocks_ipv6_localhost() {
        assert!(ssrf::check_ssrf("http://[::1]/admin").is_err());
        assert!(ssrf::check_ssrf("http://[::1]:8080/api").is_err());
    }

    #[test]
    fn test_extract_host_ipv6() {
        let h = ssrf::extract_host("http://[::1]:8080/path");
        assert_eq!(h, "[::1]:8080");

        let h2 = ssrf::extract_host("https://[::1]/path");
        assert_eq!(h2, "[::1]:443");

        let h3 = ssrf::extract_host("http://[::1]/path");
        assert_eq!(h3, "[::1]:80");
    }
}
