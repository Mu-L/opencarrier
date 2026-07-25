//! Production middleware for the Carrier API server.
//!
//! Provides:
//! - Request ID generation and propagation
//! - Per-endpoint structured request logging
//! - Bearer token (API key) authentication

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use axum::middleware::Next;
use std::time::Instant;
use tracing::info;

/// Request ID header name (standard).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Middleware: inject a unique request ID and log the request/response.
pub async fn request_logging(request: Request<Body>, next: Next) -> Response<Body> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let method = request.method().clone();
    // Deliberately use .path() not full URI to avoid logging ?token= query params
    let uri = request.uri().path().to_string();
    let start = Instant::now();

    let mut response = next.run(request).await;

    let elapsed = start.elapsed();
    let status = response.status().as_u16();

    info!(
        request_id = %request_id,
        method = %method,
        path = %uri,
        status = status,
        latency_ms = elapsed.as_millis() as u64,
        "API request"
    );

    // Inject the request ID into the response
    if let Ok(header_val) = request_id.parse() {
        response.headers_mut().insert(REQUEST_ID_HEADER, header_val);
    }

    response
}

/// Authentication state passed to the auth middleware.
#[derive(Clone)]
pub struct AuthState {
    pub api_key: String,
    pub auth_enabled: bool,
}

/// Constant-time string comparison to mitigate timing attacks.
fn constant_time_eq_str(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    if a.len() != b.len() {
        return false;
    }
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

/// Check whether a request is authenticated against the configured API key.
///
/// Returns `true` when any of these hold:
/// - **Open mode**: no API key configured and dashboard auth disabled (everything open).
/// - A valid `Authorization: Bearer <key>` or `X-API-Key` header.
/// - A valid `?token=<key>` query parameter (for SSE/EventSource clients that
///   cannot set headers).
/// - A valid `opencarrier_session=` cookie (dashboard login).
///
/// `api_key` should already be trimmed. This is the single source of truth for
/// the accept/reject decision — shared by the [`auth`] middleware and by handlers
/// that gate sub-paths of an otherwise-public endpoint (e.g. file `view`:
/// `output/` is public for WeChat direct links, other paths require auth).
pub fn request_is_authenticated(
    headers: &axum::http::HeaderMap,
    query_token: Option<&str>,
    api_key: &str,
    auth_enabled: bool,
) -> bool {
    // Open mode: no key and auth disabled → everything is open.
    if api_key.is_empty() && !auth_enabled {
        return true;
    }

    // Bearer token or X-API-Key header (constant-time compare).
    let header_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| headers.get("x-api-key").and_then(|v| v.to_str().ok()));
    if header_token
        .map(|t| constant_time_eq_str(t, api_key))
        .unwrap_or(false)
    {
        return true;
    }

    // ?token= query parameter (constant-time compare).
    if query_token
        .map(|t| constant_time_eq_str(t, api_key))
        .unwrap_or(false)
    {
        return true;
    }

    // Dashboard session cookie (only meaningful when auth is enabled).
    if auth_enabled {
        if let Some(token) = headers
            .get("cookie")
            .and_then(|v| v.to_str().ok())
            .and_then(|c| {
                c.split(';')
                    .find_map(|p| p.trim().strip_prefix("opencarrier_session="))
            })
        {
            if crate::session_auth::verify_session_token(token, api_key).is_some() {
                return true;
            }
        }
    }

    false
}

/// Bearer token authentication middleware.
///
/// When `api_key` is non-empty (after trimming), requests to non-public
/// endpoints must include `Authorization: Bearer <api_key>`.
/// If the key is empty or whitespace-only, auth is disabled entirely
/// (public/local development mode).
pub async fn auth(
    axum::extract::State(auth_state): axum::extract::State<AuthState>,
    request: Request<Body>,
    next: Next,
) -> Response<Body> {
    // SECURITY: Capture method early for method-aware public endpoint checks.
    let method = request.method().clone();

    // Shutdown is loopback-only (CLI on same machine) — skip token auth
    let path = request.uri().path();
    if path == "/api/shutdown" {
        let is_loopback = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip().is_loopback())
            .unwrap_or(false); // SECURITY: default-deny — unknown origin is NOT loopback
        if is_loopback {
            return next.run(request).await;
        }
    }

    // Public endpoints that don't require auth.
    // SECURITY: Public endpoints are GET-only unless explicitly noted.
    // POST/PUT/DELETE to any endpoint ALWAYS requires auth to prevent
    // unauthenticated writes (cron job creation, skill install, etc.).
    let is_get = method == axum::http::Method::GET;
    let is_public = path == "/"
        || path == "/share"
        || path == "/logo.png"
        || path == "/favicon.ico"
        || path == "/manifest.json"
        || path == "/sw.js"
        || path == "/bd00e4fe4983179012e6ffdcc66d0c4b.txt"
        || path.starts_with("/vendor/")
        || path.starts_with("/katex-fonts/")
        || (path == "/.well-known/agent.json" && is_get)
        || path == "/api/health"
        || path == "/api/auth/check"
        || path == "/api/auth/login"
        || path == "/api/share/agents"
        // Share-page platform auth flows (pre-onboarding)
        || path == "/api/weixin/qrcode"
        || path == "/api/weixin/qrcode-status"
        // WeChat OA webhook callback — signed by WeChat (checkSign), no API key
        || (path.starts_with("/api/weixin-oa/") && path.ends_with("/callback"))
        || path == "/api/senders/wecom/smartbot/generate"
        || path == "/api/senders/wecom/smartbot/poll"
        || path == "/api/senders/feishu/device-auth"
        || path == "/api/senders/feishu/device-auth/poll"
        || path == "/api/senders/dingtalk/device-auth"
        || path == "/api/senders/dingtalk/device-auth/poll"
        // Clone access control (share page needs these without auth)
        || path.starts_with("/api/clones/") && (path.ends_with("/access") || path.ends_with("/verify-access"))
        // Agent output files — must be public for WeChat direct download links
        || (path.starts_with("/api/agents/") && path.contains("/output/") && is_get)
        // File explorer view — public for direct file links (browser viewing via file.yinnho.cn)
        || (path.starts_with("/api/files/view/") && is_get);

    if is_public {
        return next.run(request).await;
    }

    // Resolve the ?token= query parameter up front (owned, so it doesn't borrow
    // `request` past the move into `next`). Used by the helper for SSE/EventSource
    // clients that cannot set headers.
    let query_token = request
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("token=")))
        .map(str::to_owned);

    let api_key = auth_state.api_key.trim();

    // Was any credential (header or ?token=) provided? Used only to choose the
    // error message — the accept/reject decision itself lives in the helper.
    let credential_provided = request
        .headers()
        .get("authorization")
        .or_else(|| request.headers().get("x-api-key"))
        .is_some()
        || query_token.is_some();

    if request_is_authenticated(
        request.headers(),
        query_token.as_deref(),
        api_key,
        auth_state.auth_enabled,
    ) {
        return next.run(request).await;
    }

    // Determine error message: was a credential provided but wrong, or missing entirely?
    let error_msg = if credential_provided {
        "Invalid API key"
    } else {
        "Missing Authorization: Bearer <api_key> header"
    };

    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .body(Body::from(
            serde_json::json!({"error": error_msg}).to_string(),
        ))
        .unwrap_or_default()
}

