//! Tool and MCP server endpoints.

use crate::routes::common::*;
use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use runtime::tool_context::ToolContext;
use runtime::tool_runner::builtin_tool_definitions;
use std::sync::Arc;
// ---------------------------------------------------------------------------
// MCP server endpoints
// ---------------------------------------------------------------------------

/// GET /api/mcp/servers — List configured MCP servers and their tools.
pub async fn list_mcp_servers(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Get configured servers from config
    let config_servers: Vec<serde_json::Value> = state
        .kernel
        .config
        .mcp_servers
        .iter()
        .map(|s| {
            let transport = match &s.transport {
                types::config::McpTransportEntry::Stdio { command, args } => {
                    serde_json::json!({
                        "type": "stdio",
                        "command": command,
                        "args": args,
                    })
                }
                types::config::McpTransportEntry::Sse { url } => {
                    serde_json::json!({
                        "type": "sse",
                        "url": url,
                    })
                }
            };
            serde_json::json!({
                "name": s.name,
                "transport": transport,
                "timeout_secs": s.timeout_secs,
                "env": s.env,
            })
        })
        .collect();

    // Get connected servers and their tools from the live MCP connections
    let connected: Vec<serde_json::Value> = state
        .kernel
        .plugins
        .mcp_connections
        .iter()
        .map(|entry| {
            let tools: Vec<serde_json::Value> = entry
                .value()
                .tools()
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                    })
                })
                .collect();
            serde_json::json!({
                "name": entry.value().name(),
                "tools_count": tools.len(),
                "tools": tools,
                "connected": true,
            })
        })
        .collect();

    Json(serde_json::json!({
        "configured": config_servers,
        "connected": connected,
        "total_configured": config_servers.len(),
        "total_connected": connected.len(),
    }))
}
// ---------------------------------------------------------------------------
// Tools endpoint
// ---------------------------------------------------------------------------

/// GET /api/tools — List all tool definitions (built-in + MCP).
pub async fn list_tools(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let mut tools: Vec<serde_json::Value> = builtin_tool_definitions(state.kernel.config.cli_exec.clone())
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            })
        })
        .collect();

    // Include MCP tools so they're visible in Settings -> Tools
    if let Ok(mcp_tools) = state.kernel.plugins.mcp_tools.lock() {
        for t in mcp_tools.iter() {
            tools.push(serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
                "source": "mcp",
            }));
        }
    }

    Json(serde_json::json!({"tools": tools, "total": tools.len()}))
}

// ── Capability catalog (for clone-creator / generate / upgrade) ──────────

/// Static deprecations — tools or MCP servers that must not be declared in new clones.
fn deprecated_capabilities() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "browser-mcp",
            "kind": "mcp_server",
            "replace_with": "browser_navigate, browser_read_page, browser_click, ... (builtin)",
            "note": "Browser automation is built-in via AginxBrowser; do not declare browser-mcp"
        }),
        serde_json::json!({
            "name": "mcp_searxng_web_search",
            "kind": "tool",
            "replace_with": "web_search",
            "note": "Use builtin web_search; SearXNG is wired as the search backend when configured"
        }),
        serde_json::json!({
            "name": "skill_load",
            "kind": "tool",
            "replace_with": "flow_load",
            "note": "skills renamed to flows"
        }),
    ]
}

/// Scan shared system flows (`~/.opencarrier/flows`) for name + tools frontmatter.
fn scan_shared_flows() -> Vec<serde_json::Value> {
    let flows_dir = types::config::home_dir().join("flows");
    if !flows_dir.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&flows_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let (name, content) = if path.is_dir() {
            let flow_md = path.join("flow.md");
            let skill_md = path.join("SKILL.md");
            let md = if flow_md.exists() {
                flow_md
            } else if skill_md.exists() {
                skill_md
            } else {
                continue;
            };
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let Ok(c) = std::fs::read_to_string(&md) else {
                continue;
            };
            (name, c)
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let name = path
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            let Ok(c) = std::fs::read_to_string(&path) else {
                continue;
            };
            (name, c)
        } else {
            continue;
        };
        if name.is_empty() || name == "SKILLS" {
            continue;
        }
        let tools = parse_flow_tools_frontmatter(&content);
        let description = parse_flow_description_frontmatter(&content);
        out.push(serde_json::json!({
            "name": name,
            "description": description,
            "tools": tools,
        }));
    }
    out.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });
    out
}

