//! WeChat Official Account webhook callback endpoint.
//!
//! WeChat's server POSTs inbound OA messages here as XML (and uses GET with an
//! `echostr` for the initial URL verification). We verify the signature, parse
//! the message, build a PluginMessage, and inject it into the bridge.
//!
//! Configure the WeChat backend to point this exact URL at the OA's server config:
//!   `https://<host>/api/weixin-oa/<app_id>/callback`

use crate::routes::state::AppState;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use channel_weixin_oa::api::{check_sign, get_access_token, get_user_unionid};
use channel_weixin_oa::{build_plugin_message, parse_xml_message, ProxyMessage};

/// Shared HTTP client for WeChat API + 86bus `bind-openid` calls.
/// The whole bind-resolution is bounded by an outer `tokio::time::timeout`,
/// so this client's own timeout is a generous backstop.
static BIND_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_default()
});

struct CachedToken {
    token: String,
    expires_at: Instant,
}

/// Per-app_id access_token cache. WeChat tokens are valid ~2h; refresh shortly
/// before expiry to avoid one token fetch per inbound message.
static OA_TOKENS: LazyLock<Mutex<HashMap<String, CachedToken>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cached access_token for the given app_id/app_secret.
async fn oa_access_token(app_id: &str, app_secret: &str) -> Result<String, String> {
    {
        let cache = OA_TOKENS.lock().unwrap();
        if let Some(t) = cache.get(app_id) {
            if t.expires_at > Instant::now() + Duration::from_secs(120) {
                return Ok(t.token.clone());
            }
        }
    }
    let tok = get_access_token(&BIND_CLIENT, app_id, app_secret).await?;
    let token = tok.access_token.ok_or("no access_token in response")?;
    let expires_in = tok.expires_in.unwrap_or(7200);
    OA_TOKENS.lock().unwrap().insert(
        app_id.to_string(),
        CachedToken {
            token: token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        },
    );
    Ok(token)
}

/// Query params sent by WeChat on every callback (GET verification + POST messages).
#[derive(serde::Deserialize, Debug)]
pub struct WechatSignParams {
    signature: Option<String>,
    timestamp: Option<String>,
    nonce: Option<String>,
    #[serde(default)]
    echostr: Option<String>,
    // Some intermediate proxies pass openid in the query string.
    #[serde(default)]
    openid: Option<String>,
}

/// Load the session file for an app_id from disk.
///
/// The session.json is the source of truth for the OA token (checkSign secret)
/// and bind_agent. Reading it here avoids coupling the HTTP layer to the
/// channel's in-memory state.
fn load_session(state: &Arc<AppState>, app_id: &str) -> Option<channel_weixin_oa::WeixinOaSessionFile> {
    let path = state
        .kernel
        .config
        .home_dir
        .join("senders")
        .join(app_id)
        .join("session.json");
    let data = std::fs::read_to_string(&path).ok()?;
    let session: channel_weixin_oa::WeixinOaSessionFile = serde_json::from_str(&data).ok()?;
    if session.channel == "weixin-oa" && session.app_id == app_id {
        Some(session)
    } else {
        None
    }
}

/// GET `/api/weixin-oa/{app_id}/callback` — WeChat URL verification.
///
/// WeChat sends signature/timestamp/nonce/echostr; we echo back echostr if the
/// signature is valid. This is the handshake performed when first configuring
/// the server URL in the 公众号后台.
pub async fn weixin_oa_verify(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Query(params): Query<WechatSignParams>,
) -> impl IntoResponse {
    let session = match load_session(&state, &app_id) {
        Some(s) => s,
        None => {
            tracing::warn!(%app_id, "weixin-oa verify: session not found");
            return (StatusCode::NOT_FOUND, "session not found".to_string());
        }
    };

    let (signature, timestamp, nonce, echostr) = match (
        params.signature,
        params.timestamp,
        params.nonce,
        params.echostr,
    ) {
        (Some(a), Some(b), Some(c), Some(d)) => (a, b, c, d),
        _ => {
            return (StatusCode::BAD_REQUEST, "missing signature params".to_string());
        }
    };

    if session.token.is_empty() {
        tracing::warn!(%app_id, "weixin-oa verify: token not configured in session.json");
        return (StatusCode::INTERNAL_SERVER_ERROR, "token not configured".to_string());
    }

    if !check_sign(&session.token, &timestamp, &nonce, &signature) {
        tracing::warn!(%app_id, "weixin-oa verify: signature mismatch");
        return (StatusCode::FORBIDDEN, "signature mismatch".to_string());
    }

    tracing::info!(%app_id, "weixin-oa verify: signature OK");
    (StatusCode::OK, echostr)
}

