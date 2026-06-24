//! Brain API key management endpoints.
//!
//! Single-layer model: there is one brain backend (aginxbrain) with one
//! API key env var. These endpoints manage that single key.

use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

/// The single brain provider name.
const BRAIN_PROVIDER: &str = "aginxbrain";

/// GET /api/providers/keys — List the brain API key status.
pub async fn list_provider_keys(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();

    let has_key = kernel::dotenv::has_env_key(&config.api_key_env);

    Json(serde_json::json!({
        "providers": [{
            "name": BRAIN_PROVIDER,
            "api_key_env": config.api_key_env,
            "has_key": has_key,
            "base_url": config.base_url,
        }]
    }))
}

/// POST /api/providers/{name}/key — Set the brain API key.
///
/// `{ "key": "sk-xxx" }`. `name` is accepted for path compatibility but
/// always refers to the single brain backend.
pub async fn set_provider_key(
    State(state): State<Arc<AppState>>,
    Path(_name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();

    if config.api_key_env.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "brain.json has no api_key_env configured"})),
        );
    }

    // Simple API key auth
    let key = body["key"].as_str().unwrap_or("").trim();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Missing 'key' field"})),
        );
    }
    if let Err(e) = kernel::dotenv::save_env_key(&config.api_key_env, key) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        );
    }

    let reload_result = state.kernel.reload_brain();
    state.kernel.audit_log.record(
        "system",
        runtime::audit::AuditAction::ConfigChange,
        "API key set for brain".to_string(),
        if reload_result.is_ok() {
            "ok"
        } else {
            "reload_failed"
        },
    );
    match reload_result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))),
        Err(e) => (
            StatusCode::OK,
            Json(
                serde_json::json!({"status": "ok", "warning": format!("Key saved but brain reload failed: {}", e)}),
            ),
        ),
    }
}

/// DELETE /api/providers/{name}/key — Remove the brain API key.
pub async fn delete_provider_key(
    State(state): State<Arc<AppState>>,
    Path(_name): Path<String>,
) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();

    let _ = kernel::dotenv::delete_env_key(&config.api_key_env);

    let _ = state.kernel.reload_brain();
    state.kernel.audit_log.record(
        "system",
        runtime::audit::AuditAction::ConfigChange,
        "API key removed for brain",
        "ok",
    );
    (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/providers/keys", routing::get(list_provider_keys))
        .route(
            "/api/providers/{name}/key",
            routing::delete(delete_provider_key).post(set_provider_key),
        )
}
