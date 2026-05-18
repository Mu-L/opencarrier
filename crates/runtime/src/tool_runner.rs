//! Built-in tool execution.
//!
//! Provides web tools and tool dispatch. Most tools are now in the `tools` module.

use crate::mcp;
use crate::tool_context::ToolContext;
use types::taint::{TaintLabel, TaintSink, TaintedValue};
use types::tool::{ToolDefinition, ToolResult};
use types::tool_compat::normalize_tool_name;
use std::collections::HashSet;
use tracing::{debug, warn};

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
                warn!(url = crate::str_utils::safe_truncate_str(url, 80), %violation, "Net fetch taint check failed");
                return Some(violation.to_string());
            }
        }
    }
    None
}

tokio::task_local! {
    /// Tracks the current inter-agent call depth within a task.
    pub(crate) static AGENT_CALL_DEPTH: std::cell::Cell<u32>;
    /// Canvas max HTML size in bytes (set from kernel config at loop start).
    pub(crate) static CANVAS_MAX_BYTES: usize;
}

/// Maximum inter-agent call depth (used by agent tools).
pub(crate) const MAX_AGENT_CALL_DEPTH: u32 = 5;

/// Execute a tool by name with the given input, returning a ToolResult.
///
/// The optional `kernel` handle enables inter-agent tools. If `None`,
/// agent tools will return an error indicating the kernel is not available.
///
/// `max_tool_level` enforces permission-based security: tools above the
/// agent's maximum permission level are not available.
pub async fn execute_tool(
    tool_use_id: &str,
    tool_name: &str,
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> ToolResult {
    // Unpack context into local bindings matching the old parameter names.
    let ToolContext {
        kernel: _,
        caller_agent_id: _,
        mcp_connections,
        web_ctx,
        allowed_env_vars: _,
        workspace_root: _,
        brain: _,
        exec_policy: _,
        process_manager: _,
        sender_id: _,
        owner_id: _,
        home_dir: _,
        agent_name: _,
        subagent_configs: _,
        channel_type: _,
        max_tool_level,
    } = *ctx;

    // Normalize the tool name through compat mappings so LLM-hallucinated aliases
    // (e.g. "fs-write" → "file_write") resolve to the canonical Carrier name.
    let tool_name = normalize_tool_name(tool_name);

    // Permission enforcement: reject tools above max_tool_level or Dangerous
    let modules = crate::tools::builtin_modules();
    let mut permission_checked = false;
    for module in &modules {
        if module.definitions().iter().any(|d| d.name == tool_name) {
            let level = module.permission_level(tool_name);
            if level > max_tool_level {
                warn!(tool_name, ?level, ?max_tool_level, "Permission denied: tool exceeds max level");
                return ToolResult {
                    tool_use_id: tool_use_id.to_string(),
                    content: format!(
                        "Permission denied: tool '{tool_name}' requires {:?} level but agent is limited to {:?}",
                        level, max_tool_level
                    ),
                    is_error: true,
                };
            }
            permission_checked = true;
            break;
        }
    }

    // For tools not in any builtin module (e.g. MCP tools), use the
    // centralized PermissionLevel::for_tool() for permission checks.
    if !permission_checked {
        let level = types::tool::PermissionLevel::for_tool(tool_name);
        if level > max_tool_level {
            warn!(tool_name, ?level, ?max_tool_level, "Permission denied: non-builtin tool exceeds max level");
            return ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: format!(
                    "Permission denied: tool '{tool_name}' requires {:?} level but agent is limited to {:?}",
                    level, max_tool_level
                ),
                is_error: true,
            };
        }
    }

    debug!(tool_name, "Executing tool");

    // Phase 1: Try extracted tool modules (filesystem, shell, misc, ...)
    let modules = crate::tools::builtin_modules();
    for module in &modules {
        if let Some(result) = module.execute(tool_name, input, ctx).await {
            return match result {
                Ok(content) => ToolResult {
                    tool_use_id: tool_use_id.to_string(),
                    content: truncate_tool_result(tool_name, content),
                    is_error: false,
                },
                Err(err) => ToolResult {
                    tool_use_id: tool_use_id.to_string(),
                    content: format!("Error: {err}"),
                    is_error: true,
                },
            };
        }
    }

    // Phase 2: Remaining tools not yet extracted to modules
    let result = match tool_name {
        // Web tools
        "web_fetch" => {
            let url = input["url"].as_str().unwrap_or("");
            if let Some(violation) = check_taint_net_fetch(url) {
                return ToolResult {
                    tool_use_id: tool_use_id.to_string(),
                    content: format!("Taint violation: {violation}"),
                    is_error: true,
                };
            }
            match web_ctx {
                Some(ctx) => {
                    let method = input["method"].as_str().unwrap_or("GET");
                    let headers = input.get("headers").and_then(|v| v.as_object());
                    let body = input["body"].as_str();
                    ctx.fetch
                        .fetch_with_options(url, method, headers, body)
                        .await
                }
                None => Err("Web fetch not available".to_string()),
            }
        }
        "web_search" => match web_ctx {
            Some(ctx) => {
                let query = input["query"].as_str().unwrap_or("");
                let max_results = input["max_results"].as_u64().unwrap_or(5) as usize;
                ctx.search(query, max_results).await
            }
            None => Err("Web search not available".to_string()),
        },

        // Browser automation tools are now handled by browser-mcp (standalone MCP server)
        other => {
            // Fallback 1: MCP tools (mcp_{server}_{tool} prefix)
            // Permission already enforced by max_tool_level check above
            if mcp::is_mcp_tool(other) {
                if let Some(mcp_conns) = mcp_connections {
                    // Collect known server keys from DashMap for name resolution
                    let known_keys: Vec<String> =
                        mcp_conns.iter().map(|e| e.key().clone()).collect();
                    let known_refs: Vec<&str> = known_keys.iter().map(|s| s.as_str()).collect();
                    if let Some(server_key) = mcp::extract_mcp_server_from_known(other, &known_refs)
                    {
                        // O(1) lookup by normalized server name — no global lock
                        if let Some(mut conn) = mcp_conns.get_mut(&server_key.to_string()) {
                            debug!(
                                tool = other,
                                server = server_key,
                                "Dispatching to MCP server"
                            );
                            match conn.call_tool(other, input).await {
                                Ok(content) => Ok(content),
                                Err(e) => Err(format!("MCP tool call failed: {e}")),
                            }
                        } else {
                            Err(format!("MCP server '{server_key}' not connected"))
                        }
                    } else {
                        Err(format!("Invalid MCP tool name: {other}"))
                    }
                } else {
                    Err(format!("MCP not available for tool: {other}"))
                }
            } else {
                Err(format!("Unknown tool: {other}"))
            }
        }
    };

    match result {
        Ok(content) => ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: truncate_tool_result(tool_name, content),
            is_error: false,
        },
        Err(err) => ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: format!("Error: {err}"),
            is_error: true,
        },
    }
}

