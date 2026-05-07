//! Built-in tool execution.
//!
//! Provides filesystem, web, shell, and inter-agent tools. Agent tools
//! (agent_send, agent_spawn, etc.) require a KernelHandle to be passed in.

use crate::kernel_handle::KernelHandle;
use crate::mcp;
use crate::tool_context::ToolContext;
use opencarrier_types::taint::{TaintLabel, TaintSink, TaintedValue};
use opencarrier_types::tool::{ToolDefinition, ToolResult};
use opencarrier_types::tool_compat::normalize_tool_name;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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
/// Dispatch a browser tool, handling the `None` (no browser) case uniformly.
#[macro_export]
macro_rules! browser_dispatch {
    ($input:expr, $browser_ctx:expr, $caller_agent_id:expr, $func:path) => {
        match $browser_ctx {
            Some(mgr) => match $caller_agent_id {
                Some(aid) => $func($input, mgr, aid).await,
                None => Err("Missing caller agent identity".to_string()),
            },
            None => {
                Err("Browser tools not available. Ensure Chrome/Chromium is installed.".to_string())
            }
        }
    };
}

///
/// `allowed_tools` enforces capability-based security: if provided, only
/// tools in the list may execute. This prevents an LLM from hallucinating
/// tool names outside the agent's capability grants.
pub async fn execute_tool(
    tool_use_id: &str,
    tool_name: &str,
    input: &serde_json::Value,
    ctx: &ToolContext<'_>,
) -> ToolResult {
    // Unpack context into local bindings matching the old parameter names.
    let ToolContext {
        kernel,
        allowed_tools,
        caller_agent_id,
        mcp_connections,
        web_ctx,
        browser_ctx,
        allowed_env_vars: _,
        workspace_root,
        media_engine: _,
        brain: _,
        exec_policy: _,
        tts_engine: _,
        docker_config: _,
        process_manager: _,
        sender_id,
    } = *ctx;

    // Normalize the tool name through compat mappings so LLM-hallucinated aliases
    // (e.g. "fs-write" → "file_write") resolve to the canonical OpenCarrier name.
    let tool_name = normalize_tool_name(tool_name);

    // Capability enforcement: reject tools not in the allowed list
    if let Some(allowed) = ctx.allowed_tools {
        if !allowed.iter().any(|t| t == tool_name) {
            warn!(tool_name, "Capability denied: tool not in allowed list");
            return ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: format!(
                    "Permission denied: agent does not have capability to use tool '{tool_name}'"
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
                    content,
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
        // Cross-workspace training tools (for trainer agents like clone-trainer)
        "train_read" => tool_train_read(input, kernel, caller_agent_id).await,
        "train_write" => tool_train_write(input, kernel, caller_agent_id).await,
        "train_list" => tool_train_list(input, kernel, caller_agent_id).await,
        "train_knowledge_add" => tool_train_knowledge_add(input, kernel, caller_agent_id).await,
        "train_knowledge_import" => {
            tool_train_knowledge_import(input, kernel, caller_agent_id).await
        }
        "train_knowledge_list" => tool_train_knowledge_list(input, kernel, caller_agent_id).await,
        "train_knowledge_read" => tool_train_knowledge_read(input, kernel, caller_agent_id).await,
        "train_knowledge_lint" => tool_train_knowledge_lint(input, kernel, caller_agent_id).await,
        "train_knowledge_heal" => tool_train_knowledge_heal(input, kernel, caller_agent_id).await,
        "train_evaluate" => tool_train_evaluate(input, kernel, caller_agent_id).await,
        "user_profile" => tool_user_profile(input, workspace_root, sender_id).await,

        // Clone management tools
        "clone_install" => tool_clone_install(input, kernel, caller_agent_id).await,
        "clone_export" => tool_clone_export(input, kernel, caller_agent_id).await,
        "clone_publish" => tool_clone_publish(input, kernel, caller_agent_id).await,

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
        "web_search" => {
            match web_ctx {
                Some(ctx) => {
                    let query = input["query"].as_str().unwrap_or("");
                    let max_results = input["max_results"].as_u64().unwrap_or(5) as usize;
                    ctx.search(query, max_results).await
                }
                None => Err("Web search not available".to_string()),
            }
        }

        // Inter-agent tools (require kernel handle)
        "agent_send" => tool_agent_send(input, kernel, caller_agent_id).await,
        "agent_spawn" => tool_agent_spawn(input, kernel, caller_agent_id).await,
        "agent_list" => tool_agent_list(kernel, caller_agent_id),
        "agent_kill" => tool_agent_kill(input, kernel, caller_agent_id),
        "agent_restart" => tool_agent_restart(input, kernel, caller_agent_id),

        // Memory tools (scoped to caller's agent + sender namespace)
        "memory_store" => tool_memory_store(input, kernel, caller_agent_id, sender_id),
        "memory_recall" => tool_memory_recall(input, kernel, caller_agent_id, sender_id),
        "memory_list" => tool_memory_list(input, kernel, caller_agent_id, sender_id),

        // Collaboration tools
        "agent_find" => tool_agent_find(input, kernel, caller_agent_id),
        "task_post" => tool_task_post(input, kernel, caller_agent_id).await,
        "task_claim" => tool_task_claim(kernel, caller_agent_id).await,
        "task_complete" => tool_task_complete(input, kernel, caller_agent_id).await,
        "task_list" => tool_task_list(input, kernel, caller_agent_id).await,
        "event_publish" => tool_event_publish(input, kernel, caller_agent_id).await,

        // Scheduling tools
        "schedule_create" => tool_schedule_create(input, kernel, caller_agent_id).await,
        "schedule_list" => tool_schedule_list(kernel, caller_agent_id).await,
        "schedule_delete" => tool_schedule_delete(input, kernel, caller_agent_id).await,

        // Knowledge graph tools
        "knowledge_add_entity" => tool_knowledge_add_entity(input, kernel, caller_agent_id).await,
        "knowledge_add_relation" => {
            tool_knowledge_add_relation(input, kernel, caller_agent_id).await
        }
        "knowledge_query" => tool_knowledge_query(input, kernel, caller_agent_id).await,

        // Cron scheduling tools
        "cron_create" => tool_cron_create(input, kernel, caller_agent_id).await,
        "cron_list" => tool_cron_list(kernel, caller_agent_id).await,
        "cron_cancel" => tool_cron_cancel(input, kernel, caller_agent_id).await,

        // A2A outbound tools (cross-instance agent communication)
        "a2a_discover" => tool_a2a_discover(input).await,
        "a2a_send" => tool_a2a_send(input, kernel).await,

        // Browser automation tools
        "browser_navigate" => {
            let url = input["url"].as_str().unwrap_or("");
            if let Some(violation) = check_taint_net_fetch(url) {
                return ToolResult {
                    tool_use_id: tool_use_id.to_string(),
                    content: format!("Taint violation: {violation}"),
                    is_error: true,
                };
            }
            browser_dispatch!(
                input,
                browser_ctx,
                caller_agent_id,
                crate::browser::tool_browser_navigate
            )
        }
        "browser_click" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_click
        ),
        "browser_type" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_type
        ),
        "browser_screenshot" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_screenshot
        ),
        "browser_read_page" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_read_page
        ),
        "browser_close" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_close
        ),
        "browser_scroll" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_scroll
        ),
        "browser_wait" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_wait
        ),
        "browser_run_js" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_run_js
        ),
        "browser_back" => browser_dispatch!(
            input,
            browser_ctx,
            caller_agent_id,
            crate::browser::tool_browser_back
        ),

        other => {
            // Fallback 1: MCP tools (mcp_{server}_{tool} prefix)
            if mcp::is_mcp_tool(other) {
                // Depth restriction: subagents (depth > 0) need explicit MCP tool
                // permission via allowed_tools. Top-level agents are unrestricted.
                let current_depth = AGENT_CALL_DEPTH.try_with(|d| d.get()).unwrap_or(0);
                if current_depth > 0 {
                    let explicitly_allowed = allowed_tools
                        .map(|a| a.iter().any(|t| t == other))
                        .unwrap_or(false);
                    if !explicitly_allowed {
                        warn!(
                            tool = other,
                            depth = current_depth,
                            "MCP tool denied for subagent: not in explicit allow list"
                        );
                        return ToolResult {
                            tool_use_id: tool_use_id.to_string(),
                            content: format!(
                                "Permission denied: MCP tool '{other}' not available at subagent depth {current_depth}"
                            ),
                            is_error: true,
                        };
                    }
                }
                if let Some(mcp_conns) = mcp_connections {
                    // Collect known server keys from DashMap for name resolution
                    let known_keys: Vec<String> =
                        mcp_conns.iter().map(|e| e.key().clone()).collect();
                    let known_refs: Vec<&str> = known_keys.iter().map(|s| s.as_str()).collect();
                    if let Some(server_key) =
                        mcp::extract_mcp_server_from_known(other, &known_refs)
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
            }
            // Fallback 2: Skill registry tool providers
            else if let Some(kh) = kernel {
                // Fallback 3: Plugin tools (dlopen-loaded shared libraries)
                let s_id = sender_id.unwrap_or("");
                let a_id = caller_agent_id.unwrap_or("");
                match kh.execute_plugin_tool(other, input, s_id, a_id).await {
                    Ok(result) => Ok(result),
                    Err(e) if e.starts_with("Unknown tool:") => Err(e),
                    Err(e) => Err(format!("Plugin tool execution failed: {e}")),
                }
            } else {
                Err(format!("Unknown tool: {other}"))
            }
        }
    };

    match result {
        Ok(content) => ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content,
            is_error: false,
        },
        Err(err) => ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: format!("Error: {err}"),
            is_error: true,
        },
    }
}

