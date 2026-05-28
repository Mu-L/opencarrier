//! Sender management API — manage channel senders and device auth flows.
//!
//! Senders are channel endpoints (WeCom bot_id, Feishu app_id, DingTalk app_key, etc.)
//! stored in `~/.opencarrier/senders/{sender_id}/config.json`. Routes are managed
//! by `SenderRouter` which maps sender_id → agent_id.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::routes::state::AppState;

// ---------------------------------------------------------------------------
// POST /api/senders/wecom/smartbot/generate — step 1: get auth URL
// ---------------------------------------------------------------------------

pub async fn wecom_smartbot_generate(
    State(state): State<Arc<AppState>>,
    Json(req_body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_name = req_body
        .get("agent_name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let http = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = "https://work.weixin.qq.com/ai/qc/generate?source=wecom_cli_external&plat=1";

    match http.get(url).send().await {
        Ok(resp) => match resp.text().await {
            Ok(resp_body) => match serde_json::from_str::<serde_json::Value>(&resp_body) {
                Ok(data) => {
                    let inner = data.get("data").unwrap_or(&data);
                    let scode = inner.get("scode").and_then(|v| v.as_str()).unwrap_or("");
                    if scode.is_empty() {
                        return (
                            StatusCode::BAD_GATEWAY,
                            Json(serde_json::json!({ "error": "WeCom API 返回了空的 scode" })),
                        );
                    }
                    let auth_url = inner
                        .get("auth_url")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| {
                            format!(
                                "https://work.weixin.qq.com/ai/qc/gen?source=wecom_cli_external&scode={scode}"
                            )
                        });
                    // Store agent_name for auto-bind in poll
                    if let Some(ref name) = agent_name {
                        cleanup_wecom_pending();
                        WECOM_PENDING_AGENTS
                            .lock()
                            .unwrap()
                            .insert(scode.to_string(), (name.clone(), std::time::Instant::now()));
                        // Background poll: server proactively checks WeCom result
                        spawn_background_wecom_poll(state, scode.to_string(), name.clone());
                    }
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({
                            "scode": scode,
                            "auth_url": auth_url,
                        })),
                    )
                }
                Err(_) => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({ "error": "无法解析 WeCom API 响应" })),
                ),
            },
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": "无法读取 WeCom API 响应" })),
            ),
        },
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "无法连接 WeCom API" })),
        ),
    }
}

// ---------------------------------------------------------------------------
// GET /api/senders/wecom/smartbot/poll — step 2: poll creation result
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PollQuery {
    scode: String,
}

pub async fn wecom_smartbot_poll(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PollQuery>,
) -> impl IntoResponse {
    if query.scode.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Missing scode parameter" })),
        );
    }

    let http = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let url = format!(
        "https://work.weixin.qq.com/ai/qc/query_result?scode={}",
        query.scode
    );

    match http.get(&url).send().await {
        Ok(resp) => match resp.text().await {
            Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(data) => {
                    let inner = data.get("data").unwrap_or(&data);
                    let status = inner
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let mut result = serde_json::json!({
                        "status": status,
                    });

                    if status == "success" {
                        if let Some(bot_info) = inner.get("bot_info") {
                            let bot_id =
                                bot_info.get("botid").and_then(|v| v.as_str()).unwrap_or("");
                            let secret = bot_info
                                .get("secret")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            result.as_object_mut().unwrap().insert(
                                "bot_id".into(),
                                serde_json::Value::String(bot_id.to_string()),
                            );
                            result.as_object_mut().unwrap().insert(
                                "secret".into(),
                                serde_json::Value::String(secret.to_string()),
                            );
                            // Auto-create bot and bind to agent
                            cleanup_wecom_pending();
                            let agent_name =
                                WECOM_PENDING_AGENTS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() }).remove(&query.scode).map(|(v, _)| v);
                            if let Some(agent_name) = agent_name {
                                if let Err(e) =
                                    register_bot_from_scan(&state, "wecom", &serde_json::json!({
                                        "bot_id": bot_id,
                                        "secret": secret,
                                    }), &agent_name).await
                                {
                                    tracing::warn!(
                                        agent = %agent_name,
                                        error = %e,
                                        "Register WeCom bot from scan failed"
                                    );
                                }
                            }
                        }
                    }

                    (StatusCode::OK, Json(result))
                }
                Err(_) => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({ "error": "无法解析 WeCom API 响应" })),
                ),
            },
            Err(_) => (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({ "error": "无法读取 WeCom API 响应" })),
            ),
        },
        Err(_) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": "无法连接 WeCom API" })),
        ),
    }
}

