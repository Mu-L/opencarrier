//! wecom-mcp — WeCom (WeChat Work) MCP Server (multi-tenant, 47 tools)
//!
//! Each tool call carries its own credentials (`corp_id` + `secret` for direct
//! API tools, additionally `bot_id` + `bot_secret` for MCP proxy tools), allowing
//! a single MCP server process to serve multiple WeCom organizations simultaneously.
//!
//! # Tool Categories
//!
//! - **Direct API** (6 tools): Standard WeCom REST API calls
//! - **MCP Proxy** (41 tools): Routed through WeCom's MCP gateway
//!
//! # Usage
//!
//! No environment variables needed — credentials are passed per tool call.
//! Set `WECOM_MCP_LOG` to control log level (default: warn).

mod wecom;

use std::sync::Arc;

use anyhow::Result;
use mcp_common::json::{error_response, json_to_string, truncate_result};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, transport::stdio as stdio_transport, ServiceExt};
use serde_json::Value;
use wecom::WecomMcpProxy;

// ================================================================== //
//  Parameter struct macros                                            //
// ================================================================== //

/// Define a params struct with all 4 MCP proxy credential fields.
macro_rules! define_wecom_mcp_params {
    ($name:ident { $($field:tt)* }) => {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
        struct $name {
            #[schemars(description = "企业 Corp ID")]
            corp_id: String,
            #[schemars(description = "应用 Secret")]
            secret: String,
            #[schemars(description = "MCP Bot ID")]
            bot_id: String,
            #[schemars(description = "MCP Bot Secret")]
            bot_secret: String,
            $($field)*
        }
    };
}

/// Define a params struct with corp_id + secret only (direct API tools).
macro_rules! define_wecom_direct_params {
    ($name:ident { $($field:tt)* }) => {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
        struct $name {
            #[schemars(description = "企业 Corp ID")]
            corp_id: String,
            #[schemars(description = "应用 Secret")]
            secret: String,
            $($field)*
        }
    };
}

// ================================================================== //
//  Direct API tool parameter structs                                  //
// ================================================================== //

define_wecom_direct_params!(WecomSendMessageParams {
    #[schemars(description = "应用 AgentID")]
    agent_id: i64,
    #[schemars(description = "接收者 UserID（多人用 | 分隔）")]
    user_id: String,
    #[schemars(description = "消息内容")]
    content: String,
});

define_wecom_direct_params!(WecomSendKfMessageParams {
    #[schemars(description = "客服账号 ID（open_kfid）")]
    open_kfid: String,
    #[schemars(description = "接收者 UserID")]
    user_id: String,
    #[schemars(description = "消息内容")]
    content: String,
});

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct WecomBotGenerateParams {
    #[schemars(description = "企业 Corp ID（占位，不参与鉴权）")]
    corp_id: String,
    #[schemars(description = "应用 Secret（占位，不参与鉴权）")]
    secret: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct WecomBotPollParams {
    #[schemars(description = "企业 Corp ID（占位，不参与鉴权）")]
    corp_id: String,
    #[schemars(description = "应用 Secret（占位，不参与鉴权）")]
    secret: String,
    #[schemars(description = "bot_generate 返回的 scode")]
    scode: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
#[allow(dead_code)]
struct WecomQrcodeParams {
    #[schemars(description = "企业 Corp ID（占位，不参与鉴权）")]
    corp_id: String,
    #[schemars(description = "应用 Secret（占位，不参与鉴权）")]
    secret: String,
    #[schemars(description = "要编码为二维码的 URL")]
    url: String,
}

// ================================================================== //
//  MCP Proxy tool parameter structs                                   //
// ================================================================== //

// --- Contact ---

define_wecom_mcp_params!(WecomGetUserlistParams {});

// --- Doc ---

define_wecom_mcp_params!(WecomGetDocContentParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "文档 URL")]
    url: Option<String>,
    #[schemars(description = "文档类型（默认 2）")]
    doc_type: Option<i64>,
    #[schemars(description = "任务 ID")]
    task_id: Option<String>,
});

define_wecom_mcp_params!(WecomCreateDocParams {
    #[schemars(description = "文档类型（3=文档 10=智能表）")]
    doc_type: i64,
    #[schemars(description = "文档名称")]
    doc_name: String,
});

define_wecom_mcp_params!(WecomEditDocContentParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "文档 URL")]
    url: Option<String>,
    #[schemars(description = "编辑内容")]
    content: String,
    #[schemars(description = "内容类型（默认 1）")]
    content_type: Option<i64>,
});

