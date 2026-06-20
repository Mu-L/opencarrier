---
name: article-writer
description: 根据大纲撰写完整 Markdown 文章正文
version: 7
tools:
  - file_read
  - file_write
  - web_search
  - web_fetch
---
# Article Writer

根据大纲撰写完整 Markdown 文章正文。本 skill 是系统级共享工作流；写作风格（语气、术语、句式）从分身 system prompt 的 knowledge(writing-style) 读取。

## 工具名规范

- 写文件 = `file_write`（不是 ~~mcp__tools__file_write~~）
- 读文件 = `file_read`
- 搜索 = `web_search`

## 写作风格

你的 system prompt 中的 knowledge 部分包含当前用户的专属写作风格（writing-style）。正文的语气、术语、句式、排版必须严格遵循该风格。如果风格未指定，使用通用新媒体写作风格。

## ⚠️ 工具调用规则

**所有 required 参数必须在一次调用中全部传齐。** 缺任何一个 required 字段都会报错 `missing field xxx`。

## Process

### 1. 读取大纲

从 message 提取流水线 ID：

```
file_read(path="output/<pipeline_id>/大纲.md")
```

knowledge/writing-style 可能不存在，读不到就跳过。

### 2. 补充搜索

用 `web_search` 补充案例和数据。时间线、人名、机构名必须核实。

### 3. 撰写正文

遵循 system prompt 中的 writing-style 写作。每篇至少 2-3 个金句。字数参考：行业分析 2000-3500，热点评论 1000-2000（具体以 writing-style 为准）。

### 4. 保存正文

```
file_write(path="output/<pipeline_id>/正文.md", content="流水线ID: <pipeline_id>\n\n# 标题\n\n正文内容（Markdown）")
```

### 5. 输出结果

完成后输出以下信息，由主控 agent 手动推进下一步：
- 流水线 ID
- 文章标题
- 正文字数
- 正文路径

## Important Principles

- **流水线 ID 必须从 message 里提取，所有路径用它派生**
- **所有中间数据存 `output/<pipeline_id>/` 目录，不用 knowledge_add**
- 正文风格严格遵循 system prompt 中的 writing-style
