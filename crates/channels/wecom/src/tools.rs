//! WeCom (企业微信客服 kf) plugin tools — send rich messages to the current
//! kf customer (external_userid). Mirrors weixin-oa/tools.rs.
//!
//! context.bot_id = wecom session name (e.g. "86bus-kf"); context.sender_id =
//! the customer's external_userid (wm...). All tools resolve the bot +
//! recipient + access_token + open_kfid from context, so the agent just calls
//! them inside a wecom kf conversation.

use serde_json::Value;
use types::plugin::PluginToolContext;
use types::tool::{PluginToolDef, PluginToolError, ToolProvider};

use crate::token::{self, WECOM_STATE};

/// Run an async send on a dedicated current_thread runtime (tool `execute` is
/// sync, called from spawn_blocking — same pattern as weixin-oa tools.rs).
fn run<F>(f: F) -> Result<String, PluginToolError>
where
    F: std::future::Future<Output = Result<String, PluginToolError>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| PluginToolError::tool(format!("runtime error: {e}")))?;
    rt.block_on(f)
}

/// Resolve `(http, access_token, open_kfid, external_userid)` from the tool
/// context. `session` (DashMap Ref) is dropped here — only owned data leaves,
/// so the returned tuple is `Send`-safe across the awaited HTTP calls.
fn resolve_bot(
    context: &PluginToolContext,
) -> Result<(reqwest::Client, String, String, String), PluginToolError> {
    let external_userid = if context.sender_id.is_empty() {
        return Err(PluginToolError::tool(
            "no sender_id (external_userid) in context — wecom_send_* can only be used inside a wecom kf conversation",
        ));
    } else {
        context.sender_id.clone()
    };
    let bot_id = if context.bot_id.is_empty() {
        return Err(PluginToolError::tool(
            "no bot_id (wecom session name) in context",
        ));
    } else {
        context.bot_id.clone()
    };
    let session = WECOM_STATE
        .get_session_for_send(&bot_id)
        .ok_or_else(|| PluginToolError::tool(format!("no wecom session for bot_id {bot_id}")))?;
    let open_kfid = session
        .entry
        .open_kfid()
        .map(|s| s.to_string())
        .ok_or_else(|| PluginToolError::tool("this wecom session is not Kf mode (no open_kfid)"))?;
    let access_token = session
        .entry
        .get_access_token()
        .map_err(PluginToolError::tool)?;
    let http = session.entry.http.clone();
    Ok((http, access_token, open_kfid, external_userid))
}

/// Resolve a media_id from a pre-uploaded media_id OR a local file_path
/// (uploaded on the fly). media_type = image|voice|video|file.
async fn resolve_media_id(
    http: &reqwest::Client,
    access_token: &str,
    media_type: &str,
    media_id: Option<&str>,
    file_path: Option<&str>,
) -> Result<String, PluginToolError> {
    if let Some(mid) = media_id {
        return Ok(mid.to_string());
    }
    let path = file_path.ok_or_else(|| {
        PluginToolError::tool(format!("must provide media_id or file_path (for {media_type})"))
    })?;
    let resolved = if path.starts_with('/') {
        std::path::PathBuf::from(path)
    } else {
        types::config::home_dir().join(path)
    };
    let bytes = std::fs::read(&resolved)
        .map_err(|e| PluginToolError::tool(format!("failed to read {resolved:?}: {e}")))?;
    let filename = resolved
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(media_type)
        .to_string();
    token::upload_kf_media(http, access_token, media_type, bytes, &filename)
        .await
        .map_err(PluginToolError::tool)
}

