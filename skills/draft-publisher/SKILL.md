---
name: draft-publisher
description: 将排版好的文章发布到微信公众号草稿箱
version: 8
tools:
  - file_list
  - file_read
  - mcp_wechat_oa_upload_media
  - mcp_wechat_oa_create_draft
  - mcp_wechat_oa_list_drafts
  - mcp_wechat_oa_publish_draft
  - mcp_wechat_oa_list_materials
---
# Draft Publisher

将排版好的文章发布到微信公众号草稿箱。本 skill 是系统级共享工作流，所有写作类分身通用；公众号账号、作者署名等个性化信息从 User Profile 读取。

## 工具名规范

- 读文件 = `file_read`
- 微信工具 = `mcp_wechat_oa_` 前缀（如 `mcp_wechat_oa_create_draft`）

## 公众号凭证

**凭证已在 User Profile 的 preferences.wechat_accounts 中自动提供。** JSON 数组：

```json
[{"name": "账号名", "app_id": "wx...", "app_secret": "..."}, ...]
```

- 用户未指定账号时，取 **第一个** 账号
- 用户说"发到XX公众号"时，按 name 匹配
- 如果 preferences 中没有凭证，提示用户提供 app_id 和 app_secret

**不要用 kv_get 获取凭证。** 凭证直接从 User Profile 读取。

## ⚠️ MCP 工具调用规则（2026-06 实测）

**所有 required 参数必须在一次调用中全部传齐。** 缺任何 required 字段都会报 `missing field xxx`。调用前先确认：这个工具有哪些 required 字段？我都备齐了吗？

已知 required 清单：

### `mcp_wechat_oa_create_draft`
必传：`app_id`, `app_secret`, `title`, `content`, **`need_open_comment`**（0=关闭评论 / 1=开启）
可选：`author`, `thumb_media_id`
⚠️ `need_open_comment` 是新增强制字段，漏了会报 `missing field need_open_comment`。

### `mcp_wechat_oa_list_materials`
必传：`app_id`, `app_secret`, `type`(如 "image"), `count`, **`offset`**（从 0 开始）
⚠️ `offset` 是强制字段。

### `mcp_wechat_oa_publish_draft`
必传：`app_id`, `app_secret`, `media_id`
⚠️ 正式发布，必须用户明确说"发布"才执行。

**不要调用 get_access_token —— token 自动管理。**
**create_draft 自动执行，publish_draft 需用户确认。**

## Process

### 1. 读取排版结果和标题

从触发 message 提取流水线 ID（如有）：

```
file_read(path="output/<pipeline_id>/正文.html")   // 排版后 HTML
file_read(path="output/<pipeline_id>/正文.md")      // Markdown，取首行标题
```

若 message 无流水线 ID，走交互流程（用户提供 HTML 和标题）。

### 2. 确认账号

从 User Profile preferences.wechat_accounts 取凭证。指定账号名按 name 匹配，否则取第一个。

### 3. 封面图处理

**⚠️ 不要直接调 image_generate！先检查已有图片：**

1. `file_list(path="output/")` — 查看 output 目录下已有的图片（`image_*.png`）
2. 找到合适的图片 → `mcp_wechat_oa_upload_media(file_path="output/image_xxx.png")` 上传取 media_id
3. output 里没有图片 → `mcp_wechat_oa_list_materials(app_id, app_secret, type="image", count=1, offset=0)` 从素材库取最近一张
4. 都没有 → 跳过 thumb_media_id（不调 image_generate，封面图由用户后续在后台添加）

### 4. 创建草稿

```
mcp_wechat_oa_create_draft(
  app_id, app_secret,
  title="文章标题",
  content="HTML 正文",
  need_open_comment=0,
  author="<作者署名>",
  thumb_media_id="封面 media_id"
)
```

成功返回 `{"item":[...],"media_id":"..."}`，把 media_id 回报用户。

## Important Principles

- 流水线 ID 从 message 提取，文件从 `output/<pipeline_id>/` 读取
- 凭证从 User Profile preferences.wechat_accounts 读取，不用 kv_get
- 多公众号默认取第一个，用户指定按 name 匹配
- 调用任何 MCP 工具前，先核对 required 字段全部备齐
- 发布前告知用户这是正式操作