define_wecom_mcp_params!(WecomSmartpageExportTaskParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "文档 URL")]
    url: Option<String>,
    #[schemars(description = "内容类型")]
    content_type: Option<i64>,
});

define_wecom_mcp_params!(WecomSmartpageGetExportResultParams {
    #[schemars(description = "任务 ID")]
    task_id: String,
});

define_wecom_mcp_params!(WecomSmartsheetGetSheetParams {
    #[schemars(description = "文档 ID")]
    docid: String,
});

define_wecom_mcp_params!(WecomSmartsheetAddSheetParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet 属性，包含 title")]
    properties: Value,
});

define_wecom_mcp_params!(WecomSmartsheetUpdateSheetParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet 属性，包含 sheet_id 和 title")]
    properties: Value,
});

define_wecom_mcp_params!(WecomSmartsheetDeleteSheetParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
});

define_wecom_mcp_params!(WecomSmartsheetGetFieldsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
});

define_wecom_mcp_params!(WecomSmartsheetAddFieldsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
    #[schemars(description = "字段列表（JSON 数组）")]
    fields: Value,
});

define_wecom_mcp_params!(WecomSmartsheetUpdateFieldsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
    #[schemars(description = "字段列表（JSON 数组）")]
    fields: Value,
});

define_wecom_mcp_params!(WecomSmartsheetDeleteFieldsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
    #[schemars(description = "字段 ID 列表（JSON 数组）")]
    field_ids: Value,
});

define_wecom_mcp_params!(WecomSmartsheetGetRecordsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "文档 URL")]
    url: Option<String>,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
});

define_wecom_mcp_params!(WecomSmartsheetAddRecordsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
    #[schemars(description = "记录列表（JSON 数组）")]
    records: Value,
});

define_wecom_mcp_params!(WecomSmartsheetUpdateRecordsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
    #[schemars(description = "主键类型")]
    key_type: Option<String>,
    #[schemars(description = "记录列表（JSON 数组）")]
    records: Value,
});

define_wecom_mcp_params!(WecomSmartsheetDeleteRecordsParams {
    #[schemars(description = "文档 ID")]
    docid: String,
    #[schemars(description = "Sheet ID")]
    sheet_id: String,
    #[schemars(description = "记录 ID 列表（JSON 数组）")]
    record_ids: Value,
});

// --- Msg ---

define_wecom_mcp_params!(WecomGetMsgChatListParams {
    #[schemars(description = "开始时间")]
    begin_time: Option<i64>,
    #[schemars(description = "结束时间")]
    end_time: Option<i64>,
    #[schemars(description = "分页游标")]
    cursor: Option<String>,
});

define_wecom_mcp_params!(WecomGetMessageParams {
    #[schemars(description = "会话类型（1=单聊 2=群聊）")]
    chat_type: i64,
    #[schemars(description = "会话 ID")]
    chatid: String,
    #[schemars(description = "开始时间")]
    begin_time: Option<i64>,
    #[schemars(description = "结束时间")]
    end_time: Option<i64>,
    #[schemars(description = "分页游标")]
    cursor: Option<String>,
});

define_wecom_mcp_params!(WecomMsgSendMessageParams {
    #[schemars(description = "会话类型")]
    chat_type: i64,
    #[schemars(description = "会话 ID")]
    chatid: String,
    #[schemars(description = "消息类型（默认 text）")]
    msgtype: Option<String>,
    #[schemars(description = "文本消息内容，包含 content 字段")]
    text: Value,
});

// --- Todo ---

define_wecom_mcp_params!(WecomGetTodoListParams {
    #[schemars(description = "创建开始时间")]
    create_begin_time: Option<i64>,
    #[schemars(description = "创建结束时间")]
    create_end_time: Option<i64>,
    #[schemars(description = "提醒开始时间")]
    remind_begin_time: Option<i64>,
    #[schemars(description = "提醒结束时间")]
    remind_end_time: Option<i64>,
    #[schemars(description = "返回数量上限")]
    limit: Option<i64>,
    #[schemars(description = "分页游标")]
    cursor: Option<String>,
});

define_wecom_mcp_params!(WecomGetTodoDetailParams {
    #[schemars(description = "待办 ID 列表（最多 20 个）")]
    todo_id_list: Vec<String>,
});