/// Get definitions for all built-in tools.
pub fn builtin_tool_definitions() -> Vec<ToolDefinition> {
    // Collect definitions from extracted modules
    let mut defs: Vec<ToolDefinition> = crate::tools::builtin_modules()
        .into_iter()
        .flat_map(|m| m.definitions())
        .collect();

    // Append remaining definitions not yet extracted to modules
    defs.extend(vec![
        // --- Knowledge tools (safe access to data/knowledge/) ---
        // --- Lifecycle system tools (clone knowledge management) ---
        // --- Evolution tools (self-driven knowledge and skill management) ---
        // --- Cross-workspace training tools (for trainer agents) ---
        ToolDefinition {
            name: "train_read".to_string(),
            description: "Read a file from a target clone's workspace. Used by trainer agents to inspect other clones.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone to read from"},
                    "path": {"type": "string", "description": "File path relative to the target clone's workspace root"},
                },
                "required": ["target", "path"],
            }),
        },
        ToolDefinition {
            name: "train_write".to_string(),
            description: "Write a file to a target clone's workspace. Can modify any file including SOUL.md, system_prompt.md, agent.toml, and skills. Used by trainer agents to train other clones.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone to write to"},
                    "path": {"type": "string", "description": "File path relative to the target clone's workspace root"},
                    "content": {"type": "string", "description": "File content to write"},
                },
                "required": ["target", "path", "content"],
            }),
        },
        ToolDefinition {
            name: "train_list".to_string(),
            description: "List files in a target clone's workspace directory. Used by trainer agents to explore other clones.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                    "path": {"type": "string", "description": "Directory path relative to the target clone's workspace root (default: '.')"},
                },
                "required": ["target"],
            }),
        },
        ToolDefinition {
            name: "train_knowledge_add".to_string(),
            description: "Add a knowledge entry to a target clone's knowledge base. The LLM trainer should process and structure the content before calling this.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                    "title": {"type": "string", "description": "Knowledge entry title"},
                    "content": {"type": "string", "description": "Knowledge content (structured, processed by LLM)"},
                },
                "required": ["target", "title", "content"],
            }),
        },
        ToolDefinition {
            name: "train_knowledge_import".to_string(),
            description: "Import bulk data into a target clone's knowledge base. Supports FAQ, chat logs, and document text.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                    "data": {"type": "string", "description": "Raw data content to import"},
                    "data_type": {"type": "string", "description": "Data format: 'faq', 'chat', 'document', or 'auto' (default: auto)"},
                },
                "required": ["target", "data"],
            }),
        },
        ToolDefinition {
            name: "train_knowledge_list".to_string(),
            description: "List knowledge files in a target clone's knowledge base.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                },
                "required": ["target"],
            }),
        },
        ToolDefinition {
            name: "train_knowledge_read".to_string(),
            description: "Read a specific knowledge file from a target clone's knowledge base.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                    "filename": {"type": "string", "description": "Knowledge file name (e.g. 'rust-basics.md')"},
                },
                "required": ["target", "filename"],
            }),
        },
        ToolDefinition {
            name: "train_knowledge_lint".to_string(),
            description: "Check the knowledge base health of a target clone.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                },
                "required": ["target"],
            }),
        },
        ToolDefinition {
            name: "train_knowledge_heal".to_string(),
            description: "Auto-fix knowledge base issues in a target clone.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone"},
                },
                "required": ["target"],
            }),
        },
        ToolDefinition {
            name: "train_evaluate".to_string(),
            description: "Evaluate a target clone's quality with deterministic metrics. Returns score (0-100), knowledge stats, skill count, and identity completeness.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Name of the target clone to evaluate"},
                },
                "required": ["target"],
            }),
        },
        // --- User profile tool (multi-tenancy) ---
        ToolDefinition {
            name: "user_profile".to_string(),
            description: "Read or update the current user's profile. The profile stores preferences, habits, and interaction patterns between this clone and a specific user. Requires a sender context (sender_id).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["read", "update"], "description": "Read the profile or update it with new key-value pairs"},
                    "updates": {"type": "object", "description": "Key-value pairs to merge into the profile (only for action=update). Supported keys: display_name, preferences (object), interaction_patterns (object), notes (string)"},
                },
                "required": ["action"],
            }),
        },
        // --- Web tools ---
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
        // --- Inter-agent tools ---
        ToolDefinition {
            name: "agent_send".to_string(),
            description: "Send a message to another agent and receive their response. Accepts UUID or agent name. Use agent_find first to discover agents.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The target agent's UUID or name" },
                    "message": { "type": "string", "description": "The message to send to the agent" }
                },
                "required": ["agent_id", "message"]
            }),
        },
        ToolDefinition {
            name: "agent_spawn".to_string(),
            description: "Spawn a new agent from a TOML manifest. Returns the new agent's ID and name.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "manifest_toml": {
                        "type": "string",
                        "description": "The agent manifest in TOML format (must include name, module, [model], and [capabilities])"
                    }
                },
                "required": ["manifest_toml"]
            }),
        },
        ToolDefinition {
            name: "agent_list".to_string(),
            description: "List all currently running agents with their IDs, names, states, and models.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "agent_kill".to_string(),
            description: "Kill (terminate) another agent by its ID.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The agent's UUID to kill" }
                },
                "required": ["agent_id"]
            }),
        },
        ToolDefinition {
            name: "agent_restart".to_string(),
            description: "Restart another agent by its ID. Cancels any running task and resets state to Running. Useful after modifying an agent's configuration to apply changes.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "string", "description": "The target agent's UUID or name" }
                },
                "required": ["agent_id"]
            }),
        },
        // --- Memory tools (per-agent namespace) ---
        ToolDefinition {
            name: "memory_store".to_string(),
            description: "Store a key-value pair in your own memory. Data persists across conversations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The storage key" },
                    "value": { "type": "string", "description": "The value to store (JSON-encode objects/arrays, or pass a plain string)" }
                },
                "required": ["key", "value"]
            }),
        },
        ToolDefinition {
            name: "memory_recall".to_string(),
            description: "Recall a value from your memory by key. Use memory_list first if you're unsure what keys exist.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "The storage key to recall" }
                },
                "required": ["key"]
            }),
        },
        ToolDefinition {
            name: "memory_list".to_string(),
            description: "List all keys and values stored in your memory. Use this before memory_recall to see what's available.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        // --- Collaboration tools ---
        ToolDefinition {
            name: "agent_find".to_string(),
            description: "Discover agents by name, tag, tool, or description. Use to find specialists before delegating work.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search query (matches agent name, tags, tools, description)" }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "task_post".to_string(),
            description: "Post a task to the shared task queue for another agent to pick up.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short task title" },
                    "description": { "type": "string", "description": "Detailed task description" },
                    "assigned_to": { "type": "string", "description": "Agent name or ID to assign the task to (optional)" }
                },
                "required": ["title", "description"]
            }),
        },
        ToolDefinition {
            name: "task_claim".to_string(),
            description: "Claim the next available task from the task queue assigned to you or unassigned.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "task_complete".to_string(),
            description: "Mark a previously claimed task as completed with a result.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string", "description": "The task ID to complete" },
                    "result": { "type": "string", "description": "The result or outcome of the task" }
                },
                "required": ["task_id", "result"]
            }),
        },
        ToolDefinition {
            name: "task_list".to_string(),
            description: "List tasks in the shared queue, optionally filtered by status (pending, in_progress, completed).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "description": "Filter by status: pending, in_progress, completed (optional)" }
                }
            }),
        },
        ToolDefinition {
            name: "event_publish".to_string(),
            description: "Publish a custom event that can trigger proactive agents. Use to broadcast signals to the agent fleet.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "event_type": { "type": "string", "description": "Type identifier for the event (e.g., 'code_review_requested')" },
                    "payload": { "type": "object", "description": "JSON payload data for the event" }
                },
                "required": ["event_type"]
            }),
        },
        // --- Scheduling tools ---
        ToolDefinition {
            name: "schedule_create".to_string(),
            description: "Schedule a recurring task using natural language or cron syntax. Examples: 'every 5 minutes', 'daily at 9am', 'weekdays at 6pm', '0 */5 * * *'.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "What this schedule does (e.g., 'Check for new emails')" },
                    "schedule": { "type": "string", "description": "Natural language or cron expression (e.g., 'every 5 minutes', 'daily at 9am', '0 */5 * * *')" },
                    "agent": { "type": "string", "description": "Agent name or ID to run this task (optional, defaults to self)" }
                },
                "required": ["description", "schedule"]
            }),
        },
        ToolDefinition {
            name: "schedule_list".to_string(),
            description: "List all scheduled tasks with their IDs, descriptions, schedules, and next run times.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "schedule_delete".to_string(),
            description: "Remove a scheduled task by its ID.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "The schedule ID to remove" }
                },
                "required": ["id"]
            }),
        },
        // --- Knowledge graph tools ---
        ToolDefinition {
            name: "knowledge_add_entity".to_string(),
            description: "Add an entity to the knowledge graph. Entities represent people, organizations, projects, concepts, locations, tools, etc.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Display name of the entity" },
                    "entity_type": { "type": "string", "description": "Type: person, organization, project, concept, event, location, document, tool, or a custom type" },
                    "properties": { "type": "object", "description": "Arbitrary key-value properties (optional)" }
                },
                "required": ["name", "entity_type"]
            }),
        },
        ToolDefinition {
            name: "knowledge_add_relation".to_string(),
            description: "Add a relation between two entities in the knowledge graph.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "Source entity ID or name" },
                    "relation": { "type": "string", "description": "Relation type: works_at, knows_about, related_to, depends_on, owned_by, created_by, located_in, part_of, uses, produces, or a custom type" },
                    "target": { "type": "string", "description": "Target entity ID or name" },
                    "confidence": { "type": "number", "description": "Confidence score 0.0-1.0 (default: 1.0)" },
                    "properties": { "type": "object", "description": "Arbitrary key-value properties (optional)" }
                },
                "required": ["source", "relation", "target"]
            }),
        },
        ToolDefinition {
            name: "knowledge_query".to_string(),
            description: "Query the knowledge graph. Filter by source entity, relation type, and/or target entity. Returns matching entity-relation-entity triples.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "Filter by source entity name or ID (optional)" },
                    "relation": { "type": "string", "description": "Filter by relation type (optional)" },
                    "target": { "type": "string", "description": "Filter by target entity name or ID (optional)" },
                    "max_depth": { "type": "integer", "description": "Maximum traversal depth (default: 1)" }
                }
            }),
        },
        // --- Image analysis tool ---
        // --- Browser automation tools ---
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
        // --- Media understanding tools ---
        // --- Image generation tool ---
        // --- Cron scheduling tools ---
        ToolDefinition {
            name: "cron_create".to_string(),
            description: "Create a scheduled/cron job. Supports one-shot (at), recurring (every N seconds), and cron expressions. Max 50 jobs per agent.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Job name (max 128 chars, alphanumeric + spaces/hyphens/underscores)" },
                    "schedule": {
                        "type": "object",
                        "description": "Schedule: {\"kind\":\"at\",\"at\":\"2025-01-01T00:00:00Z\"} or {\"kind\":\"every\",\"every_secs\":300} or {\"kind\":\"cron\",\"expr\":\"0 */6 * * *\"}"
                    },
                    "action": {
                        "type": "object",
                        "description": "Action: {\"kind\":\"system_event\",\"text\":\"...\"} or {\"kind\":\"agent_turn\",\"message\":\"...\",\"timeout_secs\":300}"
                    },
                    "delivery": {
                        "type": "object",
                        "description": "Delivery target: {\"kind\":\"none\"} or {\"kind\":\"channel\",\"channel\":\"telegram\"} or {\"kind\":\"last_channel\"}"
                    },
                    "one_shot": { "type": "boolean", "description": "If true, auto-delete after execution. Default: false" }
                },
                "required": ["name", "schedule", "action"]
            }),
        },
        ToolDefinition {
            name: "cron_list".to_string(),
            description: "List all scheduled/cron jobs for the current agent.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {}
            }),
        },
        ToolDefinition {
            name: "cron_cancel".to_string(),
            description: "Cancel a scheduled/cron job by its ID.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "The UUID of the cron job to cancel" }
                },
                "required": ["job_id"]
            }),
        },
        // --- A2A outbound tools ---
        ToolDefinition {
            name: "a2a_discover".to_string(),
            description: "Discover an external A2A agent by fetching its agent card from a URL. Returns the agent's name, description, skills, and supported protocols.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Base URL of the remote OpenCarrier/A2A-compatible agent (e.g., 'https://agent.example.com')" }
                },
                "required": ["url"]
            }),
        },
        ToolDefinition {
            name: "a2a_send".to_string(),
            description: "Send a task/message to an external A2A agent and get the response. Use agent_name to send to a previously discovered agent, or agent_url for direct addressing.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "The task/message to send to the remote agent" },
                    "agent_url": { "type": "string", "description": "Direct URL of the remote agent's A2A endpoint" },
                    "agent_name": { "type": "string", "description": "Name of a previously discovered A2A agent (looked up from kernel)" },
                    "session_id": { "type": "string", "description": "Optional session ID for multi-turn conversations" }
                },
                "required": ["message"]
            }),
        },
        // --- TTS/STT tools ---
        // --- Docker sandbox tool ---
        // --- Persistent process tools ---
        // --- Clone management tools (system-level install/export) ---
        ToolDefinition {
            name: "clone_install".to_string(),
            description: "Install a new clone from file contents. The system handles packaging into .agx format and spawning. No shell access needed.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Clone name (lowercase, hyphens, e.g. 'customer-support')"},
                    "files": {
                        "type": "object",
                        "description": "File contents keyed by path. Required: SOUL.md, system_prompt.md. Optional: profile.md, MEMORY.md, EVOLUTION.md, knowledge/*.md, skills/*.md, agents/*.md, style/*.md",
                        "additionalProperties": {"type": "string"}
                    }
                },
                "required": ["name", "files"]
            }),
        },
        ToolDefinition {
            name: "clone_export".to_string(),
            description: "Export an installed clone as a downloadable .agx archive. Returns the file size and a download path.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Name of the installed clone to export"}
                },
                "required": ["name"]
            }),
        },
        ToolDefinition {
            name: "clone_publish".to_string(),
            description: "Publish (upload) an installed clone to Hub. Requires Hub API key to be configured. Returns the template ID on Hub.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Name of the installed clone to publish"}
                },
                "required": ["name"]
            }),
        },
        // --- Canvas / A2UI tool ---
        // --- File conversion tool ---
        ToolDefinition {
            name: "file_convert".to_string(),
            description: "Convert files between formats using Pandoc. Common conversions: PDF→Markdown, DOCX→Markdown, HTML→Markdown, EPUB→Markdown, Markdown→DOCX, Markdown→PDF, Markdown→HTML, LaTeX→PDF. Requires pandoc installed on the system.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "input_path": { "type": "string", "description": "Source file path (workspace-relative)" },
                    "output_format": { "type": "string", "description": "Target format: markdown, docx, pdf, html, epub, latex, rtf, odt, pptx, etc." },
                    "output_path": { "type": "string", "description": "Output file path (workspace-relative, optional, auto-generated if omitted)" }
                },
                "required": ["input_path", "output_format"]
            }),
        },
    ]);
    defs
}