// ---------------------------------------------------------------------------
// Shared in-memory store for device-auth flows
// ---------------------------------------------------------------------------

#[derive(Clone)]
#[allow(dead_code)]
struct DeviceAuthSession {
    device_code: String,
    auth_url: String,
    expires_at: std::time::Instant,
    platform: String,
    // stored credentials after poll success
    credentials: Option<serde_json::Value>,
    // Reuse the same HTTP client so cookies/session from init→begin→poll are preserved.
    client: reqwest::Client,
    // Auto-bind: agent name to bind after successful auth
    agent_name: Option<String>,
}

static DEVICE_AUTH_SESSIONS: LazyLock<Mutex<HashMap<String, DeviceAuthSession>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_DEVICE_AUTH_SESSIONS: usize = 1000;

fn generate_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn cleanup_expired_sessions() {
    let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
    let now = std::time::Instant::now();
    sessions.retain(|_, v| v.expires_at > now);
}

/// Spawn a background task that polls DingTalk/Feishu for auth result
/// and auto-creates + binds the bot when auth succeeds.
fn spawn_background_device_poll(state: Arc<AppState>, session_id: String) {
    let session = {
        let sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        match sessions.get(&session_id).cloned() {
            Some(s) => s,
            None => return,
        }
    };

    let agent_name = match session.agent_name {
        Some(ref n) if !n.is_empty() => n.clone(),
        _ => return, // no agent to bind, skip background poll
    };

    let platform = session.platform.clone();
    let device_code = session.device_code.clone();
    let client = session.client.clone();
    let expires_at = session.expires_at;
    let sid = session_id.clone();

    tokio::spawn(async move {
        let poll_interval = std::time::Duration::from_secs(3);
        let mut interval = tokio::time::interval(poll_interval);
        interval.tick().await; // first tick is immediate

        loop {
            interval.tick().await;

            if std::time::Instant::now() > expires_at {
                tracing::info!(session_id = %sid, platform = %platform, "Background poll: session expired");
                return;
            }

            // Check if credentials were already stored (client poll got there first)
            {
                let sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
                if let Some(s) = sessions.get(&sid) {
                    if s.credentials.is_some() {
                        tracing::info!(session_id = %sid, "Background poll: already resolved");
                        return;
                    }
                }
            }

            let result = match platform.as_str() {
                "dingtalk" => poll_dingtalk(&client, &device_code).await,
                "feishu" => {
                    let base_url = if session.platform == "lark" {
                        "https://accounts.larksuite.com"
                    } else {
                        "https://accounts.feishu.cn"
                    };
                    poll_feishu(&client, &device_code, base_url).await
                }
                _ => return,
            };

            match result {
                PollResult::Success(creds) => {
                    // Store credentials in session
                    {
                        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
                        if let Some(s) = sessions.get_mut(&sid) {
                            s.credentials = Some(creds.clone());
                        }
                    }

                    tracing::info!(
                        session_id = %sid,
                        platform = %platform,
                        agent = %agent_name,
                        "Background poll: auth succeeded, auto-creating bot"
                    );

                    // Auto-create and bind
                    if let Err(e) =
                        register_bot_from_scan(&state, &platform, &creds, &agent_name).await
                    {
                        tracing::warn!(
                            session_id = %sid,
                            agent = %agent_name,
                            error = %e,
                            "Background poll: auto-create/bind failed"
                        );
                    }
                    return;
                }
                PollResult::Expired => {
                    tracing::info!(session_id = %sid, "Background poll: platform returned expired");
                    return;
                }
                PollResult::Pending => continue,
            }
        }
    });
}

