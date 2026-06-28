//! Clone admin management endpoints.

use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use runtime::plugin::admin_store;
use std::sync::Arc;

fn find_workspace(state: &Arc<AppState>, name: &str) -> Option<std::path::PathBuf> {
    state
        .kernel
        .registry
        .find_by_name(name)
        .and_then(|e| e.manifest.workspace.clone())
}

/// GET /api/clones/{name}/admins — List admins and pending requests.
pub async fn list_clone_admins(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let ws = match find_workspace(&state, &name) {
        Some(ws) => ws,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Agent not found"}))),
    };

    let admins = admin_store::read_admins(&ws);
    (StatusCode::OK, Json(serde_json::json!(admins)))
}

/// POST /api/clones/{name}/admins/approve — Approve a pending admin request.
pub async fn approve_clone_admin(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let ws = match find_workspace(&state, &name) {
        Some(ws) => ws,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Agent not found"}))),
    };

    let sender_id = match body["sender_id"].as_str() {
        Some(id) => id.to_string(),
        None => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Missing sender_id"}))),
    };

    match admin_store::approve(&ws, &sender_id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) if e.contains("not_found") => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Not found in pending list"}))),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "Failed to approve"}))),
    }
}

/// DELETE /api/clones/{name}/admins/{sender_id} — Revoke admin status.
pub async fn revoke_clone_admin(
    State(state): State<Arc<AppState>>,
    Path((name, sender_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let ws = match find_workspace(&state, &name) {
        Some(ws) => ws,
        None => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "Agent not found"}))),
    };

    match admin_store::revoke(&ws, &sender_id) {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))),
        Err(e) if e.contains("cannot_revoke_creator") => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Cannot revoke creator"}))),
        Err(e) if e.contains("not_found") => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "Admin not found"}))),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "Failed to revoke"}))),
    }
}

pub fn router() -> axum::Router<Arc<AppState>> {
    axum::Router::new()
        .route("/api/clones/{name}/admins", axum::routing::get(list_clone_admins))
        .route("/api/clones/{name}/admins/approve", axum::routing::post(approve_clone_admin))
        .route("/api/clones/{name}/admins/{sender_id}", axum::routing::delete(revoke_clone_admin))
}