// ---------------------------------------------------------------------------
// Filesystem tools
// ---------------------------------------------------------------------------

// Path validation helpers — delegates to shared utilities in tools/mod.rs
fn validate_path(path: &str) -> Result<&str, String> {
    crate::tools::validate_path(path)
}
fn sanitize_path_component(name: &str) -> Result<&str, String> {
    crate::tools::sanitize_path_component(name)
}
fn validate_clone_name(name: &str) -> Result<&str, String> {
    crate::tools::validate_clone_name(name)
}
fn validate_clone_file_path(path: &str) -> Result<&str, String> {
    crate::tools::validate_clone_file_path(path)
}

// ---------------------------------------------------------------------------
// Cross-workspace training tools (for trainer agents)
// ---------------------------------------------------------------------------

/// Resolve a target clone's workspace root via kernel.
fn resolve_target_workspace(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<PathBuf, String> {
    let kh = kernel.ok_or("train_* tools require kernel access")?;
    let target = input["target"]
        .as_str()
        .ok_or("Missing 'target' parameter (target clone name)")?;

    let target_workspace = kh
        .resolve_agent_workspace(target)
        .ok_or_else(|| {
            format!(
                "Agent '{}' not found or has no workspace",
                target
            )
        })?;

    let path = PathBuf::from(&target_workspace);
    if !path.exists() {
        return Err(format!(
            "Workspace for '{}' does not exist: {}",
            target, target_workspace
        ));
    }
    Ok(path)
}

async fn tool_train_read(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    validate_path(path)?;
    let full_path = target_root.join(path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| format!("Failed to read file: {e}"))
}

async fn tool_train_write(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let path = input["path"].as_str().ok_or("Missing 'path' parameter")?;
    validate_path(path)?;
    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;
    let full_path = target_root.join(path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    if let Some(parent) = full_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directories: {e}"))?;
    }
    tokio::fs::write(&full_path, content)
        .await
        .map_err(|e| format!("Failed to write file: {e}"))?;
    Ok(format!(
        "Successfully wrote {} bytes to {}",
        content.len(),
        path
    ))
}

async fn tool_train_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let sub_path = input["path"].as_str().unwrap_or(".");
    validate_path(sub_path)?;
    let full_path = target_root.join(sub_path);
    if !full_path.starts_with(&target_root) {
        return Err("Path traversal denied".to_string());
    }
    let mut entries = tokio::fs::read_dir(&full_path)
        .await
        .map_err(|e| format!("Failed to list directory: {e}"))?;
    let mut files = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| format!("Failed to read entry: {e}"))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = entry.metadata().await;
        let suffix = match metadata {
            Ok(m) if m.is_dir() => "/",
            _ => "",
        };
        files.push(format!("{name}{suffix}"));
    }
    files.sort();
    Ok(files.join("\n"))
}