/// Shared send for media-type messages (image/voice/video/file). Builds
/// `{msgtype, <msgtype>:{media_id}}` and sends via send_kf_msg.
async fn send_media(
    msgtype: &str,
    media_type: &str,
    args: &Value,
    context: &PluginToolContext,
) -> Result<String, PluginToolError> {
    let (http, access_token, open_kfid, ext) = resolve_bot(context)?;
    let media_id = resolve_media_id(
        &http,
        &access_token,
        media_type,
        args["media_id"].as_str(),
        args["file_path"].as_str(),
    )
    .await?;
    let mut body = serde_json::Map::new();
    body.insert("msgtype".into(), Value::String(msgtype.to_string()));
    let mut inner = serde_json::Map::new();
    inner.insert("media_id".into(), Value::String(media_id.clone()));
    body.insert(msgtype.to_string(), Value::Object(inner));
    token::send_kf_msg(
        &http,
        &access_token,
        &open_kfid,
        &ext,
        Value::Object(body),
    )
    .await
    .map_err(PluginToolError::tool)?;
    Ok(format!("{msgtype} sent to {ext} (media_id={media_id})"))
}

// ---------------------------------------------------------------------------
// image / voice / video / file — four thin tools over send_media
// ---------------------------------------------------------------------------

pub struct WecomSendImageTool;
impl ToolProvider for WecomSendImageTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_image".to_string(),
            description: "发图片给当前企业微信客服(wecom kf)用户(external_userid 自动从上下文取)。通过 kf/send_msg。给 media_id(已上传) 或 file_path(本地上传)。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"media_id":{"type":"string","description":"已上传的 media_id(media/upload 返回,3天有效)"},"file_path":{"type":"string","description":"本地图片路径,自动上传后发送。绝对路径或相对 ~/.opencarrier"}},"required":[]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move { send_media("image", "image", &args, &context).await })
    }
}

pub struct WecomSendVoiceTool;
impl ToolProvider for WecomSendVoiceTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_voice".to_string(),
            description: "发语音给当前企业微信客服用户。给 media_id 或 file_path(amr/speex/mp3/wma,≤2MB,≤60s)。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"media_id":{"type":"string"},"file_path":{"type":"string","description":"本地语音路径,自动上传"}},"required":[]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move { send_media("voice", "voice", &args, &context).await })
    }
}

pub struct WecomSendVideoTool;
impl ToolProvider for WecomSendVideoTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_video".to_string(),
            description: "发视频给当前企业微信客服用户。给 media_id 或 file_path(mp4,≤10MB)。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"media_id":{"type":"string"},"file_path":{"type":"string","description":"本地视频路径,自动上传"}},"required":[]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move { send_media("video", "video", &args, &context).await })
    }
}

pub struct WecomSendFileTool;
impl ToolProvider for WecomSendFileTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_file".to_string(),
            description: "发文件给当前企业微信客服用户。给 media_id 或 file_path(≤20MB)。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"media_id":{"type":"string"},"file_path":{"type":"string","description":"本地文件路径,自动上传"}},"required":[]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move { send_media("file", "file", &args, &context).await })
    }
}

// ---------------------------------------------------------------------------
// link — 图文链接 (pic_url is a URL, no upload)
// ---------------------------------------------------------------------------

pub struct WecomSendLinkTool;
impl ToolProvider for WecomSendLinkTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_link".to_string(),
            description: "发图文链接卡片给当前企业微信客服用户。pic_url 是可公开访问的图片URL(不用上传)。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"title":{"type":"string"},"desc":{"type":"string","description":"描述/摘要"},"url":{"type":"string","description":"点击跳转的链接"},"pic_url":{"type":"string","description":"封面图URL(可公开访问的图片)"}},"required":["title","url"]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move {
            let (http, access_token, open_kfid, ext) = resolve_bot(&context)?;
            let body = serde_json::json!({
                "msgtype": "link",
                "link": {
                    "title": args["title"].as_str().unwrap_or(""),
                    "desc": args["desc"].as_str().unwrap_or(""),
                    "url": args["url"].as_str().unwrap_or(""),
                    "pic_url": args["pic_url"].as_str().unwrap_or(""),
                }
            });
            token::send_kf_msg(&http, &access_token, &open_kfid, &ext, body)
                .await
                .map_err(PluginToolError::tool)?;
            Ok(format!("link sent to {ext}"))
        })
    }
}

// ---------------------------------------------------------------------------
// miniprogram — 小程序卡片 (thumb_media_id via media/upload)
// ---------------------------------------------------------------------------