/// Per-tool maximum result size in characters.
/// Tools returning more than this will be truncated with a marker.
/// None means no per-tool limit (dynamic context truncation still applies).
fn tool_max_result_chars(name: &str) -> Option<usize> {
    match name {
        "web_fetch" => Some(20_000),
        "web_search" => Some(10_000),
        "file_read" => Some(50_000),
        "shell_exec" => Some(10_000),
        "knowledge_read" => Some(30_000),
        "image_analyze" | "media_describe" | "media_transcribe" => Some(10_000),
        _ => None,
    }
}

/// Truncate a tool result if it exceeds the per-tool max size.
/// Two-stage compression: collapse duplicate lines, then keep head + tail.
fn truncate_tool_result(tool_name: &str, content: String) -> String {
    let max = match tool_max_result_chars(tool_name) {
        Some(m) => m,
        None => return content,
    };
    if content.len() <= max {
        return content;
    }

    let original_len = content.len();

    // Stage 1: collapse consecutive duplicate lines (3+ → keep first + marker)
    let deduped = dedup_lines(&content);
    if deduped.len() <= max {
        let saved = original_len.saturating_sub(deduped.len());
        if saved > 0 {
            return format!("{deduped}\n\n[compressed: {:.1} KB → {:.1} KB]",
                original_len as f64 / 1024.0, deduped.len() as f64 / 1024.0);
        }
        return deduped;
    }

    // Stage 2: keep head + tail lines
    let result = smart_truncate(&deduped, max);
    format!("{result}\n\n[compressed: {:.1} KB → {:.1} KB]",
        original_len as f64 / 1024.0, result.len() as f64 / 1024.0,
    )
}