/// Spawn a background task that polls WeCom smartbot creation result.
fn spawn_background_wecom_poll(state: Arc<AppState>, scode: String, agent_name: String) {
    tokio::spawn(async move {
        let poll_interval = std::time::Duration::from_secs(2);
        let mut interval = tokio::time::interval(poll_interval);
        interval.tick().await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);

        loop {
            interval.tick().await;

            if std::time::Instant::now() > deadline {
                tracing::info!(scode = %scode, "WeCom background poll: timed out");
                WECOM_PENDING_AGENTS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() }).remove(&scode);
                return;
            }

            let http = reqwest::Client::builder()
                .cookie_store(true)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new());
            let url = format!(
                "https://work.weixin.qq.com/ai/qc/query_result?scode={}",
                scode
            );

            let resp = match http.get(&url).send().await {
                Ok(r) => r,
                Err(_) => continue,
            };
            let body = match resp.text().await {
                Ok(b) => b,
                Err(_) => continue,
            };
            let data = match serde_json::from_str::<serde_json::Value>(&body) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let inner = data.get("data").unwrap_or(&data);
            let status = inner
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");

            if status == "success" {
                if let Some(bot_info) = inner.get("bot_info") {
                    let bot_id = bot_info.get("botid").and_then(|v| v.as_str()).unwrap_or("");
                    let secret = bot_info
                        .get("secret")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");

                    if !bot_id.is_empty() {
                        tracing::info!(
                            scode = %scode,
                            agent = %agent_name,
                            bot_id = %bot_id,
                            "WeCom background poll: auth succeeded, auto-creating bot"
                        );
                        let creds = serde_json::json!({
                            "bot_id": bot_id,
                            "secret": secret,
                        });
                        if let Err(e) =
                            register_bot_from_scan(&state, "wecom", &creds, &agent_name).await
                        {
                            tracing::warn!(
                                agent = %agent_name,
                                error = %e,
                                "WeCom background poll: auto-create/bind failed"
                            );
                        }
                    }
                }
                WECOM_PENDING_AGENTS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() }).remove(&scode);
                return;
            }

            if status == "expired" || status == "fail" {
                tracing::info!(scode = %scode, status = %status, "WeCom background poll: terminal status");
                WECOM_PENDING_AGENTS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() }).remove(&scode);
                return;
            }
        }
    });
}

enum PollResult {
    Success(serde_json::Value),
    Expired,
    Pending,
}

async fn poll_dingtalk(client: &reqwest::Client, device_code: &str) -> PollResult {
    let res = match client
        .post("https://oapi.dingtalk.com/app/registration/poll")
        .json(&serde_json::json!({"device_code": device_code}))
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(_) => return PollResult::Pending,
        },
        Err(_) => return PollResult::Pending,
    };

    let status = res
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();

    if status == "SUCCESS" {
        let client_id = res.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
        let client_secret = res
            .get("client_secret")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !client_id.is_empty() && !client_secret.is_empty() {
            return PollResult::Success(serde_json::json!({
                "client_id": client_id,
                "client_secret": client_secret,
            }));
        }
    }

    if status == "EXPIRED" || status == "FAIL" {
        return PollResult::Expired;
    }

    PollResult::Pending
}

async fn poll_feishu(client: &reqwest::Client, device_code: &str, base_url: &str) -> PollResult {
    let poll_url = format!("{}/oauth/v1/app/registration", base_url);
    let poll_body = format!("action=poll&device_code={}", device_code);

    let res = match client
        .post(&poll_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(poll_body)
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(_) => return PollResult::Pending,
        },
        Err(_) => return PollResult::Pending,
    };

    let status = res.get("status").and_then(|v| v.as_str()).unwrap_or("");

    let app_id = res.get("app_id").and_then(|v| v.as_str()).unwrap_or("");
    let client_id = res.get("client_id").and_then(|v| v.as_str()).unwrap_or("");
    let app_secret = res.get("app_secret").and_then(|v| v.as_str()).unwrap_or("");
    let client_secret = res
        .get("client_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let id = if !app_id.is_empty() {
        app_id
    } else {
        client_id
    };
    let secret = if !app_secret.is_empty() {
        app_secret
    } else {
        client_secret
    };

    if !id.is_empty() && !secret.is_empty() {
        return PollResult::Success(serde_json::json!({
            "app_id": id,
            "app_secret": secret,
        }));
    }

    if status.eq_ignore_ascii_case("EXPIRED") || status.eq_ignore_ascii_case("FAIL") {
        return PollResult::Expired;
    }

    PollResult::Pending
}

