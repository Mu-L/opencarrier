---
name: outline-writer
description: 根据素材撰写文章大纲
version: 3
tools:
  - file_read
  - file_write
  - mcp_searxng_web_search
  - web_fetch
---
# Outline Writer

根据素材撰写文章大纲。本 skill 是系统级共享工作流；写作风格从分身 system prompt 的 knowledge(writing-style) 读取，分身各自定义语气和结构偏好。

## 工具名规范

- 写文件 = `file_write`（不是 ~~mcp__tools__file_write~~）
- 读文件 = `file_read`
- 搜索 = `mcp_searxng_web_search`

## 写作风格

你的 system prompt 中的 knowledge 部分包含当前用户的专属写作风格（writing-style）。大纲的结构、语气、术语密度必须严格遵循该风格。如果风格未指定，使用通用新媒体写作风格。

## ⚠️ 工具调用规则

**所有 required 参数必须在一次调用中全部传齐。** 缺任何一个 required 字段都会报错 `missing field xxx`。

## Process

### 1. 读取素材

从触发 message 里找 "流水线ID = xxx"，提取流水线 ID：

```
file_read(path="output/<pipeline_id>/素材.md")
```

knowledge/writing-style 可能不存在，读不到就跳过，不影响流程。

### 2. 补充搜索（可选）

素材数据不够时，用 `mcp_searxng_web_search` 补充。时间线、人名、机构名必须核实。

### 3. 撰写大纲

遵循 system prompt 中的 writing-style，生成大纲结构：标题备选（3个）、核心论点、文章结构、关键数据点、金句预留位、写作风格设定。

### 4. 保存大纲

```
file_write(path="output/<pipeline_id>/大纲.md", content="流水线ID: <pipeline_id>\n\n大纲内容")
```

### 5. 输出结果

完成后输出以下信息，由主控 agent 手动推进下一步：
- 流水线 ID
- 大纲路径
- 标题备选（3个）
- 一句话核心论点

## Important Principles

- **流水线 ID 必须从触发 message 里提取，所有路径用它派生**
- **所有中间数据存 `output/<pipeline_id>/` 目录，不用 knowledge_add**
- 大纲风格严格遵循 system prompt 中的 writing-style