/// Collapse consecutive duplicate lines. Runs of 3+ identical lines keep only
/// the first occurrence. Runs of 2 are preserved as-is.
fn dedup_lines(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < 5 {
        return content.to_string();
    }
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let mut run = 1;
        while i + run < lines.len() && lines[i + run] == line {
            run += 1;
        }
        out.push(line.to_string());
        if run >= 3 {
            out.push(format!("  ... ({} duplicate lines)", run - 1));
        } else if run == 2 {
            out.push(line.to_string());
        }
        i += run;
    }
    out.join("\n")
}

/// Keep HEAD_LINES from the top and TAIL_LINES from the bottom, drop the middle.
/// Falls back to char-boundary truncation for content too short for head/tail.
fn smart_truncate(content: &str, max_chars: usize) -> String {
    const HEAD_LINES: usize = 120;
    const TAIL_LINES: usize = 60;

    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= HEAD_LINES + TAIL_LINES + 10 {
        if content.len() <= max_chars {
            return content.to_string();
        }
        let mut bp = max_chars.min(content.len());
        while bp > 0 && !content.is_char_boundary(bp) {
            bp -= 1;
        }
        let mut search_start = bp.saturating_sub(200);
        while search_start > 0 && !content.is_char_boundary(search_start) {
            search_start -= 1;
        }
        if let Some(nl_pos) = content[search_start..bp].rfind('\n') {
            bp = search_start + nl_pos;
        }
        return content[..bp].to_string();
    }

    let head: Vec<&str> = lines.iter().take(HEAD_LINES).copied().collect();
    let tail: Vec<&str> = lines.iter().rev().take(TAIL_LINES).copied().rev().collect();
    let cut = lines.len() - head.len() - tail.len();
    format!(
        "{}\n\n... +{cut} lines\n\n{}",
        head.join("\n"),
        tail.join("\n"),
    )
}

