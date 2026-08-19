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

/// (bot_id, ws_userid) → 网关 chat_id 的学习缓存。
///
/// SmartBot 的双 id 空间：WS 回调的 `from.userid` 是同企业成员 userid
/// （如 `XiaTianTian`），网关 send 的 `chat_id` 是全局成员 id（`wo…`，
/// 与 sessions list / whoami 同域）。跨企业用户两域恰好一致（外部联系人
/// 的 ws userid 就是全局 id），同企业用户必须靠入站时刻的 sessions
/// last_msg_time 秒级对齐来学习映射。
///
/// 映射持久化在 `senders/<bot_id>/chat_ids.json`（id 稳定，学到即长期
/// 有效）；内存缓存只是热路径。重启不丢——cron 推送在任务创建与触发
/// 之间常隔着部署重启。
static CHAT_IDS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();

/// 学习映射用的共享 HTTP 客户端（smartbot WS 回调路径无 BotEntry.http）。
static LEARN_HTTP: std::sync::LazyLock<Client> = std::sync::LazyLock::new(Client::new);

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
/// 解析顺序：① 内存缓存；② `senders/<bot_id>/chat_ids.json` 持久映射
/// （入站时刻对齐所学，同企业成员）；③ 单聊 `chat_id == user_id` 精确
/// 匹配（跨企业用户两域一致）；④ 失败。
pub async fn resolve_single_chat(
    http: &Client,
    bot_id: &str,
    secret: &str,
    user_id: &str,
) -> CarrierResult<String> {
    let key = format!("{bot_id}:{user_id}");
    if let Some(chat_id) = CHAT_IDS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        return Ok(chat_id);
    }
    if let Some(chat_id) = load_persisted_chat_id(bot_id, user_id) {
        CHAT_IDS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, chat_id.clone());
        return Ok(chat_id);
    }
    let sessions = list_sessions(http, bot_id, secret).await?;
    sessions
        .iter()
        .find(|s| s.chat_type == "single" && s.chat_id == user_id)
        .map(|s| s.chat_id.clone())
        .ok_or_else(|| {
            CarrierError::InvalidInput(format!(
                "recipient {user_id} not resolvable on bot {bot_id}: no learned chat_id \
                 mapping and no exact single-chat match (same-corp member ids need an \
                 inbound message to learn the gateway chat_id; SmartBot push is \
                 relationship-gated, best-effort)"
            ))
        })
}

/// `senders/<bot_id>/chat_ids.json` 路径。
fn chat_id_map_path(bot_id: &str) -> std::path::PathBuf {
    types::config::home_dir()
        .join("senders")
        .join(bot_id)
        .join("chat_ids.json")
}

/// 读取持久化的 ws_userid → chat_id 映射（缺失/损坏返回 None，不报错）。
fn load_persisted_chat_id(bot_id: &str, user_id: &str) -> Option<String> {
    let map: HashMap<String, String> =
        serde_json::from_str(&std::fs::read_to_string(chat_id_map_path(bot_id)).ok()?).ok()?;
    map.get(user_id).cloned()
}

/// 追加写持久映射（load-modify-write；调用方已按 bot 串行化）。
fn persist_chat_id(bot_id: &str, user_id: &str, chat_id: &str) {
    let path = chat_id_map_path(bot_id);
    let mut map: HashMap<String, String> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    map.insert(user_id.to_string(), chat_id.to_string());
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        if let Err(e) = std::fs::write(&path, json) {
            warn!(bot_id, user_id, error = %e, "chat_id map persist failed");
        }
    }
}

// ---------------------------------------------------------------------------
// 双 id 空间桥接：入站即学习 (bot, ws_userid) → 网关 chat_id
// ---------------------------------------------------------------------------

