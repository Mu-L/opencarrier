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

// ── Comment (留言) management ──────────────────────────────
//
// Thin proxies of the eight /cgi-bin/comment/* APIs, ported from the retired
// wechat-oa-mcp server (2026-08-18). All bodies carry `msg_data_id` (from the
// article's publish status) and optional `index` (article position in a
// multi-article post, default 0).

/// Parse an i64 from a JSON number or a numeric string — deterministic
/// callers occasionally quote ids.
fn flexible_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Extract the common `(msg_data_id, index)` pair from a comment body.
fn comment_target(
    body: &serde_json::Value,
) -> Result<(i64, u32), (StatusCode, Json<serde_json::Value>)> {
    match flexible_i64(&body["msg_data_id"]) {
        Some(id) => Ok((id, flexible_i64(&body["index"]).unwrap_or(0).max(0) as u32)),
        None => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing msg_data_id"})),
        )),
    }
}

/// POST /api/wechat-oa/{app_id}/comment/open — open an article's comment
/// section (a prerequisite for the others: list fails on a closed section).
pub async fn comment_open(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_open(&HTTP, &token, msg_data_id, index).await {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/comment/close — close an article's comment
/// section.
pub async fn comment_close(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_close(&HTTP, &token, msg_data_id, index).await {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/comment/list — reader comments for an
/// article. Body: `{msg_data_id, index?, comment_type?, offset?, count?}`
/// (comment_type: 0=all 1=normal 2=featured; count clamped 1..=50).
pub async fn comment_list(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let comment_type = flexible_i64(&body["comment_type"]).unwrap_or(0).max(0) as u32;
    let begin = flexible_i64(&body["offset"]).unwrap_or(0).max(0) as u32;
    let count = flexible_i64(&body["count"]).unwrap_or(10).clamp(1, 50) as u32;
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_list(
                &HTTP, &token, msg_data_id, index, comment_type, begin, count,
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

/// POST /api/wechat-oa/{app_id}/comment/markelect — feature (精选) a comment.
pub async fn comment_mark_elect(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(comment_id) = flexible_i64(&body["comment_id"]) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing comment_id"})),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_mark_elect(&HTTP, &token, msg_data_id, index, comment_id)
                .await
            {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/comment/unmarkelect — drop the featured mark.
pub async fn comment_unmark_elect(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(comment_id) = flexible_i64(&body["comment_id"]) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing comment_id"})),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_unmark_elect(
                &HTTP, &token, msg_data_id, index, comment_id,
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

/// POST /api/wechat-oa/{app_id}/comment/delete — delete a comment
/// (irreversible).
pub async fn comment_delete(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(comment_id) = flexible_i64(&body["comment_id"]) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing comment_id"})),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_delete(&HTTP, &token, msg_data_id, index, comment_id)
                .await
            {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => wechat_error(e),
            }
        }
        Err(resp) => resp,
    }
}

/// POST /api/wechat-oa/{app_id}/comment/reply — official reply to a comment.
/// Body: `{msg_data_id, index?, comment_id, content}`.
pub async fn comment_reply(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(comment_id) = flexible_i64(&body["comment_id"]) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing comment_id"})),
        );
    };
    let Some(content) = body["content"].as_str().filter(|s| !s.trim().is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing content"})),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_reply_add(
                &HTTP, &token, msg_data_id, index, comment_id, content,
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

/// POST /api/wechat-oa/{app_id}/comment/reply/delete — delete an official
/// reply. Body: `{msg_data_id, index?, comment_id, reply_id}`.
pub async fn comment_reply_delete(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let (msg_data_id, index) = match comment_target(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(comment_id) = flexible_i64(&body["comment_id"]) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing comment_id"})),
        );
    };
    let Some(reply_id) = flexible_i64(&body["reply_id"]) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing reply_id"})),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => {
            match wechat_oa::api::comment_reply_delete(
                &HTTP, &token, msg_data_id, index, comment_id, reply_id,
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

/// POST /api/wechat-oa/{app_id}/datacube/article — per-article read stats
/// for a single day (`POST /datacube/getarticletotal`, T+1). Body:
/// `{date: "YYYY-MM-DD"}` (yesterday or older; begin==end). Raw passthrough:
/// `{"list":[{msg_data_id,title,details:[{stat_date,int_page_read_count,
/// int_page_read_user,share_count,...}]}]}` — join against article URLs via
/// the URL's `mid=` parameter (same id space as `msg_data_id`).
pub async fn datacube_article(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let Some(date) = body["date"]
        .as_str()
        .map(str::trim)
        .filter(|s| s.len() == 10 && s.as_bytes().get(4) == Some(&b'-') && s.as_bytes().get(7) == Some(&b'-'))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "missing/invalid date (expect YYYY-MM-DD, single day, yesterday or older)"
            })),
        );
    };
    match resolve_token(&state, &app_id).await {
        Ok(token) => match wechat_oa::api::article_total(&HTTP, &token, date).await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => wechat_error(e),
        },
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
        .route(
            "/api/wechat-oa/{app_id}/datacube/article",
            post(datacube_article),
        )
        // Comment (留言) family — ported from the retired wechat-oa-mcp server
        // (2026-08-18); the reader-comment knowledge-base plan consumes these.
        .route("/api/wechat-oa/{app_id}/comment/open", post(comment_open))
        .route("/api/wechat-oa/{app_id}/comment/close", post(comment_close))
        .route("/api/wechat-oa/{app_id}/comment/list", post(comment_list))
        .route(
            "/api/wechat-oa/{app_id}/comment/markelect",
            post(comment_mark_elect),
        )
        .route(
            "/api/wechat-oa/{app_id}/comment/unmarkelect",
            post(comment_unmark_elect),
        )
        .route(
            "/api/wechat-oa/{app_id}/comment/delete",
            post(comment_delete),
        )
        .route(
            "/api/wechat-oa/{app_id}/comment/reply",
            post(comment_reply),
        )
        .route(
            "/api/wechat-oa/{app_id}/comment/reply/delete",
            post(comment_reply_delete),
        )
}
