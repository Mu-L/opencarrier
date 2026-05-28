//! feishu-mcp — Feishu/Lark MCP Server (multi-tenant)
//!
//! Each tool call carries `app_id` and `app_secret`, allowing a single MCP
//! server process to serve multiple Feishu apps simultaneously.
//!
//! Tenant access tokens are cached per `app_id` and auto-refreshed.
//!
//! # Usage
//!
//! No environment variables needed — credentials are passed per tool call.
//! Each OpenCarrier clone stores its own Feishu credentials in its
//! knowledge/config and passes them when invoking tools.

mod feishu;

use std::sync::Arc;

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, transport::stdio as stdio_transport, ServiceExt};
use serde_json::Value;

use feishu::FeishuClient;
use mcp_common::define_app_params;
use mcp_common::json::{error_response, json_to_string, truncate_result};

// ================================================================== //
//  Tool parameter structs                                              //
//  Every struct carries app_id + app_secret for multi-tenant support. //
// ================================================================== //

// ---- IM ----

define_app_params!(SendMessageParams {
    #[schemars(description = "接收者ID")]
    receive_id: String,
    #[schemars(description = "接收者ID类型: open_id, user_id, union_id, email, chat_id")]
    receive_id_type: String,
    #[schemars(description = "消息类型: text, post, image, file, etc.")]
    msg_type: String,
    #[schemars(description = "消息内容JSON字符串")]
    content: String,
});

define_app_params!(ReplyMessageParams {
    #[schemars(description = "要回复的消息ID")]
    message_id: String,
    #[schemars(description = "消息类型")]
    msg_type: String,
    #[schemars(description = "消息内容JSON字符串")]
    content: String,
});

define_app_params!(SearchMessagesParams {
    #[schemars(description = "搜索关键词")]
    query: String,
    #[schemars(description = "群聊ID")]
    chat_id: Option<String>,
    #[schemars(description = "消息类型过滤")]
    message_type: Option<String>,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

define_app_params!(ListMessagesParams {
    #[schemars(description = "容器ID")]
    container_id: String,
    #[schemars(description = "容器ID类型: chat")]
    container_id_type: Option<String>,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(CreateChatParams {
    #[schemars(description = "群名")]
    name: String,
    #[schemars(description = "群描述")]
    description: Option<String>,
    #[schemars(description = "群模式")]
    chat_mode: Option<String>,
    #[schemars(description = "群类型")]
    chat_type: Option<String>,
    #[schemars(description = "群成员用户ID列表")]
    user_id_list: Option<Vec<String>>,
});

define_app_params!(UpdateChatParams {
    #[schemars(description = "群聊ID")]
    chat_id: String,
    #[schemars(description = "群名")]
    name: Option<String>,
    #[schemars(description = "群描述")]
    description: Option<String>,
});

define_app_params!(ListChatsParams {
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(DownloadResourceParams {
    #[schemars(description = "消息ID")]
    message_id: String,
    #[schemars(description = "文件key")]
    file_key: String,
    #[schemars(description = "资源类型: image, file, video")]
    r#type: String,
});

// ---- Doc ----

define_app_params!(CreateDocParams {
    #[schemars(description = "文档标题")]
    title: String,
    #[schemars(description = "文件夹token")]
    folder_token: Option<String>,
});

define_app_params!(GetDocParams {
    #[schemars(description = "文档ID")]
    document_id: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(UpdateDocParams {
    #[schemars(description = "文档ID")]
    document_id: String,
    #[schemars(description = "更新请求列表(JSON)")]
    requests: Value,
});

define_app_params!(SearchDocsParams {
    #[schemars(description = "搜索关键词")]
    search_key: String,
    #[schemars(description = "所有者ID列表")]
    owner_ids: Option<Vec<String>>,
    #[schemars(description = "群聊ID列表")]
    chat_ids: Option<Vec<String>>,
    #[schemars(description = "每页数量")]
    count: Option<i64>,
    #[schemars(description = "偏移量")]
    offset: Option<i64>,
});

define_app_params!(GetDocRawParams {
    #[schemars(description = "文档ID")]
    document_id: String,
});

define_app_params!(UpdateDocRawParams {
    #[schemars(description = "文档ID")]
    document_id: String,
    #[schemars(description = "文档内容")]
    content: String,
});

// ---- Sheets ----

define_app_params!(CreateSheetParams {
    #[schemars(description = "表格标题")]
    title: String,
    #[schemars(description = "文件夹token")]
    folder_token: Option<String>,
});

define_app_params!(ReadSheetParams {
    #[schemars(description = "表格token")]
    spreadsheet_token: String,
    #[schemars(description = "读取范围(如 Sheet1!A1:B2)")]
    range: String,
});

define_app_params!(WriteSheetParams {
    #[schemars(description = "表格token")]
    spreadsheet_token: String,
    #[schemars(description = "写入范围")]
    range: String,
    #[schemars(description = "写入数据(JSON)")]
    values: Value,
});

define_app_params!(AppendSheetParams {
    #[schemars(description = "表格token")]
    spreadsheet_token: String,
    #[schemars(description = "追加范围")]
    range: String,
    #[schemars(description = "追加数据(JSON)")]
    values: Value,
});

define_app_params!(FindInSheetParams {
    #[schemars(description = "表格token")]
    spreadsheet_token: String,
    #[schemars(description = "工作表ID")]
    sheet_id: String,
    #[schemars(description = "查找条件(JSON)")]
    find_condition: Value,
});

define_app_params!(ExportSheetParams {
    #[schemars(description = "表格token")]
    spreadsheet_token: String,
    #[schemars(description = "工作表ID")]
    sheet_id: Option<String>,
    #[schemars(description = "导出格式: xlsx, csv")]
    export_format: Option<String>,
});

// ---- Base/Bitable ----

define_app_params!(ListTablesParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(CreateTableParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "表格定义(JSON)")]
    table: Value,
});

define_app_params!(ListFieldsParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "数据表ID")]
    table_id: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

define_app_params!(CreateFieldParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "数据表ID")]
    table_id: String,
    #[schemars(description = "字段名")]
    field_name: String,
    #[schemars(description = "字段类型")]
    field_type: i64,
});

define_app_params!(ListRecordsParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "数据表ID")]
    table_id: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
    #[schemars(description = "过滤条件")]
    filter: Option<String>,
    #[schemars(description = "排序条件")]
    sort: Option<String>,
});

