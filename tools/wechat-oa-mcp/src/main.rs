//! wechat-oa-mcp — WeChat Official Account MCP Server (multi-tenant)
//!
//! Each tool call carries `app_id` and `app_secret`, allowing a single MCP
//! server process to serve multiple WeChat Official Accounts simultaneously.
//!
//! Tokens are cached per `app_id` and auto-refreshed before expiry.
//!
//! # Usage
//!
//! No environment variables needed — credentials are passed per tool call.
//! Each OpenCarrier clone stores its own WeChat OA credentials in its
//! knowledge/config and passes them when invoking tools.

mod wechat;

use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, transport::stdio as stdio_transport, ServiceExt};
use schemars::JsonSchema;
use wechat::WeChatClient;
use mcp_common::json::{error_response, json_to_string, url_encode};

// ================================================================== //
//  Tool parameter structs                                              //
//  Every struct carries app_id + app_secret for multi-tenant support. //
// ================================================================== //

/// Deserialize an i64 from either a JSON number or a numeric string.
/// LLMs sometimes pass `"20"` instead of `20` for integer parameters.
fn deserialize_string_or_i64<'de, D: serde::Deserializer<'de>>(de: D) -> Result<i64, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct StringOrI64;
    impl<'de> Visitor<'de> for StringOrI64 {
        type Value = i64;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an integer or a numeric string")
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> { Ok(v) }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            v.try_into().map_err(de::Error::custom)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            v.trim().parse().map_err(de::Error::custom)
        }
    }
    de.deserialize_any(StringOrI64)
}

/// Deserialize Option<i64> from either a JSON number, a numeric string, or null.
fn deserialize_opt_string_or_i64<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<i64>, D::Error> {
    use serde::de::{self, Visitor};
    use std::fmt;

    struct OptStringOrI64;
    impl<'de> Visitor<'de> for OptStringOrI64 {
        type Value = Option<i64>;
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an integer, a numeric string, or null")
        }
        fn visit_none<E: de::Error>(self) -> Result<Option<i64>, E> { Ok(None) }
        fn visit_unit<E: de::Error>(self) -> Result<Option<i64>, E> { Ok(None) }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<i64>, E> { Ok(Some(v)) }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<i64>, E> {
            Ok(Some(v.try_into().map_err(de::Error::custom)?))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<i64>, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() { return Ok(None); }
            Ok(Some(trimmed.parse().map_err(de::Error::custom)?))
        }
    }
    de.deserialize_any(OptStringOrI64)
}

macro_rules! define_params {
    ($name:ident { $($field:tt)* }) => {
        #[derive(Debug, serde::Deserialize, JsonSchema)]
        #[allow(dead_code)] // structs are constructed by rmcp deserialization, not manually
        struct $name {
            #[schemars(description = "公众号 AppID")]
            app_id: String,
            #[schemars(description = "公众号 AppSecret")]
            app_secret: String,
            $($field)*
        }
    };
}

define_params!(GetAccessTokenParams {});

define_params!(UploadMediaParams {
    #[schemars(description = "Media type: image, voice, video, thumb")]
    media_type: String,
    #[serde(default)]
    #[schemars(description = "Filename (e.g. cover.jpg). Optional: defaults to 'upload' if omitted.")]
    filename: Option<String>,
    #[serde(default)]
    #[schemars(description = "Base64-encoded media data (required if file_path not provided)")]
    data_base64: Option<String>,
    #[serde(default)]
    #[schemars(description = "Local file path to read (e.g., output/image.png - reads file directly, bypassing base64 size limits)")]
    file_path: Option<String>,
});

define_params!(UploadMediaFromUrlParams {
    #[schemars(description = "Media type: image, voice, video, thumb")]
    media_type: String,
    #[serde(default)]
    #[schemars(description = "Filename (e.g. cover.jpg). Optional: inferred from URL if omitted.")]
    filename: Option<String>,
    #[schemars(description = "URL of the media to download and upload")]
    url: String,
});

