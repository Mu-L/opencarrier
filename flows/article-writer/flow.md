---
name: article-writer
description: 根据大纲撰写完整 Markdown 文章正文
version: 9
privilege: system
tools:
  - file_read
  - file_write
  - web_search
  - web_fetch
  - shell_exec
shell_allow:
  - "python3 .flows/article-writer/scripts/*"
  - "python .flows/article-writer/scripts/*"
---

# Article Writer

根据大纲撰写完整 Markdown 文章正文。本 skill 是系统级共享工作流；写作风格（语气、术语、句式）从分身 system prompt 的 knowledge(writing-style) 读取。

## 硬规则（必须遵守）

这些工具已在 frontmatter `tools` 声明，匹配本 flow 后会自动注入。**禁止**再用 `tool_search` 查找它们。

1. **只使用声明工具**：`file_read`、`file_write`、`web_search`、`web_fetch`。写文件 = `file_write`（不是 mcp 变体名）。
2. **每个 path 最多读 1 次**：`大纲.md`、`素材.md` 各 `file_read` 一次即可。禁止在同一任务里对同一 path 反复 `file_read`（空转会撞 iteration 上限）。
3. **读完必须写**：完成读取（及可选的一次搜索）后，**本任务内必须**调用一次  
   `file_write(path="output/<pipeline_id>/正文.md", content=...)`  
   写出完整正文后再结束。禁止只读不写。
4. **参数一次传齐**：`file_write` 必须同时带 `path` + `content`；缺 required 字段会直接失败。
5. **素材不足时搜一次就写**：可用 `web_search` 最多补充一轮，然后立即撰写并 `file_write`，不要边搜边反复读大纲。

## ⚠️ META 头规范（最高优先级，必须遵守）

`正文.md` **必须以 HTML 注释 META 头开头**，然后才是正文标题和内容。这是全流水线的结构化元信息层，handler 从这里提取标题/作者/摘要。

### META 头格式

```html
<!--
META_TITLE: 最终确定的文章标题
META_AUTHOR: 小载
META_DIGEST: 一句话摘要，30-60字，用于公众号草稿摘要
META_TYPE: 行业分析 | 热点评论 | 产品文章 | 深度教程
META_PIPELINE: pipeline-YYYYMMDD-topic
-->
```

### 正文.md 完整结构

```
<!-- META 头（5行，见上方格式）-->

# 文章标题

正文内容...
```

### 规则
- **META 头是文件第一行**，在 `# 标题` 之前
- 从 `大纲.md` 的 META 头继承 title/author/type/pipeline（大纲已确定）
- `META_DIGEST` 在写完正文后回填：用一句话概括全文核心，30-60字
- **绝对禁止**在 META 头之外（正文区域）写 `流水线ID:` 或任何元信息标记
- **绝对禁止**省略 META 头直接写 `# 标题`——缺 META = 下游拿不到作者/摘要

## 工具名规范

- 写文件 = `file_write`（不是 ~~mcp__tools__file_write~~）
- 读文件 = `file_read`
- 搜索 = `web_search`

## web_search 用法

`web_search(q="关键词")` 默认走 baidu/google/sogou。**搜微信公众号文章**时指定引擎：

```
web_search(q="关键词", engines=["sogou_wechat"])
```

需要同时搜正文时加 `fetch_top`（搜完自动抓前 N 条正文，一步完成"搜→读"）：

```
web_search(q="关键词", engines=["sogou_wechat"], fetch_top=3)
```

## 写作风格

你的 system prompt 中的 knowledge 部分包含当前用户的专属写作风格（writing-style）。正文的语气、术语、句式、排版必须严格遵循该风格。如果风格未指定，使用通用新媒体写作风格。

## Process

### 1. 读取大纲（一次）

从 message 提取流水线 ID：

```
file_read(path="output/<pipeline_id>/大纲.md")
```

从大纲的 META 头提取：`META_TITLE`（最终标题）、`META_AUTHOR`、`META_TYPE`、`META_PIPELINE`。

若存在 `output/<pipeline_id>/素材.md`，再 `file_read` **一次**。不要重读。

### 2. 补充搜索（可选，最多一轮）

用 `web_search` 补充案例和数据。时间线、人名、机构名必须核实。搜完进入撰写，不要再读大纲。

### 3. 撰写并保存正文（必须带 META 头）

遵循 writing-style 写作。每篇至少 2-3 个金句。字数参考：行业分析 2000-3500，热点评论 1000-2000。

**立即**保存（不要先输出长文再「准备写文件」）：

```
file_write(
  path="output/<pipeline_id>/正文.md",
  content="""
<!--
META_TITLE: 从大纲继承的最终标题
META_AUTHOR: 小载
META_DIGEST: 写完全文后总结的一句话摘要30-60字
META_TYPE: 从大纲继承的类型
META_PIPELINE: <pipeline_id>
-->

# 文章标题

正文内容（Markdown）...
"""
)
```

### 4. 验证（宣布完成前必跑）

写完 `正文.md` 后立即跑校验器，按报错修，重跑直到 OK：

```
shell_exec(command="python3 .flows/article-writer/scripts/validate_article.py output/<pipeline_id>/正文.md")
```

- 看到 `ARTICLE_OK` 才算完成；看到 `ARTICLE_INVALID:N` 就按列出的 `ERROR:` 逐条修再重跑（常见：漏 META 头 / 无 `## ` 章节 / 字数低于该类型下限 / 正文写了 `流水线ID:` / 残留占位符）。
- 校验只查结构（META/章节/字数/占位），不判写作质量——质量靠遵循 writing-style + 人审。

### 5. 输出结果

完成后输出以下信息，由主控 agent 手动推进下一步：
- 流水线 ID
- 文章标题（从 META_TITLE）
- 正文字数
- 正文路径

## Important Principles

- **流水线 ID 必须从 message 里提取，所有路径用它派生**
- **所有中间数据存 `output/<pipeline_id>/` 目录，不用 knowledge_add**
- **正文.md 必须以 `<!-- META -->` 头开头（5行），然后才是 `# 标题` + 正文**
- **从大纲的 META 头继承 title/author/type/pipeline，不要自己编**
- **META_DIGEST 写完全文后回填，30-60字概括全文**
- **绝对禁止在正文区域写 `流水线ID:` 或任何元信息标记**
- 正文风格严格遵循 system prompt 中的 writing-style
- 成功路径若发现更好的工具/步骤，用 `flow_update` 写回本 flow