// ---------------------------------------------------------------------------
// Feishu device-auth: POST /api/senders/feishu/device-auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct FeishuDeviceAuthBeginBody {
    #[serde(default)]
    brand: String,
    #[serde(default)]
    agent_name: String,
}

pub async fn feishu_device_auth_begin(
    State(state): State<Arc<AppState>>,
    Json(body): Json<FeishuDeviceAuthBeginBody>,
) -> impl IntoResponse {
    cleanup_expired_sessions();

    let brand = if body.brand.trim().is_empty() {
        "feishu"
    } else {
        &body.brand
    };

    let base_url = if brand == "lark" {
        "https://accounts.larksuite.com"
    } else {
        "https://accounts.feishu.cn"
    };

    let http = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    // Step 1: init
    let init_url = format!("{}/oauth/v1/app/registration", base_url);
    let init_res = match http
        .post(&init_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("action=init")
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": format!("飞书 init 响应解析失败: {}", e) })),
                )
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("飞书 init 请求失败: {}", e) })),
            )
        }
    };

    let nonce = init_res.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
    if nonce.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": "飞书 init 未返回 nonce" })),
        );
    }

    // Step 2: begin
    let begin_body = format!(
        "action=begin&nonce={}&archetype=PersonalAgent&auth_method=client_secret&request_user_info=open_id",
        nonce
    );
    let begin_res = match http
        .post(&init_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(begin_body)
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": format!("飞书 begin 响应解析失败: {}", e) })),
                )
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("飞书 begin 请求失败: {}", e) })),
            )
        }
    };

    let device_code = begin_res
        .get("device_code")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let auth_url = begin_res
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if device_code.is_empty() || auth_url.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(
                serde_json::json!({"error": "飞书 begin 未返回 device_code 或 auth_url", "raw": begin_res }),
            ),
        );
    }

    let session_id = generate_session_id();
    let expires_in = begin_res
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(240u64);

    let agent_name_feishu = if body.agent_name.trim().is_empty() {
        None
    } else {
        Some(body.agent_name.trim().to_string())
    };

    let session = DeviceAuthSession {
        device_code: device_code.to_string(),
        auth_url: auth_url.to_string(),
        expires_at: std::time::Instant::now() + std::time::Duration::from_secs(expires_in),
        platform: "feishu".to_string(),
        credentials: None,
        client: http,
        agent_name: agent_name_feishu,
    };

    {
        cleanup_expired_sessions();
        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        if sessions.len() >= MAX_DEVICE_AUTH_SESSIONS {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Too many pending auth sessions"})),
            );
        }
        sessions.insert(session_id.clone(), session);
    }

    // Background poll: server proactively checks auth result
    spawn_background_device_poll(state, session_id.clone());

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "session_id": session_id,
            "device_code": device_code,
            "auth_url": auth_url,
            "expires_in": expires_in,
        })),
    )
}

#[derive(Deserialize)]
pub struct FeishuDeviceAuthPollQuery {
    session_id: String,
}

