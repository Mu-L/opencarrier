//! Per-agent channel bindings: 服务号对话 (weixin-oa) + 微信客服 (wecom kf).
//!
//! Source of truth: `~/.opencarrier/senders/{id}/session.json` with `bind_agent`.
//! Distinct from user-side 公众号发文章 credentials (profile.wechat_accounts).

use crate::routes::common::resolve_to_name;
use crate::routes::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use std::sync::Arc;
use tracing::{info, warn};

// ─── List ───────────────────────────────────────────────────────────────────

/// GET /api/agents/{agent}/channels
///
/// Returns weixin-oa and wecom-kf sessions bound to this agent.
pub async fn list_agent_channels(
    State(state): State<Arc<AppState>>,
    Path(agent): Path<String>,
) -> impl IntoResponse {
    let agent_name = match resolve_to_name(&agent, &state.kernel.registry) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };

    let home = types::config::home_dir();

    let mut weixin_oa = Vec::new();
    let mut wecom_kf = Vec::new();

    for (sender_id, json) in types::config::scan_sender_sessions(&home) {
        let channel = json.get("channel").and_then(|v| v.as_str()).unwrap_or("");
        let bind = json
            .get("bind_agent")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if bind != agent_name {
            continue;
        }

        match channel {
            "weixin-oa" => {
                let app_id = json
                    .get("app_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&sender_id)
                    .to_string();
                let name = json
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let token_ok = json
                    .get("token")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.is_empty());
                let secret_ok = json
                    .get("app_secret")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.is_empty());
                // Relative path; admin fills host in 公众号后台 (or prepends known domain).
                let callback_url = format!("/api/weixin-oa/{app_id}/callback");
                weixin_oa.push(serde_json::json!({
                    "type": "weixin-oa",
                    "id": app_id,
                    "app_id": app_id,
                    "name": name,
                    "wechat_id": json.get("wechat_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "has_token": token_ok,
                    "has_app_secret": secret_ok,
                    "callback_url": callback_url,
                    "bind_openid_url": json.get("bind_openid_url").and_then(|v| v.as_str()),
                }));
            }
            "wecom" => {
                let mode = json.get("mode").and_then(|v| v.as_str()).unwrap_or("");
                if mode != "kf" {
                    continue;
                }
                let name = json
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&sender_id)
                    .to_string();
                let open_kfid = json
                    .get("open_kfid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let corp_id = json
                    .get("corp_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let secret_ok = json
                    .get("secret")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| !t.is_empty());
                wecom_kf.push(serde_json::json!({
                    "type": "wecom-kf",
                    "id": name,
                    "name": name,
                    "open_kfid": open_kfid,
                    "corp_id": corp_id,
                    "has_secret": secret_ok,
                    "webhook_port": json.get("webhook_port").and_then(|v| v.as_u64()).unwrap_or(9100),
                }));
            }
            _ => {}
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "agent": agent_name,
            "weixin_oa": weixin_oa,
            "wecom_kf": wecom_kf,
        })),
    )
        .into_response()
}

// ─── Weixin OA bind / unbind ────────────────────────────────────────────────