fn parse_flow_tools_frontmatter(content: &str) -> Vec<String> {
    let Some(rest) = content.strip_prefix("---") else {
        return Vec::new();
    };
    let Some(end) = rest.find("\n---") else {
        return Vec::new();
    };
    let fm = &rest[..end];
    let mut tools = Vec::new();
    let mut in_tools = false;
    for line in fm.lines() {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("tools:") {
            let val = val.trim();
            if val.starts_with('[') && val.ends_with(']') {
                let inner = &val[1..val.len() - 1];
                return inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            in_tools = true;
            continue;
        }
        if in_tools {
            if let Some(item) = trimmed.strip_prefix('-') {
                let t = item.trim().trim_matches('"').trim_matches('\'').to_string();
                if !t.is_empty() {
                    tools.push(t);
                }
            } else if !trimmed.is_empty() {
                break;
            }
        }
    }
    tools
}

fn parse_flow_description_frontmatter(content: &str) -> String {
    let Some(rest) = content.strip_prefix("---") else {
        return String::new();
    };
    let Some(end) = rest.find("\n---") else {
        return String::new();
    };
    for line in rest[..end].lines() {
        if let Some(val) = line.trim().strip_prefix("description:") {
            return val.trim().trim_matches('"').to_string();
        }
    }
    String::new()
}

/// GET /api/v1/capability-catalog
///
/// Real-time capability surface for clone-creator (generate / upgrade / evaluate).
/// Combines core tools, builtin tools, live MCP servers, deprecations, and shared flows.
pub async fn capability_catalog(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let format = query
        .get("format")
        .map(|s| s.as_str())
        .unwrap_or("full");
    let compact = format == "compact";

    let core_set: std::collections::HashSet<&str> =
        types::tool::CORE_TOOL_NAMES.iter().copied().collect();
    let core_tools: Vec<&str> = types::tool::CORE_TOOL_NAMES.to_vec();

    let builtin_tools: Vec<serde_json::Value> =
        builtin_tool_definitions(state.kernel.config.cli_exec.clone())
            .into_iter()
            .map(|t| {
                let level = types::tool::PermissionLevel::for_tool(&t.name);
                let level_str = match level {
                    types::tool::PermissionLevel::None => "none",
                    types::tool::PermissionLevel::ReadOnly => "readonly",
                    types::tool::PermissionLevel::Write => "write",
                    types::tool::PermissionLevel::Execute => "execute",
                    types::tool::PermissionLevel::Dangerous => "dangerous",
                };
                let is_core = core_set.contains(t.name.as_str());
                if compact {
                    serde_json::json!({
                        "name": t.name,
                        "source": "builtin",
                        "core": is_core,
                        "permission_level": level_str,
                        "status": "active",
                    })
                } else {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "source": "builtin",
                        "core": is_core,
                        "permission_level": level_str,
                        "status": "active",
                    })
                }
            })
            .collect();

    // Configured MCP names
    let configured: std::collections::HashMap<String, &types::config::McpServerConfigEntry> = state
        .kernel
        .config
        .mcp_servers
        .iter()
        .map(|s| (s.name.clone(), s))
        .collect();

    let mut mcp_servers: Vec<serde_json::Value> = Vec::new();
    let mut seen_mcp: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Connected first (rich tool lists)
    for entry in state.kernel.plugins.mcp_connections.iter() {
        let name = entry.value().name().to_string();
        seen_mcp.insert(name.clone());
        let tools: Vec<serde_json::Value> = entry
            .value()
            .tools()
            .iter()
            .map(|t| {
                if compact {
                    serde_json::json!({ "name": t.name })
                } else {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                    })
                }
            })
            .collect();
        mcp_servers.push(serde_json::json!({
            "name": name,
            "configured": configured.contains_key(&name),
            "connected": true,
            "status": "active",
            "tools_count": tools.len(),
            "tools": tools,
        }));
    }

    // Configured but not connected
    for name in configured.keys() {
        if seen_mcp.contains(name) {
            continue;
        }
        mcp_servers.push(serde_json::json!({
            "name": name,
            "configured": true,
            "connected": false,
            "status": "configured_offline",
            "tools_count": 0,
            "tools": [],
        }));
    }

    mcp_servers.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .cmp(b["name"].as_str().unwrap_or(""))
    });

    let shared_flows = scan_shared_flows();
    let deprecated = deprecated_capabilities();

    // Declarable names (non-core builtins + connected MCP tools) for compact prompt injection
    let mut declarable: Vec<String> = builtin_tools
        .iter()
        .filter(|t| t["core"].as_bool() != Some(true))
        .filter_map(|t| t["name"].as_str().map(|s| s.to_string()))
        .collect();
    for srv in &mcp_servers {
        if srv["connected"].as_bool() != Some(true) {
            continue;
        }
        if let Some(tools) = srv["tools"].as_array() {
            for t in tools {
                if let Some(n) = t["name"].as_str() {
                    declarable.push(n.to_string());
                }
            }
        }
    }
    declarable.sort();
    declarable.dedup();

    Json(serde_json::json!({
        "schema_version": 1,
        "opencarrier_version": env!("CARGO_PKG_VERSION"),
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "core_tools": core_tools,
        "builtin_tools": builtin_tools,
        "mcp_servers": mcp_servers,
        "deprecated": deprecated,
        "shared_flows": shared_flows,
        "declarable_tools": declarable,
        "notes": [
            "core_tools are always loaded — do not list them in flow frontmatter tools:",
            "Prefer builtin tools over mcp_* equivalents when both exist",
            "Only declare mcp_servers that appear with connected=true (or configured if offline install is intentional)",
            "Use flow.md under flows/<name>/ (not skills/SKILL.md)"
        ],
    }))
}