/// POST `/api/weixin-oa/{app_id}/callback` — inbound WeChat OA message.
///
/// Body is either raw WeChat XML (`<xml><MsgType>...</MsgType></xml>`) or a
/// JSON-wrapped proxy payload. We parse, build a PluginMessage, and inject it
/// into the bridge for routing to the bound agent.
pub async fn weixin_oa_callback(
    State(state): State<Arc<AppState>>,
    Path(app_id): Path<String>,
    Query(params): Query<WechatSignParams>,
    headers: axum::http::HeaderMap,
    body: String,
) -> impl IntoResponse {
    let session = match load_session(&state, &app_id) {
        Some(s) => s,
        None => {
            tracing::warn!(%app_id, "weixin-oa callback: session not found");
            // Still return 200 so WeChat doesn't retry aggressively
            return (StatusCode::OK, "success".to_string());
        }
    };

    // Verify signature (WeChat signs both GET verification and POST messages)
    if session.token.is_empty() {
        tracing::warn!(%app_id, "weixin-oa callback: no token configured, rejecting");
        return (StatusCode::FORBIDDEN, "no token configured".to_string());
    }
    if let (Some(sig), Some(ts), Some(nc)) = (params.signature.as_ref(), params.timestamp.as_ref(), params.nonce.as_ref()) {
        if !check_sign(&session.token, ts, nc, sig) {
            tracing::warn!(%app_id, "weixin-oa callback: signature mismatch");
            return (StatusCode::FORBIDDEN, "signature mismatch".to_string());
        }
    }

    // Parse the message body. Two supported formats:
    //  1. Raw WeChat XML
    //  2. JSON-wrapped proxy payload: {"key": "...", "data": "<xml>...", "openid": "..."}
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let msg = parse_body(&body, &content_type, params.openid.as_deref());

    let msg = match msg {
        Some(m) => m,
        None => {
            tracing::warn!(%app_id, %content_type, "weixin-oa callback: could not parse message");
            return (StatusCode::OK, "success".to_string());
        }
    };

    let from_user = msg.from_user.clone();

    // Drop pure-receipt events (TEMPLATESENDJOBFINISH, unsubscribe, etc.)
    // before they reach the agent — zero token cost.
    if !channel_weixin_oa::needs_reply(&msg) {
        tracing::debug!(
            %app_id,
            openid = %from_user,
            msg_type = %msg.msg_type,
            event = %msg.event,
            "weixin-oa: dropped no-reply event"
        );
        return (StatusCode::OK, "success".to_string());
    }

    let plugin_msg = build_plugin_message(&msg, &app_id);

    tracing::info!(
        %app_id,
        openid = %from_user,
        msg_type = %msg.msg_type,
        "weixin-oa: received inbound message"
    );

    // 86bus bind-openid: notify the backend of this openid_sa → identity mapping
    // (so it can route its own notifications) and cache the returned role for the
    // agent's system prompt. Only fires on the TTL boundary, so most messages add
    // no network cost. Bounded to 2s; failure never blocks the webhook.
    if let Some(ref bind_url) = session.bind_openid_url {
        if !from_user.is_empty()
            && runtime::wechat_identity::needs_refresh(
                &from_user,
                runtime::wechat_identity::DEFAULT_TTL_SECS,
            )
        {
            let app_id_c = app_id.clone();
            let secret = session.app_secret.clone();
            let url_c = bind_url.clone();
            let openid = from_user.clone();
            match tokio::time::timeout(
                Duration::from_millis(2000),
                resolve_and_bind(&url_c, &app_id_c, &secret, &openid),
            )
            .await
            {
                Ok(Ok(role)) => {
                    tracing::info!(
                        %app_id,
                        openid = %from_user,
                        matched = %role,
                        "weixin-oa: bind-openid identity resolved"
                    );
                    runtime::wechat_identity::set(&from_user, &role);
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        %app_id,
                        openid = %from_user,
                        error = %e,
                        "weixin-oa: bind-openid resolve failed (skipping identity cache)"
                    );
                }
                Err(_) => {
                    tracing::warn!(
                        %app_id,
                        openid = %from_user,
                        "weixin-oa: bind-openid resolve timed out (2s)"
                    );
                }
            }
        }
    }

    // Inject into the bridge via the channel manager
    let channel_manager = match state.channel_manager.as_ref() {
        Some(cm) => cm.clone(),
        None => {
            tracing::error!(%app_id, "weixin-oa callback: channel manager not available");
            return (StatusCode::OK, "success".to_string());
        }
    };

    let sender = {
        let cm = channel_manager.lock().await;
        cm.bridge_sender()
    };

    if let Err(e) = sender.send(plugin_msg).await {
        tracing::warn!(%app_id, error = %e, "weixin-oa callback: bridge channel full/closed");
    }

    // WeChat expects a 200 "success" ack (or an XML passive reply). We rely on
    // the customer service message API for actual replies, so a plain ack is fine.
    (StatusCode::OK, "success".to_string())
}