define_params!(CreateDraftParams {
    #[serde(default)]
    #[schemars(description = "Article type: \"news\" (default) or \"newspic\" (image gallery)")]
    article_type: Option<String>,
    #[schemars(description = "Article title")]
    title: String,
    #[schemars(description = "Article content (HTML for news, plain text for newspic)")]
    content: String,
    #[serde(default)]
    #[schemars(description = "Author name")]
    author: Option<String>,
    #[serde(default)]
    #[schemars(description = "Original article URL")]
    content_source_url: Option<String>,
    #[serde(default)]
    #[schemars(description = "Article digest / summary")]
    digest: Option<String>,
    #[serde(default)]
    #[schemars(description = "Cover image media_id for news type (from upload_media)")]
    thumb_media_id: Option<String>,
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    #[schemars(description = "Open comment section (1=yes 0=no, default 1)")]
    need_open_comment: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Image info for newspic type. JSON object: {\"image_list\": [{\"image_media_id\": \"...\"}]}")]
    image_info: Option<serde_json::Value>,
    #[serde(default)]
    #[schemars(description = "Cover crop for newspic type. JSON object: {\"crop_percent_list\": [{\"ratio\": \"1_1\", \"x1\": \"0\", \"y1\": \"0\", \"x2\": \"1\", \"y2\": \"1\"}]}")]
    cover_info: Option<serde_json::Value>,
});

define_params!(GetDraftParams {
    #[schemars(description = "Draft media_id")]
    media_id: String,
});

define_params!(ListDraftsParams {
    #[serde(default)]
    #[schemars(description = "Page offset (0-based, default 0)")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    offset: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Page size (max 20, default 20)")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    count: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Set to 1 to omit article content (saves bandwidth)")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    no_content: Option<i64>,
});

define_params!(DeleteDraftParams {
    #[schemars(description = "Draft media_id to delete")]
    media_id: String,
});

define_params!(PublishDraftParams {
    #[schemars(description = "Draft media_id to publish")]
    media_id: String,
});

define_params!(GetPublishStatusParams {
    #[schemars(description = "publish_id returned by publish_draft")]
    publish_id: String,
});

define_params!(ListMaterialsParams {
    #[schemars(description = "Material type: image, video, voice, news")]
    r#type: String,
    #[serde(default)]
    #[schemars(description = "Page offset (0-based, default 0)")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    offset: Option<i64>,
    #[serde(default)]
    #[schemars(description = "Page size (max 20, default 20)")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    count: Option<i64>,
});

define_params!(DeleteMaterialParams {
    #[schemars(description = "Material media_id to delete")]
    media_id: String,
});

// ---- Comment management params ----

define_params!(OpenCommentParams {
    #[schemars(description = "图文消息的 msg_data_id（从群发通知或 publish_status 获取）")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0即第一篇）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
});

define_params!(CloseCommentParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
});

define_params!(ListCommentsParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
    #[serde(default)]
    #[schemars(description = "评论类型：0=全部 1=普通评论 2=精选评论（默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    comment_type: Option<i64>,
    #[serde(default)]
    #[schemars(description = "页偏移（0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    offset: Option<i64>,
    #[serde(default)]
    #[schemars(description = "每页条数（默认10）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    count: Option<i64>,
});

define_params!(MarkElectParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
    #[schemars(description = "评论 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    comment_id: i64,
});

define_params!(UnmarkElectParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
    #[schemars(description = "评论 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    comment_id: i64,
});

define_params!(DeleteCommentParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
    #[schemars(description = "评论 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    comment_id: i64,
});

define_params!(ReplyCommentParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
    #[schemars(description = "评论 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    comment_id: i64,
    #[schemars(description = "回复内容")]
    content: String,
});

define_params!(DeleteReplyParams {
    #[schemars(description = "图文消息的 msg_data_id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    msg_data_id: i64,
    #[serde(default)]
    #[schemars(description = "多图文时第几篇文章（从0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    index: Option<i64>,
    #[schemars(description = "评论 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    comment_id: i64,
    #[schemars(description = "回复 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    reply_id: i64,
});

// ---- Publish (freepublish) params ----

define_params!(ListPublishedParams {
    #[serde(default)]
    #[schemars(description = "页偏移（0开始，默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    offset: Option<i64>,
    #[serde(default)]
    #[schemars(description = "每页条数（默认20）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    count: Option<i64>,
    #[serde(default)]
    #[schemars(description = "1=返回内容 0=不返回（默认0）")]
    #[serde(deserialize_with = "deserialize_opt_string_or_i64")]
    no_content: Option<i64>,
});

