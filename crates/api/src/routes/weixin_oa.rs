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
use std::sync::Arc;

use channel_weixin_oa::api::check_sign;
use channel_weixin_oa::{build_plugin_message, parse_xml_message, ProxyMessage};

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
    if !session.token.is_empty() {
        if let (Some(sig), Some(ts), Some(nc)) = (params.signature.as_ref(), params.timestamp.as_ref(), params.nonce.as_ref()) {
            if !check_sign(&session.token, ts, nc, sig) {
                tracing::warn!(%app_id, "weixin-oa callback: signature mismatch");
                return (StatusCode::FORBIDDEN, "signature mismatch".to_string());
            }
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
    let plugin_msg = build_plugin_message(&msg, &app_id);

    tracing::info!(
        %app_id,
        openid = %from_user,
        msg_type = %msg.msg_type,
        "weixin-oa: received inbound message"
    );

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
