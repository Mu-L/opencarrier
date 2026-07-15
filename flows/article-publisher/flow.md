---
name: article-publisher
description: 流水线 Step 4 —— 读正文、排版成公众号 HTML、发 PUBLISH 标记触发后台自动发布
version: 1
tools:
  - file_read
  - file_write
---
# Article Publisher（流水线 Step 4）

写作流水线的最后一步：把 `正文.md` 排版成公众号 HTML，然后发 `[PUBLISH:app_id]` 标记，由后台 handler 自动完成「封面→草稿→发布」。

## Process

### 1. 排版：正文.md → 正文.html

```
flow_load("article-formatter")
```

按 article-formatter 的规则，把 `output/<pipeline_id>/正文.md` 转成公众号内联样式 HTML，写到同目录 `正文.html`。

**务必保证** `正文.md` 首行是 `# 文章标题`——它会被发布 handler 用作文章标题。

### 2. 取 app_id

从 User Profile 的 `preferences.wechat_accounts` 取目标公众号 `app_id`（默认第一个；指定按 name 匹配）。

### 3. 发 PUBLISH 标记

回复最后一行发：

```
[PUBLISH:<app_id>]output/<pipeline_id>/正文.html[/PUBLISH]
```

路径用你 `file_read` 时用的同一路径（相对 `~/.opencarrier` 或绝对都行）。

## 标记之后（不用你管）

后台 handler 自动：读 `正文.html` + 同名 `.md` 首行标题 → 生成封面（失败取素材库第一张）→ 建草稿 → 正式发布 → 把结果推给用户。

**不要**调用 `image_generate` 或任何 `mcp_wechat_oa_*` 工具。发标记即可。

## Important Principles

- 先排版产出 `正文.html`，再发标记；`正文.html` 不存在不要发
- `正文.md` 首行必须是标题
- 只发一个标记，放回复最后一行
- 标记前用一句话告知用户「正在发布…」