// ── MCP HTTP Endpoint ───────────────────────────────────────────────────

/// POST /mcp — Handle MCP JSON-RPC requests over HTTP.
///
/// Exposes the same MCP protocol normally served via stdio, allowing
/// external MCP clients to connect over HTTP instead.
///
/// SECURITY: This endpoint has no agent context, so inter-agent tools
/// (agent_send, agent_spawn, train_*, etc.) are blocked. Only utility
/// tools (web, file, knowledge) are available.
pub async fn mcp_http(
    State(state): State<Arc<AppState>>,
    Json(request): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Gather all available tools (builtin + MCP)
    let mut tools = builtin_tool_definitions(state.kernel.config.cli_exec.clone());
    if let Ok(mcp_tools) = state.kernel.plugins.mcp_tools.lock() {
        tools.extend(mcp_tools.iter().cloned());
    }

    // Check if this is a tools/call that needs real execution
    let method = request["method"].as_str().unwrap_or("");
    if method == "tools/call" {
        let tool_name = request["params"]["name"].as_str().unwrap_or("");
        let arguments = request["params"]
            .get("arguments")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        // Block inter-agent tools — MCP HTTP has no agent context
        const BLOCKED_TOOLS: &[&str] = &[
            "agent_send",
            "agent_spawn",
            "agent_list",
            "agent_kill",
            "train_read",
            "train_write",
            "train_list",
            "train_knowledge_add",
            "train_knowledge_import",
            "train_knowledge_list",
            "train_knowledge_read",
            "train_knowledge_lint",
            "train_knowledge_heal",
            "train_evaluate",
            "task_post",
            "task_claim",
            "task_complete",
            "task_list",
        ];
        if BLOCKED_TOOLS.contains(&tool_name) {
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned(),
                "error": {"code": -32602, "message": format!("Tool '{tool_name}' is not available via MCP HTTP — it requires agent context")}
            }));
        }

        // Verify the tool exists
        if !tools.iter().any(|t| t.name == tool_name) {
            return Json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": request.get("id").cloned(),
                "error": {"code": -32602, "message": format!("Unknown tool: {tool_name}")}
            }));
        }

        // Execute the tool via the kernel's tool runner
        let kernel_handle: Arc<dyn runtime::kernel_handle::KernelHandle> =
            state.kernel.clone() as Arc<dyn runtime::kernel_handle::KernelHandle>;
        let tool_ctx = ToolContext {
            kernel: Some(&kernel_handle),
            memory: None,
            caller_agent_id: None,
            mcp_connections: Some(&state.kernel.plugins.mcp_connections),
            fetch_engine: Some(&state.kernel.services.fetch_engine),
            allowed_env_vars: None,
            workspace_root: None,
            brain: None,
            exec_policy: Some(&state.kernel.config.exec_policy),
            cli_exec_config: None,
            process_manager: Some(&*state.kernel.coordination.process_manager),
            sender_id: None,
            owner_id: None,
            home_dir: None,
            agent_name: None,
            subagent_configs: None,
            channel_type: None,
            max_tool_level: types::tool::PermissionLevel::Write,
            is_clone_admin: false,
        };
        let result = runtime::tool_runner::execute_tool(
            "mcp-http", tool_name, &arguments, &tool_ctx,
        )
        .await;

        return Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned(),
            "result": {
                "content": [{"type": "text", "text": result.content}],
                "isError": result.is_error,
            }
        }));
    }

    // For non-tools/call methods (initialize, tools/list, etc.), delegate to the handler
    let response = runtime::mcp_server::handle_mcp_request(&request, &tools).await;
    Json(response)
}
/// GET /api/agents/{id}/tools — Get an agent's tool allowlist/blocklist.
pub async fn get_agent_tools(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (_agent_id, entry) = match parse_and_get_agent(&id, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "tool_allowlist": entry.manifest.tool_allowlist,
            "tool_blocklist": entry.manifest.tool_blocklist,
        })),
    )
}
/// PUT /api/agents/{id}/tools — Update an agent's tool allowlist/blocklist.
pub async fn set_agent_tools(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_id = match resolve_agent_id_from_path(&id, &state.kernel.registry) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let allowlist = body
        .get("tool_allowlist")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });
    let blocklist = body
        .get("tool_blocklist")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        });

    if allowlist.is_none() && blocklist.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Provide 'tool_allowlist' and/or 'tool_blocklist'"})),
        );
    }

    match state
        .kernel
        .set_agent_tool_filters(agent_id, allowlist, blocklist)
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        ),
    }
}
// ── Per-Agent MCP Endpoints ────────────────────────────────────────────

