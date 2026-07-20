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

## ⚠️ META 头提取规范（必须遵守）

`正文.md` 以 `<!-- META -->` HTML 注释头开头，包含 5 个结构化字段。排版和发布时**必须从 META 头提取这些字段**，不能靠猜首行。

META 头格式：
```html
<!--
META_TITLE: 文章标题
META_AUTHOR: 小载
META_DIGEST: 一句话摘要
META_TYPE: 行业分析 | 热点评论 | 产品文章 | 深度教程
META_PIPELINE: pipeline-YYYYMMDD-topic
-->
```

**提取规则：**
- `META_TITLE` → 文章标题（传给 handler / 草稿箱标题）
- `META_AUTHOR` → 作者名
- `META_DIGEST` → 草稿箱摘要
- 如果 META 头缺失（旧文件），回退到 `# 标题` 首行作为标题，但要在输出中**警告** META 头缺失

## Process

### 1. 排版：正文.md → 正文.html

```
flow_load("article-formatter")
```

按 article-formatter 的规则，把 `output/<pipeline_id>/正文.md` 转成公众号内联样式 HTML，写到同目录 `正文.html`。

**排版时：**
- 从 `正文.md` 的 META 头提取 `META_TITLE` 作为文章标题
- **剥离 META 头**：HTML 中不包含 `<!-- META -->` 注释内容，从 `# 标题` 对应的 HTML 开始输出
- 如果 META 头缺失，回退到 `# 标题` 首行

### 2. 取 app_id

从 User Profile 的 `preferences.wechat_accounts` 取目标公众号 `app_id`（默认第一个；指定按 name 匹配）。

### 3. 发 PUBLISH 标记

回复最后一行发：

```
[PUBLISH:<app_id>]output/<pipeline_id>/正文.html[/PUBLISH]
```

路径用你 `file_read` 时用的同一路径（相对 `~/.opencarrier` 或绝对都行）。

**发标记前确认：**
- `正文.html` 存在且非空
- 从 META 头提取到了标题（不是流水线 ID）

## 标记之后（不用你管）

后台 handler 自动：读 `正文.html` + 同名 `.md` 首行标题 → 生成封面（失败取素材库第一张）→ 建草稿 → 正式发布 → 把结果推给用户。

**不要**调用 `image_generate` 或任何 `mcp_wechat_oa_*` 工具。发标记即可。

## Important Principles

- 先排版产出 `正文.html`，再发标记；`正文.html` 不存在不要发
- **从 `正文.md` 的 `<!-- META -->` 头提取标题/作者/摘要，不靠猜首行**
- **排版 HTML 时剥离 META 头**，从正文标题开始输出
- 只发一个标记，放回复最后一行
- 标记前用一句话告知用户「正在发布…」
- 如果 META 头缺失，回退到 `# 标题` 首行，并在输出中警告
