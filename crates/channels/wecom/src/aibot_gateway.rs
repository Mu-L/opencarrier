//! aibot CLI 网关 — SmartBot 主动推送与读写面。
//!
//! 协议逆向自 WecomTeam/wecom-cli（MIT），2026-08-19 生产 bot 凭证实证：
//!
//! - 鉴权引导：POST `cgi-bin/aibot/cli/get_cli_config`，body 携带
//!   `sha256_hex(secret + bot_id + time + nonce)` 签名 → 返回 Bearer token。
//! - 网关调用：POST `https://qyapi.weixin.qq.com/cli<path>`，请求体为
//!   payload 字符串信封 `{"payload": "<json-string>"}`；响应的
//!   `result`/`results_json` 同为 JSON 字符串（可能多层嵌套），逐层解包。
//! - token 失效（errcode 853004）时用 bot 凭证静默换新并重放一次。
//! - per-service 授权：bot 创建者需在企微「工作台-智能机器人」逐服务授权，
//!   未授权返回 850002，`help_message` 含该 bot 的专属授权链接（自愈种子）。
//! - `sessions/list` 返回的 `chat_id` 由框架加密输出，必须当次取当次用、
//!   原样透传给 `send`（禁历史缓存/用户直供——同 iLink 关系路由教义）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use types::error::{CarrierError, CarrierResult};

/// 鉴权引导端点（bot_id+secret 签名换取 Bearer token）。
const BOOTSTRAP_URL: &str = "https://qyapi.weixin.qq.com/cgi-bin/aibot/cli/get_cli_config";
/// 网关基础 URL，业务路径形如 `/message/aibot/send`。
const GATEWAY_BASE: &str = "https://qyapi.weixin.qq.com/cli";
/// token 失效 errcode —— 用 bot 凭证静默换新重放（同 wecom-cli 行为）。
const ERR_TOKEN_EXPIRED: i64 = 853004;
/// 服务未授权 errcode —— bot 创建者需在企微授权该服务。
const ERR_NO_AUTHORIZATION: i64 = 850002;

/// 网关侧结构化错误：errcode + errmsg（+ 850002 时的授权链接）。
#[derive(Debug)]
pub struct GatewayError {
    pub errcode: i64,
    pub errmsg: String,
    /// 850002 时后端返回的授权指引（含专属授权 URL），原样保留供自愈。
    pub help_message: Option<String>,
}

impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "aibot gateway error {}: {}", self.errcode, self.errmsg)?;
        if self.errcode == ERR_NO_AUTHORIZATION {
            write!(
                f,
                " — bot creator must authorize this service in WeCom (help: {:?})",
                self.help_message.as_deref().unwrap_or("")
            )?;
        }
        Ok(())
    }
}

impl From<GatewayError> for CarrierError {
    fn from(e: GatewayError) -> Self {
        CarrierError::Network(e.to_string())
    }
}