pub async fn feishu_device_auth_poll(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FeishuDeviceAuthPollQuery>,
) -> impl IntoResponse {
    cleanup_expired_sessions();

    let session = {
        let sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        match sessions.get(&query.session_id) {
            Some(s) => s.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "会话不存在或已过期" })),
                )
            }
        }
    };

    if let Some(creds) = session.credentials {
        return (StatusCode::OK, Json(creds));
    }

    if std::time::Instant::now() > session.expires_at {
        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        sessions.remove(&query.session_id);
        return (
            StatusCode::OK,
            Json(serde_json::json!({"status": "expired" })),
        );
    }

    let base_url = if session.platform == "lark" {
        "https://accounts.larksuite.com"
    } else {
        "https://accounts.feishu.cn"
    };

    let poll_url = format!("{}/oauth/v1/app/registration", base_url);
    let poll_body = format!("action=poll&device_code={}", session.device_code);

    let poll_res = match session
        .client
        .post(&poll_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(poll_body)
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(_) => {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "pending" })),
                )
            }
        },
        Err(_) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({"status": "pending" })),
            )
        }
    };

    let status = poll_res
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    tracing::info!(
        session_id = %query.session_id,
        raw_status = %status,
        raw_response = %poll_res,
        "Feishu device-auth poll response"
    );

    // Feishu may return credentials directly without a status field, or with status=SUCCESS
    let app_id = poll_res
        .get("app_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let client_id = poll_res
        .get("client_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let app_secret = poll_res
        .get("app_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let client_secret = poll_res
        .get("client_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let id = if !app_id.is_empty() {
        app_id
    } else {
        client_id
    };
    let secret = if !app_secret.is_empty() {
        app_secret
    } else {
        client_secret
    };

    if !id.is_empty() && !secret.is_empty() {
        let result = serde_json::json!({
            "status": "success",
            "app_id": id,
            "app_secret": secret,
        });

        {
            let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
            if let Some(s) = sessions.get_mut(&query.session_id) {
                s.credentials = Some(result.clone());
            }
        }

        // Auto-create bot and bind to agent if agent_name was provided
        if let Some(ref agent_name) = session.agent_name {
            let creds = serde_json::json!({
                "app_id": id,
                "app_secret": secret,
            });
            if let Err(e) = register_bot_from_scan(&state, "feishu", &creds, agent_name).await {
                tracing::warn!(agent = %agent_name, error = %e, "Register Feishu bot from scan failed");
            }
        }

        return (StatusCode::OK, Json(result));
    }

    if status.eq_ignore_ascii_case("SUCCESS") {
        // SUCCESS but missing credentials — surface it for debugging
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "status": "pending",
                "debug": "SUCCESS without app_id/app_secret/client_id/client_secret",
                "raw": poll_res,
            })),
        );
    }

    if status.eq_ignore_ascii_case("EXPIRED") || status.eq_ignore_ascii_case("FAIL") {
        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        sessions.remove(&query.session_id);
        return (
            StatusCode::OK,
            Json(serde_json::json!({"status": "expired" })),
        );
    }

    // Unknown status — return the raw upstream response for debugging
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "pending",
            "raw_feishu": poll_res,
        })),
    )
}

// ---------------------------------------------------------------------------
// DingTalk device-auth: POST /api/senders/dingtalk/device-auth
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct DingtalkDeviceAuthPollQuery {
    session_id: String,
}

pub async fn dingtalk_device_auth_begin(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    cleanup_expired_sessions();

    let http = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let base_url = "https://oapi.dingtalk.com";

    // Step 1: init
    let init_res = match http
        .post(format!("{}/app/registration/init", base_url))
        .json(&serde_json::json!({"source": "carrier"}))
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": format!("钉钉 init 响应解析失败: {}", e) })),
                )
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("钉钉 init 请求失败: {}", e) })),
            )
        }
    };

    let nonce = init_res.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
    if nonce.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({"error": "钉钉 init 未返回 nonce" })),
        );
    }

    // Step 2: begin
    let begin_res = match http
        .post(format!("{}/app/registration/begin", base_url))
        .json(&serde_json::json!({"nonce": nonce}))
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({"error": format!("钉钉 begin 响应解析失败: {}", e) })),
                )
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": format!("钉钉 begin 请求失败: {}", e) })),
            )
        }
    };

    let device_code = begin_res
        .get("device_code")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let auth_url = begin_res
        .get("verification_uri_complete")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if device_code.is_empty() || auth_url.is_empty() {
        return (
            StatusCode::BAD_GATEWAY,
            Json(
                serde_json::json!({"error": "钉钉 begin 未返回 device_code 或 auth_url", "raw": begin_res }),
            ),
        );
    }

    let session_id = generate_session_id();
    let expires_in = begin_res
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(7200u64);

    let agent_name = body
        .get("agent_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let session = DeviceAuthSession {
        device_code: device_code.to_string(),
        auth_url: auth_url.to_string(),
        expires_at: std::time::Instant::now() + std::time::Duration::from_secs(expires_in),
        platform: "dingtalk".to_string(),
        credentials: None,
        client: http,
        agent_name,
    };

    {
        cleanup_expired_sessions();
        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        if sessions.len() >= MAX_DEVICE_AUTH_SESSIONS {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"error": "Too many pending auth sessions"})),
            );
        }
        sessions.insert(session_id.clone(), session);
    }

    // Background poll: server proactively checks auth result
    spawn_background_device_poll(state, session_id.clone());

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "session_id": session_id,
            "device_code": device_code,
            "auth_url": auth_url,
            "expires_in": expires_in,
        })),
    )
}

