//! Hub template marketplace endpoints.

use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

/// GET /api/hub/templates — List templates from the connected Hub.
pub async fn list_hub_templates(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let hub_url = state.kernel.config.hub.url.clone();
    let hub_api_key = match clone::hub::read_api_key(&state.kernel.config.hub.api_key_env) {
        Ok(k) => k,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("Hub API key not configured: {e}")
                })),
            );
        }
    };

    match clone::hub::search_templates_json(&hub_url, &hub_api_key, None, Some(50)).await {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": format!("Hub request failed: {e}")})),
        ),
    }
}

/// POST /api/hub/templates/{name}/install — Download and install a template from Hub.
pub async fn install_hub_template(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let sender_id = body.get("sender_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let alias = body.get("alias").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
    let hub_url = state.kernel.config.hub.url.clone();
    let hub_api_key = match clone::hub::read_api_key(&state.kernel.config.hub.api_key_env) {
        Ok(k) => k,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("Hub API key not configured: {e}")
                })),
            );
        }
    };

    tracing::info!(template = %name, "Downloading from Hub for install");

    let agx_bytes = match clone::hub::download_template_bytes(&hub_url, &hub_api_key, &name, None).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("Hub download failed: {e}")})),
            );
        }
    };

    match state.kernel.clone_install(&name, &agx_bytes).await {
        Ok((agent_id, agent_name, display_name)) => {
            // Bind to sender if sender_id provided
            if let Some(ref sid) = sender_id {
                if let Some(ref pm_arc) = state.channel_manager {
                    let pm = pm_arc.lock().await;
                    pm.set_sender_route(sid, &agent_id);
                    let effective_alias = alias.as_deref().or_else(|| {
                        if !display_name.is_empty() { Some(&display_name) } else { None }
                    });
                    if let Some(alias_name) = effective_alias {
                        pm.set_sender_alias(sid, alias_name, &agent_id);
                    }
                    tracing::info!(sender = %sid, agent = %agent_id, "Bound installed clone to sender");
                }
            }
            let serial = allocate_serial_number(&state.kernel.config.data_dir);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "agent_id": agent_id,
                    "name": agent_name,
                    "size": agx_bytes.len(),
                    "serial_number": serial,
                })),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// Allocate a globally unique serial number (OC-XXXXXX) using a persistent counter file.
fn allocate_serial_number(data_dir: &std::path::Path) -> String {
    let counter_path = data_dir.join("install_counter");
    let current = std::fs::read_to_string(&counter_path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    let next = current + 1;
    let _ = std::fs::write(&counter_path, next.to_string());
    format!("OC-{:06}", next)
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/hub/templates", routing::get(list_hub_templates))
        .route(
            "/api/hub/templates/{name}/install",
            routing::post(install_hub_template),
        )
}