/// Bearer token 缓存（bot_id → token）。失效不主动过期，靠 853004 换新。
static TOKENS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// nonce 计数器——网关只要求 nonce 唯一，不要求密码学随机。
static NONCE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 签名算法：`sha256_hex(secret + bot_id + time + nonce)`（小写零填充 hex）。
fn sign(secret: &str, bot_id: &str, time: u64, nonce: &str) -> String {
    let input = format!("{secret}{bot_id}{time}{nonce}");
    let digest = Sha256::digest(input.as_bytes());
    hex::encode(digest)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// 生成网关形制的 nonce：`cli_<ms>_<8hex>`（同 wecom-cli `gen_req_id`）。
fn gen_nonce() -> String {
    let n = NONCE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("cli_{}_{n:08x}", now_millis())
}

/// `X-WeCom-Cli-Info` 头（网关遥测；字段与 2026-08-19 实证通过的值一致）。
fn cli_info_header() -> String {
    serde_json::json!({
        "platform": "linux/x86_64",
        "build_time": "2026-08-19T00:00:00Z",
        "commit_id": "opencarrier",
        "version": "1.1.0",
        "distribution": "opencarrier",
    })
    .to_string()
}

/// 用 bot 凭证签名换取 Bearer token。
async fn bootstrap_token(http: &Client, bot_id: &str, secret: &str) -> CarrierResult<String> {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let nonce = gen_nonce();
    let body = serde_json::json!({
        "bot_id": bot_id,
        "time": time,
        "nonce": nonce,
        "signature": sign(secret, bot_id, time, &nonce),
        "bind_source": 2, // Qrcode
    });

    let resp: serde_json::Value = http
        .post(BOOTSTRAP_URL)
        .header("X-WeCom-Cli-Info", cli_info_header())
        .json(&body)
        .send()
        .await
        .map_err(|e| CarrierError::Network(format!("aibot bootstrap request: {e}")))?
        .json()
        .await
        .map_err(|e| CarrierError::Serialization(format!("aibot bootstrap parse: {e}")))?;

    let errcode = resp["errcode"].as_i64().unwrap_or(0);
    if errcode != 0 {
        return Err(CarrierError::Network(format!(
            "aibot bootstrap error {errcode}: {}",
            resp["errmsg"].as_str().unwrap_or("unknown")
        )));
    }
    resp["token"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| CarrierError::Serialization("aibot bootstrap: no token".into()))
}

fn cache_token(bot_id: &str, token: &str) {
    let map = TOKENS.get_or_init(|| Mutex::new(HashMap::new()));
    map.lock().unwrap_or_else(|e| e.into_inner())
        .insert(bot_id.to_string(), token.to_string());
}

fn cached_token(bot_id: &str) -> Option<String> {
    let map = TOKENS.get_or_init(|| Mutex::new(HashMap::new()));
    map.lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(bot_id)
        .cloned()
}

/// 逐层解包网关响应：`{errcode, result|results_json}` → 业务 JSON。
///
/// 成功时外层 errcode==0 且 `result`/`results_json` 为 JSON 字符串（可能
/// 再嵌一层）；错误时外层直接平铺 `{errcode, errmsg, help_message}`。
fn unwrap_gateway_value(mut v: serde_json::Value) -> Result<serde_json::Value, GatewayError> {
    loop {
        if !v.is_object() {
            return Ok(v);
        }
        let errcode = v["errcode"].as_i64().unwrap_or(0);
        if errcode != 0 {
            return Err(GatewayError {
                errcode,
                errmsg: v["errmsg"].as_str().unwrap_or("unknown").to_string(),
                help_message: v["help_message"].as_str().map(|s| s.to_string()),
            });
        }
        let next = if let Some(s) = v.get("result").and_then(|x| x.as_str()) {
            serde_json::from_str::<serde_json::Value>(s)
                .map_err(|e| GatewayError {
                    errcode: 0,
                    errmsg: format!("result unwrap parse: {e}"),
                    help_message: None,
                })?
        } else if let Some(s) = v.get("results_json").and_then(|x| x.as_str()) {
            serde_json::from_str::<serde_json::Value>(s).map_err(|e| GatewayError {
                errcode: 0,
                errmsg: format!("results_json unwrap parse: {e}"),
                help_message: None,
            })?
        } else {
            // 没有字符串包装层了——去掉信封字段后即业务结果
            if let Some(obj) = v.as_object_mut() {
                obj.remove("errcode");
                obj.remove("errmsg");
            }
            return Ok(v);
        };
        v = next;
    }
}

/// 带当前 token 发一次网关调用（不处理换新）。
async fn call_with_token(
    http: &Client,
    token: &str,
    path: &str,
    payload: &serde_json::Value,
) -> Result<serde_json::Value, GatewayError> {
    let body = serde_json::json!({ "payload": payload.to_string() });
    let resp: serde_json::Value = http
        .post(format!("{GATEWAY_BASE}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .header("X-WeCom-Cli-Info", cli_info_header())
        .json(&body)
        .send()
        .await
        .map_err(|e| GatewayError {
            errcode: 0,
            errmsg: format!("network: {e}"),
            help_message: None,
        })?
        .json()
        .await
        .map_err(|e| GatewayError {
            errcode: 0,
            errmsg: format!("parse: {e}"),
            help_message: None,
        })?;
    unwrap_gateway_value(resp)
}

/// 网关调用（token 缓存 + 853004 静默换新重放一次）。
pub async fn gateway_call(
    http: &Client,
    bot_id: &str,
    secret: &str,
    path: &str,
    payload: &serde_json::Value,
) -> CarrierResult<serde_json::Value> {
    let token = match cached_token(bot_id) {
        Some(t) => t,
        None => {
            let t = bootstrap_token(http, bot_id, secret).await?;
            cache_token(bot_id, &t);
            t
        }
    };
    match call_with_token(http, &token, path, payload).await {
        Ok(v) => Ok(v),
        Err(e) if e.errcode == ERR_TOKEN_EXPIRED => {
            warn!(bot_id, "aibot token expired, re-bootstrapping");
            let t = bootstrap_token(http, bot_id, secret).await?;
            cache_token(bot_id, &t);
            call_with_token(http, &t, path, payload).await.map_err(Into::into)
        }
        Err(e) => {
            debug!(bot_id, path, errcode = e.errcode, "aibot gateway error");
            Err(e.into())
        }
    }
}

// ---------------------------------------------------------------------------
// 会话与消息（message 服务）
// ---------------------------------------------------------------------------

/// `sessions/list` 返回的会话项（`chat_id` 框架加密，当次取当次用）。
#[derive(Debug, Clone, Deserialize)]
pub struct AibotSession {
    pub chat_id: String,
    #[serde(default)]
    pub chat_name: String,
    #[serde(default)]
    pub chat_type: String,
    #[serde(default)]
    pub last_msg_time: String,
}

/// 最近会话列表（≤20，按最后消息时间倒序；关系门控的可见域）。
pub async fn list_sessions(
    http: &Client,
    bot_id: &str,
    secret: &str,
) -> CarrierResult<Vec<AibotSession>> {
    let v = gateway_call(http, bot_id, secret, "/message/aibot/sessions/list", &serde_json::json!({})).await?;
    #[derive(Deserialize)]
    struct Rsp {
        #[serde(default)]
        sessions: Vec<AibotSession>,
    }
    let rsp: Rsp = serde_json::from_value(v)
        .map_err(|e| CarrierError::Serialization(format!("sessions list parse: {e}")))?;
    Ok(rsp.sessions)
}

/// 在最近会话中按 user_id 定位单聊 chat_id（关系路由；找不到即无关系）。
///
/// 单聊的 `chat_id` 实证等于成员 userid，但以当次 `sessions/list` 返回的
/// 值为准（框架加密语义，不自行构造）。
pub async fn resolve_single_chat(
    http: &Client,
    bot_id: &str,
    secret: &str,
    user_id: &str,
) -> CarrierResult<String> {
    let sessions = list_sessions(http, bot_id, secret).await?;
    sessions
        .iter()
        .find(|s| s.chat_type == "single" && s.chat_id == user_id)
        .map(|s| s.chat_id.clone())
        .ok_or_else(|| {
            CarrierError::InvalidInput(format!(
                "recipient {user_id} not in bot {bot_id}'s recent sessions \
                 (relationship required; SmartBot push is best-effort)"
            ))
        })
}

/// 以机器人身份向会话发送 markdown（普通文本也走 markdown，同官方 skill 约定）。
pub async fn send_markdown(
    http: &Client,
    bot_id: &str,
    secret: &str,
    chat_id: &str,
    content: &str,
) -> CarrierResult<()> {
    let payload = serde_json::json!({
        "chat_id": chat_id,
        "msg_type": "markdown",
        "markdown": { "content": content },
    });
    gateway_call(http, bot_id, secret, "/message/aibot/send", &payload).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 签名与 wecom-cli 参考实现（%02x 小写零填充 sha256）一致。
    #[test]
    fn sign_matches_reference_algorithm() {
        // sha256("secretbot11000cli_1_00000001") 的固定值——由算法定义直接
        // 计算（与实现无共享代码路径，防止实现漂移）。
        let expected = {
            let input = "secretbot11000cli_1_00000001";
            let d = Sha256::digest(input.as_bytes());
            hex::encode(d)
        };
        assert_eq!(sign("secret", "bot1", 1000, "cli_1_00000001"), expected);
        // 不同 nonce → 不同签名
        assert_ne!(sign("s", "b", 1, "n1"), sign("s", "b", 1, "n2"));
    }

    #[test]
    fn nonce_is_unique_and_prefixed() {
        let a = gen_nonce();
        let b = gen_nonce();
        assert!(a.starts_with("cli_"));
        assert_ne!(a, b);
    }

    /// 成功响应：多层 result/results_json 字符串解包后得到业务 JSON。
    #[test]
    fn unwrap_resolves_nested_string_envelopes() {
        let inner = serde_json::json!({ "success": true });
        let mid = serde_json::json!({ "errcode": 0, "result": inner.to_string() });
        let outer = serde_json::json!({ "errcode": 0, "results_json": mid.to_string() });
        let v = unwrap_gateway_value(outer).expect("unwrap ok");
        assert_eq!(v["success"], serde_json::json!(true));
    }

    /// 错误响应：外层平铺 errcode/errmsg/help_message。
    #[test]
    fn unwrap_surfaces_gateway_error_with_help() {
        let outer = serde_json::json!({
            "errcode": 850002,
            "errmsg": "no authorization",
            "help_message": "授权链接",
        });
        let e = unwrap_gateway_value(outer).expect_err("must fail");
        assert_eq!(e.errcode, ERR_NO_AUTHORIZATION);
        assert_eq!(e.help_message.as_deref(), Some("授权链接"));
        assert!(e.to_string().contains("authorize this service"));
    }

    /// token 失效错误可被判别（触发换新重放的路径分派）。
    #[test]
    fn token_expired_error_code_is_recognized() {
        let outer = serde_json::json!({ "errcode": 853004, "errmsg": "token expired" });
        let e = unwrap_gateway_value(outer).expect_err("must fail");
        assert_eq!(e.errcode, ERR_TOKEN_EXPIRED);
    }

    /// 无字符串包装层的响应（如 bootstrap 形状）去掉信封字段后透传。
    #[test]
    fn unwrap_passthrough_when_no_envelope() {
        let outer = serde_json::json!({ "errcode": 0, "token": "tok" });
        let v = unwrap_gateway_value(outer).expect("unwrap ok");
        assert_eq!(v["token"], serde_json::json!("tok"));
        assert!(v.get("errcode").is_none());
    }
}