pub struct WecomSendMiniprogramTool;
impl ToolProvider for WecomSendMiniprogramTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_miniprogram".to_string(),
            description: "发小程序卡片给当前企业微信客服用户。需 appid/pagepath/title + 封面(thumb_file 本地图片 或 thumb_url 下载)。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"appid":{"type":"string","description":"小程序 appid"},"pagepath":{"type":"string","description":"小程序页面路径(含参数)"},"title":{"type":"string"},"thumb_file":{"type":"string","description":"本地封面图路径,自动上传"},"thumb_url":{"type":"string","description":"封面图URL,下载后上传"}},"required":["appid","pagepath","title"]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move {
            let (http, access_token, open_kfid, ext) = resolve_bot(&context)?;
            let thumb = if let Some(mid) = args["thumb_media_id"].as_str() {
                mid.to_string()
            } else {
                // thumb_file (local) or thumb_url (download)
                let bytes = if let Some(fp) = args["thumb_file"].as_str() {
                    let resolved = if fp.starts_with('/') {
                        std::path::PathBuf::from(fp)
                    } else {
                        types::config::home_dir().join(fp)
                    };
                    std::fs::read(&resolved)
                        .map_err(|e| PluginToolError::tool(format!("read thumb {resolved:?}: {e}")))?
                } else if let Some(url) = args["thumb_url"].as_str() {
                    let resp = http.get(url).send().await
                        .map_err(|e| PluginToolError::tool(format!("download thumb: {e}")))?;
                    resp.bytes().await
                        .map_err(|e| PluginToolError::tool(format!("read thumb body: {e}")))?
                        .to_vec()
                } else {
                    return Err(PluginToolError::tool("must provide thumb_media_id, thumb_file, or thumb_url"));
                };
                token::upload_kf_media(&http, &access_token, "image", bytes, "thumb.jpg")
                    .await
                    .map_err(PluginToolError::tool)?
            };
            let body = serde_json::json!({
                "msgtype": "miniprogram",
                "miniprogram": {
                    "appid": args["appid"].as_str().unwrap_or(""),
                    "pagepath": args["pagepath"].as_str().unwrap_or(""),
                    "title": args["title"].as_str().unwrap_or(""),
                    "thumb_media_id": thumb,
                }
            });
            token::send_kf_msg(&http, &access_token, &open_kfid, &ext, body)
                .await
                .map_err(PluginToolError::tool)?;
            Ok(format!("miniprogram card sent to {ext}"))
        })
    }
}

// ---------------------------------------------------------------------------
// menu — 菜单消息
// ---------------------------------------------------------------------------

pub struct WecomSendMenuTool;
impl ToolProvider for WecomSendMenuTool {
    fn definition(&self) -> PluginToolDef {
        PluginToolDef {
            name: "wecom_send_menu".to_string(),
            description: "发菜单消息给当前企业微信客服用户。list 每项 {type:click|view|miniprogram, content, ...}。".to_string(),
            parameters_json: r#"{"type":"object","properties":{"head_content":{"type":"string","description":"菜单头部引导文字"},"list":{"type":"array","description":"菜单项数组,每项 {type:'click'|'view'|'miniprogram', content:'显示文本', click:{id,content} 或 view:{url,content} 或 miniprogram:{appid,pagepath,content}}"}},"required":["list"]}"#.to_string(),
        }
    }
    fn execute(&self, args: &Value, context: &PluginToolContext) -> Result<String, PluginToolError> {
        let args = args.clone();
        let context = context.clone();
        run(async move {
            let (http, access_token, open_kfid, ext) = resolve_bot(&context)?;
            let body = serde_json::json!({
                "msgtype": "menu",
                "menu": {
                    "head_content": args["head_content"].as_str().unwrap_or(""),
                    "list": args["list"].clone(),
                }
            });
            token::send_kf_msg(&http, &access_token, &open_kfid, &ext, body)
                .await
                .map_err(PluginToolError::tool)?;
            Ok(format!("menu sent to {ext}"))
        })
    }
}