/// Parse an inbound body into an OaMessage, trying XML then JSON proxy formats.
fn parse_body(
    body: &str,
    content_type: &str,
    query_openid: Option<&str>,
) -> Option<channel_weixin_oa::OaMessage> {
    // JSON-wrapped proxy payload
    if content_type.contains("json") || body.trim_start().starts_with('{') {
        if let Ok(proxy) = serde_json::from_str::<ProxyMessage>(body) {
            if let Some(msg) = proxy.to_oa_message() {
                return Some(msg);
            }
        }
    }

    // Raw WeChat XML
    if let Some(msg) = parse_xml_message(body) {
        // Some proxies omit FromUserName but pass openid in the query string
        if msg.from_user.is_empty() {
            if let Some(openid) = query_openid {
                let mut msg = msg;
                msg.from_user = openid.to_string();
                return Some(msg);
            }
        }
        return Some(msg);
    }

    None
}

/// Build a router with all routes for this module.
pub fn router() -> axum::Router<std::sync::Arc<crate::routes::state::AppState>> {
    use axum::routing;
    axum::Router::new().route(
        "/api/weixin-oa/{app_id}/callback",
        routing::get(weixin_oa_verify).post(weixin_oa_callback),
    )
}

/// Resolve the user's unionid (cached per openid, queried at most once) and
/// POST the 86bus `bind-openid` endpoint with `openid_sa` (+ optional unionid).
///
/// Method 1 (openid_sa + unionid) is more accurate; unionid comes from
/// `cgi-bin/user/info`. On a transient unionid-fetch error we fall back to
/// method 2 (openid_sa only) without caching the miss, so the next message
/// retries. Returns the raw `matched` string (`"admin"` / `"carrier_user"` / `""`).
async fn resolve_and_bind(
    url: &str,
    app_id: &str,
    app_secret: &str,
    openid_sa: &str,
) -> Result<String, String> {
    // 1. Resolve unionid (cached per openid — query at most once).
    let unionid: Option<String> = match runtime::wechat_identity::get_unionid(openid_sa) {
        Some(u) if !u.is_empty() => Some(u),
        Some(_) => None, // queried before, no unionid
        None => {
            match oa_access_token(app_id, app_secret).await {
                Ok(token) => match get_user_unionid(&BIND_CLIENT, &token, openid_sa).await {
                    Ok(Some(u)) => {
                        runtime::wechat_identity::set_unionid(openid_sa, &u);
                        Some(u)
                    }
                    Ok(None) => {
                        // User not a follower / no unionid — cache so we don't re-query.
                        runtime::wechat_identity::set_unionid(openid_sa, "");
                        None
                    }
                    Err(e) => {
                        // Transient error — don't cache, fall back to method 2 this time.
                        tracing::warn!(
                            openid = %openid_sa,
                            error = %e,
                            "weixin-oa: unionid fetch failed, falling back to openid-only bind"
                        );
                        None
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        openid = %openid_sa,
                        error = %e,
                        "weixin-oa: access_token fetch failed, falling back to openid-only bind"
                    );
                    None
                }
            }
        }
    };

    // 2. Call bind-openid with openid_sa (+ unionid when available).
    let mut body = serde_json::json!({ "openid_sa": openid_sa });
    if let Some(ref u) = unionid {
        body["unionid"] = serde_json::Value::String(u.clone());
    }
    let resp = BIND_CLIENT
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("bind-openid request failed: {e}"))?;
    let val: serde_json::Value = resp.json().await.map_err(|e| format!("bind-openid parse failed: {e}"))?;
    Ok(val
        .get("matched")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}
