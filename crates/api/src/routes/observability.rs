//! Health, metrics, audit, logs, and usage endpoints.

use crate::routes::state::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::collections::HashMap;
use std::sync::Arc;
/// GET /api/health — Minimal liveness probe (public, no auth required).
/// Returns only status and version to prevent information leakage.
/// Use GET /api/health/detail for full diagnostics (requires auth).
pub async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Run the database check on a blocking thread so we never hold the
    // std::sync::Mutex<Connection> on a tokio worker thread.  This prevents
    // the health probe from starving the async runtime when the agent loop
    // is holding the database lock for session saves.
    let memory = state.kernel.memory.clone();
    let db_ok = tokio::task::spawn_blocking(move || {
        let shared_id = types::agent::AgentId(uuid::Uuid::from_bytes([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]));
        memory
            .system_kv_get(&shared_id.to_string(), "", "", "__health_check__")
            .is_ok()
    })
    .await
    .unwrap_or(false);

    let status = if db_ok { "ok" } else { "degraded" };

    Json(serde_json::json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
/// GET /api/health/detail — Full health diagnostics (requires auth).
pub async fn health_detail(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let health = state.kernel.runtime.supervisor.health();

    let memory = state.kernel.memory.clone();
    let db_ok = tokio::task::spawn_blocking(move || {
        let shared_id = types::agent::AgentId(uuid::Uuid::from_bytes([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        ]));
        memory
            .system_kv_get(&shared_id.to_string(), "", "", "__health_check__")
            .is_ok()
    })
    .await
    .unwrap_or(false);

    let config_warnings = state.kernel.config.validate();
    let status = if db_ok { "ok" } else { "degraded" };

    Json(serde_json::json!({
        "status": status,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": state.started_at.elapsed().as_secs(),
        "panic_count": health.panic_count,
        "restart_count": health.restart_count,
        "agent_count": state.kernel.registry.count(),
        "database": if db_ok { "connected" } else { "error" },
        "config_warnings": config_warnings,
    }))
}
// ---------------------------------------------------------------------------
// Prometheus metrics endpoint
// ---------------------------------------------------------------------------

fn escape_prometheus_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// GET /api/metrics — Prometheus text-format metrics.
///
/// Returns counters and gauges for monitoring Carrier in production:
/// - `carrier_agents_active` — number of active agents
/// - `carrier_uptime_seconds` — seconds since daemon started
/// - `carrier_tokens_total` — total tokens consumed (per agent)
/// - `carrier_tool_calls_total` — total tool calls (per agent)
/// - `carrier_panics_total` — supervisor panic count
/// - `carrier_restarts_total` — supervisor restart count
pub async fn prometheus_metrics(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let mut out = String::with_capacity(2048);

    // Uptime
    let uptime = state.started_at.elapsed().as_secs();
    out.push_str("# HELP carrier_uptime_seconds Time since daemon started.\n");
    out.push_str("# TYPE carrier_uptime_seconds gauge\n");
    out.push_str(&format!("carrier_uptime_seconds {uptime}\n\n"));

    // Active agents
    let agents = state.kernel.registry.list();
    let active = agents
        .iter()
        .filter(|a| matches!(a.state, types::agent::AgentState::Running))
        .count();
    out.push_str("# HELP carrier_agents_active Number of active agents.\n");
    out.push_str("# TYPE carrier_agents_active gauge\n");
    out.push_str(&format!("carrier_agents_active {active}\n"));
    out.push_str("# HELP carrier_agents_total Total number of registered agents.\n");
    out.push_str("# TYPE carrier_agents_total gauge\n");
    out.push_str(&format!("carrier_agents_total {}\n\n", agents.len()));

    // Per-agent token and tool usage
    out.push_str("# HELP carrier_tokens_total Total tokens consumed (rolling hourly window).\n");
    out.push_str("# TYPE carrier_tokens_total gauge\n");
    out.push_str("# HELP carrier_tool_calls_total Total tool calls (rolling hourly window).\n");
    out.push_str("# TYPE carrier_tool_calls_total gauge\n");
    for agent in &agents {
        let name = escape_prometheus_label(&agent.name);
        let modality = &agent.manifest.model.modality;
        let model = escape_prometheus_label(&agent.manifest.name);
        if let Some((tokens, tools)) = state.kernel.runtime.scheduler.get_usage(agent.id) {
            out.push_str(&format!(
                "carrier_tokens_total{{agent=\"{name}\",modality=\"{modality}\",model=\"{model}\"}} {tokens}\n"
            ));
            out.push_str(&format!(
                "carrier_tool_calls_total{{agent=\"{name}\"}} {tools}\n"
            ));
        }
    }
    out.push('\n');

    // Supervisor health
    let health = state.kernel.runtime.supervisor.health();
    out.push_str("# HELP carrier_panics_total Total supervisor panics since start.\n");
    out.push_str("# TYPE carrier_panics_total counter\n");
    out.push_str(&format!("carrier_panics_total {}\n", health.panic_count));
    out.push_str("# HELP carrier_restarts_total Total supervisor restarts since start.\n");
    out.push_str("# TYPE carrier_restarts_total counter\n");
    out.push_str(&format!(
        "carrier_restarts_total {}\n\n",
        health.restart_count
    ));

    // Version info
    out.push_str("# HELP carrier_info Carrier version and build info.\n");
    out.push_str("# TYPE carrier_info gauge\n");
    out.push_str(&format!(
        "carrier_info{{version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION")
    ));

    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        out,
    )
        .into_response()
}
// ---------------------------------------------------------------------------
// Audit endpoints
// ---------------------------------------------------------------------------