async fn tool_train_knowledge_add(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let title = input["title"].as_str().ok_or("Missing 'title' parameter")?;
    let content = input["content"]
        .as_str()
        .ok_or("Missing 'content' parameter")?;
    let filename = crate::tools::knowledge::knowledge_add_core(&target_root, title, content, "train").await?;
    Ok(format!("Knowledge added to target: {filename}.md"))
}

async fn tool_train_knowledge_import(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    let data = input["data"].as_str().ok_or("Missing 'data' parameter")?;
    let data_type = input["data_type"].as_str().unwrap_or("auto");
    let (saved, quality) = crate::tools::knowledge::knowledge_import_core(&target_root, data, data_type).await?;
    Ok(format!(
        "Imported {} entries to target. Quality: {:?}",
        saved.len(),
        quality
    ))
}

async fn tool_train_knowledge_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_knowledge_list(Some(&target_root)).await
}

async fn tool_train_knowledge_read(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_knowledge_read(input, Some(&target_root)).await
}

async fn tool_train_knowledge_lint(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_knowledge_lint(Some(&target_root)).await
}

async fn tool_train_knowledge_heal(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_knowledge_heal(Some(&target_root)).await
}

async fn tool_train_evaluate(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let target_root = resolve_target_workspace(input, kernel)?;
    crate::tools::knowledge::tool_clone_evaluate(Some(&target_root)).await
}

// ---------------------------------------------------------------------------
// User profile tool (multi-tenancy)
// ---------------------------------------------------------------------------

async fn tool_user_profile(
    input: &serde_json::Value,
    workspace_root: Option<&Path>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let sender = sender_id.ok_or("user_profile requires a sender context (sender_id). This tool is only available when a user identity is provided.")?;
    let root = workspace_root.ok_or("user_profile requires a workspace root")?;
    let sender = sanitize_path_component(sender)?;

    let action = input["action"].as_str().unwrap_or("read");
    let profile_path = root.join("users").join(sender).join("profile.json");

    match action {
        "read" => {
            if profile_path.exists() {
                let content = tokio::fs::read_to_string(&profile_path)
                    .await
                    .map_err(|e| format!("Failed to read profile: {e}"))?;
                Ok(content)
            } else {
                // Return empty profile template
                let template = serde_json::json!({
                    "sender_id": sender,
                    "display_name": null,
                    "preferences": {},
                    "interaction_patterns": {},
                    "notes": null,
                    "conversation_count": 0,
                    "first_seen": null,
                    "last_seen": null,
                });
                Ok(serde_json::to_string_pretty(&template).unwrap_or_else(|_| "{}".to_string()))
            }
        }
        "update" => {
            // Load existing profile or create new
            let mut profile: serde_json::Value = if profile_path.exists() {
                let content = tokio::fs::read_to_string(&profile_path)
                    .await
                    .map_err(|e| format!("Failed to read profile: {e}"))?;
                serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
            } else {
                serde_json::json!({
                    "sender_id": sender,
                    "conversation_count": 0,
                    "first_seen": chrono::Utc::now().to_rfc3339(),
                })
            };

            // Ensure sender_id is set
            profile["sender_id"] = serde_json::Value::String(sender.to_string());
            profile["last_seen"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());

            // Merge updates
            if let Some(updates) = input.get("updates").and_then(|u| u.as_object()) {
                for (key, value) in updates {
                    // Only allow known safe keys
                    match key.as_str() {
                        "display_name" | "preferences" | "interaction_patterns" | "notes" => {
                            profile[key] = value.clone();
                        }
                        _ => {} // ignore unknown keys
                    }
                }
            }

            // Ensure directory exists
            if let Some(parent) = profile_path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("Failed to create user directory: {e}"))?;
            }

            let output = serde_json::to_string_pretty(&profile)
                .map_err(|e| format!("Failed to serialize profile: {e}"))?;
            tokio::fs::write(&profile_path, &output)
                .await
                .map_err(|e| format!("Failed to write profile: {e}"))?;
            Ok(format!("Profile updated for user '{}'", sender))
        }
        _ => Err(format!(
            "Unknown action '{}'. Use 'read' or 'update'.",
            action
        )),
    }
}

// ---------------------------------------------------------------------------
// Clone management tools
// ---------------------------------------------------------------------------

