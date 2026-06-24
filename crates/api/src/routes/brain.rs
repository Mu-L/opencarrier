//! Brain configuration and management endpoints.
//!
//! Single-layer model: one shared aginxbrain backend (base_url + api_key_env),
//! routed by modality name. No provider/endpoint CRUD — those concepts are gone.

use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

/// GET /api/brain — Brain configuration and status.
///
/// Returns base_url, api_key_env, default_modality, and supported modalities.
pub async fn brain_info(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();
    let driver_ready = brain.status().drivers_ready > 0;

    let modalities: serde_json::Map<String, serde_json::Value> = config
        .modalities
        .iter()
        .map(|(name, me)| {
            (
                name.clone(),
                serde_json::json!({ "description": me.description }),
            )
        })
        .collect();

    Json(serde_json::json!({
        "loaded": true,
        "base_url": config.base_url,
        "api_key_env": config.api_key_env,
        "default_modality": config.default_modality,
        "modalities": modalities,
        "driver_ready": driver_ready,
    }))
}

/// GET /api/brain/status — Brain health status (driver readiness, latency, success/failure).
pub async fn brain_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let status = brain.status();
    Json(serde_json::to_value(&status).unwrap_or_default())
}

/// GET /api/brain/modalities/{name} — Resolved endpoint for a single modality.
pub async fn brain_modality_detail(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let brain = state.kernel.brain_info();
    let config = brain.config();
    let me = config.modalities.get(&name);
    if me.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Modality '{}' not found", name)})),
        );
    }
    let endpoints = brain.endpoints_for(&name);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "modality": name,
            "description": me.map(|m| m.description.clone()).unwrap_or_default(),
            "endpoints": endpoints,
        })),
    )
}

// ── Brain config management ────────────────────────────────────────────────

/// PUT /api/brain/modalities/{name} — create or update a Brain modality.
///
/// Body: { "description": "..." }. The modality name is the routing tag.
pub async fn set_brain_modality(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let description = body["description"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    let result = state.kernel.update_brain(|config| {
        config.modalities.insert(
            name.clone(),
            types::brain::ModalityEntry { description },
        );
    });

    match result {
        Ok(()) => {
            state.kernel.audit_log.record(
                "system",
                runtime::audit::AuditAction::ConfigChange,
                format!("brain modality '{name}' updated"),
                "ok",
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ok", "modality": name})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// DELETE /api/brain/modalities/{name} — remove a Brain modality.
pub async fn delete_brain_modality(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Cannot delete default modality
    let guard = state.kernel.brain_read();
    if guard.config().default_modality == name {
        drop(guard);
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": format!("Cannot delete default modality '{name}'")})),
        );
    }
    drop(guard);

    let result = state.kernel.update_brain(|config| {
        config.modalities.remove(&name);
    });

    match result {
        Ok(()) => {
            state.kernel.audit_log.record(
                "system",
                runtime::audit::AuditAction::ConfigChange,
                format!("brain modality '{name}' deleted"),
                "ok",
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ok", "deleted": name})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// PUT /api/brain/default-modality — set the default modality.
pub async fn set_brain_default_modality(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let modality = body["default_modality"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if modality.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Missing 'default_modality' field"})),
        );
    }

    let result = state.kernel.update_brain(|config| {
        if !config.modalities.contains_key(&modality) {
            return;
        }
        config.default_modality = modality.clone();
    });

    match result {
        Ok(()) => {
            state.kernel.audit_log.record(
                "system",
                runtime::audit::AuditAction::ConfigChange,
                format!("default modality set to '{modality}'"),
                "ok",
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ok", "default_modality": modality})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// POST /api/brain/reload — reload Brain from disk.
pub async fn reload_brain(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.kernel.reload_brain() {
        Ok(()) => {
            state.kernel.audit_log.record(
                "system",
                runtime::audit::AuditAction::ConfigChange,
                "brain reloaded from disk",
                "ok",
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "ok", "message": "Brain reloaded"})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

/// GET /api/brain/config — Return raw brain.json content.
pub async fn get_brain_config_raw(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let path = state.kernel.brain_path();
    match std::fs::read_to_string(path) {
        Ok(json_str) => match serde_json::from_str::<serde_json::Value>(&json_str) {
            Ok(value) => (StatusCode::OK, Json(value)),
            Err(_) => (StatusCode::OK, Json(serde_json::json!({"_raw": json_str}))),
        },
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("Cannot read brain.json: {e}")})),
        ),
    }
}

/// PUT /api/brain/config — Update brain.json from raw JSON.
pub async fn put_brain_config_raw(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Validate it's a valid BrainConfig before writing
    let config: types::brain::BrainConfig = match serde_json::from_value(body.clone()) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Invalid brain config: {e}")})),
            );
        }
    };

    let path = state.kernel.brain_path();
    let json_str = match serde_json::to_string_pretty(&config) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Serialize error: {e}")})),
            );
        }
    };

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(path, json_str) {
        Ok(()) => {
            let reload_result = state.kernel.reload_brain();
            state.kernel.audit_log.record(
                "system",
                runtime::audit::AuditAction::ConfigChange,
                "brain.json updated via API",
                if reload_result.is_ok() {
                    "ok"
                } else {
                    "reload_failed"
                },
            );
            match reload_result {
                Ok(()) => (StatusCode::OK, Json(serde_json::json!({"status": "ok"}))),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Saved but reload failed: {e}")})),
                ),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Write error: {e}")})),
        ),
    }
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/brain", routing::get(brain_info))
        .route(
            "/api/brain/config",
            routing::put(put_brain_config_raw).get(get_brain_config_raw),
        )
        .route(
            "/api/brain/default-modality",
            routing::put(set_brain_default_modality),
        )
        .route(
            "/api/brain/modalities/{name}",
            routing::delete(delete_brain_modality)
                .get(brain_modality_detail)
                .put(set_brain_modality),
        )
        .route("/api/brain/reload", routing::post(reload_brain))
        .route("/api/brain/status", routing::get(brain_status))
}