/// GET /api/agents/{id}/mcp_servers — Get an agent's MCP server assignment info.
pub async fn get_agent_mcp_servers(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let (_agent_id, entry) = match parse_and_get_agent(&id, &state.kernel.registry) {
        Ok(r) => r,
        Err(resp) => return resp,
    };
    // Collect known MCP server names from connected tools
    let mut available: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in state.kernel.plugins.mcp_connections.iter() {
        let server = entry.key().clone();
        if seen.insert(server.clone()) {
            available.push(server);
        }
    }
    let mode = if entry.manifest.mcp_servers.is_empty() {
        "all"
    } else {
        "allowlist"
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "assigned": entry.manifest.mcp_servers,
            "available": available,
            "mode": mode,
        })),
    )
}
/// PUT /api/agents/{id}/mcp_servers — Update an agent's MCP server allowlist.
pub async fn set_agent_mcp_servers(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_id = match resolve_agent_id_from_path(&id, &state.kernel.registry) {
        Ok(id) => id,
        Err(resp) => return resp,
    };
    let servers: Vec<String> = body["mcp_servers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    match state
        .kernel
        .set_agent_mcp_servers(agent_id, servers.clone())
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "ok", "mcp_servers": servers})),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": format!("{e}")})),
        ),
    }
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route(
            "/api/agents/{id}/mcp_servers",
            routing::put(set_agent_mcp_servers).get(get_agent_mcp_servers),
        )
        .route(
            "/api/agents/{id}/tools",
            routing::put(set_agent_tools).get(get_agent_tools),
        )
        .route("/api/mcp/servers", routing::get(list_mcp_servers))
        .route("/api/tools", routing::get(list_tools))
        .route(
            "/api/v1/capability-catalog",
            routing::get(capability_catalog),
        )
}