define_app_params!(CreateRecordParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "数据表ID")]
    table_id: String,
    #[schemars(description = "记录字段(JSON)")]
    fields: Value,
});

define_app_params!(UpdateRecordParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "数据表ID")]
    table_id: String,
    #[schemars(description = "记录ID")]
    record_id: String,
    #[schemars(description = "记录字段(JSON)")]
    fields: Value,
});

define_app_params!(DeleteRecordParams {
    #[schemars(description = "多维表格app_token")]
    app_token: String,
    #[schemars(description = "数据表ID")]
    table_id: String,
    #[schemars(description = "记录ID")]
    record_id: String,
});

// ---- Calendar ----

define_app_params!(CreateEventParams {
    #[schemars(description = "日历ID")]
    calendar_id: String,
    #[schemars(description = "事件标题")]
    summary: String,
    #[schemars(description = "事件描述")]
    description: Option<String>,
    #[schemars(description = "开始时间(ISO格式)")]
    start_time: Option<String>,
    #[schemars(description = "结束时间(ISO格式)")]
    end_time: Option<String>,
    #[schemars(description = "参会者(JSON)")]
    attendees: Option<Value>,
    #[schemars(description = "可见性: default, public, private")]
    visibility: Option<String>,
    #[schemars(description = "提醒设置(JSON)")]
    reminders: Option<Value>,
});

define_app_params!(GetEventParams {
    #[schemars(description = "日历ID")]
    calendar_id: String,
    #[schemars(description = "事件ID")]
    event_id: String,
});