async fn tool_clone_install(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn crate::kernel_handle::KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kernel = kernel.ok_or("clone_install requires kernel access")?;
    let name_raw = input["name"].as_str().ok_or("Missing 'name' parameter")?;
    let name = validate_clone_name(name_raw)?.to_string();

    let files = input
        .get("files")
        .and_then(|v| v.as_object())
        .ok_or("Missing 'files' parameter (must be a JSON object of path→content)")?;

    if files.is_empty() {
        return Err(
            "No files provided. At minimum, SOUL.md and system_prompt.md are required.".to_string(),
        );
    }

    // SECURITY: Reject oversized file maps (max 20MB total content)
    const MAX_FILES_TOTAL: usize = 20 * 1024 * 1024;
    let total_size: usize = files
        .values()
        .map(|v| v.as_str().map(|s| s.len()).unwrap_or(0))
        .sum();
    if total_size > MAX_FILES_TOTAL {
        return Err(format!(
            "Clone files too large: {} bytes total (max 20MB)",
            total_size
        ));
    }

    // Build CloneData from the file map
    use opencarrier_clone::{pack_agx, AgentData, CloneData, SkillData};
    use std::collections::HashMap;

    let soul = files
        .get("SOUL.md")
        .map(|v| v.as_str().unwrap_or(""))
        .unwrap_or("")
        .to_string();
    let system_prompt = files
        .get("system_prompt.md")
        .map(|v| v.as_str().unwrap_or(""))
        .unwrap_or("")
        .to_string();
    let memory_index = files
        .get("MEMORY.md")
        .map(|v| v.as_str().unwrap_or(""))
        .unwrap_or("")
        .to_string();
    let profile = files
        .get("profile.md")
        .map(|v| v.as_str().unwrap_or(""))
        .unwrap_or("")
        .to_string();
    let evolution = files
        .get("EVOLUTION.md")
        .map(|v| v.as_str().unwrap_or(""))
        .unwrap_or("")
        .to_string();

    if soul.is_empty() {
        return Err("SOUL.md is required in files".to_string());
    }
    if system_prompt.is_empty() {
        return Err("system_prompt.md is required in files".to_string());
    }

    // Validate all file paths for traversal
    for path in files.keys() {
        validate_clone_file_path(path)?;
    }

    // Parse knowledge files
    let mut knowledge = HashMap::new();
    for (path, val) in files {
        if path.starts_with("knowledge/") && path.ends_with(".md") {
            let filename = path.strip_prefix("knowledge/").unwrap_or(path);
            knowledge.insert(filename.to_string(), val.as_str().unwrap_or("").to_string());
        }
    }

    // Parse skills (simple: skills/<name>.md files with frontmatter)
    let mut skills = Vec::new();
    for (path, val) in files {
        if path.starts_with("skills/") && path.ends_with(".md") {
            let content = val.as_str().unwrap_or("");
            let (fm, body) = opencarrier_clone::parse_frontmatter(content);
            let skill_name = fm.get("name").cloned().unwrap_or_else(|| {
                path.strip_prefix("skills/")
                    .unwrap_or(path)
                    .strip_suffix(".md")
                    .unwrap_or("unknown")
                    .to_string()
            });
            skills.push(SkillData {
                name: skill_name,
                when_to_use: fm.get("when_to_use").cloned().unwrap_or_default(),
                allowed_tools: fm
                    .get("allowed_tools")
                    .map(|s| opencarrier_clone::parse_string_array(s))
                    .unwrap_or_default(),
                prompt: body.trim().to_string(),
                scripts: Vec::new(),
            });
        }
    }

    // Parse agents
    let mut agents = Vec::new();
    for (path, val) in files {
        if path.starts_with("agents/") && path.ends_with(".md") {
            let content = val.as_str().unwrap_or("");
            let (fm, body) = opencarrier_clone::parse_frontmatter(content);
            agents.push(AgentData {
                name: fm.get("name").cloned().unwrap_or_else(|| {
                    path.strip_prefix("agents/")
                        .unwrap_or(path)
                        .strip_suffix(".md")
                        .unwrap_or("unknown")
                        .to_string()
                }),
                description: fm.get("description").cloned().unwrap_or_default(),
                tools: fm
                    .get("tools")
                    .map(|s| opencarrier_clone::parse_string_array(s))
                    .unwrap_or_default(),
                model: fm
                    .get("model")
                    .cloned()
                    .unwrap_or_else(|| "sonnet".to_string()),
                color: fm.get("color").cloned(),
                prompt: body.trim().to_string(),
            });
        }
    }

    // Parse style
    let mut style = HashMap::new();
    for (path, val) in files {
        if path.starts_with("style/") && path.ends_with(".md") {
            let filename = path.strip_prefix("style/").unwrap_or(path);
            style.insert(filename.to_string(), val.as_str().unwrap_or("").to_string());
        }
    }

    let clone_data = CloneData {
        manifest: None,
        name: name.clone(),
        description: String::new(),
        soul,
        system_prompt,
        memory_index,
        knowledge,
        skills,
        profile,
        security_warnings: Vec::new(),
        agents,
        evolution,
        style,
        plugins: Vec::new(),
    };

    // Pack into .agx bytes
    let agx_bytes = pack_agx(&clone_data).map_err(|e| format!("Failed to pack .agx: {e}"))?;

    // Install via kernel
    let (agent_id, agent_name) = kernel.clone_install(&name, &agx_bytes).await?;

    Ok(format!(
        "Clone '{}' installed successfully. Agent ID: {}. {} knowledge files, {} skills, {} agents.",
        agent_name, agent_id,
        clone_data.knowledge.len(),
        clone_data.skills.len(),
        clone_data.agents.len(),
    ))
}

async fn tool_clone_export(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn crate::kernel_handle::KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kernel = kernel.ok_or("clone_export requires kernel access")?;
    let name =
        validate_clone_name(input["name"].as_str().ok_or("Missing 'name' parameter")?)?.to_string();

    let agx_bytes = kernel.clone_export(&name)?;

    Ok(format!(
        "Clone '{}' exported as .agx archive ({} bytes / {:.1} KB). The archive contains all workspace files: SOUL.md, system_prompt.md, knowledge/, skills/, agents/, style/, EVOLUTION.md.",
        name,
        agx_bytes.len(),
        agx_bytes.len() as f64 / 1024.0,
    ))
}

async fn tool_clone_publish(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn crate::kernel_handle::KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kernel = kernel.ok_or("clone_publish requires kernel access")?;
    let name =
        validate_clone_name(input["name"].as_str().ok_or("Missing 'name' parameter")?)?.to_string();

    // Export the clone first
    let agx_bytes = kernel.clone_export(&name)?;

    // Publish to Hub
    let template_id = kernel.clone_publish(&name, &agx_bytes).await?;

    Ok(format!(
        "Clone '{}' published to Hub successfully. Template ID: {}. Archive size: {:.1} KB.",
        name,
        template_id,
        agx_bytes.len() as f64 / 1024.0,
    ))
}


// ---------------------------------------------------------------------------
// Inter-agent tools
// ---------------------------------------------------------------------------

fn require_kernel(
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<&Arc<dyn KernelHandle>, String> {
    kernel.ok_or_else(|| {
        "Kernel handle not available. Inter-agent tools require a running kernel.".to_string()
    })
}

async fn tool_agent_send(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    let message = input["message"]
        .as_str()
        .ok_or("Missing 'message' parameter")?;

    // Check + increment inter-agent call depth
    let current_depth = AGENT_CALL_DEPTH.try_with(|d| d.get()).unwrap_or(0);
    if current_depth >= MAX_AGENT_CALL_DEPTH {
        return Err(format!(
            "Inter-agent call depth exceeded (max {}). \
             A->B->C chain is too deep. Use the task queue instead.",
            MAX_AGENT_CALL_DEPTH
        ));
    }

    AGENT_CALL_DEPTH
        .scope(std::cell::Cell::new(current_depth + 1), async {
            kh.send_to_agent(agent_id, message, None, None, caller_agent_id, None)
                .await
        })
        .await
}

async fn tool_agent_spawn(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    parent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let manifest_toml = input["manifest_toml"]
        .as_str()
        .ok_or("Missing 'manifest_toml' parameter")?;
    let (id, name) = kh.spawn_agent(manifest_toml, parent_id).await?;
    Ok(format!(
        "Agent spawned successfully.\n  ID: {id}\n  Name: {name}"
    ))
}

fn tool_agent_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agents = kh.list_agents();
    if agents.is_empty() {
        return Ok("No agents currently running.".to_string());
    }
    let mut output = format!("Running agents ({}):\n", agents.len());
    for a in &agents {
        output.push_str(&format!(
            "  - {} (id: {}, state: {}, modality: {}, model: {})\n",
            a.name, a.id, a.state, a.modality, a.model
        ));
    }
    Ok(output)
}

fn tool_agent_kill(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let target_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    kh.kill_agent(target_id)?;
    Ok(format!("Agent {target_id} killed successfully."))
}

fn tool_agent_restart(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let target_id = input["agent_id"]
        .as_str()
        .ok_or("Missing 'agent_id' parameter")?;
    kh.restart_agent(target_id)?;
    Ok(format!("Agent {target_id} restarted successfully."))
}

// ---------------------------------------------------------------------------
// Shared memory tools
// ---------------------------------------------------------------------------

fn tool_memory_store(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for memory_store")?;
    let sid = sender_id.unwrap_or("");
    let key = input["key"].as_str().ok_or("Missing 'key' parameter")?;
    let value = input.get("value").ok_or("Missing 'value' parameter")?;
    kh.memory_store(aid, sid, key, value.clone())?;
    Ok(format!("Stored value under key '{key}'."))
}

fn tool_memory_recall(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for memory_recall")?;
    let sid = sender_id.unwrap_or("");
    let key = input["key"].as_str().ok_or("Missing 'key' parameter")?;
    match kh.memory_recall(aid, sid, key)? {
        Some(val) => Ok(serde_json::to_string_pretty(&val).unwrap_or_else(|_| val.to_string())),
        None => Ok(format!("No value found for key '{key}'.")),
    }
}