define_wecom_mcp_params!(WecomCreateTodoParams {
    #[schemars(description = "待办内容")]
    content: String,
    #[schemars(description = "关注人列表")]
    follower_list: Option<Vec<String>>,
    #[schemars(description = "提醒时间")]
    remind_time: Option<i64>,
});

define_wecom_mcp_params!(WecomUpdateTodoParams {
    #[schemars(description = "待办 ID")]
    todo_id: String,
    #[schemars(description = "待办内容")]
    content: Option<String>,
    #[schemars(description = "关注人列表")]
    follower_list: Option<Vec<String>>,
    #[schemars(description = "待办状态（0=完成 1=进行中 2=删除）")]
    todo_status: Option<i64>,
    #[schemars(description = "提醒时间")]
    remind_time: Option<i64>,
});

define_wecom_mcp_params!(WecomDeleteTodoParams {
    #[schemars(description = "待办 ID")]
    todo_id: String,
});

define_wecom_mcp_params!(WecomChangeTodoUserStatusParams {
    #[schemars(description = "待办 ID")]
    todo_id: String,
    #[schemars(description = "用户状态（0=拒绝 1=接受 2=完成）")]
    user_status: i64,
});

// --- Meeting ---

define_wecom_mcp_params!(WecomCreateMeetingParams {
    #[schemars(description = "会议标题")]
    title: String,
    #[schemars(description = "会议开始时间（Unix 时间戳）")]
    meeting_start_datetime: i64,
    #[schemars(description = "会议时长（秒）")]
    meeting_duration: i64,
    #[schemars(description = "会议描述")]
    description: Option<String>,
    #[schemars(description = "会议地点")]
    location: Option<String>,
    #[schemars(description = "参会人列表（JSON）")]
    invitees: Option<Value>,
    #[schemars(description = "会议设置（JSON）")]
    settings: Option<Value>,
});

define_wecom_mcp_params!(WecomListUserMeetingsParams {
    #[schemars(description = "开始时间（Unix 时间戳）")]
    begin_datetime: i64,
    #[schemars(description = "结束时间（Unix 时间戳）")]
    end_datetime: i64,
    #[schemars(description = "分页游标")]
    cursor: Option<String>,
    #[schemars(description = "返回数量上限")]
    limit: Option<i64>,
});

define_wecom_mcp_params!(WecomGetMeetingInfoParams {
    #[schemars(description = "会议 ID")]
    meetingid: String,
    #[schemars(description = "会议号")]
    meeting_code: Option<String>,
    #[schemars(description = "子会议 ID")]
    sub_meetingid: Option<String>,
});

define_wecom_mcp_params!(WecomCancelMeetingParams {
    #[schemars(description = "会议 ID")]
    meetingid: String,
});

define_wecom_mcp_params!(WecomSetInviteMeetingMembersParams {
    #[schemars(description = "会议 ID")]
    meetingid: String,
    #[schemars(description = "参会人列表，包含 userid 数组")]
    invitees: Value,
});

// --- Schedule ---

define_wecom_mcp_params!(WecomGetScheduleListByRangeParams {
    #[schemars(description = "开始时间")]
    start_time: i64,
    #[schemars(description = "结束时间")]
    end_time: i64,
});

define_wecom_mcp_params!(WecomGetScheduleDetailParams {
    #[schemars(description = "日程 ID 列表（1-50 个）")]
    schedule_id_list: Vec<String>,
});

define_wecom_mcp_params!(WecomCreateScheduleParams {
    #[schemars(description = "日程信息，包含 start_time, end_time, summary, description, location, is_whole_day, attendees, reminders")]
    schedule: Value,
});

define_wecom_mcp_params!(WecomUpdateScheduleParams {
    #[schemars(description = "日程信息，包含 schedule_id 及需更新的字段")]
    schedule: Value,
});

define_wecom_mcp_params!(WecomCancelScheduleParams {
    #[schemars(description = "日程 ID")]
    schedule_id: String,
});

define_wecom_mcp_params!(WecomAddScheduleAttendeesParams {
    #[schemars(description = "日程 ID")]
    schedule_id: String,
    #[schemars(description = "参会人列表，包含 userid 数组")]
    attendees: Value,
});

define_wecom_mcp_params!(WecomDelScheduleAttendeesParams {
    #[schemars(description = "日程 ID")]
    schedule_id: String,
    #[schemars(description = "参会人列表，包含 userid 数组")]
    attendees: Value,
});