define_params!(DeletePublishedParams {
    #[schemars(description = "已发布图文的 article_id")]
    article_id: String,
});

define_params!(GetArticleParams {
    #[schemars(description = "已发布图文的 article_id")]
    article_id: String,
});

// ---- User management: tags ----

define_params!(CreateTagParams {
    #[schemars(description = "标签名称（30字符以内）")]
    name: String,
});

define_params!(UpdateTagParams {
    #[schemars(description = "标签 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    id: i64,
    #[schemars(description = "新标签名称")]
    name: String,
});

define_params!(DeleteTagParams {
    #[schemars(description = "标签 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    id: i64,
});

define_params!(BatchTaggingParams {
    #[schemars(description = "标签 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    tag_id: i64,
    #[schemars(description = "用户 openid 列表（JSON数组）")]
    openid_list: serde_json::Value,
});

define_params!(BatchUntaggingParams {
    #[schemars(description = "标签 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    tag_id: i64,
    #[schemars(description = "用户 openid 列表（JSON数组）")]
    openid_list: serde_json::Value,
});

define_params!(GetTagUsersParams {
    #[schemars(description = "标签 id")]
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    tag_id: i64,
    #[serde(default)]
    #[schemars(description = "翻页 openid，第一页不传")]
    next_openid: Option<String>,
});

define_params!(GetUserTagsParams {
    #[schemars(description = "用户 openid")]
    openid: String,
});

// ---- User management: user info ----

define_params!(GetUserInfoParams {
    #[schemars(description = "用户 openid")]
    openid: String,
    #[serde(default)]
    #[schemars(description = "语言：zh_CN, zh_TW, en（默认 zh_CN）")]
    lang: Option<String>,
});

define_params!(BatchGetUserInfoParams {
    #[schemars(description = "用户列表（JSON数组，每项含 openid 字段）")]
    user_list: serde_json::Value,
});

define_params!(GetFollowersParams {
    #[serde(default)]
    #[schemars(description = "翻页 openid，第一页不传")]
    next_openid: Option<String>,
});

define_params!(UpdateRemarkParams {
    #[schemars(description = "用户 openid")]
    openid: String,
    #[schemars(description = "备注名")]
    remark: String,
});

// ---- User management: blacklist ----

define_params!(GetBlacklistParams {
    #[serde(default)]
    #[schemars(description = "翻页 openid，第一页不传")]
    begin_openid: Option<String>,
});

define_params!(BatchBlacklistParams {
    #[schemars(description = "用户 openid 列表（JSON数组）")]
    openid_list: serde_json::Value,
});

define_params!(BatchUnblacklistParams {
    #[schemars(description = "用户 openid 列表（JSON数组）")]
    openid_list: serde_json::Value,
});

// ---- User management: tags (list has no extra params) ----

define_params!(ListTagsParams {});

// ---- OpenID conversion ----

define_params!(ConvertOpenIdParams {
    #[schemars(description = "原公众号的 appid")]
    from_appid: String,
    #[schemars(description = "需要转换的 openid 列表（JSON数组）")]
    openid_list: serde_json::Value,
});

// ---- Data statistics (datacube) ----

define_params!(DatacubeParams {
    #[schemars(description = "开始日期（格式：yyyy-MM-dd）")]
    begin_date: String,
    #[schemars(description = "结束日期（格式：yyyy-MM-dd，最大跨度见微信文档）")]
    end_date: String,
});

// ================================================================== //
//  MCP Server                                                         //
// ================================================================== //

#[derive(Clone)]
struct WeChatOaServer {
    client: Arc<WeChatClient>,
}

#[tool_router(server_handler)]
impl WeChatOaServer {
    // ---- Token ----