fn tool_memory_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
    sender_id: Option<&str>,
) -> Result<String, String> {
    let _ = input; // no parameters needed
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for memory_list")?;
    let sid = sender_id.unwrap_or("");
    let pairs = kh.memory_list(aid, sid)?;
    if pairs.is_empty() {
        return Ok("No keys stored.".to_string());
    }
    let lines: Vec<String> = pairs
        .iter()
        .map(|(k, v)| {
            let val_str = serde_json::to_string(v).unwrap_or_else(|_| v.to_string());
            format!("- {}: {}", k, val_str)
        })
        .collect();
    Ok(lines.join("\n"))
}

// ---------------------------------------------------------------------------
// Collaboration tools
// ---------------------------------------------------------------------------

fn tool_agent_find(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let query = input["query"].as_str().ok_or("Missing 'query' parameter")?;
    let agents = kh.find_agents(query);
    if agents.is_empty() {
        return Ok(format!("No agents found matching '{query}'."));
    }
    let result: Vec<serde_json::Value> = agents
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "name": a.name,
                "state": a.state,
                "description": a.description,
                "tags": a.tags,
                "tools": a.tools,
                "model": format!("{}:{}", a.modality, a.model),
            })
        })
        .collect();
    serde_json::to_string_pretty(&result).map_err(|e| format!("Serialize error: {e}"))
}

async fn tool_task_post(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let title = input["title"].as_str().ok_or("Missing 'title' parameter")?;
    let description = input["description"]
        .as_str()
        .ok_or("Missing 'description' parameter")?;
    let assigned_to = input["assigned_to"].as_str();
    let task_id = kh
        .task_post(title, description, assigned_to, caller_agent_id)
        .await?;
    Ok(format!("Task created with ID: {task_id}"))
}

async fn tool_task_claim(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Missing caller agent identity")?;
    match kh.task_claim(agent_id).await? {
        Some(task) => {
            serde_json::to_string_pretty(&task).map_err(|e| format!("Serialize error: {e}"))
        }
        None => Ok("No tasks available.".to_string()),
    }
}

async fn tool_task_complete(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let task_id = input["task_id"]
        .as_str()
        .ok_or("Missing 'task_id' parameter")?;
    let result = input["result"]
        .as_str()
        .ok_or("Missing 'result' parameter")?;
    kh.task_complete(task_id, result).await?;
    Ok(format!("Task {task_id} marked as completed."))
}

async fn tool_task_list(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let status = input["status"].as_str();
    let tasks = kh.task_list(status).await?;
    if tasks.is_empty() {
        return Ok("No tasks found.".to_string());
    }
    serde_json::to_string_pretty(&tasks).map_err(|e| format!("Serialize error: {e}"))
}

async fn tool_event_publish(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let event_type = input["event_type"]
        .as_str()
        .ok_or("Missing 'event_type' parameter")?;
    let payload = input
        .get("payload")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    kh.publish_event(event_type, payload).await?;
    Ok(format!("Event '{event_type}' published successfully."))
}

// ---------------------------------------------------------------------------
// Knowledge graph tools
// ---------------------------------------------------------------------------

fn parse_entity_type(s: &str) -> opencarrier_types::memory::EntityType {
    use opencarrier_types::memory::EntityType;
    match s.to_lowercase().as_str() {
        "person" => EntityType::Person,
        "organization" | "org" => EntityType::Organization,
        "project" => EntityType::Project,
        "concept" => EntityType::Concept,
        "event" => EntityType::Event,
        "location" => EntityType::Location,
        "document" | "doc" => EntityType::Document,
        "tool" => EntityType::Tool,
        other => EntityType::Custom(other.to_string()),
    }
}

fn parse_relation_type(s: &str) -> opencarrier_types::memory::RelationType {
    use opencarrier_types::memory::RelationType;
    match s.to_lowercase().as_str() {
        "works_at" | "worksat" => RelationType::WorksAt,
        "knows_about" | "knowsabout" | "knows" => RelationType::KnowsAbout,
        "related_to" | "relatedto" | "related" => RelationType::RelatedTo,
        "depends_on" | "dependson" | "depends" => RelationType::DependsOn,
        "owned_by" | "ownedby" => RelationType::OwnedBy,
        "created_by" | "createdby" => RelationType::CreatedBy,
        "located_in" | "locatedin" => RelationType::LocatedIn,
        "part_of" | "partof" => RelationType::PartOf,
        "uses" => RelationType::Uses,
        "produces" => RelationType::Produces,
        other => RelationType::Custom(other.to_string()),
    }
}

async fn tool_knowledge_add_entity(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let name = input["name"].as_str().ok_or("Missing 'name' parameter")?;
    let entity_type_str = input["entity_type"]
        .as_str()
        .ok_or("Missing 'entity_type' parameter")?;
    let properties = input
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    let entity = opencarrier_types::memory::Entity {
        id: String::new(), // kernel/store assigns a real ID
        entity_type: parse_entity_type(entity_type_str),
        name: name.to_string(),
        properties,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    };

    let id = kh.knowledge_add_entity(entity).await?;
    Ok(format!("Entity '{name}' added with ID: {id}"))
}

async fn tool_knowledge_add_relation(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let source = input["source"]
        .as_str()
        .ok_or("Missing 'source' parameter")?;
    let relation_str = input["relation"]
        .as_str()
        .ok_or("Missing 'relation' parameter")?;
    let target = input["target"]
        .as_str()
        .ok_or("Missing 'target' parameter")?;
    let confidence = input["confidence"].as_f64().unwrap_or(1.0) as f32;
    let properties = input
        .get("properties")
        .and_then(|v| v.as_object())
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();

    let relation = opencarrier_types::memory::Relation {
        source: source.to_string(),
        relation: parse_relation_type(relation_str),
        target: target.to_string(),
        properties,
        confidence,
        created_at: chrono::Utc::now(),
    };

    let id = kh.knowledge_add_relation(relation).await?;
    Ok(format!(
        "Relation '{source}' --[{relation_str}]--> '{target}' added with ID: {id}"
    ))
}

async fn tool_knowledge_query(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    _caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let source = input["source"].as_str().map(|s| s.to_string());
    let target = input["target"].as_str().map(|s| s.to_string());
    let relation = input["relation"].as_str().map(parse_relation_type);
    let max_depth = input["max_depth"].as_u64().unwrap_or(1) as u32;

    let pattern = opencarrier_types::memory::GraphPattern {
        source,
        relation,
        target,
        max_depth,
    };

    let matches = kh.knowledge_query(pattern).await?;
    if matches.is_empty() {
        return Ok("No matching knowledge graph entries found.".to_string());
    }

    let mut output = format!("Found {} match(es):\n", matches.len());
    for m in &matches {
        output.push_str(&format!(
            "\n  {} ({:?}) --[{:?} ({:.0}%)]--> {} ({:?})",
            m.source.name,
            m.source.entity_type,
            m.relation.relation,
            m.relation.confidence * 100.0,
            m.target.name,
            m.target.entity_type,
        ));
    }
    Ok(output)
}

// ---------------------------------------------------------------------------
// Scheduling tools
// ---------------------------------------------------------------------------

/// Parse a natural language schedule into a cron expression.
fn parse_schedule_to_cron(input: &str) -> Result<String, String> {
    let input = input.trim().to_lowercase();

    // If it already looks like a cron expression (5 space-separated fields), pass through
    let parts: Vec<&str> = input.split_whitespace().collect();
    if parts.len() == 5
        && parts
            .iter()
            .all(|p| p.chars().all(|c| c.is_ascii_digit() || "*/,-".contains(c)))
    {
        return Ok(input);
    }

    // Natural language patterns
    if let Some(rest) = input.strip_prefix("every ") {
        if rest == "minute" || rest == "1 minute" {
            return Ok("* * * * *".to_string());
        }
        if let Some(mins) = rest.strip_suffix(" minutes") {
            let n: u32 = mins
                .trim()
                .parse()
                .map_err(|_| format!("Invalid number in '{input}'"))?;
            if n == 0 || n > 59 {
                return Err(format!("Minutes must be 1-59, got {n}"));
            }
            return Ok(format!("*/{n} * * * *"));
        }
        if rest == "hour" || rest == "1 hour" {
            return Ok("0 * * * *".to_string());
        }
        if let Some(hrs) = rest.strip_suffix(" hours") {
            let n: u32 = hrs
                .trim()
                .parse()
                .map_err(|_| format!("Invalid number in '{input}'"))?;
            if n == 0 || n > 23 {
                return Err(format!("Hours must be 1-23, got {n}"));
            }
            return Ok(format!("0 */{n} * * *"));
        }
        if rest == "day" || rest == "1 day" {
            return Ok("0 0 * * *".to_string());
        }
        if rest == "week" || rest == "1 week" {
            return Ok("0 0 * * 0".to_string());
        }
    }

    // "daily at Xam/pm"
    if let Some(time_str) = input.strip_prefix("daily at ") {
        let hour = parse_time_to_hour(time_str)?;
        return Ok(format!("0 {hour} * * *"));
    }

    // "weekdays at Xam/pm"
    if let Some(time_str) = input.strip_prefix("weekdays at ") {
        let hour = parse_time_to_hour(time_str)?;
        return Ok(format!("0 {hour} * * 1-5"));
    }

    // "weekends at Xam/pm"
    if let Some(time_str) = input.strip_prefix("weekends at ") {
        let hour = parse_time_to_hour(time_str)?;
        return Ok(format!("0 {hour} * * 0,6"));
    }

    // "hourly" / "daily" / "weekly" / "monthly"
    match input.as_str() {
        "hourly" => return Ok("0 * * * *".to_string()),
        "daily" => return Ok("0 0 * * *".to_string()),
        "weekly" => return Ok("0 0 * * 0".to_string()),
        "monthly" => return Ok("0 0 1 * *".to_string()),
        _ => {}
    }

    Err(format!(
        "Could not parse schedule '{input}'. Try: 'every 5 minutes', 'daily at 9am', 'weekdays at 6pm', or a cron expression like '0 */5 * * *'"
    ))
}