/// POST /api/agents/{agent}/channels/weixin-oa
///
/// Create or update a 服务号 session and bind it to this agent.
pub async fn bind_weixin_oa(
    State(state): State<Arc<AppState>>,
    Path(agent): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_name = match resolve_to_name(&agent, &state.kernel.registry) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };

    let app_id = match body.get("app_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 app_id"})),
            )
                .into_response();
        }
    };

    let home = types::config::home_dir();
    let dir = home.join("senders").join(&app_id);
    let path = dir.join("session.json");

    // Load existing if any (merge credentials so re-bind can omit secrets)
    let mut existing: Option<channel_weixin_oa::WeixinOaSessionFile> = None;
    if path.exists() {
        if let Ok(data) = std::fs::read_to_string(&path) {
            existing = serde_json::from_str(&data).ok();
        }
    }

    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|e| e.name.clone()))
        .unwrap_or_else(|| app_id.clone());

    let app_secret = body
        .get("app_secret")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|e| e.app_secret.clone()))
        .unwrap_or_default();

    let token = body
        .get("token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|e| e.token.clone()))
        .unwrap_or_default();

    if app_secret.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "缺少 app_secret（新建必须填写）"})),
        )
            .into_response();
    }
    if token.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "缺少 token（公众号后台 URL 验证用）"})),
        )
            .into_response();
    }

    let wechat_id = body
        .get("wechat_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().map(|e| e.wechat_id.clone()))
        .unwrap_or_default();

    let bind_openid_url = body
        .get("bind_openid_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().and_then(|e| e.bind_openid_url.clone()));

    let sf = channel_weixin_oa::WeixinOaSessionFile {
        channel: "weixin-oa".to_string(),
        sender_key: "app_id".to_string(),
        name: name.clone(),
        app_id: app_id.clone(),
        app_secret,
        token,
        wechat_id,
        bind_agent: Some(agent_name.clone()),
        bind_openid_url,
    };

    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("创建目录失败: {e}")})),
        )
            .into_response();
    }
    let json = match serde_json::to_string_pretty(&sf) {
        Ok(j) => j,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("序列化失败: {e}")})),
            )
                .into_response();
        }
    };
    if let Err(e) = std::fs::write(&path, &json) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("写入 session 失败: {e}")})),
        )
            .into_response();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    // Hot-load account into runtime + route
    channel_weixin_oa::WEIXIN_OA_STATE.accounts.insert(
        app_id.clone(),
        Arc::new(channel_weixin_oa::OaAccountState::from_session(&sf)),
    );

    if let Some(ref pm) = state.channel_manager {
        let pm = pm.lock().await;
        pm.set_sender_route(&app_id, &agent_name);
    }

    info!(%app_id, agent = %agent_name, "Bound weixin-oa channel to agent");

    let callback_url = format!("/api/weixin-oa/{app_id}/callback");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "type": "weixin-oa",
            "app_id": app_id,
            "name": name,
            "agent": agent_name,
            "callback_url": callback_url,
            "callback_url_hint": "填公众号后台时加上域名，如 https://your.host" ,
        })),
    )
        .into_response()
}

/// DELETE /api/agents/{agent}/channels/weixin-oa/{app_id}
///
/// Soft-unbind: clear bind_agent, keep credentials on disk.
pub async fn unbind_weixin_oa(
    State(state): State<Arc<AppState>>,
    Path((agent, app_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let agent_name = match resolve_to_name(&agent, &state.kernel.registry) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };

    let home = types::config::home_dir();
    let path = home.join("senders").join(&app_id).join("session.json");
    if !path.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "服务号 session 不存在"})),
        )
            .into_response();
    }

    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let mut sf: channel_weixin_oa::WeixinOaSessionFile = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("解析 session 失败: {e}")})),
            )
                .into_response();
        }
    };

    if sf.bind_agent.as_deref() != Some(agent_name.as_str()) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "该服务号当前绑定的是 {:?}，不是 {agent_name}",
                    sf.bind_agent
                )
            })),
        )
            .into_response();
    }

    sf.bind_agent = None;
    if let Ok(json) = serde_json::to_string_pretty(&sf) {
        let _ = std::fs::write(&path, json);
    }

    // Update runtime bind_agent by replacing account entry
    channel_weixin_oa::WEIXIN_OA_STATE.accounts.insert(
        app_id.clone(),
        Arc::new(channel_weixin_oa::OaAccountState::from_session(&sf)),
    );

    if let Some(ref pm) = state.channel_manager {
        let pm = pm.lock().await;
        let _ = pm.remove_sender_route(&app_id);
    }

    info!(%app_id, agent = %agent_name, "Unbound weixin-oa channel from agent");

    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "app_id": app_id, "unbound": true})),
    )
        .into_response()
}

// ─── WeCom 微信客服 bind / unbind ───────────────────────────────────────────

