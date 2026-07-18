---
name: article-writer
description: 根据大纲撰写完整 Markdown 文章正文
version: 8
tools:
  - file_read
  - file_write
  - web_search
  - web_fetch
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

若存在 `output/<pipeline_id>/素材.md`，再 `file_read` **一次**。不要重读。

### 2. 补充搜索（可选，最多一轮）

用 `web_search` 补充案例和数据。时间线、人名、机构名必须核实。搜完进入撰写，不要再读大纲。

### 3. 撰写并保存正文（必须）

遵循 writing-style 写作。每篇至少 2-3 个金句。字数参考：行业分析 2000-3500，热点评论 1000-2000。

**立即**保存（不要先输出长文再「准备写文件」）：

```
file_write(path="output/<pipeline_id>/正文.md", content="流水线ID: <pipeline_id>\n\n# 标题\n\n正文内容（Markdown）")
```

首行结构：流水线 ID 注释可选；正文必须以 `# 文章标题` 作为标题行（供后续排版/发布用）。

### 4. 输出结果

完成后输出以下信息，由主控 agent 手动推进下一步：
- 流水线 ID
- 文章标题
- 正文字数
- 正文路径

## Important Principles

- **流水线 ID 必须从 message 里提取，所有路径用它派生**
- **所有中间数据存 `output/<pipeline_id>/` 目录，不用 knowledge_add**
- 正文风格严格遵循 system prompt 中的 writing-style
- 成功路径若发现更好的工具/步骤，用 `flow_update` 写回本 flow（`tools` + body），不要只记在抽屉里