pub async fn dingtalk_device_auth_poll(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DingtalkDeviceAuthPollQuery>,
) -> impl IntoResponse {
    cleanup_expired_sessions();

    let session = {
        let sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        match sessions.get(&query.session_id) {
            Some(s) => s.clone(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "会话不存在或已过期" })),
                )
            }
        }
    };

    if let Some(creds) = session.credentials {
        return (StatusCode::OK, Json(creds));
    }

    if std::time::Instant::now() > session.expires_at {
        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        sessions.remove(&query.session_id);
        return (
            StatusCode::OK,
            Json(serde_json::json!({"status": "expired" })),
        );
    }

    let poll_res = match session
        .client
        .post("https://oapi.dingtalk.com/app/registration/poll")
        .json(&serde_json::json!({"device_code": session.device_code}))
        .send()
        .await
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(_) => {
                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "pending" })),
                )
            }
        },
        Err(_) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({"status": "pending" })),
            )
        }
    };

    let status = poll_res
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();

    if status == "SUCCESS" {
        let client_id = poll_res
            .get("client_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let client_secret = poll_res
            .get("client_secret")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if !client_id.is_empty() && !client_secret.is_empty() {
            let result = serde_json::json!({
                "status": "success",
                "client_id": client_id,
                "client_secret": client_secret,
            });

            {
                let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
                if let Some(s) = sessions.get_mut(&query.session_id) {
                    s.credentials = Some(result.clone());
                }
            }

            // Auto-create bot and bind to agent if agent_name was provided
            if let Some(ref agent_name) = session.agent_name {
                let creds = serde_json::json!({
                    "client_id": client_id,
                    "client_secret": client_secret,
                });
                if let Err(e) =
                    register_bot_from_scan(&state, "dingtalk", &creds, agent_name).await
                {
                    tracing::warn!(agent = %agent_name, error = %e, "Register DingTalk bot from scan failed");
                }
            }

            return (StatusCode::OK, Json(result));
        }
    }

    if status == "EXPIRED" || status == "FAIL" {
        let mut sessions = DEVICE_AUTH_SESSIONS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
        sessions.remove(&query.session_id);
        return (
            StatusCode::OK,
            Json(serde_json::json!({"status": "expired" })),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "pending" })),
    )
}

// ---------------------------------------------------------------------------
// Sender management — routes backed by SenderRouter (senders/ directory)
// ---------------------------------------------------------------------------

/// GET /api/senders — list all senders and their agent bindings.
pub async fn list_senders(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let Some(ref pm) = state.channel_manager else {
        return (StatusCode::OK, Json(serde_json::json!({"senders": [], "total": 0})));
    };
    let pm = pm.lock().await;
    let routes = pm.list_sender_routes();
    let senders: Vec<serde_json::Value> = routes
        .iter()
        .map(|(sender_id, agent_id)| {
            serde_json::json!({
                "sender_id": sender_id,
                "agent_id": agent_id,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "senders": senders,
            "total": senders.len(),
        })),
    )
}

/// GET /api/senders/{id} — get a single sender's routing info.
pub async fn get_sender(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref pm) = state.channel_manager else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "发送者不存在"})),
        );
    };
    let pm = pm.lock().await;
    match pm.get_sender_route(&id) {
        Some(agent_id) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "sender_id": id,
                "agent_id": agent_id,
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "发送者不存在"})),
        ),
    }
}

/// POST /api/senders — create a sender and bind it to an agent.
pub async fn create_sender(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let sender_id = match body.get("sender_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 sender_id"})),
            )
        }
    };
    let agent_id_raw = match body.get("agent_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 agent_id"})),
            )
        }
    };
    let agent_id = crate::routes::common::resolve_to_name(&agent_id_raw, &state.kernel.registry)
        .unwrap_or_else(|_| agent_id_raw.clone());

    let Some(ref pm) = state.channel_manager else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Channel manager not available"})),
        );
    };
    let pm = pm.lock().await;

    // Check if already exists
    if pm.get_sender_route(&sender_id).is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "发送者已存在"})),
        );
    }

    pm.set_sender_route(&sender_id, &agent_id);
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "sender_id": sender_id,
            "agent_id": agent_id,
        })),
    )
}

