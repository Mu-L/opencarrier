---
name: outline-writer
description: 根据素材撰写文章大纲
version: 4
privilege: system
tools:
  - file_read
  - file_write
  - web_search
  - web_fetch
  - shell_exec
shell_allow:
  - "python3 .flows/outline-writer/scripts/*"
  - "python .flows/outline-writer/scripts/*"
---

# Outline Writer

根据素材撰写文章大纲。本 skill 是系统级共享工作流；写作风格从分身 system prompt 的 knowledge(writing-style) 读取，分身各自定义语气和结构偏好。

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

你的 system prompt 中的 knowledge 部分包含当前用户的专属写作风格（writing-style）。大纲的结构、语气、术语密度必须严格遵循该风格。如果风格未指定，使用通用新媒体写作风格。

## ⚠️ 工具调用规则

**所有 required 参数必须在一次调用中全部传齐。** 缺任何一个 required 字段都会报错 `missing field xxx`。

## ⚠️ META 头规范（必须遵守）

所有流水线中间文件（大纲.md、正文.md）**必须以 HTML 注释 META 头开头**，结构化记录文章元信息。这条规则贯穿全流水线，不可跳过。

META 头格式（放在文件**最开头**，正文之前）：

```html
<!--
META_TITLE: 文章标题（从备选中选定一个，不要带书名号外的多余字符）
META_AUTHOR: 小载
META_DIGEST: 一句话摘要，30-60字，用于公众号草稿摘要字段
META_TYPE: 行业分析 | 热点评论 | 产品文章 | 深度教程
META_PIPELINE: pipeline-YYYYMMDD-topic
-->
```

**规则：**
- META 头是 `<!-- -->` HTML 注释，不会被 Markdown 渲染，不影响排版
- `META_TITLE` 必须是**最终确定的标题**，不是备选
- `META_DIGEST` 是给公众号草稿箱用的摘要，不是文章导语
- **禁止**把 `流水线ID:` 写在正文区域——它只活在 META 头里

## Process

### 1. 读取素材

从触发 message 里找 "流水线ID = xxx"，提取流水线 ID：

```
file_read(path="output/<pipeline_id>/素材.md")
```

knowledge/writing-style 可能不存在，读不到就跳过，不影响流程。

### 2. 补充搜索（可选）

素材数据不够时，用 `web_search` 补充。时间线、人名、机构名必须核实。

### 3. 撰写大纲

遵循 system prompt 中的 writing-style，生成大纲结构：标题备选（3个）、核心论点、文章结构、关键数据点、金句预留位、写作风格设定。

### 4. 保存大纲（必须带 META 头）

```
file_write(
  path="output/<pipeline_id>/大纲.md",
  content="""
<!--
META_TITLE: 从备选中选定的最终标题
META_AUTHOR: 小载
META_DIGEST: 一句话摘要30-60字
META_TYPE: 行业分析
META_PIPELINE: <pipeline_id>
-->

# 大纲

## 标题备选
1. xxx
2. xxx
3. xxx

## 核心论点
...

## 文章结构
...
"""
)
```

### 5. 验证（宣布完成前必跑）

写完 `大纲.md` 后立即跑校验器，按报错修，重跑直到 OK：

```
shell_exec(command="python3 .flows/outline-writer/scripts/validate_outline.py output/<pipeline_id>/大纲.md")
```

- 看到 `OUTLINE_OK` 才算完成；看到 `OUTLINE_INVALID:N` 就按列出的 `ERROR:` 逐条修再重跑（常见：漏 META 头 / META_DIGEST 不在 30-60 字 / 标题备选不足 3 个 / 正文写了 `流水线ID:`）。
- 校验只查结构，不判大纲质量——质量靠遵循 writing-style + 人审。

### 6. 输出结果

完成后输出以下信息，由主控 agent 手动推进下一步：
- 流水线 ID
- 大纲路径
- 标题备选（3个）
- 一句话核心论点

## Important Principles

- **流水线 ID 必须从触发 message 里提取，所有路径用它派生**
- **所有中间数据存 `output/<pipeline_id>/` 目录，不用 knowledge_add**
- **大纲.md 必须以 `<!-- META -->` 头开头，标题/作者/摘要/类型/流水线ID 全部在 META 头里**
- **禁止在正文区域写 `流水线ID:`**——它只活在 META 头里
- 大纲风格严格遵循 system prompt 中的 writing-style