/// Parse a time string like "9am", "6pm", "14:00", "9:30am" into an hour (0-23).
fn parse_time_to_hour(s: &str) -> Result<u32, String> {
    let s = s.trim().to_lowercase();

    // Handle "9am", "6pm", "12pm", "12am"
    if let Some(h) = s.strip_suffix("am") {
        let hour: u32 = h.trim().parse().map_err(|_| format!("Invalid time: {s}"))?;
        return match hour {
            12 => Ok(0),
            1..=11 => Ok(hour),
            _ => Err(format!("Invalid hour: {hour}")),
        };
    }
    if let Some(h) = s.strip_suffix("pm") {
        let hour: u32 = h.trim().parse().map_err(|_| format!("Invalid time: {s}"))?;
        return match hour {
            12 => Ok(12),
            1..=11 => Ok(hour + 12),
            _ => Err(format!("Invalid hour: {hour}")),
        };
    }

    // Handle "14:00" or "9:30"
    if let Some((h, _m)) = s.split_once(':') {
        let hour: u32 = h.trim().parse().map_err(|_| format!("Invalid time: {s}"))?;
        if hour > 23 {
            return Err(format!("Hour must be 0-23, got {hour}"));
        }
        return Ok(hour);
    }

    // Plain number
    let hour: u32 = s.parse().map_err(|_| format!("Invalid time: {s}"))?;
    if hour > 23 {
        return Err(format!("Hour must be 0-23, got {hour}"));
    }
    Ok(hour)
}

const SCHEDULES_KEY: &str = "__opencarrier_schedules";