/// POST /api/agents/{agent}/channels/wecom-kf
pub async fn bind_wecom_kf(
    State(state): State<Arc<AppState>>,
    Path(agent): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_name = match resolve_to_name(&agent, &state.kernel.registry) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };

    let name = match body.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 name（客服账号标识，如 86bus-kf）"})),
            )
                .into_response();
        }
    };

    let home = types::config::home_dir();
    // kf sender_id = name
    let path = home.join("senders").join(&name).join("session.json");
    let mut existing: Option<channel_wecom::token::WecomSessionFile> = None;
    if path.exists() {
        if let Ok(data) = std::fs::read_to_string(&path) {
            existing = serde_json::from_str(&data).ok();
        }
    }

    let corp_id = body
        .get("corp_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().and_then(|e| e.corp_id.clone()))
        .unwrap_or_default();
    let open_kfid = body
        .get("open_kfid")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().and_then(|e| e.open_kfid.clone()))
        .unwrap_or_default();
    let secret = body
        .get("secret")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().and_then(|e| e.secret.clone()))
        .unwrap_or_default();

    if corp_id.is_empty() || open_kfid.is_empty() || secret.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "缺少 corp_id / open_kfid / secret"})),
        )
            .into_response();
    }

    let callback_token = body
        .get("callback_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().and_then(|e| e.callback_token.clone()));
    let encoding_aes_key = body
        .get("encoding_aes_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| existing.as_ref().and_then(|e| e.encoding_aes_key.clone()));
    let webhook_port = body
        .get("webhook_port")
        .and_then(|v| v.as_u64())
        .map(|n| n as u16)
        .or_else(|| existing.as_ref().and_then(|e| e.webhook_port))
        .unwrap_or(9100);

    let sf = channel_wecom::token::WecomSessionFile {
        channel: "wecom".to_string(),
        sender_key: "bot_id".to_string(),
        name: name.clone(),
        mode: "kf".to_string(),
        bot_id: None,
        agent_id: None,
        corp_id: Some(corp_id.clone()),
        open_kfid: Some(open_kfid.clone()),
        secret: Some(secret),
        secret_env: None,
        webhook_port: Some(webhook_port),
        encoding_aes_key,
        callback_token,
        mcp_bot_id: None,
        mcp_bot_secret: None,
        bind_agent: Some(agent_name.clone()),
    };

    channel_wecom::token::WECOM_STATE.save_session(&sf);
    channel_wecom::token::WECOM_STATE.load_new_from_dir();

    if let Some(ref pm) = state.channel_manager {
        let pm = pm.lock().await;
        pm.set_sender_route(&name, &agent_name);
        if let Err(e) = pm.start_sender("wecom", &name) {
            warn!(sender_id = %name, error = %e, "start_sender wecom-kf failed (may already be running)");
        }
    }

    info!(%name, %open_kfid, agent = %agent_name, "Bound wecom-kf channel to agent");

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "type": "wecom-kf",
            "name": name,
            "open_kfid": open_kfid,
            "corp_id": corp_id,
            "agent": agent_name,
        })),
    )
        .into_response()
}

/// DELETE /api/agents/{agent}/channels/wecom-kf/{name}
pub async fn unbind_wecom_kf(
    State(state): State<Arc<AppState>>,
    Path((agent, name)): Path<(String, String)>,
) -> impl IntoResponse {
    let agent_name = match resolve_to_name(&agent, &state.kernel.registry) {
        Ok(n) => n,
        Err(e) => return e.into_response(),
    };

    let home = types::config::home_dir();
    let path = home.join("senders").join(&name).join("session.json");
    if !path.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "微信客服 session 不存在"})),
        )
            .into_response();
    }

    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
                .into_response();
        }
    };
    let mut sf: channel_wecom::token::WecomSessionFile = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("解析 session 失败: {e}")})),
            )
                .into_response();
        }
    };

    if sf.mode != "kf" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "该 session 不是微信客服 (mode=kf)"})),
        )
            .into_response();
    }

    if sf.bind_agent.as_deref() != Some(agent_name.as_str()) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "该客服当前绑定的是 {:?}，不是 {agent_name}",
                    sf.bind_agent
                )
            })),
        )
            .into_response();
    }

    sf.bind_agent = None;
    channel_wecom::token::WECOM_STATE.save_session(&sf);

    if let Some(ref pm) = state.channel_manager {
        let pm = pm.lock().await;
        let _ = pm.remove_sender_route(&name);
    }

    info!(%name, agent = %agent_name, "Unbound wecom-kf channel from agent");

    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "name": name, "unbound": true})),
    )
        .into_response()
}

// ─── Router ─────────────────────────────────────────────────────────────────

pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing::{delete, get, post};
    axum::Router::new()
        .route(
            "/api/agents/{agent}/channels",
            get(list_agent_channels),
        )
        .route(
            "/api/agents/{agent}/channels/weixin-oa",
            post(bind_weixin_oa),
        )
        .route(
            "/api/agents/{agent}/channels/weixin-oa/{app_id}",
            delete(unbind_weixin_oa),
        )
        .route(
            "/api/agents/{agent}/channels/wecom-kf",
            post(bind_wecom_kf),
        )
        .route(
            "/api/agents/{agent}/channels/wecom-kf/{name}",
            delete(unbind_wecom_kf),
        )
}
