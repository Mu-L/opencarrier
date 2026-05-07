//! Dashboard session authentication endpoints.

use crate::routes::state::AppState;
use crate::session_auth;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;
use axum::body::Body;

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
    let token =
        session_auth::create_session_token(None, "admin", username, secret, auth_config.session_ttl_hours);

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

/// Build a router with auth routes.
pub fn router() -> axum::Router<std::sync::Arc<AppState>> {
    use axum::routing;
    axum::Router::new()
        .route("/api/auth/check", routing::get(auth_check))
        .route("/api/auth/login", routing::post(auth_login))
}
