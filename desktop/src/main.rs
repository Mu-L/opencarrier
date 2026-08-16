//! OpenCarrier desktop client.
//!
//! Pure remote client (no local daemon): every call goes to the configured
//! opencarrier server over its public HTTP API (docs/AGENT-APP-API.md).
//! All HTTP/SSE traffic is proxied through Rust `invoke` commands — the
//! webview never talks to the server directly (server CORS allowlist does
//! not include the tauri origin, and this keeps the future user-token
//! injection in one place).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use tauri::{AppHandle, Emitter};

/// Shared HTTP client: no total timeout — chat turns run for minutes; a
/// connect timeout is all we want (SSE responses stream deltas for the
/// whole turn).
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .expect("reqwest client")
}

/// Normalize a user-entered server address into `{scheme}://host[:port]`
/// (no trailing slash). Bare host defaults to https.
fn normalize_server(server: &str) -> String {
    let s = server.trim().trim_end_matches('/');
    if s.starts_with("http://") || s.starts_with("https://") {
        s.to_string()
    } else {
        format!("https://{s}")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiArgs {
    server: String,
    api_key: String,
    method: String,
    /// Path with leading slash, e.g. "/api/agents"
    path: String,
    #[serde(default)]
    body: Option<Value>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ApiResponse {
    status: u16,
    body: Value,
}

/// Generic passthrough for REST calls. The frontend supplies config; the
/// Rust side attaches auth so the key never appears in webview fetches.
#[tauri::command]
async fn api_request(args: ApiArgs) -> Result<ApiResponse, String> {
    let url = format!("{}{}", normalize_server(&args.server), args.path);
    let method = reqwest::Method::from_bytes(args.method.to_uppercase().as_bytes())
        .map_err(|e| format!("bad method: {e}"))?;
    let mut req = http_client()
        .request(method, &url)
        .header("Authorization", format!("Bearer {}", args.api_key));
    if let Some(body) = &args.body {
        req = req.json(body);
    }
    let resp = req.send().await.map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    Ok(ApiResponse { status, body })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatArgs {
    server: String,
    api_key: String,
    /// Agent name or UUID (server accepts both)
    agent: String,
    message: String,
    /// Session identity — the server routes to the `user:<sender_id>`
    /// session, so the desktop conversation stays isolated from channel users.
    sender_id: String,
    sender_name: Option<String>,
    active_flow: Option<String>,
}

/// Send a chat message and stream the agent turn as Tauri events.
///
/// Uses the SSE endpoint (`POST /api/agents/{id}/message/stream`) — plain
/// HTTPS POST, no websocket upgrade needed through proxies. Events emitted
/// (all carry `agent`):
///   chat://delta  {text}                  — incremental assistant text
///   chat://tool   {tool, input?, phase}   — tool activity
///   chat://phase  {phase, detail}         — turn phase changes
///   chat://done   {usage}                 — turn finished
///   chat://error  {message}               — turn failed
#[tauri::command]
async fn chat_stream(app: AppHandle, args: ChatArgs) -> Result<(), String> {
    let url = format!(
        "{}/api/agents/{}/message/stream",
        normalize_server(&args.server),
        args.agent
    );
    let payload = serde_json::json!({
        "message": args.message,
        "sender_id": args.sender_id,
        "sender_name": args.sender_name,
        "active_flow": args.active_flow,
    });
    let resp = http_client()
        .post(&url)
        .header("Authorization", format!("Bearer {}", args.api_key))
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        let _ = app.emit(
            "chat://error",
            serde_json::json!({"agent": args.agent, "message": format!("HTTP {status}: {text}")}),
        );
        return Ok(());
    }

    let agent = args.agent.clone();
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut acc = String::new();

    // Minimal SSE frame parser: split on blank lines, each frame is
    // "event: <name>\ndata: <json>" (server sends keep-alive comments too).
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("stream broke: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buf.find("\n\n") {
            let frame: String = buf.drain(..pos + 2).collect();
            let mut event = String::new();
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data = v.trim().to_string();
                }
            }
            if data.is_empty() {
                continue;
            }
            let json: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            match event.as_str() {
                "chunk" => {
                    if let Some(text) = json["content"].as_str() {
                        acc.push_str(text);
                        let _ = app.emit(
                            "chat://delta",
                            serde_json::json!({"agent": agent, "text": text}),
                        );
                    }
                }
                "tool_use" => {
                    let _ = app.emit(
                        "chat://tool",
                        serde_json::json!({"agent": agent, "tool": json["tool"], "phase": "start"}),
                    );
                }
                "tool_result" => {
                    let _ = app.emit(
                        "chat://tool",
                        serde_json::json!({
                            "agent": agent,
                            "tool": json["tool"],
                            "input": json["input"],
                            "phase": "end",
                        }),
                    );
                }
                "phase" => {
                    let _ = app.emit(
                        "chat://phase",
                        serde_json::json!({
                            "agent": agent,
                            "phase": json["phase"],
                            "detail": json["detail"],
                        }),
                    );
                }
                "done" => {
                    let _ = app.emit(
                        "chat://done",
                        serde_json::json!({"agent": agent, "usage": json["usage"], "text": acc}),
                    );
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    // Stream ended without a `done` frame (e.g. connection cut mid-turn).
    let _ = app.emit(
        "chat://error",
        serde_json::json!({
            "agent": agent,
            "message": "连接在回合结束前中断",
            "partial": acc,
        }),
    );
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![api_request, chat_stream])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
