//! Provider API key management endpoints.

use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;
// ── Provider API Key management ────────────────────────────────────────────

/// GET /api/providers/keys — List all providers with API key status.
pub async fn list_provider_keys(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();

    let providers: Vec<serde_json::Value> = config
        .providers
        .iter()
        .map(|(name, pc)| {
            let endpoints: Vec<String> = config
                .endpoints
                .values()
                .filter(|ep| ep.provider == *name)
                .map(|ep| ep.model.clone())
                .collect();

            let has_key = kernel::dotenv::has_env_key(&pc.api_key_env);

            serde_json::json!({
                "name": name,
                "api_key_env": pc.api_key_env,
                "has_key": has_key,
                "endpoints": endpoints,
            })
        })
        .collect();

    Json(serde_json::json!({ "providers": providers }))
}
/// POST /api/providers/{name}/key — Set API key for a provider.
///
/// For `apikey` auth type: `{ "key": "sk-xxx" }`
/// For `jwt` auth type: `{ "params": { "access_key_env": "val", "secret_key_env": "val" } }`
pub async fn set_provider_key(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();

    let pc = match config.providers.get(&name) {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Provider '{}' not found", name)})),
            );
        }
    };

    // Simple API key auth
    let key = body["key"].as_str().unwrap_or("").trim();
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Missing 'key' field"})),
        );
    }
    if let Err(e) = kernel::dotenv::save_env_key(&pc.api_key_env, key) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        );
    }

    let reload_result = state.kernel.reload_brain();
    state.kernel.audit_log.record(
        "system",
        runtime::audit::AuditAction::ConfigChange,
        format!("API key set for provider '{}'", name),
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
/// DELETE /api/providers/{name}/key — Remove API key for a provider.
pub async fn delete_provider_key(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();

    let pc = match config.providers.get(&name) {
        Some(p) => p.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": format!("Provider '{}' not found", name)})),
            );
        }
    };

    let _ = kernel::dotenv::delete_env_key(&pc.api_key_env);

    let _ = state.kernel.reload_brain();
    state.kernel.audit_log.record(
        "system",
        runtime::audit::AuditAction::ConfigChange,
        format!("API key removed for provider '{}'", name),
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