/// DELETE /api/senders/{id} — remove a sender and its route.
pub async fn delete_sender(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref pm) = state.channel_manager else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "发送者不存在"})),
        );
    };
    let pm = pm.lock().await;
    match pm.remove_sender_route(&id) {
        Some(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "deleted", "sender_id": id})),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "发送者不存在"})),
        ),
    }
}

/// PUT /api/senders/{id}/bind — bind a sender to an agent.
pub async fn bind_sender(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let agent_id_raw = match body.get("agent_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 agent_id"})),
            )
        }
    };
    let agent_id = crate::routes::common::resolve_to_name(&agent_id_raw, &state.kernel.registry)
        .unwrap_or_else(|_| agent_id_raw.clone());

    let Some(ref pm) = state.channel_manager else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Channel manager not available"})),
        );
    };
    let pm = pm.lock().await;
    pm.set_sender_route(&id, &agent_id);
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "sender_id": id,
            "agent_id": agent_id,
        })),
    )
}

/// DELETE /api/senders/{id}/bind — unbind a sender (remove its route).
pub async fn unbind_sender(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref pm) = state.channel_manager else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "发送者不存在"})),
        );
    };
    let pm = pm.lock().await;
    match pm.remove_sender_route(&id) {
        Some(_) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "unbound", "sender_id": id})),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "发送者不存在"})),
        ),
    }
}

/// POST /api/senders/{id}/send — send a message through a sender's channel.
pub async fn sender_send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let user_id = match body.get("user_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 user_id"})),
            )
        }
    };
    let text = match body.get("text").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "缺少 text"})),
            )
        }
    };

    let Some(ref pm) = state.channel_manager else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Channel manager not available"})),
        );
    };
    let pm = pm.lock().await;
    match pm.channel_send_by_bot(&id, &user_id, &text) {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"status": "sent"})),
        ),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

// WeCom scode → agent_name mapping for auto-bind
static WECOM_PENDING_AGENTS: LazyLock<Mutex<HashMap<String, (String, std::time::Instant)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cleanup_wecom_pending() {
    const WECOM_TTL_SECS: u64 = 600; // 10 minutes
    let mut pending = WECOM_PENDING_AGENTS.lock().unwrap_or_else(|e| { tracing::warn!("Mutex poisoned, recovering"); e.into_inner() });
    let now = std::time::Instant::now();
    pending.retain(|_, (_, created_at)| now.duration_since(*created_at).as_secs() < WECOM_TTL_SECS);
}