define_app_params!(ListEventsParams {
    #[schemars(description = "日历ID")]
    calendar_id: String,
    #[schemars(description = "开始时间(ISO格式)")]
    start_time: String,
    #[schemars(description = "结束时间(ISO格式)")]
    end_time: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(UpdateEventParams {
    #[schemars(description = "日历ID")]
    calendar_id: String,
    #[schemars(description = "事件ID")]
    event_id: String,
    #[schemars(description = "事件标题")]
    summary: Option<String>,
    #[schemars(description = "事件描述")]
    description: Option<String>,
    #[schemars(description = "开始时间(ISO格式)")]
    start_time: Option<String>,
    #[schemars(description = "结束时间(ISO格式)")]
    end_time: Option<String>,
});

define_app_params!(FreebusyParams {
    #[schemars(description = "查询开始时间(ISO格式)")]
    time_min: String,
    #[schemars(description = "查询结束时间(ISO格式)")]
    time_max: String,
    #[schemars(description = "用户ID")]
    user_id: Option<String>,
});

define_app_params!(SearchEventParams {
    #[schemars(description = "搜索关键词")]
    query: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

// ---- Drive ----

define_app_params!(UploadFileParams {
    #[schemars(description = "父节点")]
    parent_node: String,
    #[schemars(description = "文件名")]
    file_name: String,
    #[schemars(description = "文件类型")]
    file_type: Option<String>,
    #[schemars(description = "标题")]
    title: Option<String>,
});

define_app_params!(DownloadFileParams {
    #[schemars(description = "文件token")]
    file_token: String,
    #[schemars(description = "文件类型")]
    file_type: Option<String>,
});

define_app_params!(CreateFolderParams {
    #[schemars(description = "父文件夹token")]
    parent_token: String,
    #[schemars(description = "文件夹名")]
    name: String,
    #[schemars(description = "文件夹类型")]
    folder_type: Option<String>,
});

define_app_params!(SearchDriveParams {
    #[schemars(description = "搜索关键词")]
    search_key: String,
    #[schemars(description = "所有者ID列表")]
    owner_ids: Option<Vec<String>>,
    #[schemars(description = "每页数量")]
    count: Option<i64>,
    #[schemars(description = "偏移量")]
    offset: Option<i64>,
});

define_app_params!(MoveFileParams {
    #[schemars(description = "文件token")]
    file_token: String,
    #[schemars(description = "文件类型")]
    file_type: Option<String>,
    #[schemars(description = "目标文件夹token")]
    folder_token: String,
});

define_app_params!(DeleteFileParams {
    #[schemars(description = "文件token")]
    file_token: String,
    #[schemars(description = "文件类型")]
    file_type: Option<String>,
});

// ---- Contact ----

define_app_params!(SearchUserParams {
    #[schemars(description = "搜索关键词")]
    query: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

define_app_params!(GetUserParams {
    #[schemars(description = "用户ID")]
    user_id: String,
    #[schemars(description = "用户ID类型: open_id, user_id, union_id")]
    user_id_type: Option<String>,
});

// ---- Task ----

define_app_params!(CreateTaskParams {
    #[schemars(description = "任务标题")]
    summary: String,
    #[schemars(description = "任务描述")]
    description: Option<String>,
    #[schemars(description = "截止时间(时间戳)")]
    due_date: Option<String>,
    #[schemars(description = "负责人(JSON)")]
    assignees: Option<Value>,
});

define_app_params!(GetTaskParams {
    #[schemars(description = "任务ID")]
    task_id: String,
});

define_app_params!(UpdateTaskParams {
    #[schemars(description = "任务ID")]
    task_id: String,
    #[schemars(description = "任务标题")]
    summary: Option<String>,
    #[schemars(description = "任务描述")]
    description: Option<String>,
    #[schemars(description = "截止时间(时间戳)")]
    due_date: Option<String>,
});

define_app_params!(CompleteTaskParams {
    #[schemars(description = "任务ID")]
    task_id: String,
});

define_app_params!(ListMyTasksParams {
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
    #[schemars(description = "开始时间")]
    start_time: Option<String>,
    #[schemars(description = "结束时间")]
    end_time: Option<String>,
});

define_app_params!(SearchTasksParams {
    #[schemars(description = "搜索关键词")]
    query: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

// ---- Mail ----

define_app_params!(SendMailParams {
    #[schemars(description = "邮箱ID")]
    mailbox_id: String,
    #[schemars(description = "邮件主题")]
    subject: String,
    #[schemars(description = "邮件内容")]
    content: String,
    #[schemars(description = "收件人列表")]
    to: Vec<String>,
    #[schemars(description = "抄送列表")]
    cc: Option<Vec<String>>,
    #[schemars(description = "密送列表")]
    bcc: Option<Vec<String>>,
    #[schemars(description = "回复邮件ID")]
    reply_to_mail_id: Option<String>,
});

define_app_params!(ListMailParams {
    #[schemars(description = "邮箱ID")]
    mailbox_id: String,
    #[schemars(description = "文件夹ID")]
    folder_id: Option<String>,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(GetMailParams {
    #[schemars(description = "邮箱ID")]
    mailbox_id: String,
    #[schemars(description = "邮件ID")]
    message_id: String,
});

define_app_params!(ReplyMailParams {
    #[schemars(description = "邮箱ID")]
    mailbox_id: String,
    #[schemars(description = "原邮件ID")]
    message_id: String,
    #[schemars(description = "回复内容")]
    content: String,
    #[schemars(description = "是否回复全部")]
    reply_all: Option<bool>,
});

define_app_params!(ForwardMailParams {
    #[schemars(description = "邮箱ID")]
    mailbox_id: String,
    #[schemars(description = "原邮件ID")]
    message_id: String,
    #[schemars(description = "转发收件人列表")]
    to: Vec<String>,
    #[schemars(description = "附言")]
    content: Option<String>,
});

define_app_params!(TriageMailParams {
    #[schemars(description = "邮箱ID")]
    mailbox_id: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

// ---- VC ----

define_app_params!(SearchMeetingParams {
    #[schemars(description = "搜索关键词")]
    query: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

define_app_params!(GetMeetingParams {
    #[schemars(description = "会议ID")]
    meeting_id: String,
});

define_app_params!(GetRecordingParams {
    #[schemars(description = "会议ID")]
    meeting_id: String,
});

// ---- Wiki ----

define_app_params!(ListSpacesParams {
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(CreateNodeParams {
    #[schemars(description = "空间ID")]
    space_id: String,
    #[schemars(description = "节点类型")]
    node_type: String,
    #[schemars(description = "节点标题")]
    title: String,
    #[schemars(description = "父节点token")]
    parent_node_token: Option<String>,
});

define_app_params!(GetNodeParams {
    #[schemars(description = "节点token")]
    token: String,
});

// ---- Approval ----

define_app_params!(GetApprovalParams {
    #[schemars(description = "审批实例ID")]
    instance_id: String,
    #[schemars(description = "用户ID类型")]
    user_id_type: Option<String>,
});

define_app_params!(ApproveTaskParams {
    #[schemars(description = "任务ID")]
    task_id: String,
    #[schemars(description = "用户ID")]
    user_id: String,
    #[schemars(description = "审批意见")]
    comment: Option<String>,
});

define_app_params!(RejectTaskParams {
    #[schemars(description = "任务ID")]
    task_id: String,
    #[schemars(description = "用户ID")]
    user_id: String,
    #[schemars(description = "驳回意见")]
    comment: Option<String>,
});

define_app_params!(ListApprovalsParams {
    #[schemars(description = "审批定义Code")]
    approval_code: String,
    #[schemars(description = "开始时间(时间戳)")]
    start_time: String,
    #[schemars(description = "结束时间(时间戳)")]
    end_time: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

// ---- OKR ----

define_app_params!(ListOkrCyclesParams {
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
    #[schemars(description = "分页标记")]
    page_token: Option<String>,
});

define_app_params!(GetOkrDetailParams {
    #[schemars(description = "周期ID")]
    cycle_id: String,
    #[schemars(description = "用户ID")]
    user_id: Option<String>,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

// ---- Attendance ----

define_app_params!(GetAttendanceParams {
    #[schemars(description = "用户ID列表")]
    user_ids: Vec<String>,
    #[schemars(description = "开始时间")]
    start_time: String,
    #[schemars(description = "结束时间")]
    end_time: String,
});

// ---- Slides ----

define_app_params!(CreateSlidesParams {
    #[schemars(description = "标题")]
    title: String,
    #[schemars(description = "文件夹token")]
    folder_token: Option<String>,
});

define_app_params!(ReplaceSlideParams {
    #[schemars(description = "演示文稿ID")]
    presentation_id: String,
    #[schemars(description = "幻灯片ID")]
    slide_id: String,
    #[schemars(description = "替换内容(JSON)")]
    replacements: Value,
});

// ---- Minutes ----

define_app_params!(SearchMinutesParams {
    #[schemars(description = "搜索关键词")]
    query: String,
    #[schemars(description = "每页数量")]
    page_size: Option<i64>,
});

define_app_params!(GetMinutesParams {
    #[schemars(description = "妙记ID")]
    minutes_id: String,
});

// ---- Whiteboard ----

define_app_params!(QueryWhiteboardParams {
    #[schemars(description = "白板ID")]
    whiteboard_id: String,
});

define_app_params!(UpdateWhiteboardParams {
    #[schemars(description = "白板ID")]
    whiteboard_id: String,
    #[schemars(description = "白板标题")]
    title: String,
});

// ================================================================== //
//  MCP Server                                                         //
// ================================================================== //

#[derive(Clone)]
struct FeishuServer {
    client: Arc<FeishuClient>,
}

#[tool_router(server_handler)]
impl FeishuServer {
    // ==== IM (8 tools) ====

    #[tool(description = "发送飞书消息")]
    async fn feishu_send_message(
        &self,
        Parameters(params): Parameters<SendMessageParams>,
    ) -> String {
        let path = format!(
            "open-apis/im/v1/messages?receive_id_type={}",
            params.receive_id_type
        );
        let body = serde_json::json!({
            "receive_id": params.receive_id,
            "msg_type": params.msg_type,
            "content": params.content,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "回复飞书消息")]
    async fn feishu_reply_message(
        &self,
        Parameters(params): Parameters<ReplyMessageParams>,
    ) -> String {
        let path = format!("open-apis/im/v1/messages/{}/reply", params.message_id);
        let body = serde_json::json!({
            "content": params.content,
            "msg_type": params.msg_type,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "搜索飞书消息")]
    async fn feishu_search_messages(
        &self,
        Parameters(params): Parameters<SearchMessagesParams>,
    ) -> String {
        let path = "open-apis/im/v1/messages/search";
        let mut body = serde_json::json!({
            "query": params.query,
        });
        if let Some(chat_id) = &params.chat_id {
            body["chat_id"] = Value::String(chat_id.clone());
        }
        if let Some(message_type) = &params.message_type {
            body["message_type"] = Value::String(message_type.clone());
        }
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书聊天消息列表")]
    async fn feishu_list_messages(
        &self,
        Parameters(params): Parameters<ListMessagesParams>,
    ) -> String {
        let path = "open-apis/im/v1/messages";
        let mut query = serde_json::json!({
            "container_id": params.container_id,
        });
        if let Some(container_id_type) = &params.container_id_type {
            query["container_id_type"] = Value::String(container_id_type.clone());
        }
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建飞书群聊")]
    async fn feishu_create_chat(
        &self,
        Parameters(params): Parameters<CreateChatParams>,
    ) -> String {
        let path = "open-apis/im/v1/chats";
        let mut body = serde_json::json!({
            "name": params.name,
        });
        if let Some(description) = &params.description {
            body["description"] = Value::String(description.clone());
        }
        if let Some(chat_mode) = &params.chat_mode {
            body["chat_mode"] = Value::String(chat_mode.clone());
        }
        if let Some(chat_type) = &params.chat_type {
            body["chat_type"] = Value::String(chat_type.clone());
        }
        if let Some(user_id_list) = &params.user_id_list {
            body["user_id_list"] = serde_json::to_value(user_id_list).unwrap_or(Value::Null);
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新飞书群聊信息")]
    async fn feishu_update_chat(
        &self,
        Parameters(params): Parameters<UpdateChatParams>,
    ) -> String {
        let path = format!("open-apis/im/v1/chats/{}", params.chat_id);
        let mut body = serde_json::json!({});
        if let Some(name) = &params.name {
            body["name"] = Value::String(name.clone());
        }
        if let Some(description) = &params.description {
            body["description"] = Value::String(description.clone());
        }
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PUT,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书群聊列表")]
    async fn feishu_list_chats(
        &self,
        Parameters(params): Parameters<ListChatsParams>,
    ) -> String {
        let path = "open-apis/im/v1/chats";
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "下载飞书消息中的资源文件")]
    async fn feishu_download_resource(
        &self,
        Parameters(params): Parameters<DownloadResourceParams>,
    ) -> String {
        let path = format!(
            "open-apis/im/v1/messages/{}/resources/{}",
            params.message_id, params.file_key
        );
        let query = serde_json::json!({
            "type": params.r#type,
        });
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Doc (6 tools) ====

    #[tool(description = "创建飞书文档")]
    async fn feishu_create_doc(
        &self,
        Parameters(params): Parameters<CreateDocParams>,
    ) -> String {
        let path = "open-apis/docx/v1/documents";
        let mut body = serde_json::json!({
            "title": params.title,
        });
        if let Some(folder_token) = &params.folder_token {
            body["folder_token"] = Value::String(folder_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书文档内容和块列表")]
    async fn feishu_get_doc(
        &self,
        Parameters(params): Parameters<GetDocParams>,
    ) -> String {
        let path = format!(
            "open-apis/docx/v1/documents/{}/blocks",
            params.document_id
        );
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "批量更新飞书文档块")]
    async fn feishu_update_doc(
        &self,
        Parameters(params): Parameters<UpdateDocParams>,
    ) -> String {
        let path = format!(
            "open-apis/docx/v1/documents/{}/blocks/batch_update",
            params.document_id
        );
        let body = serde_json::json!({
            "requests": params.requests,
        });
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PATCH,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "搜索飞书文档")]
    async fn feishu_search_docs(
        &self,
        Parameters(params): Parameters<SearchDocsParams>,
    ) -> String {
        let path = "open-apis/suite/docs/search";
        let mut body = serde_json::json!({
            "search_key": params.search_key,
        });
        if let Some(owner_ids) = &params.owner_ids {
            body["owner_ids"] = serde_json::to_value(owner_ids).unwrap_or(Value::Null);
        }
        if let Some(chat_ids) = &params.chat_ids {
            body["chat_ids"] = serde_json::to_value(chat_ids).unwrap_or(Value::Null);
        }
        if let Some(count) = params.count {
            body["count"] = Value::Number(count.into());
        }
        if let Some(offset) = params.offset {
            body["offset"] = Value::Number(offset.into());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书旧版文档原始内容")]
    async fn feishu_get_doc_raw(
        &self,
        Parameters(params): Parameters<GetDocRawParams>,
    ) -> String {
        let path = format!("open-apis/doc/v2/documents/{}", params.document_id);
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新飞书旧版文档原始内容")]
    async fn feishu_update_doc_raw(
        &self,
        Parameters(params): Parameters<UpdateDocRawParams>,
    ) -> String {
        let path = format!("open-apis/doc/v2/documents/{}", params.document_id);
        let body = serde_json::json!({
            "content": params.content,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Sheets (6 tools) ====

    #[tool(description = "创建飞书电子表格")]
    async fn feishu_create_sheet(
        &self,
        Parameters(params): Parameters<CreateSheetParams>,
    ) -> String {
        let path = "open-apis/sheets/v3/spreadsheets";
        let mut body = serde_json::json!({
            "title": params.title,
        });
        if let Some(folder_token) = &params.folder_token {
            body["folder_token"] = Value::String(folder_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "读取飞书电子表格数据")]
    async fn feishu_read_sheet(
        &self,
        Parameters(params): Parameters<ReadSheetParams>,
    ) -> String {
        let path = format!(
            "open-apis/sheets/v2/spreadsheets/{}/values/{}",
            params.spreadsheet_token, params.range
        );
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "写入飞书电子表格数据")]
    async fn feishu_write_sheet(
        &self,
        Parameters(params): Parameters<WriteSheetParams>,
    ) -> String {
        let path = format!(
            "open-apis/sheets/v2/spreadsheets/{}/values",
            params.spreadsheet_token
        );
        let body = serde_json::json!({
            "range": params.range,
            "values": params.values,
        });
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PUT,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "追加飞书电子表格数据")]
    async fn feishu_append_sheet(
        &self,
        Parameters(params): Parameters<AppendSheetParams>,
    ) -> String {
        let path = format!(
            "open-apis/sheets/v2/spreadsheets/{}/values_append",
            params.spreadsheet_token
        );
        let body = serde_json::json!({
            "range": params.range,
            "values": params.values,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "在飞书电子表格中查找数据")]
    async fn feishu_find_in_sheet(
        &self,
        Parameters(params): Parameters<FindInSheetParams>,
    ) -> String {
        let path = format!(
            "open-apis/sheets/v2/spreadsheets/{}/sheets/{}/find",
            params.spreadsheet_token, params.sheet_id
        );
        let body = serde_json::json!({
            "find_condition": params.find_condition,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "导出飞书电子表格")]
    async fn feishu_export_sheet(
        &self,
        Parameters(params): Parameters<ExportSheetParams>,
    ) -> String {
        let path = format!(
            "open-apis/sheets/v2/spreadsheets/{}/export",
            params.spreadsheet_token
        );
        let mut body = serde_json::json!({});
        if let Some(sheet_id) = &params.sheet_id {
            body["sheet_id"] = Value::String(sheet_id.clone());
        }
        if let Some(export_format) = &params.export_format {
            body["export_format"] = Value::String(export_format.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Base/Bitable (8 tools) ====

    #[tool(description = "获取飞书多维表格数据表列表")]
    async fn feishu_list_tables(
        &self,
        Parameters(params): Parameters<ListTablesParams>,
    ) -> String {
        let path = format!("open-apis/bitable/v1/apps/{}/tables", params.app_token);
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建飞书多维表格数据表")]
    async fn feishu_create_table(
        &self,
        Parameters(params): Parameters<CreateTableParams>,
    ) -> String {
        let path = format!("open-apis/bitable/v1/apps/{}/tables", params.app_token);
        let body = serde_json::json!({
            "table": params.table,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书多维表格字段列表")]
    async fn feishu_list_fields(
        &self,
        Parameters(params): Parameters<ListFieldsParams>,
    ) -> String {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/fields",
            params.app_token, params.table_id
        );
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建飞书多维表格字段")]
    async fn feishu_create_field(
        &self,
        Parameters(params): Parameters<CreateFieldParams>,
    ) -> String {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/fields",
            params.app_token, params.table_id
        );
        let body = serde_json::json!({
            "field_name": params.field_name,
            "type": params.field_type,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书多维表格记录列表")]
    async fn feishu_list_records(
        &self,
        Parameters(params): Parameters<ListRecordsParams>,
    ) -> String {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/records",
            params.app_token, params.table_id
        );
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        if let Some(filter) = &params.filter {
            query["filter"] = Value::String(filter.clone());
        }
        if let Some(sort) = &params.sort {
            query["sort"] = Value::String(sort.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建飞书多维表格记录")]
    async fn feishu_create_record(
        &self,
        Parameters(params): Parameters<CreateRecordParams>,
    ) -> String {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/records",
            params.app_token, params.table_id
        );
        let body = serde_json::json!({
            "fields": params.fields,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新飞书多维表格记录")]
    async fn feishu_update_record(
        &self,
        Parameters(params): Parameters<UpdateRecordParams>,
    ) -> String {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/records/{}",
            params.app_token, params.table_id, params.record_id
        );
        let body = serde_json::json!({
            "fields": params.fields,
        });
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PUT,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "删除飞书多维表格记录")]
    async fn feishu_delete_record(
        &self,
        Parameters(params): Parameters<DeleteRecordParams>,
    ) -> String {
        let path = format!(
            "open-apis/bitable/v1/apps/{}/tables/{}/records/{}",
            params.app_token, params.table_id, params.record_id
        );
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::DELETE,
                &path,
                None,
                None,
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Calendar (6 tools) ====

    #[tool(description = "创建飞书日历事件")]
    async fn feishu_create_event(
        &self,
        Parameters(params): Parameters<CreateEventParams>,
    ) -> String {
        let path = format!(
            "open-apis/calendar/v4/calendars/{}/events",
            params.calendar_id
        );
        let mut body = serde_json::json!({
            "summary": params.summary,
        });
        if let Some(description) = &params.description {
            body["description"] = Value::String(description.clone());
        }
        if let Some(start_time) = &params.start_time {
            body["start_time"] = serde_json::json!({ "timestamp": start_time });
        }
        if let Some(end_time) = &params.end_time {
            body["end_time"] = serde_json::json!({ "timestamp": end_time });
        }
        if let Some(attendees) = &params.attendees {
            body["attendees"] = attendees.clone();
        }
        if let Some(visibility) = &params.visibility {
            body["visibility"] = Value::String(visibility.clone());
        }
        if let Some(reminders) = &params.reminders {
            body["reminders"] = reminders.clone();
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书日历事件详情")]
    async fn feishu_get_event(
        &self,
        Parameters(params): Parameters<GetEventParams>,
    ) -> String {
        let path = format!(
            "open-apis/calendar/v4/calendars/{}/events/{}",
            params.calendar_id, params.event_id
        );
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书日历事件列表")]
    async fn feishu_list_events(
        &self,
        Parameters(params): Parameters<ListEventsParams>,
    ) -> String {
        let path = format!(
            "open-apis/calendar/v4/calendars/{}/events",
            params.calendar_id
        );
        let mut query = serde_json::json!({
            "start_time": params.start_time,
            "end_time": params.end_time,
        });
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新飞书日历事件")]
    async fn feishu_update_event(
        &self,
        Parameters(params): Parameters<UpdateEventParams>,
    ) -> String {
        let path = format!(
            "open-apis/calendar/v4/calendars/{}/events/{}",
            params.calendar_id, params.event_id
        );
        let mut body = serde_json::json!({});
        if let Some(summary) = &params.summary {
            body["summary"] = Value::String(summary.clone());
        }
        if let Some(description) = &params.description {
            body["description"] = Value::String(description.clone());
        }
        if let Some(start_time) = &params.start_time {
            body["start_time"] = serde_json::json!({ "timestamp": start_time });
        }
        if let Some(end_time) = &params.end_time {
            body["end_time"] = serde_json::json!({ "timestamp": end_time });
        }
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PATCH,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "查询飞书日历忙闲信息")]
    async fn feishu_freebusy(
        &self,
        Parameters(params): Parameters<FreebusyParams>,
    ) -> String {
        let path = "open-apis/calendar/v4/freebusy/list";
        let mut body = serde_json::json!({
            "time_min": params.time_min,
            "time_max": params.time_max,
        });
        if let Some(user_id) = &params.user_id {
            body["user_id"] = Value::String(user_id.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "搜索飞书日历事件")]
    async fn feishu_search_event(
        &self,
        Parameters(params): Parameters<SearchEventParams>,
    ) -> String {
        let path = "open-apis/calendar/v4/events/search";
        let mut body = serde_json::json!({
            "query": params.query,
        });
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            body["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Drive (6 tools) ====

    #[tool(description = "上传飞书云文档文件")]
    async fn feishu_upload_file(
        &self,
        Parameters(params): Parameters<UploadFileParams>,
    ) -> String {
        let path = "open-apis/drive/v1/files/upload_all";
        let mut body = serde_json::json!({
            "parent_node": params.parent_node,
            "file_name": params.file_name,
        });
        if let Some(file_type) = &params.file_type {
            body["file_type"] = Value::String(file_type.clone());
        }
        if let Some(title) = &params.title {
            body["title"] = Value::String(title.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "下载飞书云文档文件")]
    async fn feishu_download_file(
        &self,
        Parameters(params): Parameters<DownloadFileParams>,
    ) -> String {
        let path = format!("open-apis/drive/v1/files/{}", params.file_token);
        let mut query = serde_json::json!({});
        if let Some(file_type) = &params.file_type {
            query["file_type"] = Value::String(file_type.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建飞书云文档文件夹")]
    async fn feishu_create_folder(
        &self,
        Parameters(params): Parameters<CreateFolderParams>,
    ) -> String {
        let path = "open-apis/drive/v1/files/create_folder";
        let mut body = serde_json::json!({
            "parent_token": params.parent_token,
            "name": params.name,
        });
        if let Some(folder_type) = &params.folder_type {
            body["folder_type"] = Value::String(folder_type.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "搜索飞书云文档")]
    async fn feishu_search_drive(
        &self,
        Parameters(params): Parameters<SearchDriveParams>,
    ) -> String {
        let path = "open-apis/suite/docs/search";
        let mut body = serde_json::json!({
            "search_key": params.search_key,
        });
        if let Some(owner_ids) = &params.owner_ids {
            body["owner_ids"] = serde_json::to_value(owner_ids).unwrap_or(Value::Null);
        }
        if let Some(count) = params.count {
            body["count"] = Value::Number(count.into());
        }
        if let Some(offset) = params.offset {
            body["offset"] = Value::Number(offset.into());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "移动飞书云文档文件")]
    async fn feishu_move_file(
        &self,
        Parameters(params): Parameters<MoveFileParams>,
    ) -> String {
        let path = format!("open-apis/drive/v1/files/{}/move", params.file_token);
        let mut body = serde_json::json!({
            "folder_token": params.folder_token,
        });
        if let Some(file_type) = &params.file_type {
            body["file_type"] = Value::String(file_type.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "删除飞书云文档文件")]
    async fn feishu_delete_file(
        &self,
        Parameters(params): Parameters<DeleteFileParams>,
    ) -> String {
        let path = format!("open-apis/drive/v1/files/{}", params.file_token);
        let mut query = serde_json::json!({});
        if let Some(file_type) = &params.file_type {
            query["file_type"] = Value::String(file_type.clone());
        }
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::DELETE,
                &path,
                Some(&query),
                None,
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Contact (2 tools) ====

    #[tool(description = "搜索飞书用户")]
    async fn feishu_search_user(
        &self,
        Parameters(params): Parameters<SearchUserParams>,
    ) -> String {
        let path = "open-apis/search/v2/user";
        let mut body = serde_json::json!({
            "query": params.query,
        });
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书用户信息")]
    async fn feishu_get_user(
        &self,
        Parameters(params): Parameters<GetUserParams>,
    ) -> String {
        let path = format!("open-apis/contact/v3/users/{}", params.user_id);
        let mut query = serde_json::json!({});
        if let Some(user_id_type) = &params.user_id_type {
            query["user_id_type"] = Value::String(user_id_type.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Task (6 tools) ====

    #[tool(description = "创建飞书任务")]
    async fn feishu_create_task(
        &self,
        Parameters(params): Parameters<CreateTaskParams>,
    ) -> String {
        let path = "open-apis/task/v2/tasks";
        let mut body = serde_json::json!({
            "summary": params.summary,
        });
        if let Some(description) = &params.description {
            body["description"] = Value::String(description.clone());
        }
        if let Some(due_date) = &params.due_date {
            body["due_date"] = serde_json::json!({ "timestamp": due_date });
        }
        if let Some(assignees) = &params.assignees {
            body["assignees"] = assignees.clone();
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书任务详情")]
    async fn feishu_get_task(
        &self,
        Parameters(params): Parameters<GetTaskParams>,
    ) -> String {
        let path = format!("open-apis/task/v2/tasks/{}", params.task_id);
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新飞书任务")]
    async fn feishu_update_task(
        &self,
        Parameters(params): Parameters<UpdateTaskParams>,
    ) -> String {
        let path = format!("open-apis/task/v2/tasks/{}", params.task_id);
        let mut body = serde_json::json!({});
        if let Some(summary) = &params.summary {
            body["summary"] = Value::String(summary.clone());
        }
        if let Some(description) = &params.description {
            body["description"] = Value::String(description.clone());
        }
        if let Some(due_date) = &params.due_date {
            body["due_date"] = serde_json::json!({ "timestamp": due_date });
        }
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PATCH,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "完成飞书任务")]
    async fn feishu_complete_task(
        &self,
        Parameters(params): Parameters<CompleteTaskParams>,
    ) -> String {
        let path = format!("open-apis/task/v2/tasks/{}/complete", params.task_id);
        let body = serde_json::json!({});
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取我的飞书任务列表")]
    async fn feishu_list_my_tasks(
        &self,
        Parameters(params): Parameters<ListMyTasksParams>,
    ) -> String {
        let path = "open-apis/task/v2/tasks";
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        if let Some(start_time) = &params.start_time {
            query["start_time"] = Value::String(start_time.clone());
        }
        if let Some(end_time) = &params.end_time {
            query["end_time"] = Value::String(end_time.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "搜索飞书任务")]
    async fn feishu_search_tasks(
        &self,
        Parameters(params): Parameters<SearchTasksParams>,
    ) -> String {
        let path = "open-apis/task/v2/tasks/search";
        let mut body = serde_json::json!({
            "query": params.query,
        });
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            body["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Mail (6 tools) ====

    #[tool(description = "发送飞书邮件")]
    async fn feishu_send_mail(
        &self,
        Parameters(params): Parameters<SendMailParams>,
    ) -> String {
        let path = format!(
            "open-apis/mail/v1/user_mailboxes/{}/drafts",
            params.mailbox_id
        );
        let mut body = serde_json::json!({
            "subject": params.subject,
            "content": params.content,
            "to": params.to,
        });
        if let Some(cc) = &params.cc {
            body["cc"] = serde_json::to_value(cc).unwrap_or(Value::Null);
        }
        if let Some(bcc) = &params.bcc {
            body["bcc"] = serde_json::to_value(bcc).unwrap_or(Value::Null);
        }
        if let Some(reply_to_mail_id) = &params.reply_to_mail_id {
            body["reply_to_mail_id"] = Value::String(reply_to_mail_id.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书邮件列表")]
    async fn feishu_list_mail(
        &self,
        Parameters(params): Parameters<ListMailParams>,
    ) -> String {
        let path = format!(
            "open-apis/mail/v1/user_mailboxes/{}/messages",
            params.mailbox_id
        );
        let mut query = serde_json::json!({});
        if let Some(folder_id) = &params.folder_id {
            query["folder_id"] = Value::String(folder_id.clone());
        }
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书邮件详情")]
    async fn feishu_get_mail(
        &self,
        Parameters(params): Parameters<GetMailParams>,
    ) -> String {
        let path = format!(
            "open-apis/mail/v1/user_mailboxes/{}/messages/{}",
            params.mailbox_id, params.message_id
        );
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "回复飞书邮件")]
    async fn feishu_reply_mail(
        &self,
        Parameters(params): Parameters<ReplyMailParams>,
    ) -> String {
        let path = format!(
            "open-apis/mail/v1/user_mailboxes/{}/drafts",
            params.mailbox_id
        );
        let mut body = serde_json::json!({
            "reply_message_id": params.message_id,
            "content": params.content,
        });
        if let Some(reply_all) = params.reply_all {
            body["reply_all"] = Value::Bool(reply_all);
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "转发飞书邮件")]
    async fn feishu_forward_mail(
        &self,
        Parameters(params): Parameters<ForwardMailParams>,
    ) -> String {
        let path = format!(
            "open-apis/mail/v1/user_mailboxes/{}/drafts",
            params.mailbox_id
        );
        let mut body = serde_json::json!({
            "forward_message_id": params.message_id,
            "to": params.to,
        });
        if let Some(content) = &params.content {
            body["content"] = Value::String(content.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "快速浏览飞书邮件(分诊)")]
    async fn feishu_triage_mail(
        &self,
        Parameters(params): Parameters<TriageMailParams>,
    ) -> String {
        let path = format!(
            "open-apis/mail/v1/user_mailboxes/{}/messages",
            params.mailbox_id
        );
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== VC (3 tools) ====

    #[tool(description = "搜索飞书会议")]
    async fn feishu_search_meeting(
        &self,
        Parameters(params): Parameters<SearchMeetingParams>,
    ) -> String {
        let path = "open-apis/vc/v1/meetings/search";
        let mut body = serde_json::json!({
            "query": params.query,
        });
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书会议详情")]
    async fn feishu_get_meeting(
        &self,
        Parameters(params): Parameters<GetMeetingParams>,
    ) -> String {
        let path = format!("open-apis/vc/v1/meetings/{}", params.meeting_id);
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书会议录制")]
    async fn feishu_get_recording(
        &self,
        Parameters(params): Parameters<GetRecordingParams>,
    ) -> String {
        let path = format!(
            "open-apis/vc/v1/meetings/{}/recording",
            params.meeting_id
        );
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Wiki (3 tools) ====

    #[tool(description = "获取飞书知识库空间列表")]
    async fn feishu_list_spaces(
        &self,
        Parameters(params): Parameters<ListSpacesParams>,
    ) -> String {
        let path = "open-apis/wiki/v2/spaces";
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "创建飞书知识库节点")]
    async fn feishu_create_node(
        &self,
        Parameters(params): Parameters<CreateNodeParams>,
    ) -> String {
        let path = format!("open-apis/wiki/v2/spaces/{}/nodes", params.space_id);
        let mut body = serde_json::json!({
            "node_type": params.node_type,
            "title": params.title,
        });
        if let Some(parent_node_token) = &params.parent_node_token {
            body["parent_node_token"] = Value::String(parent_node_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书知识库节点信息")]
    async fn feishu_get_node(
        &self,
        Parameters(params): Parameters<GetNodeParams>,
    ) -> String {
        let path = "open-apis/wiki/v2/spaces/get_node";
        let query = serde_json::json!({
            "token": params.token,
        });
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Approval (4 tools) ====

    #[tool(description = "获取飞书审批实例详情")]
    async fn feishu_get_approval(
        &self,
        Parameters(params): Parameters<GetApprovalParams>,
    ) -> String {
        let path = format!(
            "open-apis/approval/v4/instances/{}",
            params.instance_id
        );
        let mut query = serde_json::json!({});
        if let Some(user_id_type) = &params.user_id_type {
            query["user_id_type"] = Value::String(user_id_type.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "通过飞书审批任务")]
    async fn feishu_approve_task(
        &self,
        Parameters(params): Parameters<ApproveTaskParams>,
    ) -> String {
        let path = "open-apis/approval/v4/tasks/approve";
        let mut body = serde_json::json!({
            "task_id": params.task_id,
            "user_id": params.user_id,
        });
        if let Some(comment) = &params.comment {
            body["comment"] = Value::String(comment.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "驳回飞书审批任务")]
    async fn feishu_reject_task(
        &self,
        Parameters(params): Parameters<RejectTaskParams>,
    ) -> String {
        let path = "open-apis/approval/v4/tasks/reject";
        let mut body = serde_json::json!({
            "task_id": params.task_id,
            "user_id": params.user_id,
        });
        if let Some(comment) = &params.comment {
            body["comment"] = Value::String(comment.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书审批实例列表")]
    async fn feishu_list_approvals(
        &self,
        Parameters(params): Parameters<ListApprovalsParams>,
    ) -> String {
        let path = "open-apis/approval/v4/instance/list";
        let mut body = serde_json::json!({
            "approval_code": params.approval_code,
            "start_time": params.start_time,
            "end_time": params.end_time,
        });
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            body["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== OKR (2 tools) ====

    #[tool(description = "获取飞书OKR周期列表")]
    async fn feishu_list_okr_cycles(
        &self,
        Parameters(params): Parameters<ListOkrCyclesParams>,
    ) -> String {
        let path = "open-apis/okr/v1/cycles";
        let mut query = serde_json::json!({});
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        if let Some(page_token) = &params.page_token {
            query["page_token"] = Value::String(page_token.clone());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书OKR目标详情")]
    async fn feishu_get_okr_detail(
        &self,
        Parameters(params): Parameters<GetOkrDetailParams>,
    ) -> String {
        let path = format!(
            "open-apis/okr/v1/cycles/{}/objectives",
            params.cycle_id
        );
        let mut query = serde_json::json!({});
        if let Some(user_id) = &params.user_id {
            query["user_id"] = Value::String(user_id.clone());
        }
        if let Some(page_size) = params.page_size {
            query["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, Some(&query))
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Attendance (1 tool) ====

    #[tool(description = "查询飞书考勤记录")]
    async fn feishu_get_attendance(
        &self,
        Parameters(params): Parameters<GetAttendanceParams>,
    ) -> String {
        let path = "open-apis/attendance/v1/userTasks/query";
        let body = serde_json::json!({
            "user_ids": params.user_ids,
            "start_time": params.start_time,
            "end_time": params.end_time,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Slides (2 tools) ====

    #[tool(description = "创建飞书幻灯片")]
    async fn feishu_create_slides(
        &self,
        Parameters(params): Parameters<CreateSlidesParams>,
    ) -> String {
        let path = "open-apis/slides/v1/xml_presentations";
        let mut body = serde_json::json!({
            "title": params.title,
        });
        if let Some(folder_token) = &params.folder_token {
            body["folder_token"] = Value::String(folder_token.clone());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "替换飞书幻灯片内容")]
    async fn feishu_replace_slide(
        &self,
        Parameters(params): Parameters<ReplaceSlideParams>,
    ) -> String {
        let path = format!(
            "open-apis/slides/v1/xml_presentations/{}/slides/{}/replace",
            params.presentation_id, params.slide_id
        );
        let body = serde_json::json!({
            "replacements": params.replacements,
        });
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, &path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Minutes (2 tools) ====

    #[tool(description = "搜索飞书妙记")]
    async fn feishu_search_minutes(
        &self,
        Parameters(params): Parameters<SearchMinutesParams>,
    ) -> String {
        let path = "open-apis/minutes/v1/minutes/search";
        let mut body = serde_json::json!({
            "query": params.query,
        });
        if let Some(page_size) = params.page_size {
            body["page_size"] = Value::Number(page_size.into());
        }
        match self
            .client
            .api_post(&params.app_id, &params.app_secret, path, &body)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "获取飞书妙记详情")]
    async fn feishu_get_minutes(
        &self,
        Parameters(params): Parameters<GetMinutesParams>,
    ) -> String {
        let path = format!("open-apis/minutes/v1/minutes/{}", params.minutes_id);
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    // ==== Whiteboard (2 tools) ====

    #[tool(description = "查询飞书白板详情")]
    async fn feishu_query_whiteboard(
        &self,
        Parameters(params): Parameters<QueryWhiteboardParams>,
    ) -> String {
        let path = format!(
            "open-apis-whiteboard/v1/whiteboards/{}",
            params.whiteboard_id
        );
        match self
            .client
            .api_get(&params.app_id, &params.app_secret, &path, None)
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
    }

    #[tool(description = "更新飞书白板信息")]
    async fn feishu_update_whiteboard(
        &self,
        Parameters(params): Parameters<UpdateWhiteboardParams>,
    ) -> String {
        let path = format!(
            "open-apis-whiteboard/v1/whiteboards/{}",
            params.whiteboard_id
        );
        let body = serde_json::json!({
            "title": params.title,
        });
        match self
            .client
            .api_request(
                &params.app_id,
                &params.app_secret,
                reqwest::Method::PATCH,
                &path,
                None,
                Some(&body),
            )
            .await
        {
            Ok(resp) => truncate_result(json_to_string(&resp), 60000),
            Err(e) => error_response(&e),
        }
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
            tracing_subscriber::EnvFilter::try_from_env("FEISHU_MCP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let client = FeishuClient::new();
    let server = FeishuServer {
        client: Arc::new(client),
    };

    tracing::info!("feishu-mcp starting (stdio, multi-tenant)");
    let service = server.serve(stdio_transport()).await?;
    service.waiting().await?;

    Ok(())
}