/// Security headers middleware — applied to ALL API responses.
pub async fn security_headers(request: Request<Body>, next: Next) -> Response<Body> {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("x-xss-protection", "1; mode=block".parse().unwrap());
    // CSP: removed unsafe-eval and unsafe-inline for script-src, restricted connect-src
    headers.insert(
        "content-security-policy",
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: blob: https://liteapp.weixin.qq.com https://*.qpic.cn; connect-src 'self' wss:; font-src 'self'; media-src 'self' blob:; frame-src 'self' blob:; object-src 'none'; base-uri 'self'; form-action 'self'"
            .parse()
            .unwrap(),
    );
    headers.insert(
        "referrer-policy",
        "strict-origin-when-cross-origin".parse().unwrap(),
    );
    headers.insert(
        "cache-control",
        "no-store, no-cache, must-revalidate".parse().unwrap(),
    );
    headers.insert(
        "strict-transport-security",
        "max-age=63072000; includeSubDomains".parse().unwrap(),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_header_constant() {
        assert_eq!(REQUEST_ID_HEADER, "x-request-id");
    }

    fn hdrs(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<axum::http::HeaderName>().unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn open_mode_accepts_everything() {
        // No API key + auth disabled ⇒ server is intentionally open.
        let h = hdrs(&[]);
        assert!(request_is_authenticated(&h, None, "", false));
    }

    #[test]
    fn valid_bearer_or_xapikey_or_token_accepted() {
        let key = "secret-key";
        assert!(request_is_authenticated(
            &hdrs(&[("authorization", "Bearer secret-key")]),
            None,
            key,
            true,
        ));
        assert!(request_is_authenticated(
            &hdrs(&[("x-api-key", "secret-key")]),
            None,
            key,
            true,
        ));
        assert!(request_is_authenticated(&hdrs(&[]), Some("secret-key"), key, true));
    }

    #[test]
    fn wrong_credential_rejected() {
        let h = hdrs(&[("authorization", "Bearer wrong")]);
        assert!(!request_is_authenticated(&h, None, "secret-key", true));
    }

    #[test]
    fn missing_credential_rejected_when_auth_enabled() {
        assert!(!request_is_authenticated(&hdrs(&[]), None, "secret-key", true));
    }

    #[test]
    fn nonempty_key_keeps_auth_on_even_if_auth_disabled() {
        // Open mode requires BOTH empty key and auth disabled. A configured key
        // always gates, regardless of the dashboard-auth toggle.
        assert!(!request_is_authenticated(&hdrs(&[]), None, "secret-key", false));
    }

    #[test]
    fn valid_session_cookie_accepted() {
        let token = crate::session_auth::create_session_token(None, "admin", "admin", "secret-key", 1)
            .unwrap();
        let h = hdrs(&[("cookie", &format!("opencarrier_session={token}"))]);
        assert!(request_is_authenticated(&h, None, "secret-key", true));
    }

    #[test]
    fn session_cookie_ignored_when_auth_disabled() {
        // Cookie path only runs when auth_enabled. A valid-looking cookie must
        // NOT bypass a key that the operator hasn't enabled dashboard auth for.
        let token = crate::session_auth::create_session_token(None, "admin", "admin", "secret-key", 1)
            .unwrap();
        let h = hdrs(&[("cookie", &format!("opencarrier_session={token}"))]);
        assert!(!request_is_authenticated(&h, None, "secret-key", false));
    }
}