/// 公历 → 天数（days_from_civil；锚点 1970-01-01 = 0）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// 解析网关时间串 `YYYY-MM-DD HH:MM:SS`（中国标准时间）为「CST 视作
/// UTC 的朴素纪元秒」，与 [`now_cst_unix`] 同基准，仅用于容差比较。
fn parse_cst_time(s: &str) -> Option<i64> {
    let mut it = s.split(|c: char| !c.is_ascii_digit());
    let y = it.next()?.parse::<i64>().ok()?;
    let mo = it.next()?.parse::<u32>().ok()?;
    let d = it.next()?.parse::<u32>().ok()?;
    let h = it.next().unwrap_or("0").parse::<i64>().ok()?;
    let mi = it.next().unwrap_or("0").parse::<i64>().ok()?;
    let sec = it.next().unwrap_or("0").parse::<i64>().ok()?;
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// 当前中国标准时间的「CST 视作 UTC 的朴素纪元秒」（UTC+8 无夏令时）。
fn now_cst_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        + 8 * 3600
}

/// 入站时刻学习映射：以刚到达的消息为准，在 sessions list 中找
/// `last_msg_time` 与当前时刻秒级对齐的单聊。唯一命中才缓存——
/// 零命中或歧义（多用户同秒并发）都跳过，下次入站重试。
pub async fn learn_chat_id(http: &Client, bot_id: &str, secret: &str, user_id: &str) {
    let key = format!("{bot_id}:{user_id}");
    let map = CHAT_IDS.get_or_init(|| Mutex::new(HashMap::new()));
    if map
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&key)
    {
        return;
    }
    let sessions = match list_sessions(http, bot_id, secret).await {
        Ok(s) => s,
        Err(e) => {
            debug!(bot_id, user_id, error = %e, "chat_id learn: sessions list failed");
            return;
        }
    };
    let now = now_cst_unix();
    let tol = 3i64;
    let hits: Vec<&AibotSession> = sessions
        .iter()
        .filter(|s| {
            s.chat_type == "single"
                && parse_cst_time(&s.last_msg_time)
                    .map(|t| (t - now).abs() <= tol)
                    .unwrap_or(false)
        })
        .collect();
    if let [only] = hits.as_slice() {
        map.lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, only.chat_id.clone());
        persist_chat_id(bot_id, user_id, &only.chat_id);
        debug!(bot_id, user_id, chat_id = %only.chat_id, "learned gateway chat_id");
    }
}

/// 学习入口的便捷包装（用模块级 HTTP 客户端，供 WS 回调路径调用）。
pub async fn learn_chat_id_shared(bot_id: &str, secret: &str, user_id: &str) {
    learn_chat_id(&LEARN_HTTP, bot_id, secret, user_id).await;
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

    /// days_from_civil 锚点：1970-01-01 = 0（Unix 纪元）。
    #[test]
    fn days_from_civil_epoch_anchor() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        // 2026-08-19 = 20684（与 python datetime.date(2026,8,19).toordinal()
        // - date(1970,1,1).toordinal() 一致）
        assert_eq!(days_from_civil(2026, 8, 19), 20684);
    }

    /// CST 时间串解析与 now_cst_unix 同基准（朴素纪元）。
    #[test]
    fn parse_cst_time_matches_now_basis() {
        // Unix 0 = 1970-01-01 08:00:00 CST → 朴素基准 28800
        assert_eq!(parse_cst_time("1970-01-01 08:00:00"), Some(28_800));
        assert_eq!(parse_cst_time("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_cst_time("2026-08-19 20:54:49"), Some(20_684 * 86_400 + 20 * 3600 + 54 * 60 + 49));
        assert_eq!(parse_cst_time("garbage"), None);
    }

    /// 容差匹配语义：±3 秒内算命中（由 learn_chat_id 的调用方语义保证）。
    #[test]
    fn tolerance_window_semantics() {
        let tol = 3i64;
        let now = parse_cst_time("2026-08-19 20:54:49").unwrap();
        let in_range = parse_cst_time("2026-08-19 20:54:52").unwrap();
        let out_of_range = parse_cst_time("2026-08-19 20:54:53").unwrap();
        assert!((now - in_range).abs() <= tol);
        assert!((now - out_of_range).abs() > tol);
    }
}