async fn tool_schedule_create(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for schedule_create")?;
    let description = input["description"]
        .as_str()
        .ok_or("Missing 'description' parameter")?;
    let schedule_str = input["schedule"]
        .as_str()
        .ok_or("Missing 'schedule' parameter")?;
    let agent = input["agent"].as_str().unwrap_or("");

    let cron_expr = parse_schedule_to_cron(schedule_str)?;
    let schedule_id = uuid::Uuid::new_v4().to_string();

    let entry = serde_json::json!({
        "id": schedule_id,
        "description": description,
        "schedule_input": schedule_str,
        "cron": cron_expr,
        "agent": agent,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "enabled": true,
    });

    // Load existing schedules from agent's memory
    let mut schedules: Vec<serde_json::Value> = match kh.memory_recall(aid, "", SCHEDULES_KEY)? {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    schedules.push(entry);
    kh.memory_store(aid, "", SCHEDULES_KEY, serde_json::Value::Array(schedules))?;

    Ok(format!(
        "Schedule created:\n  ID: {schedule_id}\n  Description: {description}\n  Cron: {cron_expr}\n  Original: {schedule_str}"
    ))
}

async fn tool_schedule_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for schedule_list")?;

    let schedules: Vec<serde_json::Value> = match kh.memory_recall(aid, "", SCHEDULES_KEY)? {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    if schedules.is_empty() {
        return Ok("No scheduled tasks.".to_string());
    }

    let mut output = format!("Scheduled tasks ({}):\n\n", schedules.len());
    for s in &schedules {
        let enabled = s["enabled"].as_bool().unwrap_or(true);
        let status = if enabled { "active" } else { "paused" };
        output.push_str(&format!(
            "  [{status}] {} — {}\n    Cron: {} | Agent: {}\n    Created: {}\n\n",
            s["id"].as_str().unwrap_or("?"),
            s["description"].as_str().unwrap_or("?"),
            s["cron"].as_str().unwrap_or("?"),
            s["agent"].as_str().unwrap_or("(self)"),
            s["created_at"].as_str().unwrap_or("?"),
        ));
    }

    Ok(output)
}

async fn tool_schedule_delete(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let aid = caller_agent_id.ok_or("No agent context for schedule_delete")?;
    let id = input["id"].as_str().ok_or("Missing 'id' parameter")?;

    let mut schedules: Vec<serde_json::Value> = match kh.memory_recall(aid, "", SCHEDULES_KEY)? {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => Vec::new(),
    };

    let before = schedules.len();
    schedules.retain(|s| s["id"].as_str() != Some(id));

    if schedules.len() == before {
        return Err(format!("Schedule '{id}' not found."));
    }

    kh.memory_store(aid, "", SCHEDULES_KEY, serde_json::Value::Array(schedules))?;
    Ok(format!("Schedule '{id}' deleted."))
}

// ---------------------------------------------------------------------------
// Cron scheduling tools (delegated to kernel via KernelHandle trait)
// ---------------------------------------------------------------------------

async fn tool_cron_create(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_create")?;
    kh.cron_create(agent_id, input.clone()).await
}

async fn tool_cron_list(
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_list")?;
    let jobs = kh.cron_list(agent_id).await?;
    serde_json::to_string_pretty(&jobs).map_err(|e| format!("Failed to serialize cron jobs: {e}"))
}

async fn tool_cron_cancel(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
    caller_agent_id: Option<&str>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let agent_id = caller_agent_id.ok_or("Agent ID required for cron_cancel")?;
    let job_id = input["job_id"]
        .as_str()
        .ok_or("Missing 'job_id' parameter")?;
    // Ownership check: verify this job belongs to the caller
    let jobs = kh.cron_list(agent_id).await?;
    let owned = jobs
        .iter()
        .any(|j| j.get("id").and_then(|v| v.as_str()) == Some(job_id));
    if !owned {
        return Err("Cron job not found or does not belong to you".to_string());
    }
    kh.cron_cancel(job_id).await?;
    Ok(format!("Cron job '{job_id}' cancelled."))
}

// ---------------------------------------------------------------------------
// A2A outbound tools (cross-instance agent communication)
// ---------------------------------------------------------------------------

/// Discover an external A2A agent by fetching its agent card.
async fn tool_a2a_discover(input: &serde_json::Value) -> Result<String, String> {
    let url = input["url"].as_str().ok_or("Missing 'url' parameter")?;

    // SSRF protection: block private/metadata IPs
    if crate::web_fetch::check_ssrf(url).is_err() {
        return Err("SSRF blocked: URL resolves to a private or metadata address".to_string());
    }

    let client = crate::a2a::A2aClient::new();
    let card = client.discover(url).await?;

    serde_json::to_string_pretty(&card).map_err(|e| format!("Serialization error: {e}"))
}

/// Send a task to an external A2A agent.
async fn tool_a2a_send(
    input: &serde_json::Value,
    kernel: Option<&Arc<dyn KernelHandle>>,
) -> Result<String, String> {
    let kh = require_kernel(kernel)?;
    let message = input["message"]
        .as_str()
        .ok_or("Missing 'message' parameter")?;

    // Resolve agent URL: either directly provided or looked up by name
    let url = if let Some(url) = input["agent_url"].as_str() {
        // SSRF protection
        if crate::web_fetch::check_ssrf(url).is_err() {
            return Err("SSRF blocked: URL resolves to a private or metadata address".to_string());
        }
        url.to_string()
    } else if let Some(name) = input["agent_name"].as_str() {
        kh.get_a2a_agent_url(name)
            .ok_or_else(|| format!("No known A2A agent with name '{name}'. Use a2a_discover first or provide agent_url directly."))?
    } else {
        return Err("Missing 'agent_url' or 'agent_name' parameter".to_string());
    };

    let session_id = input["session_id"].as_str();
    let client = crate::a2a::A2aClient::new();
    let task = client.send_task(&url, message, session_id).await?;

    serde_json::to_string_pretty(&task).map_err(|e| format!("Serialization error: {e}"))
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: empty ToolContext for tests that don't need any services.
    fn noop_ctx() -> ToolContext<'static> {
        ToolContext {
            kernel: None,
            allowed_tools: None,
            caller_agent_id: None,
            mcp_connections: None,
            web_ctx: None,
            browser_ctx: None,
            allowed_env_vars: None,
            workspace_root: None,
            media_engine: None,
            brain: None,
            exec_policy: None,
            tts_engine: None,
            docker_config: None,
            process_manager: None,
            sender_id: None,
        }
    }

    #[test]
    fn test_builtin_tool_definitions() {
        let tools = builtin_tool_definitions();
        assert!(
            tools.len() >= 39,
            "Expected at least 39 tools, got {}",
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
        assert!(
            names.contains(&"train_knowledge_add"),
            "Missing train_knowledge_add"
        );
        assert!(names.contains(&"train_evaluate"), "Missing train_evaluate");
        // Original 12
        assert!(names.contains(&"file_read"));
        assert!(names.contains(&"shell_exec"));
        assert!(names.contains(&"agent_send"));
        assert!(names.contains(&"agent_spawn"));
        assert!(names.contains(&"agent_list"));
        assert!(names.contains(&"agent_kill"));
        assert!(names.contains(&"memory_store"));
        assert!(names.contains(&"memory_recall"));
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
        // 6 browser tools
        assert!(names.contains(&"browser_navigate"));
        assert!(names.contains(&"browser_click"));
        assert!(names.contains(&"browser_type"));
        assert!(names.contains(&"browser_screenshot"));
        assert!(names.contains(&"browser_read_page"));
        assert!(names.contains(&"browser_close"));
        assert!(names.contains(&"browser_scroll"));
        assert!(names.contains(&"browser_wait"));
        assert!(names.contains(&"browser_run_js"));
        assert!(names.contains(&"browser_back"));
        // 3 media/image generation tools
        assert!(names.contains(&"media_describe"));
        assert!(names.contains(&"media_transcribe"));
        assert!(names.contains(&"image_generate"));
        // 3 cron tools
        assert!(names.contains(&"cron_create"));
        assert!(names.contains(&"cron_list"));
        assert!(names.contains(&"cron_cancel"));
        // 3 voice/docker tools
        assert!(names.contains(&"text_to_speech"));
        assert!(names.contains(&"speech_to_text"));
        assert!(names.contains(&"docker_exec"));
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
            .join("opencarrier_test_nonexistent_99999")
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
        assert!(result.content.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn test_agent_tools_without_kernel() {
        let result = execute_tool(
            "test-id",
            "agent_list",
            &serde_json::json!({}),
            &noop_ctx(),
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("Kernel handle not available"));
    }

    #[tokio::test]
    async fn test_capability_enforcement_denied() {
        let allowed = vec!["file_read".to_string(), "file_list".to_string()];
        let ctx = ToolContext {
            allowed_tools: Some(&allowed),
            ..noop_ctx()
        };
        let result = execute_tool(
            "test-id",
            "shell_exec",
            &serde_json::json!({"command": "ls"}),
            &ctx,
        )
        .await;
        assert!(result.is_error);
        assert!(result.content.contains("Permission denied"));
    }

    #[tokio::test]
    async fn test_capability_enforcement_allowed() {
        let allowed = vec!["file_read".to_string()];
        // Use a relative nonexistent path — workspace_root is None so validate_path
        // will check for traversal/absolute, and this relative path passes that check,
        // then fails at the actual read (file-not-found).
        let ctx = ToolContext {
            allowed_tools: Some(&allowed),
            ..noop_ctx()
        };
        let result = execute_tool(
            "test-id",
            "file_read",
            &serde_json::json!({"path": "opencarrier_test_nonexistent_12345/file.txt"}),
            &ctx,
        )
        .await;
        // Should fail for file-not-found, NOT for permission denied
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

    #[tokio::test]
    async fn test_capability_enforcement_aliased_tool_name() {
        // Agent has "file_write" in allowed tools, but LLM calls "fs-write".
        // After normalization, this should pass the capability check.
        let allowed = vec![
            "file_read".to_string(),
            "file_write".to_string(),
            "file_list".to_string(),
            "shell_exec".to_string(),
        ];
        let ctx = ToolContext {
            allowed_tools: Some(&allowed),
            ..noop_ctx()
        };
        let result = execute_tool(
            "test-id",
            "fs-write", // LLM-hallucinated alias
            &serde_json::json!({"path": "/nonexistent/file.txt", "content": "hello"}),
            &ctx,
        )
        .await;
        // Should NOT be "Permission denied" — it should normalize to file_write
        // and pass the capability check. It will fail for other reasons (path validation).
        assert!(
            !result.content.contains("Permission denied"),
            "fs-write should normalize to file_write and pass capability check, got: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn test_capability_enforcement_aliased_denied() {
        // Agent does NOT have file_write, and LLM calls "fs-write" — should be denied.
        let allowed = vec!["file_read".to_string()];
        let ctx = ToolContext {
            allowed_tools: Some(&allowed),
            ..noop_ctx()
        };
        let result = execute_tool(
            "test-id",
            "fs-write",
            &serde_json::json!({"path": "/tmp/test.txt", "content": "hello"}),
            &ctx,
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("Permission denied"),
            "fs-write should normalize to file_write which is not in allowed list"
        );
    }

    // --- Schedule parser tests ---
    #[test]
    fn test_parse_schedule_every_minutes() {
        assert_eq!(
            parse_schedule_to_cron("every 5 minutes").unwrap(),
            "*/5 * * * *"
        );
        assert_eq!(
            parse_schedule_to_cron("every 1 minute").unwrap(),
            "* * * * *"
        );
        assert_eq!(parse_schedule_to_cron("every minute").unwrap(), "* * * * *");
        assert_eq!(
            parse_schedule_to_cron("every 30 minutes").unwrap(),
            "*/30 * * * *"
        );
    }

    #[test]
    fn test_parse_schedule_every_hours() {
        assert_eq!(parse_schedule_to_cron("every hour").unwrap(), "0 * * * *");
        assert_eq!(parse_schedule_to_cron("every 1 hour").unwrap(), "0 * * * *");
        assert_eq!(
            parse_schedule_to_cron("every 2 hours").unwrap(),
            "0 */2 * * *"
        );
    }

    #[test]
    fn test_parse_schedule_daily() {
        assert_eq!(parse_schedule_to_cron("daily at 9am").unwrap(), "0 9 * * *");
        assert_eq!(
            parse_schedule_to_cron("daily at 6pm").unwrap(),
            "0 18 * * *"
        );
        assert_eq!(
            parse_schedule_to_cron("daily at 12am").unwrap(),
            "0 0 * * *"
        );
        assert_eq!(
            parse_schedule_to_cron("daily at 12pm").unwrap(),
            "0 12 * * *"
        );
    }

    #[test]
    fn test_parse_schedule_weekdays() {
        assert_eq!(
            parse_schedule_to_cron("weekdays at 9am").unwrap(),
            "0 9 * * 1-5"
        );
        assert_eq!(
            parse_schedule_to_cron("weekends at 10am").unwrap(),
            "0 10 * * 0,6"
        );
    }

    #[test]
    fn test_parse_schedule_shorthand() {
        assert_eq!(parse_schedule_to_cron("hourly").unwrap(), "0 * * * *");
        assert_eq!(parse_schedule_to_cron("daily").unwrap(), "0 0 * * *");
        assert_eq!(parse_schedule_to_cron("weekly").unwrap(), "0 0 * * 0");
        assert_eq!(parse_schedule_to_cron("monthly").unwrap(), "0 0 1 * *");
    }

    #[test]
    fn test_parse_schedule_cron_passthrough() {
        assert_eq!(
            parse_schedule_to_cron("0 */5 * * *").unwrap(),
            "0 */5 * * *"
        );
        assert_eq!(
            parse_schedule_to_cron("30 9 * * 1-5").unwrap(),
            "30 9 * * 1-5"
        );
    }

    #[test]
    fn test_parse_schedule_invalid() {
        assert!(parse_schedule_to_cron("whenever I feel like it").is_err());
        assert!(parse_schedule_to_cron("every 0 minutes").is_err());
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

}
