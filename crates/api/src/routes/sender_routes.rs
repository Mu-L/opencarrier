//! Sender route management API.

use kernel::KernelHandle;
use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct SetRouteBody {
    agent_id: String,
}

/// GET /api/sender-routes
pub async fn list_sender_routes(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let routes = if let Some(ref pm_arc) = state.channel_manager {
        let pm = pm_arc.lock().await;
        pm.list_sender_routes()
    } else {
        Vec::new()
    };
    let route_list: Vec<serde_json::Value> = routes
        .into_iter()
        .map(|(sender_id, agent_id)| {
            serde_json::json!({
                "sender_id": sender_id,
                "agent_id": agent_id,
            })
        })
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "routes": route_list })))
}

/// GET /api/sender-routes/{sender_id}
pub async fn get_sender_route(
    State(state): State<Arc<AppState>>,
    Path(sender_id): Path<String>,
) -> impl IntoResponse {
    if let Some(ref pm_arc) = state.channel_manager {
        let pm = pm_arc.lock().await;
        match pm.get_sender_route(&sender_id) {
            Some(agent_id) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "sender_id": sender_id,
                    "agent_id": agent_id,
                })),
            ),
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "No route found for this sender"})),
            ),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Plugin manager not available"})),
        )
    }
}

/// PUT /api/sender-routes/{sender_id}
pub async fn set_sender_route(
    State(state): State<Arc<AppState>>,
    Path(sender_id): Path<String>,
    Json(body): Json<SetRouteBody>,
) -> impl IntoResponse {
    // Resolve agent_id to agent_name (accept UUID or agent name)
    let agent_name = if uuid::Uuid::parse_str(&body.agent_id).is_ok() {
        // Input is UUID — resolve to agent name
        let agents = state.kernel.list_agents();
        match agents.iter().find(|a| a.id == body.agent_id) {
            Some(agent) => agent.name.clone(),
            None => body.agent_id.clone(), // Fallback: store as-is
        }
    } else {
        // Input is already a name — validate it exists
        let agents = state.kernel.list_agents();
        if !agents.iter().any(|a| a.name == body.agent_id) {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("Agent '{}' not found", body.agent_id)
                })),
            );
        }
        body.agent_id.clone()
    };

    if let Some(ref pm_arc) = state.channel_manager {
        let pm = pm_arc.lock().await;
        pm.set_sender_route(&sender_id, &agent_name);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "sender_id": sender_id,
                "agent_name": agent_name,
                "status": "set",
            })),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "Plugin manager not available"})),
        )
    }
}

/// DELETE /api/sender-routes/{sender_id}
pub async fn delete_sender_route(
    State(state): State<Arc<AppState>>,
    Path(sender_id): Path<String>,
) -> impl IntoResponse {
    if let Some(ref pm_arc) = state.channel_manager {
        let pm = pm_arc.lock().await;
        match pm.remove_sender_route(&sender_id) {
            Some(agent_id) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "sender_id": sender_id,
                    "agent_id": agent_id,
                    "status": "removed",
                })),
            ),
            None => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "No route found for this sender"})),
            ),
        }
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Plugin manager not available"})),
        )
    }
}

pub fn router() -> axum::Router<std::sync::Arc<AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/sender-routes", routing::get(list_sender_routes))
        .route(
            "/api/sender-routes/{sender_id}",
            routing::get(get_sender_route)
                .put(set_sender_route)
                .delete(delete_sender_route),
        )
}
