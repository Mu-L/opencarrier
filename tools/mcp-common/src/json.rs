//! JSON and string utilities for MCP servers.

use serde_json::Value;

/// Serialize a `Value` to JSON string, falling back to an error object on failure.
pub fn json_to_string(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|e| format!("{{\"error\": \"serialize: {}\"}}", e))
}

/// Build a safe JSON error response string.
/// Uses `serde_json::json!` to avoid injection via `format!`.
pub fn error_response(e: impl std::fmt::Display) -> String {
    serde_json::json!({ "error": e.to_string() }).to_string()
}

/// Build a safe JSON success response, truncating if needed.
/// Combines `json_to_string` + `truncate_result` in one call.
pub fn ok_response(data: &Value, max_bytes: usize) -> String {
    truncate_result(json_to_string(data), max_bytes)
}

/// Truncate a string to `max_bytes` UTF-8 bytes at a character boundary.
pub fn truncate_result(text: String, max_bytes: usize) -> String {
    if text.len() > max_bytes {
        let truncated = &text[..max_bytes];
        let boundary = truncated
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(max_bytes);
        format!(
            "{}...\n(truncated, full result is {} bytes)",
            &text[..boundary],
            text.len()
        )
    } else {
        text
    }
}

/// Strip HTML tags and decode common entities into plain text.
pub fn strip_html(s: &str) -> String {
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
    result = result.replace("&nbsp;", " ");
    result = result.replace("&lt;", "<");
    result = result.replace("&gt;", ">");
    result = result.replace("&amp;", "&");
    result = result.replace("&#39;", "'");
    result = result.replace("&quot;", "\"");
    result
}

/// Percent-encode a string for URL query parameters (application/x-www-form-urlencoded).
pub fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => {
                result.push('+');
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

/// Sanitize a header value by removing CR/LF characters to prevent HTTP header injection.
pub fn sanitize_header_value(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n').collect()
}
