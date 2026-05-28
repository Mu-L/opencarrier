//! Dashboard session authentication endpoints.

use crate::routes::state::AppState;
use crate::session_auth;
use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Arc;
use std::time::Instant;

static LOGIN_FAILURES: LazyLock<Mutex<HashMap<String, (u32, Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const MAX_LOGIN_FAILURES: u32 = 5;
const LOGIN_BAN_SECS: u64 = 900; // 15 minutes

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
    request: Request<Body>,
) -> axum::response::Response {
    // Extract body from request
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Failed to read request body"})),
            )
                .into_response()
        }
    };
    let body: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Invalid JSON body"})),
            )
                .into_response()
        }
    };

    let username = body["username"].as_str().unwrap_or("").trim();
    let password = body["password"].as_str().unwrap_or("").trim();

    if username.is_empty() || password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Username and password required"})),
        )
            .into_response();
    }

    let auth_config = &state.kernel.config.auth;
    if !auth_config.enabled {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "Session auth not enabled"})),
        )
            .into_response();
    }

    // Rate limiting check
    // Clean up expired entries on each read to prevent unbounded growth
    {
        let mut failures = LOGIN_FAILURES.lock().unwrap_or_else(|e| {
            tracing::warn!("LOGIN_FAILURES Mutex poisoned, recovering");
            e.into_inner()
        });
        failures.retain(|_, (_, first_fail)| first_fail.elapsed().as_secs() < LOGIN_BAN_SECS);
    }

    let client_ip = parts
        .headers
        .get("x-real-ip")
        .or_else(|| parts.headers.get("x-forwarded-for"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .split(',')
        .next()
        .unwrap_or("unknown")
        .trim()
        .to_string();

    {
        let mut failures = LOGIN_FAILURES.lock().unwrap_or_else(|e| { tracing::warn!("LOGIN_FAILURES Mutex poisoned, recovering"); e.into_inner() });
        if let Some((count, first_fail)) = failures.get(&client_ip) {
            if *count >= MAX_LOGIN_FAILURES && first_fail.elapsed().as_secs() < LOGIN_BAN_SECS {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(serde_json::json!({"error": "Too many login attempts. Try again later."})),
                )
                    .into_response();
            }
            if first_fail.elapsed().as_secs() >= LOGIN_BAN_SECS {
                failures.remove(&client_ip);
            }
        }
    }

    if username != auth_config.username {
        // On credential failure, record the attempt
        {
            let mut failures = LOGIN_FAILURES.lock().unwrap_or_else(|e| { tracing::warn!("LOGIN_FAILURES Mutex poisoned, recovering"); e.into_inner() });
            let entry = failures.entry(client_ip.clone()).or_insert((0, Instant::now()));
            entry.0 += 1;
            if entry.1.elapsed().as_secs() >= LOGIN_BAN_SECS {
                *entry = (1, Instant::now());
            }
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid credentials"})),
        )
            .into_response();
    }

    match session_auth::verify_password(password, &auth_config.password_hash) {
        session_auth::PasswordResult::Invalid => {
            // On credential failure, record the attempt
            {
                let mut failures = LOGIN_FAILURES.lock().unwrap_or_else(|e| { tracing::warn!("LOGIN_FAILURES Mutex poisoned, recovering"); e.into_inner() });
                let entry = failures.entry(client_ip.clone()).or_insert((0, Instant::now()));
                entry.0 += 1;
                if entry.1.elapsed().as_secs() >= LOGIN_BAN_SECS {
                    *entry = (1, Instant::now());
                }
            }
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid credentials"})),
            )
                .into_response();
        }
        session_auth::PasswordResult::Upgraded { new_hash } => {
            // Auto-upgrade legacy SHA256 hash to Argon2id in config.toml
            let config_path = state.kernel.config.home_dir.join("config.toml");
            if config_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&config_path) {
                    if let Ok(mut table) = content.parse::<toml::value::Table>() {
                        if let Some(toml::Value::Table(ref mut t)) = table.get_mut("auth") {
                            t.insert("password_hash".to_string(), toml::Value::String(new_hash));
                        }
                        if let Ok(toml_string) = toml::to_string_pretty(&table) {
                            let _ = std::fs::write(&config_path, &toml_string);
                        }
                    }
                }
            }
        }
        session_auth::PasswordResult::Valid => {}
    }

    // Clear rate limit on successful login
    {
        let mut failures = LOGIN_FAILURES.lock().unwrap_or_else(|e| { tracing::warn!("LOGIN_FAILURES Mutex poisoned, recovering"); e.into_inner() });
        failures.remove(&client_ip);
    }

    // Use API key as the HMAC secret for session tokens
    let secret = &state.kernel.config.api_key;
    let token = match session_auth::create_session_token(
        None,
        "admin",
        username,
        secret,
        auth_config.session_ttl_hours,
    ) {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to create session token"})),
            )
                .into_response();
        }
    };

    let mut response = (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "username": username,
            "role": "admin",
            "token": token.clone(),
        })),
    )
        .into_response();

    // Set session cookie
    let cookie_value = format!(
        "opencarrier_session={}; HttpOnly; SameSite=Strict; Secure; Path=/",
        token
    );
    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie_value.parse().unwrap());

    response
}

