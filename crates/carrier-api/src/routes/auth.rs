//! Dashboard session authentication endpoints.

use crate::routes::state::AppState;
use crate::session_auth;
use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;

/// GET /api/auth/check — Return auth mode and current session status.
pub async fn auth_check(
    AxumState(state): AxumState<Arc<AppState>>,
    request: Request<Body>,
) -> impl IntoResponse {
    let auth_config = &state.kernel.config.auth;

    if !auth_config.enabled {
        return Json(serde_json::json!({
            "mode": "none",
            "authenticated": false
        }));
    }

    // Check if session cookie is present and valid
    let secret = &state.kernel.config.api_key;
    let session_token = request
        .headers()
        .get("cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|cookie_str| {
            cookie_str
                .split(';')
                .find_map(|part| part.trim().strip_prefix("opencarrier_session="))
        });

    if let Some(token) = session_token {
        if let Some(info) = session_auth::verify_session_token(token, secret) {
            return Json(serde_json::json!({
                "mode": "session",
                "authenticated": true,
                "username": info.username,
                "role": info.role,
                "tenant_id": info.tenant_id
            }));
        }
    }

    Json(serde_json::json!({
        "mode": "session",
        "authenticated": false,
    }))
}

/// POST /api/auth/login — Authenticate with username/password, return session token.
pub async fn auth_login(
    AxumState(state): AxumState<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let username = body["username"].as_str().unwrap_or("").trim();
    let password = body["password"].as_str().unwrap_or("").trim();

    if username.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Username and password required"})),
        );
    }

    let auth_config = &state.kernel.config.auth;
    if !auth_config.enabled {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Session auth not enabled"})),
        );
    }

    if username != auth_config.username {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid credentials"})),
        );
    }

    if !session_auth::verify_password(password, &auth_config.password_hash) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid credentials"})),
        );
    }

    // Use API key as the HMAC secret for session tokens
    let secret = &state.kernel.config.api_key;
    let token = session_auth::create_session_token(
        None,
        "admin",
        username,
        secret,
        auth_config.session_ttl_hours,
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "username": username,
            "role": "admin",
            "token": token,
        })),
    )
}

/// POST /api/auth/change-credentials — Change username and/or password.
///
/// Requires the current password for verification. Writes to config.toml
/// and updates the in-memory config. Returns a new session token.
pub async fn change_credentials(
    AxumState(state): AxumState<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let current_password = body["current_password"].as_str().unwrap_or("").trim();
    let new_username = body["new_username"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();
    let new_password = body["new_password"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if current_password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Current password required"})),
        );
    }

    if new_username.is_empty() && new_password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Provide new username or new password"})),
        );
    }

    // Verify current password
    let auth_config = &state.kernel.config.auth;
    if !session_auth::verify_password(current_password, &auth_config.password_hash) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Current password incorrect"})),
        );
    }

    // Build updated values
    let updated_username = if new_username.is_empty() {
        auth_config.username.clone()
    } else {
        new_username
    };
    let updated_hash = if new_password.is_empty() {
        auth_config.password_hash.clone()
    } else if new_password.len() < 6 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Password must be at least 6 characters"})),
        );
    } else {
        session_auth::hash_password(&new_password)
    };

    // Write to config.toml
    let config_path = state.kernel.config.home_dir.join("config.toml");
    let mut table: toml::value::Table = if config_path.exists() {
        match std::fs::read_to_string(&config_path) {
            Ok(content) => toml::from_str(&content).unwrap_or_default(),
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": format!("Cannot read config: {e}")})),
                )
            }
        }
    } else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "config.toml not found"})),
        );
    };

    // Update [auth] section
    let auth_table = table
        .entry("auth".to_string())
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    if let toml::Value::Table(ref mut t) = auth_table {
        t.insert(
            "username".to_string(),
            toml::Value::String(updated_username.clone()),
        );
        t.insert(
            "password_hash".to_string(),
            toml::Value::String(updated_hash.clone()),
        );
        t.insert("enabled".to_string(), toml::Value::Boolean(true));
    }

    let toml_string = match toml::to_string_pretty(&table) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("Serialize failed: {e}")})),
            )
        }
    };

    if let Err(e) = std::fs::write(&config_path, &toml_string) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Write failed: {e}")})),
        );
    }

    // Reload config from disk to update in-memory state
    let _ = state.kernel.reload_config();

    // Issue new session token
    let secret = &state.kernel.config.api_key;
    let token = session_auth::create_session_token(
        None,
        "admin",
        &updated_username,
        secret,
        state.kernel.config.auth.session_ttl_hours,
    );

    state.kernel.audit_log.record(
        "system",
        carrier_runtime::audit::AuditAction::ConfigChange,
        "dashboard credentials changed",
        "ok",
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "username": updated_username,
            "token": token,
        })),
    )
}

/// Build a router with auth routes.
pub fn router() -> axum::Router<std::sync::Arc<AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/auth/check", routing::get(auth_check))
        .route("/api/auth/login", routing::post(auth_login))
        .route(
            "/api/auth/change-credentials",
            routing::post(change_credentials),
        )
}