define_wecom_mcp_params!(WecomCheckAvailabilityParams {
    #[schemars(description = "待检查用户列表（1-10 个）")]
    check_user_list: Vec<String>,
    #[schemars(description = "开始时间")]
    start_time: i64,
    #[schemars(description = "结束时间")]
    end_time: i64,
});

// ================================================================== //
//  MCP Server                                                         //
// ================================================================== //

#[derive(Clone)]
struct WecomServer {
    proxy: Arc<WecomMcpProxy>,
}

#[tool_router(server_handler)]
impl WecomServer {
    // ================================================================== //
    //  Direct API tools (6)                                               //
    // ================================================================== //

    #[tool(description = "发送应用消息（文本）到企业微信用户")]
    async fn wecom_send_message(
        &self,
        Parameters(params): Parameters<WecomSendMessageParams>,
    ) -> String {
        let body = serde_json::json!({
            "touser": params.user_id,
            "msgtype": "text",
            "agentid": params.agent_id,
            "text": { "content": params.content }
        });
        match self
            .proxy
            .client()
            .api_post(&params.corp_id, &params.secret, "/cgi-bin/message/send", &body)
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "通过微信客服发送消息")]
    async fn wecom_send_kf_message(
        &self,
        Parameters(params): Parameters<WecomSendKfMessageParams>,
    ) -> String {
        let body = serde_json::json!({
            "touser": params.user_id,
            "open_kfid": params.open_kfid,
            "msgtype": "text",
            "text": { "content": params.content }
        });
        match self
            .proxy
            .client()
            .api_post(&params.corp_id, &params.secret, "/cgi-bin/kf/send_msg", &body)
            .await
        {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "发起 AI 对话生成请求（企业微信 AI 助手）")]
    async fn wecom_bot_generate(
        &self,
        Parameters(_params): Parameters<WecomBotGenerateParams>,
    ) -> String {
        let url = format!(
            "{}/ai/qc/generate?source=wecom_cli_external&plat=1",
            "https://work.weixin.qq.com"
        );
        match self.proxy.client().http_get_json(&url).await {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "轮询 AI 对话生成结果")]
    async fn wecom_bot_poll(
        &self,
        Parameters(params): Parameters<WecomBotPollParams>,
    ) -> String {
        let url = format!(
            "{}/ai/qc/query_result?scode={}",
            "https://work.weixin.qq.com",
            params.scode
        );
        match self.proxy.client().http_get_json(&url).await {
            Ok(resp) => json_to_string(&resp),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "生成二维码图片 URL")]
    async fn wecom_qrcode(
        &self,
        Parameters(params): Parameters<WecomQrcodeParams>,
    ) -> String {
        let encoded = mcp_common::json::url_encode(&params.url);
        let qr_url = format!(
            "https://api.qrserver.com/v1/create-qr-code/?size=300x300&format=png&data={}",
            encoded
        );
        serde_json::json!({ "qrcode_url": qr_url }).to_string()
    }

    #[tool(description = "获取企业微信用户列表（通过 MCP 代理）")]
    async fn wecom_get_userlist(
        &self,
        Parameters(params): Parameters<WecomGetUserlistParams>,
    ) -> String {
        let arguments = serde_json::json!({});
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id,
                &params.secret,
                &params.bot_id,
                &params.bot_secret,
                "get_userlist",
                &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    // ================================================================== //
    //  MCP Proxy tools — Doc (18 tools)                                   //
    // ================================================================== //

    #[tool(description = "获取企业微信文档内容")]
    async fn wecom_get_doc_content(
        &self,
        Parameters(params): Parameters<WecomGetDocContentParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "docid": params.docid,
        });
        if let Some(url) = &params.url {
            arguments["url"] = Value::String(url.clone());
        }
        if let Some(dt) = params.doc_type {
            arguments["doc_type"] = Value::Number(dt.into());
        }
        if let Some(tid) = &params.task_id {
            arguments["task_id"] = Value::String(tid.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_doc_content", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建企业微信文档")]
    async fn wecom_create_doc(
        &self,
        Parameters(params): Parameters<WecomCreateDocParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "doc_type": params.doc_type,
            "doc_name": params.doc_name,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "create_doc", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "编辑企业微信文档内容")]
    async fn wecom_edit_doc_content(
        &self,
        Parameters(params): Parameters<WecomEditDocContentParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "docid": params.docid,
            "content": params.content,
        });
        if let Some(url) = &params.url {
            arguments["url"] = Value::String(url.clone());
        }
        if let Some(ct) = params.content_type {
            arguments["content_type"] = Value::Number(ct.into());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "edit_doc_content", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "发起智能文档导出任务")]
    async fn wecom_smartpage_export_task(
        &self,
        Parameters(params): Parameters<WecomSmartpageExportTaskParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "docid": params.docid,
        });
        if let Some(url) = &params.url {
            arguments["url"] = Value::String(url.clone());
        }
        if let Some(ct) = params.content_type {
            arguments["content_type"] = Value::Number(ct.into());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartpage_export_task", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取智能文档导出结果")]
    async fn wecom_smartpage_get_export_result(
        &self,
        Parameters(params): Parameters<WecomSmartpageGetExportResultParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "task_id": params.task_id,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartpage_get_export_result", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取智能表 Sheet 列表")]
    async fn wecom_smartsheet_get_sheet(
        &self,
        Parameters(params): Parameters<WecomSmartsheetGetSheetParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_get_sheet", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "新增智能表 Sheet")]
    async fn wecom_smartsheet_add_sheet(
        &self,
        Parameters(params): Parameters<WecomSmartsheetAddSheetParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "properties": params.properties,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_add_sheet", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新智能表 Sheet")]
    async fn wecom_smartsheet_update_sheet(
        &self,
        Parameters(params): Parameters<WecomSmartsheetUpdateSheetParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "properties": params.properties,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_update_sheet", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "删除智能表 Sheet")]
    async fn wecom_smartsheet_delete_sheet(
        &self,
        Parameters(params): Parameters<WecomSmartsheetDeleteSheetParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_delete_sheet", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取智能表字段列表")]
    async fn wecom_smartsheet_get_fields(
        &self,
        Parameters(params): Parameters<WecomSmartsheetGetFieldsParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_get_fields", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "新增智能表字段")]
    async fn wecom_smartsheet_add_fields(
        &self,
        Parameters(params): Parameters<WecomSmartsheetAddFieldsParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
            "fields": params.fields,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_add_fields", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新智能表字段")]
    async fn wecom_smartsheet_update_fields(
        &self,
        Parameters(params): Parameters<WecomSmartsheetUpdateFieldsParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
            "fields": params.fields,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_update_fields", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "删除智能表字段")]
    async fn wecom_smartsheet_delete_fields(
        &self,
        Parameters(params): Parameters<WecomSmartsheetDeleteFieldsParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
            "field_ids": params.field_ids,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_delete_fields", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取智能表记录")]
    async fn wecom_smartsheet_get_records(
        &self,
        Parameters(params): Parameters<WecomSmartsheetGetRecordsParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
        });
        if let Some(url) = &params.url {
            arguments["url"] = Value::String(url.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_get_records", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "新增智能表记录")]
    async fn wecom_smartsheet_add_records(
        &self,
        Parameters(params): Parameters<WecomSmartsheetAddRecordsParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
            "records": params.records,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_add_records", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新智能表记录")]
    async fn wecom_smartsheet_update_records(
        &self,
        Parameters(params): Parameters<WecomSmartsheetUpdateRecordsParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
            "records": params.records,
        });
        if let Some(kt) = &params.key_type {
            arguments["key_type"] = Value::String(kt.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_update_records", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "删除智能表记录")]
    async fn wecom_smartsheet_delete_records(
        &self,
        Parameters(params): Parameters<WecomSmartsheetDeleteRecordsParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "docid": params.docid,
            "sheet_id": params.sheet_id,
            "record_ids": params.record_ids,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "smartsheet_delete_records", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    // ================================================================== //
    //  MCP Proxy tools — Msg (3 tools)                                    //
    // ================================================================== //

    #[tool(description = "获取企业微信会话列表")]
    async fn wecom_get_msg_chat_list(
        &self,
        Parameters(params): Parameters<WecomGetMsgChatListParams>,
    ) -> String {
        let mut arguments = serde_json::json!({});
        if let Some(bt) = params.begin_time {
            arguments["begin_time"] = Value::Number(bt.into());
        }
        if let Some(et) = params.end_time {
            arguments["end_time"] = Value::Number(et.into());
        }
        if let Some(cursor) = &params.cursor {
            arguments["cursor"] = Value::String(cursor.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_msg_chat_list", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取企业微信会话消息")]
    async fn wecom_get_message(
        &self,
        Parameters(params): Parameters<WecomGetMessageParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "chat_type": params.chat_type,
            "chatid": params.chatid,
        });
        if let Some(bt) = params.begin_time {
            arguments["begin_time"] = Value::Number(bt.into());
        }
        if let Some(et) = params.end_time {
            arguments["end_time"] = Value::Number(et.into());
        }
        if let Some(cursor) = &params.cursor {
            arguments["cursor"] = Value::String(cursor.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_message", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "通过 MCP 代理发送企业微信消息")]
    async fn wecom_msg_send_message(
        &self,
        Parameters(params): Parameters<WecomMsgSendMessageParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "chat_type": params.chat_type,
            "chatid": params.chatid,
            "text": params.text,
        });
        if let Some(mt) = &params.msgtype {
            arguments["msgtype"] = Value::String(mt.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "send_message", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    // ================================================================== //
    //  MCP Proxy tools — Todo (6 tools)                                   //
    // ================================================================== //

    #[tool(description = "获取企业微信待办列表")]
    async fn wecom_get_todo_list(
        &self,
        Parameters(params): Parameters<WecomGetTodoListParams>,
    ) -> String {
        let mut arguments = serde_json::json!({});
        if let Some(v) = params.create_begin_time {
            arguments["create_begin_time"] = Value::Number(v.into());
        }
        if let Some(v) = params.create_end_time {
            arguments["create_end_time"] = Value::Number(v.into());
        }
        if let Some(v) = params.remind_begin_time {
            arguments["remind_begin_time"] = Value::Number(v.into());
        }
        if let Some(v) = params.remind_end_time {
            arguments["remind_end_time"] = Value::Number(v.into());
        }
        if let Some(v) = params.limit {
            arguments["limit"] = Value::Number(v.into());
        }
        if let Some(v) = &params.cursor {
            arguments["cursor"] = Value::String(v.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_todo_list", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取企业微信待办详情")]
    async fn wecom_get_todo_detail(
        &self,
        Parameters(params): Parameters<WecomGetTodoDetailParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "todo_id_list": params.todo_id_list,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_todo_detail", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建企业微信待办")]
    async fn wecom_create_todo(
        &self,
        Parameters(params): Parameters<WecomCreateTodoParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "content": params.content,
        });
        if let Some(v) = &params.follower_list {
            arguments["follower_list"] = serde_json::to_value(v).unwrap_or(Value::Null);
        }
        if let Some(v) = params.remind_time {
            arguments["remind_time"] = Value::Number(v.into());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "create_todo", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新企业微信待办")]
    async fn wecom_update_todo(
        &self,
        Parameters(params): Parameters<WecomUpdateTodoParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "todo_id": params.todo_id,
        });
        if let Some(v) = &params.content {
            arguments["content"] = Value::String(v.clone());
        }
        if let Some(v) = &params.follower_list {
            arguments["follower_list"] = serde_json::to_value(v).unwrap_or(Value::Null);
        }
        if let Some(v) = params.todo_status {
            arguments["todo_status"] = Value::Number(v.into());
        }
        if let Some(v) = params.remind_time {
            arguments["remind_time"] = Value::Number(v.into());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "update_todo", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "删除企业微信待办")]
    async fn wecom_delete_todo(
        &self,
        Parameters(params): Parameters<WecomDeleteTodoParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "todo_id": params.todo_id,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "delete_todo", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更改企业微信待办用户状态")]
    async fn wecom_change_todo_user_status(
        &self,
        Parameters(params): Parameters<WecomChangeTodoUserStatusParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "todo_id": params.todo_id,
            "user_status": params.user_status,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "change_todo_user_status", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    // ================================================================== //
    //  MCP Proxy tools — Meeting (5 tools)                                //
    // ================================================================== //

    #[tool(description = "创建企业微信会议")]
    async fn wecom_create_meeting(
        &self,
        Parameters(params): Parameters<WecomCreateMeetingParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "title": params.title,
            "meeting_start_datetime": params.meeting_start_datetime,
            "meeting_duration": params.meeting_duration,
        });
        if let Some(v) = &params.description {
            arguments["description"] = Value::String(v.clone());
        }
        if let Some(v) = &params.location {
            arguments["location"] = Value::String(v.clone());
        }
        if let Some(v) = &params.invitees {
            arguments["invitees"] = v.clone();
        }
        if let Some(v) = &params.settings {
            arguments["settings"] = v.clone();
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "create_meeting", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取用户会议列表")]
    async fn wecom_list_user_meetings(
        &self,
        Parameters(params): Parameters<WecomListUserMeetingsParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "begin_datetime": params.begin_datetime,
            "end_datetime": params.end_datetime,
        });
        if let Some(v) = &params.cursor {
            arguments["cursor"] = Value::String(v.clone());
        }
        if let Some(v) = params.limit {
            arguments["limit"] = Value::Number(v.into());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "list_user_meetings", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取会议详情")]
    async fn wecom_get_meeting_info(
        &self,
        Parameters(params): Parameters<WecomGetMeetingInfoParams>,
    ) -> String {
        let mut arguments = serde_json::json!({
            "meetingid": params.meetingid,
        });
        if let Some(v) = &params.meeting_code {
            arguments["meeting_code"] = Value::String(v.clone());
        }
        if let Some(v) = &params.sub_meetingid {
            arguments["sub_meetingid"] = Value::String(v.clone());
        }
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_meeting_info", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "取消企业微信会议")]
    async fn wecom_cancel_meeting(
        &self,
        Parameters(params): Parameters<WecomCancelMeetingParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "meetingid": params.meetingid,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "cancel_meeting", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "设置会议邀请成员")]
    async fn wecom_set_invite_meeting_members(
        &self,
        Parameters(params): Parameters<WecomSetInviteMeetingMembersParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "meetingid": params.meetingid,
            "invitees": params.invitees,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "set_invite_meeting_members", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    // ================================================================== //
    //  MCP Proxy tools — Schedule (8 tools)                               //
    // ================================================================== //

    #[tool(description = "按时间范围获取企业微信日程列表")]
    async fn wecom_get_schedule_list_by_range(
        &self,
        Parameters(params): Parameters<WecomGetScheduleListByRangeParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "start_time": params.start_time,
            "end_time": params.end_time,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_schedule_list_by_range", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取企业微信日程详情")]
    async fn wecom_get_schedule_detail(
        &self,
        Parameters(params): Parameters<WecomGetScheduleDetailParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "schedule_id_list": params.schedule_id_list,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "get_schedule_detail", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建企业微信日程")]
    async fn wecom_create_schedule(
        &self,
        Parameters(params): Parameters<WecomCreateScheduleParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "schedule": params.schedule,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "create_schedule", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新企业微信日程")]
    async fn wecom_update_schedule(
        &self,
        Parameters(params): Parameters<WecomUpdateScheduleParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "schedule": params.schedule,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "update_schedule", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "取消企业微信日程")]
    async fn wecom_cancel_schedule(
        &self,
        Parameters(params): Parameters<WecomCancelScheduleParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "schedule_id": params.schedule_id,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "cancel_schedule", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "添加企业微信日程参会人")]
    async fn wecom_add_schedule_attendees(
        &self,
        Parameters(params): Parameters<WecomAddScheduleAttendeesParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "schedule_id": params.schedule_id,
            "attendees": params.attendees,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "add_schedule_attendees", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "移除企业微信日程参会人")]
    async fn wecom_del_schedule_attendees(
        &self,
        Parameters(params): Parameters<WecomDelScheduleAttendeesParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "schedule_id": params.schedule_id,
            "attendees": params.attendees,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "del_schedule_attendees", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "检查企业微信用户日程空闲状态")]
    async fn wecom_check_availability(
        &self,
        Parameters(params): Parameters<WecomCheckAvailabilityParams>,
    ) -> String {
        let arguments = serde_json::json!({
            "check_user_list": params.check_user_list,
            "start_time": params.start_time,
            "end_time": params.end_time,
        });
        match self
            .proxy
            .call_mcp_tool(
                &params.corp_id, &params.secret,
                &params.bot_id, &params.bot_secret,
                "check_availability", &arguments,
            )
            .await
        {
            Ok(result) => truncate_result(result, 65536),
            Err(e) => error_response(&e),
        }
    }
}

// ================================================================== //
//  Entry point                                                         //
// ================================================================== //

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log to stderr — stdout is reserved for the MCP protocol.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("WECOM_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let proxy = WecomMcpProxy::new();
    let server = WecomServer {
        proxy: Arc::new(proxy),
    };

    tracing::info!("wecom-mcp starting (stdio, multi-tenant, 47 tools)");
    let service = server.serve(stdio_transport()).await?;
    service.waiting().await?;

    Ok(())
}