    #[tool(
        description = "Get WeChat OA access token for a specific account (auto-refreshed, cached ~2h)"
    )]
    async fn get_access_token(
        &self,
        Parameters(params): Parameters<GetAccessTokenParams>,
    ) -> String {
        match self
            .client
            .get_token(&params.app_id, &params.app_secret)
            .await
        {
            Ok(token) => serde_json::json!({ "access_token": token }).to_string(),
            Err(e) => error_response(&e),
        }
    }

    // ---- Media ----

    #[tool(
        description = "Upload image/media to a WeChat OA account's permanent material library. Returns media_id and url. Use file_path to read local files directly (recommended for images to avoid base64 size limits), or data_base64 for small data."
    )]
    async fn upload_media(&self, Parameters(params): Parameters<UploadMediaParams>) -> String {
        let data = if let Some(ref path) = params.file_path {
            // SECURITY: validate path before reading
            if let Err(e) = mcp_common::path::validate_path(path) {
                return error_response(&e);
            }
            match std::fs::read(path) {
                Ok(d) => {
                    if let Err(e) = mcp_common::path::validate_size(&d) {
                        return error_response(&e);
                    }
                    d
                }
                Err(e) => return error_response(format!("failed to read file {path}: {e}")),
            }
        } else if let Some(ref base64) = params.data_base64 {
            // Decode from base64 (for small data only)
            match base64::engine::general_purpose::STANDARD.decode(base64) {
                Ok(d) => d,
                Err(e) => return error_response(format!("invalid base64: {e}")),
            }
        } else {
            return error_response("Either file_path or data_base64 must be provided");
        };

        let filename = params.filename.as_deref().unwrap_or("upload");

        match self
            .client
            .upload_media(
                &params.app_id,
                &params.app_secret,
                &params.media_type,
                filename,
                &data,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(
        description = "Download media from a URL and upload to a WeChat OA account's permanent material library. Returns media_id and url. Use this when you have an image URL (e.g., from image_generate) and need to upload it as a cover image."
    )]
    async fn upload_media_from_url(
        &self,
        Parameters(params): Parameters<UploadMediaFromUrlParams>,
    ) -> String {
        let data = match self.client.fetch_bytes(&params.url).await {
            Ok(d) => d,
            Err(e) => return error_response(format!("failed to download: {e}")),
        };
        let filename = params.filename.unwrap_or_else(|| {
            params
                .url
                .rsplit('/')
                .next()
                .unwrap_or("upload.jpg")
                .to_string()
        });
        match self
            .client
            .upload_media(
                &params.app_id,
                &params.app_secret,
                &params.media_type,
                &filename,
                &data,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- Drafts ----

    #[tool(description = "Create a new draft article (news or newspic). For newspic image gallery, set article_type=\"newspic\" and provide image_info with image_media_ids.")]
    async fn create_draft(&self, Parameters(params): Parameters<CreateDraftParams>) -> String {
        let article_type = params
            .article_type
            .unwrap_or_else(|| "news".to_string());

        let mut article = serde_json::json!({
            "article_type": article_type,
            "title": params.title,
            "content": params.content,
            "author": params.author.unwrap_or_default(),
            "content_source_url": params.content_source_url.unwrap_or_default(),
            "digest": params.digest.unwrap_or_default(),
            "need_open_comment": params.need_open_comment.unwrap_or(1),
            "only_fans_can_comment": 0,
        });

        if article_type == "news" {
            if let Some(tid) = params.thumb_media_id {
                if !tid.is_empty() {
                    article["thumb_media_id"] = serde_json::Value::String(tid);
                }
            }
        }

        if let Some(images) = params.image_info {
            let parsed = coerce_json_value(images);
            article["image_info"] = parsed;
        }

        if let Some(crops) = params.cover_info {
            let parsed = coerce_json_value(crops);
            article["cover_info"] = parsed;
        }

        let body = serde_json::json!({ "articles": [article] });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/draft/add",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Get full draft content by media_id")]
    async fn get_draft(&self, Parameters(params): Parameters<GetDraftParams>) -> String {
        let body = serde_json::json!({ "media_id": params.media_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/draft/get",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "List drafts in the WeChat OA draft box")]
    async fn list_drafts(&self, Parameters(params): Parameters<ListDraftsParams>) -> String {
        let body = serde_json::json!({
            "offset": params.offset.unwrap_or(0),
            "count": params.count.unwrap_or(20),
            "no_content": params.no_content.unwrap_or(0),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/draft/batchget",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Delete a draft by media_id")]
    async fn delete_draft(&self, Parameters(params): Parameters<DeleteDraftParams>) -> String {
        let body = serde_json::json!({ "media_id": params.media_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/draft/delete",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- Publishing ----

    #[tool(description = "Submit a draft for publishing. Returns publish_id for status tracking.")]
    async fn publish_draft(&self, Parameters(params): Parameters<PublishDraftParams>) -> String {
        let body = serde_json::json!({ "media_id": params.media_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/freepublish/submit",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Check the publishing status of a submitted draft")]
    async fn get_publish_status(
        &self,
        Parameters(params): Parameters<GetPublishStatusParams>,
    ) -> String {
        let body = serde_json::json!({ "publish_id": params.publish_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/freepublish/get",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- Materials ----

    #[tool(description = "List permanent materials in the WeChat OA library")]
    async fn list_materials(&self, Parameters(params): Parameters<ListMaterialsParams>) -> String {
        let body = serde_json::json!({
            "type": params.r#type,
            "offset": params.offset.unwrap_or(0),
            "count": params.count.unwrap_or(20),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/material/batchget_material",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Delete a permanent material by media_id")]
    async fn delete_material(
        &self,
        Parameters(params): Parameters<DeleteMaterialParams>,
    ) -> String {
        let body = serde_json::json!({ "media_id": params.media_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/material/del_material",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- Comment management ----

    #[tool(description = "Open comment section for a published article. The account must have comment capability.")]
    async fn open_comment(
        &self,
        Parameters(params): Parameters<OpenCommentParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/open",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Close comment section for a published article.")]
    async fn close_comment(
        &self,
        Parameters(params): Parameters<CloseCommentParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/close",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "List comments for a published article. Filter by type: 0=all, 1=normal, 2=featured.")]
    async fn list_comments(
        &self,
        Parameters(params): Parameters<ListCommentsParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
            "type": params.comment_type.unwrap_or(0),
            "begin": params.offset.unwrap_or(0),
            "count": params.count.unwrap_or(10),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/list",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Mark a comment as featured (精选).")]
    async fn mark_elect(
        &self,
        Parameters(params): Parameters<MarkElectParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
            "comment_id": params.comment_id,
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/markelect",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Remove featured (精选) mark from a comment.")]
    async fn unmark_elect(
        &self,
        Parameters(params): Parameters<UnmarkElectParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
            "comment_id": params.comment_id,
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/unmarkelect",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Delete a comment from a published article.")]
    async fn delete_comment(
        &self,
        Parameters(params): Parameters<DeleteCommentParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
            "comment_id": params.comment_id,
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/delete",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Reply to a comment on a published article.")]
    async fn reply_comment(
        &self,
        Parameters(params): Parameters<ReplyCommentParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
            "comment_id": params.comment_id,
            "content": params.content,
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/reply/add",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Delete a reply to a comment.")]
    async fn delete_reply(
        &self,
        Parameters(params): Parameters<DeleteReplyParams>,
    ) -> String {
        let body = serde_json::json!({
            "msg_data_id": params.msg_data_id,
            "index": params.index.unwrap_or(0),
            "comment_id": params.comment_id,
            "reply_id": params.reply_id,
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/comment/reply/delete",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- Publish (freepublish) ----

    #[tool(description = "List published articles. Returns article_id, title, and update_time for each.")]
    async fn list_published(
        &self,
        Parameters(params): Parameters<ListPublishedParams>,
    ) -> String {
        let body = serde_json::json!({
            "offset": params.offset.unwrap_or(0),
            "count": params.count.unwrap_or(20),
            "no_content": params.no_content.unwrap_or(0),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/freepublish/batchget",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Delete a published article by article_id. This action is irreversible.")]
    async fn delete_published(
        &self,
        Parameters(params): Parameters<DeletePublishedParams>,
    ) -> String {
        let body = serde_json::json!({ "article_id": params.article_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/freepublish/delete",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Get full article content for a published article by article_id.")]
    async fn get_article(
        &self,
        Parameters(params): Parameters<GetArticleParams>,
    ) -> String {
        let body = serde_json::json!({ "article_id": params.article_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/freepublish/getarticle",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- User management: Tags ----

    #[tool(description = "List all tags for a WeChat OA account.")]
    async fn list_tags(&self, Parameters(params): Parameters<ListTagsParams>) -> String {
        let body = serde_json::json!({});
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/get",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Create a new tag for user categorization.")]
    async fn create_tag(&self, Parameters(params): Parameters<CreateTagParams>) -> String {
        let body = serde_json::json!({ "tag": { "name": params.name } });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/create",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Update an existing tag's name.")]
    async fn update_tag(&self, Parameters(params): Parameters<UpdateTagParams>) -> String {
        let body = serde_json::json!({ "tag": { "id": params.id, "name": params.name } });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/update",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Delete a tag by id.")]
    async fn delete_tag(&self, Parameters(params): Parameters<DeleteTagParams>) -> String {
        let body = serde_json::json!({ "tag": { "id": params.id } });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/delete",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Batch tag users with a given tag id.")]
    async fn batch_tagging(&self, Parameters(params): Parameters<BatchTaggingParams>) -> String {
        let openid_list = coerce_json_value(params.openid_list);
        let body = serde_json::json!({ "openid_list": openid_list, "tagid": params.tag_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/members/batchtagging",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Batch untag users from a given tag id.")]
    async fn batch_untagging(&self, Parameters(params): Parameters<BatchUntaggingParams>) -> String {
        let openid_list = coerce_json_value(params.openid_list);
        let body = serde_json::json!({ "openid_list": openid_list, "tagid": params.tag_id });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/members/batchuntagging",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Get users under a tag. Supports pagination via next_openid.")]
    async fn get_tag_users(&self, Parameters(params): Parameters<GetTagUsersParams>) -> String {
        let body = serde_json::json!({
            "tagid": params.tag_id,
            "next_openid": params.next_openid.unwrap_or_default(),
        });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/user/tag/get",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Get tag ids that a user belongs to.")]
    async fn get_user_tags(&self, Parameters(params): Parameters<GetUserTagsParams>) -> String {
        let body = serde_json::json!({ "openid": params.openid });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/getidlist",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- User management: User info ----

    #[tool(description = "Get user info by openid. Uses GET request to WeChat API.")]
    async fn get_user_info(&self, Parameters(params): Parameters<GetUserInfoParams>) -> String {
        let lang = params.lang.unwrap_or_else(|| "zh_CN".to_string());
        let query = format!("openid={}&lang={}", url_encode(&params.openid), url_encode(&lang));
        match self
            .client
            .api_get(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/user/info",
                &query,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Batch get user info for multiple openids.")]
    async fn batch_get_user_info(
        &self,
        Parameters(params): Parameters<BatchGetUserInfoParams>,
    ) -> String {
        let user_list = coerce_json_value(params.user_list);
        let body = serde_json::json!({ "user_list": user_list });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/user/info/batchget",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Get follower list (all users who follow the account). Uses GET request.")]
    async fn get_followers(&self, Parameters(params): Parameters<GetFollowersParams>) -> String {
        let query = match params.next_openid {
            Some(ref n) if !n.is_empty() => format!("next_openid={}", n),
            _ => String::new(),
        };
        match self
            .client
            .api_get(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/user/get",
                &query,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Update remark name for a user.")]
    async fn update_remark(&self, Parameters(params): Parameters<UpdateRemarkParams>) -> String {
        let body = serde_json::json!({ "openid": params.openid, "remark": params.remark });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/user/info/updateremark",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- User management: Blacklist ----

    #[tool(description = "Get blacklist (users blocked by the account). Supports pagination.")]
    async fn get_blacklist(&self, Parameters(params): Parameters<GetBlacklistParams>) -> String {
        let body = serde_json::json!({ "begin_openid": params.begin_openid.unwrap_or_default() });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/members/getblacklist",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Batch block users (add to blacklist).")]
    async fn batch_blacklist(&self, Parameters(params): Parameters<BatchBlacklistParams>) -> String {
        let openid_list = coerce_json_value(params.openid_list);
        let body = serde_json::json!({ "openid_list": openid_list });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/members/batchblacklist",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Batch unblock users (remove from blacklist).")]
    async fn batch_unblacklist(
        &self,
        Parameters(params): Parameters<BatchUnblacklistParams>,
    ) -> String {
        let openid_list = coerce_json_value(params.openid_list);
        let body = serde_json::json!({ "openid_list": openid_list });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/tags/members/batchunblacklist",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- OpenID conversion ----

    #[tool(description = "Convert openid from one appid to another (for account migration).")]
    async fn convert_openid(&self, Parameters(params): Parameters<ConvertOpenIdParams>) -> String {
        let openid_list = coerce_json_value(params.openid_list);
        let body = serde_json::json!({ "from_appid": params.from_appid, "openid_list": openid_list });
        match self
            .client
            .api_post(
                &params.app_id,
                &params.app_secret,
                "/cgi-bin/changeopenid",
                &body,
            )
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    // ---- Data statistics (datacube) ----

    /// Helper: execute a datacube API call with standard begin_date/end_date body.
    async fn datacube_get(
        &self,
        params: &DatacubeParams,
        path: &str,
    ) -> String {
        let body = serde_json::json!({
            "begin_date": params.begin_date,
            "end_date": params.end_date
        });
        match self.client.api_post(&params.app_id, &params.app_secret, path, &body).await {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "Get user growth summary (datacube). Max date range: 7 days.")]
    async fn get_user_summary(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getusersummary").await
    }

    #[tool(description = "Get cumulative user count (datacube). Max date range: 7 days.")]
    async fn get_user_cumulate(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getusercumulate").await
    }

    #[tool(description = "Get article total detail stats (datacube). Max date range: 1 day.")]
    async fn get_article_summary(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getarticletotaldetail").await
    }

    #[tool(description = "Get article read analytics (datacube). Max date range: 3 days.")]
    async fn get_user_read(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getarticleread").await
    }

    #[tool(description = "Get article read hourly analytics (datacube). Max date range: 1 day.")]
    async fn get_user_read_hour(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getarticlereadhour").await
    }

    #[tool(description = "Get article share analytics (datacube). Max date range: 7 days.")]
    async fn get_user_share(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getarticleshare").await
    }

    #[tool(description = "Get article share hourly analytics (datacube). Max date range: 1 day.")]
    async fn get_user_share_hour(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getarticlesharehour").await
    }

    #[tool(description = "Get upstream message stats (datacube). Max date range: 7 days.")]
    async fn get_upstream_msg(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsg").await
    }

    #[tool(description = "Get upstream message weekly stats (datacube). Max date range: 30 days.")]
    async fn get_upstream_msg_week(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsgweek").await
    }

    #[tool(description = "Get upstream message monthly stats (datacube). Max date range: 30 days.")]
    async fn get_upstream_msg_month(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsgmonth").await
    }

    #[tool(description = "Get upstream message distribution stats (datacube). Max date range: 15 days.")]
    async fn get_upstream_msg_dist(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsgdist").await
    }

    #[tool(description = "Get upstream message weekly distribution stats (datacube). Max date range: 30 days.")]
    async fn get_upstream_msg_dist_week(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsgdistweek").await
    }

    #[tool(description = "Get upstream message monthly distribution stats (datacube). Max date range: 30 days.")]
    async fn get_upstream_msg_dist_month(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsgdistmonth").await
    }

    #[tool(description = "Get upstream message hourly stats (datacube). Max date range: 1 day.")]
    async fn get_upstream_msg_hour(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getupstreammsghour").await
    }

    #[tool(description = "Get interface summary stats (datacube). Max date range: 30 days.")]
    async fn get_interface_summary(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getinterfacesummary").await
    }

    #[tool(description = "Get interface summary hourly stats (datacube). Max date range: 1 day.")]
    async fn get_interface_summary_hour(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getinterfacesummaryhour").await
    }

    #[tool(description = "Get business summary (datacube). Max date range: 7 days.")]
    async fn get_biz_summary(&self, Parameters(params): Parameters<DatacubeParams>) -> String {
        self.datacube_get(&params, "/datacube/getbizsummary").await
    }

}

// ================================================================== //
//  Helpers                                                             //
// ================================================================== //

/// If the LLM passed a JSON value as a string (common with complex MCP params),
/// parse it back into a proper JSON value. Otherwise return as-is.
fn coerce_json_value(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => {
            serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
        }
        other => other,
    }
}

// ================================================================== //
//  Entry point                                                         //
// ================================================================== //

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr — stdout is reserved for the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WECHAT_OA_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let client = WeChatClient::new();
    let server = WeChatOaServer {
        client: Arc::new(client),
    };

    tracing::info!("wechat-oa-mcp starting (stdio, multi-tenant)");
    let service = server.serve(stdio_transport()).await?;
    service.waiting().await?;

    Ok(())
}