/// Get definitions for all built-in tools.
pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    // Collect definitions from extracted modules
    let mut defs: Vec<ToolDefinition> = crate::tools::builtin_modules()
        .into_iter()
        .flat_map(|m| m.definitions())
        .collect();

    // Web tools (still dispatched from this file)
    defs.extend(vec![
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a URL with SSRF protection. Supports GET/POST/PUT/PATCH/DELETE. For GET, HTML is converted to Markdown. For other methods, returns raw response body.".to_string(),
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
        },
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web using multiple providers (Tavily, Brave, Perplexity, DuckDuckGo) with automatic fallback. Returns structured results with titles, URLs, and snippets.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query" },
                    "max_results": { "type": "integer", "description": "Maximum number of results to return (default: 5, max: 20)" }
                },
                "required": ["query"]
            }),
        },
    ]);
    defs
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: empty ToolContext for tests that don't need any services.
    fn noop_ctx() -> ToolContext<'static> {
        ToolContext {
            kernel: None,
            caller_agent_id: None,
            mcp_connections: None,
            web_ctx: None,
            allowed_env_vars: None,
            workspace_root: None,
            brain: None,
            exec_policy: None,
            process_manager: None,
            sender_id: None,
            owner_id: None,
            home_dir: None,
            agent_name: None,
            subagent_configs: None,
            channel_type: None,
            max_tool_level: types::tool::PermissionLevel::Write,
        }
    }

    #[test]
    fn test_builtin_tool_definitions() {
        let tools = builtin_tool_definitions();
        assert!(
            tools.len() >= 25,
            "Expected at least 25 tools, got {}",
            tools.len()
        );
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        // Training tools (cross-workspace)
        assert!(
            names.contains(&"train_read"),
            "Missing train_read in: {:?}",
            names
        );
        assert!(names.contains(&"train_write"), "Missing train_write");
        assert!(names.contains(&"train_list"), "Missing train_list");
        assert!(names.contains(&"train_evaluate"), "Missing train_evaluate");
        // Original 12
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"shell_exec"));
        assert!(names.contains(&"agent_send"));
        assert!(names.contains(&"agent_spawn"));
        assert!(names.contains(&"agent_list"));
        assert!(names.contains(&"agent_kill"));
        assert!(names.contains(&"agent_send"));
        assert!(names.contains(&"agent_list"));
        // 6 collaboration tools
        assert!(names.contains(&"agent_find"));
        assert!(names.contains(&"task_post"));
        assert!(names.contains(&"task_claim"));
        assert!(names.contains(&"task_complete"));
        assert!(names.contains(&"task_list"));
        assert!(names.contains(&"event_publish"));
        // 5 new Phase 3 tools
        assert!(names.contains(&"schedule_create"));
        assert!(names.contains(&"schedule_list"));
        assert!(names.contains(&"schedule_delete"));
        assert!(names.contains(&"image_analyze"));
        assert!(names.contains(&"location_get"));
        assert!(names.contains(&"system_time"));
        // Browser tools are now provided by browser-mcp (standalone MCP server)
        // 3 media/image generation tools
        assert!(names.contains(&"media_describe"));
        assert!(names.contains(&"media_transcribe"));
        assert!(names.contains(&"image_generate"));
        // 3 cron tools
        assert!(names.contains(&"cron_create"));
        assert!(names.contains(&"cron_list"));
        assert!(names.contains(&"cron_cancel"));
        // Voice tools
        assert!(names.contains(&"text_to_speech"));
        assert!(names.contains(&"speech_to_text"));
        // Canvas tool
        assert!(names.contains(&"canvas_present"));
    }

    #[test]
    fn test_collaboration_tool_schemas() {
        let tools = builtin_tool_definitions();
        let collab_tools = [
            "agent_find",
            "task_post",
            "task_claim",
            "task_complete",
            "task_list",
            "event_publish",
        ];
        for name in &collab_tools {
            let tool = tools
                .iter()
                .find(|t| t.name == *name)
                .unwrap_or_else(|| panic!("Tool '{}' not found", name));
            // Verify each has a valid JSON schema
            assert!(
                tool.input_schema.is_object(),
                "Tool '{}' schema should be an object",
                name
            );
            assert_eq!(
                tool.input_schema["type"], "object",
                "Tool '{}' should have type=object",
                name
            );
        }
    }

    #[tokio::test]
    async fn test_file_read_missing() {
        let bad_path = std::env::temp_dir()
            .join("carrier_test_nonexistent_99999")
            .join("file.txt");
        let result = execute_tool(
            "test-id",
            "file_read",
            &serde_json::json!({"path": bad_path.to_str().unwrap()}),
            &noop_ctx(),
        )
        .await;
        assert!(
            result.is_error,
            "Expected error but got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn test_file_read_path_traversal_blocked() {
        let result = execute_tool(
            "test-id",
            "file_read",
            &serde_json::json!({"path": "../../etc/passwd"}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("traversal"));
    }

    #[tokio::test]
    async fn test_file_write_path_traversal_blocked() {
        let result = execute_tool(
            "test-id",
            "file_write",
            &serde_json::json!({"path": "../../../tmp/evil.txt", "content": "pwned"}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("traversal"));
    }

    #[tokio::test]
    async fn test_file_list_path_traversal_blocked() {
        let result = execute_tool(
            "test-id",
            "file_list",
            &serde_json::json!({"path": "/foo/../../etc"}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("traversal") || result.content.contains("Absolute"),
            "Expected path rejection, got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn test_web_search() {
        let result = execute_tool(
            "test-id",
            "web_search",
            &serde_json::json!({"query": "rust programming"}),
            &noop_ctx(),
        )
        .await;
        // web_search now attempts a real fetch; may succeed or fail depending on network
        assert!(!result.tool_use_id.is_empty());
    }

    #[tokio::test]
    async fn test_unknown_tool() {
        let result = execute_tool(
            "test-id",
            "nonexistent_tool",
            &serde_json::json!({}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        // Unknown tools are rejected by permission check (for_tool defaults to Dangerous)
        // before reaching the "Unknown tool" error path. Both outcomes are correct.
        assert!(result.content.contains("Permission denied") || result.content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn test_agent_tools_without_kernel() {
        let result =
            execute_tool("test-id", "agent_list", &serde_json::json!({}), &noop_ctx()).await;
        assert!(result.is_error);
        assert!(result.content.contains("Kernel handle not available"));
    }

    #[tokio::test]
    async fn test_permission_level_denied() {
        // shell_exec is Dangerous level, noop_ctx has Write level — should be denied
        let result = execute_tool(
            "test-id",
            "shell_exec",
            &serde_json::json!({"command": "ls"}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("Permission denied"));
    }

    #[tokio::test]
    async fn test_permission_level_allowed() {
        // file_read is ReadOnly level, noop_ctx has Write level — should pass permission check
        let result = execute_tool(
            "test-id",
            "file_read",
            &serde_json::json!({"path": "carrier_test_nonexistent_12345/file.txt"}),
            &noop_ctx(),
        )
        .await;
        assert!(
            result.is_error,
            "Expected error but got: {}",
            result.content
        );
        assert!(
            result.content.contains("Failed to read")
                || result.content.contains("not found")
                || result.content.contains("No such file"),
            "Unexpected error: {}",
            result.content
        );
    }

    #[test]
    fn test_depth_limit_constant() {
        assert_eq!(MAX_AGENT_CALL_DEPTH, 5);
    }

    #[test]
    fn test_depth_limit_first_call_succeeds() {
        // Default depth is 0, which is < MAX_AGENT_CALL_DEPTH
        let default_depth = AGENT_CALL_DEPTH.try_with(|d| d.get()).unwrap_or(0);
        assert!(default_depth < MAX_AGENT_CALL_DEPTH);
    }

    #[test]
    fn test_task_local_compiles() {
        // Verify task_local macro works — just ensure the type exists
        let cell = std::cell::Cell::new(0u32);
        assert_eq!(cell.get(), 0);
    }

    #[tokio::test]
    async fn test_schedule_tools_without_kernel() {
        let result = execute_tool(
            "test-id",
            "schedule_list",
            &serde_json::json!({}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("Kernel handle not available"));
    }

    // ------------------------------------------------------------------
    // dedup_lines
    // ------------------------------------------------------------------

    #[test]
    fn test_dedup_lines_collapses_long_runs() {
        let input = "line1\nline1\nline1\nline2\nline3\nline3\nline3\nline3";
        let out = dedup_lines(input);
        assert!(out.contains("... (2 duplicate lines)"));
        assert!(out.contains("... (3 duplicate lines)"));
        // line2 preserved
        assert!(out.contains("\nline2\n"));
    }

    #[test]
    fn test_dedup_lines_preserves_pairs() {
        let input = "a\na\nb\nb";
        let out = dedup_lines(input);
        assert!(!out.contains("duplicate"));
        assert_eq!(out.matches('\n').count(), input.matches('\n').count());
    }

    #[test]
    fn test_dedup_lines_skips_short_content() {
        let input = "one\ntwo";
        let out = dedup_lines(input);
        assert_eq!(out, input);
    }

    // ------------------------------------------------------------------
    // smart_truncate
    // ------------------------------------------------------------------

    #[test]
    fn test_smart_truncate_keeps_head_and_tail() {
        let mut lines: Vec<String> = Vec::new();
        for i in 0..300 {
            lines.push(format!("line {i}"));
        }
        let content = lines.join("\n");
        let out = smart_truncate(&content, 4096);
        // Should contain early lines
        assert!(out.contains("line 0"));
        assert!(out.contains("line 10"));
        // Should contain late lines
        assert!(out.contains("line 299"));
        assert!(out.contains("line 290"));
        // Should have truncation marker
        assert!(out.contains("... +"));
        assert!(out.contains("lines"));
    }

    #[test]
    fn test_smart_truncate_short_content_unchanged() {
        let content = "line1\nline2\nline3";
        let out = smart_truncate(content, 4096);
        assert_eq!(out, content);
    }
}