/// Register a bot from a successful scan/auth flow.
///
/// Writes a session file to `senders/{sender_id}/session.json`, sets the sender
/// route, and immediately starts the channel connection via `start_sender()`.
async fn register_bot_from_scan(
    state: &Arc<AppState>,
    platform: &str,
    credentials: &serde_json::Value,
    agent_name: &str,
) -> Result<String, String> {
    // Resolve agent_name: accept name or UUID, store as name
    let agent_ref = crate::routes::common::resolve_to_name(agent_name, &state.kernel.registry)
        .map_err(|(_, json)| {
            let msg = json.0.get("error").and_then(|v| v.as_str()).unwrap_or("Agent not found");
            msg.to_string()
        })?;

    // Write session file to senders/{sender_id}/session.json + set route
    match platform {
        "wecom" => {
            let wecom_bot_id = credentials
                .get("bot_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let secret = credentials
                .get("secret")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let sf = channel_wecom::token::WecomSessionFile {
                channel: "wecom".to_string(),
                sender_key: "bot_id".to_string(),
                name: agent_name.to_string(),
                mode: "smartbot".to_string(),
                bot_id: Some(wecom_bot_id.clone()),
                agent_id: None,
                corp_id: None,
                open_kfid: None,
                secret: Some(secret),
                secret_env: None,
                webhook_port: None,
                encoding_aes_key: None,
                callback_token: None,
                mcp_bot_id: None,
                mcp_bot_secret: None,
                bind_agent: Some(agent_ref.clone()),
            };
            channel_wecom::token::WECOM_STATE.save_session(&sf);

            // Set sender route — sender_id = wecom bot_id
            if let Some(ref pm) = state.channel_manager {
                let pm = pm.lock().await;
                pm.set_sender_route(&wecom_bot_id, &agent_ref);
                // Immediately start the new bot's connection
                if let Err(e) = pm.start_sender("wecom", &wecom_bot_id) {
                    tracing::warn!(sender_id = %wecom_bot_id, error = %e, "start_sender failed for wecom");
                }
            }

            tracing::info!(
                platform = "wecom",
                sender_id = %wecom_bot_id,
                agent = %agent_ref,
                "Registered WeCom bot from scan"
            );
            Ok(wecom_bot_id)
        }
        "feishu" => {
            let app_id = credentials
                .get("app_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let app_secret = credentials
                .get("app_secret")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let sf = channel_feishu::models::FeishuSessionFile {
                channel: "feishu".to_string(),
                sender_key: "app_id".to_string(),
                name: agent_name.to_string(),
                app_id: app_id.clone(),
                app_secret: Some(app_secret),
                secret_env: None,
                brand: "feishu".to_string(),
                bind_agent: Some(agent_ref.clone()),
            };
            channel_feishu::FEISHU_STATE.save_session(&sf);

            // Set sender route using Feishu app_id
            if let Some(ref pm) = state.channel_manager {
                let pm = pm.lock().await;
                pm.set_sender_route(&app_id, &agent_ref);
                // Immediately start the new bot's connection
                if let Err(e) = pm.start_sender("feishu", &app_id) {
                    tracing::warn!(sender_id = %app_id, error = %e, "start_sender failed for feishu");
                }
            }

            tracing::info!(
                platform = "feishu",
                sender_id = %app_id,
                agent = %agent_ref,
                "Registered Feishu bot from scan"
            );
            Ok(app_id)
        }
        "dingtalk" => {
            let app_key = credentials
                .get("client_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let app_secret = credentials
                .get("client_secret")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let sf = channel_dingtalk::models::DingTalkSessionFile {
                channel: "dingtalk".to_string(),
                sender_key: "app_key".to_string(),
                name: agent_name.to_string(),
                app_key: app_key.clone(),
                app_secret: Some(app_secret),
                secret_env: None,
                corp_id: None,
                bind_agent: Some(agent_ref.clone()),
            };
            channel_dingtalk::DINGTALK_STATE.save_session(&sf);

            // Set sender route using DingTalk app_key
            if let Some(ref pm) = state.channel_manager {
                let pm = pm.lock().await;
                pm.set_sender_route(&app_key, &agent_ref);
                // Immediately start the new bot's connection
                if let Err(e) = pm.start_sender("dingtalk", &app_key) {
                    tracing::warn!(sender_id = %app_key, error = %e, "start_sender failed for dingtalk");
                }
            }

            tracing::info!(
                platform = "dingtalk",
                sender_id = %app_key,
                agent = %agent_ref,
                "Registered DingTalk bot from scan"
            );
            Ok(app_key)
        }
        _ => Err(format!("不支持的平台: {platform}")),
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn router() -> axum::Router<std::sync::Arc<AppState>> {
    use axum::routing;
    axum::Router::new()
        // Sender management
        .route("/api/senders", routing::get(list_senders).post(create_sender))
        .route(
            "/api/senders/{id}",
            routing::get(get_sender).delete(delete_sender),
        )
        .route(
            "/api/senders/{id}/bind",
            routing::put(bind_sender).delete(unbind_sender),
        )
        .route("/api/senders/{id}/send", routing::post(sender_send_message))
        // Device auth flows
        .route(
            "/api/senders/wecom/smartbot/generate",
            routing::post(wecom_smartbot_generate),
        )
        .route(
            "/api/senders/wecom/smartbot/poll",
            routing::get(wecom_smartbot_poll),
        )
        .route(
            "/api/senders/feishu/device-auth",
            routing::post(feishu_device_auth_begin),
        )
        .route(
            "/api/senders/feishu/device-auth/poll",
            routing::get(feishu_device_auth_poll),
        )
        .route(
            "/api/senders/dingtalk/device-auth",
            routing::post(dingtalk_device_auth_begin),
        )
        .route(
            "/api/senders/dingtalk/device-auth/poll",
            routing::get(dingtalk_device_auth_poll),
        )
}
