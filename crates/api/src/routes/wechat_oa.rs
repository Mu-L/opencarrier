//! WeChat OA admin API endpoints — the "outer skin" of the `wechat-oa` core
//! (2026-08-18). Direct, thin proxies of the WeChat data-plane APIs for
//! server-bound accounts (credentials resolved from
//! `senders/<app_id>/session.json`, never echoed).
//!
//! Prefix `/api/wechat-oa/{app_id}/...` — deliberately distinct from the
//! public webhook prefix `/api/weixin-oa/...` so the auth middleware's
//! callback whitelist pattern cannot accidentally match: everything here is
//! key-gated (管理面 tier, docs/AGENT-APP-API.md §4).
//!
//! Zero-LLM by construction: these endpoints exist for deterministic callers
//! (cron orchestration, the desktop app, operator curl); agents that need
//! WeChat data occasionally just run a normal turn.

use crate::routes::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

/// Shared HTTP client for upstream WeChat calls.
static HTTP: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .unwrap_or_default()
});

/// Resolve a server-bound account and a valid access_token.
/// The error Response never contains the app_secret.
async fn resolve_token(
    state: &Arc<AppState>,
    app_id: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let account = match wechat_oa::session::load_account(&state.kernel.config.home_dir, app_id) {
        Some(a) => a,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "account_not_found",
                    "detail": format!("no senders/{app_id}/session.json for a weixin-oa account"),
                })),
            ))
        }
    };
    match wechat_oa::token::get_token(&HTTP, &account.app_id, &account.app_secret).await {
        Ok(t) => Ok(t),
        Err(e) => {
            let detail = e.to_string();
            let code = wechat_oa::api::extract_errcode(&detail);
            Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "wechat_api",
                    "errcode": code,
                    "errmsg": detail,
                })),
            ))
        }
    }
}

/// Map a core API error into the normalized error shape.
fn wechat_error(e: types::error::CarrierError) -> (StatusCode, Json<serde_json::Value>) {
    let detail = e.to_string();
    let code = wechat_oa::api::extract_errcode(&detail);
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": "wechat_api",
            "errcode": code,
            "errmsg": detail,
        })),
    )
}

/// GET /api/wechat-oa/{app_id}/user/get?next_openid= — paged follower list.
/// The first page already carries the official `total`.
pub async fn user_get(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            let next = params.get("next_openid").map(|s| s.as_str());
            match wechat_oa::api::user_get(&HTTP, &token, next).await {
                Ok(r) => (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "total": r.total,
                        "count": r.count,
                        "next_openid": r.next_openid,
                        "data": r.raw.get("data").cloned().unwrap_or(serde_json::Value::Null),
                    })),
                ),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/freepublish/get — publish status by publish_id.
pub async fn freepublish_get(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(publish_id) = body["publish_id"].as_str().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing publish_id"})),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::freepublish_get(&HTTP, &token, publish_id).await {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/draft/batchget — draft box inventory.
/// Body: `{offset?, count?, no_content?}` (defaults 0 / 5 / true).
pub async fn draft_batchget(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let offset = body["offset"].as_u64().unwrap_or(0) as u32;
    let count = body["count"].as_u64().unwrap_or(5).clamp(1, 20) as u32;
    let no_content = body["no_content"].as_bool().unwrap_or(true);
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::draft_batchget(&HTTP, &token, offset, count, no_content).await {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/draft/count — draft box count.
pub async fn draft_count(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
) -> impl IntoResponse {
    match resolve_token(&state, &app_id).await {
        Ok(token) => match wechat_oa::api::draft_count(&HTTP, &token).await {
            Ok(n) => (StatusCode::OK, Json(serde_json::json!({"total_count": n}))),
            Err(e) => wechat_error(e),
        },
        Err(resp) => resp,
    }
}

/// GET /api/wechat-oa/{app_id}/template/list — template inventory.
pub async fn template_list(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
) -> impl IntoResponse {
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::get_all_private_template(&HTTP, &token).await {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/message/template/send — send a template
/// message (no 48h-window limit). Body:
/// `{touser, template_id, data, url?, miniprogram?}`.
pub async fn template_send(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(touser) = body["touser"].as_str().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing touser (openid)"})),
        );
    };
    let Some(template_id) = body["template_id"].as_str().filter(|s| !s.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing template_id"})),
        );
    };
    let data = body.get("data").cloned().unwrap_or(serde_json::json!({}));
    let url = body["url"].as_str();
    let miniprogram = body.get("miniprogram").filter(|v| v.is_object());
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::template_send(
                &HTTP, &token, touser, template_id, url, miniprogram, &data,
            )
            .await
            {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing::{get, post};
    axum::Router::new()
        .route("/api/wechat-oa/{app_id}/user/get", get(user_get))
        .route("/api/wechat-oa/{app_id}/freepublish/get", post(freepublish_get))
        .route("/api/wechat-oa/{app_id}/draft/batchget", post(draft_batchget))
        .route("/api/wechat-oa/{app_id}/draft/count", post(draft_count))
        .route("/api/wechat-oa/{app_id}/template/list", get(template_list))
        .route(
            "/api/wechat-oa/{app_id}/message/template/send",
            post(template_send),
        )
}