/// GET /api/audit/recent — Get recent audit log entries.
pub async fn audit_recent(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
) -> axum::response::Response {
    let n: usize = params
        .get("n")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .min(1000); // Cap at 1000

    let entries = state.kernel.audit_log.recent(n);
    let tip = state.kernel.audit_log.tip_hash();

    let items: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "seq": e.seq,
                "timestamp": e.timestamp,
                "agent_id": e.agent_id,
                "action": format!("{:?}", e.action),
                "detail": e.detail,
                "outcome": e.outcome,
                "hash": e.hash,
            })
        })
        .collect();

    Json(serde_json::json!({
        "entries": items,
        "total": state.kernel.audit_log.len(),
        "tip_hash": tip,
    }))
    .into_response()
}
/// GET /api/audit/verify — Verify the audit chain integrity.
pub async fn audit_verify(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let entry_count = state.kernel.audit_log.len();
    match state.kernel.audit_log.verify_integrity() {
        Ok(()) => {
            if entry_count == 0 {
                // SECURITY: Warn that an empty audit log has no forensic value
                Json(serde_json::json!({
                    "valid": true,
                    "entries": 0,
                    "warning": "Audit log is empty — no events have been recorded yet",
                    "tip_hash": state.kernel.audit_log.tip_hash(),
                }))
                .into_response()
            } else {
                Json(serde_json::json!({
                    "valid": true,
                    "entries": entry_count,
                    "tip_hash": state.kernel.audit_log.tip_hash(),
                }))
                .into_response()
            }
        }
        Err(msg) => Json(serde_json::json!({
            "valid": false,
            "error": msg.to_string(),
            "entries": entry_count,
        }))
        .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Usage endpoint
// ---------------------------------------------------------------------------

/// GET /api/usage — Get per-agent usage statistics.
pub async fn usage_stats(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let all_agents = state.kernel.registry.list();
    let agents: Vec<serde_json::Value> = all_agents
        .iter()
        .map(|e| {
            let (tokens, tool_calls) = state
                .kernel
                .runtime
                .scheduler
                .get_usage(e.id)
                .unwrap_or((0, 0));
            serde_json::json!({
                "agent_id": e.id.to_string(),
                "name": e.name,
                "total_tokens": tokens,
                "tool_calls": tool_calls,
            })
        })
        .collect();

    Json(serde_json::json!({"agents": agents}))
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/audit/recent", routing::get(audit_recent))
        .route("/api/audit/verify", routing::get(audit_verify))
        .route("/api/health", routing::get(health))
        .route("/api/health/detail", routing::get(health_detail))
        .route("/api/metrics", routing::get(prometheus_metrics))
        .route("/api/usage", routing::get(usage_stats))
}