/// POST /api/auth/change-credentials — Change username and/or password.
///
/// Requires the current password for verification. Writes to config.toml
/// and updates the in-memory config. Returns a new session token.
pub async fn change_credentials(
    AxumState(state): AxumState<Arc<AppState>>,
    request: Request<Body>,
) -> axum::response::Response {
    // Extract body from request
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, 1024 * 64).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Failed to read request body"})),
            )
                .into_response()
        }
    };
    let body: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "Invalid JSON body"})),
            )
                .into_response()
        }
    };

    // Verify session identity matches (if session cookie present)
    if let Some(cookie_str) = parts.headers.get("cookie").and_then(|v| v.to_str().ok()) {
        let session_token = cookie_str
            .split(';')
            .find_map(|part| part.trim().strip_prefix("opencarrier_session="));
        if let Some(token) = session_token {
            if let Some(info) =
                session_auth::verify_session_token(token, &state.kernel.config.api_key)
            {
                // Session user must match the account being modified
                let auth_config = &state.kernel.config.auth;
                if info.username != auth_config.username {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({"error": "Session identity mismatch"})),
                    )
                        .into_response();
                }
            }
        }
    }

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
        )
            .into_response();
    }

    if new_username.is_empty() && new_password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Provide new username or new password"})),
        )
            .into_response();
    }

    // Verify current password
    let auth_config = &state.kernel.config.auth;
    if !session_auth::verify_password_bool(current_password, &auth_config.password_hash) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Current password incorrect"})),
        )
            .into_response();
    }

    // Build updated values
    let updated_username = if new_username.is_empty() {
        auth_config.username.clone()
    } else {
        new_username
    };
    let updated_hash = if new_password.is_empty() {
        auth_config.password_hash.clone()
    } else if new_password.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Password must be at least 8 characters"})),
        )
            .into_response();
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
                    .into_response()
            }
        }
    } else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "config.toml not found"})),
        )
            .into_response();
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
                .into_response()
        }
    };

    if let Err(e) = std::fs::write(&config_path, &toml_string) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Write failed: {e}")})),
        )
            .into_response();
    }

    // Reload config from disk to update in-memory state
    let _ = state.kernel.reload_config();

    // Issue new session token
    let secret = &state.kernel.config.api_key;
    let token = match session_auth::create_session_token(
        None,
        "admin",
        &updated_username,
        secret,
        state.kernel.config.auth.session_ttl_hours,
    ) {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Failed to create session token"})),
            )
                .into_response();
        }
    };

    state.kernel.audit_log.record(
        "system",
        runtime::audit::AuditAction::ConfigChange,
        "dashboard credentials changed",
        "ok",
    );

    let mut response = (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "username": updated_username,
            "token": token.clone(),
        })),
    )
        .into_response();

    // Set session cookie
    let cookie_value = format!(
        "opencarrier_session={}; HttpOnly; SameSite=Strict; Secure; Path=/",
        token
    );
    response
        .headers_mut()
        .insert(axum::http::header::SET_COOKIE, cookie_value.parse().unwrap());

    response
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
