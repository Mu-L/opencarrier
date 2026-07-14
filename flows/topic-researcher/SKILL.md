---
name: topic-researcher
description: 搜索热点话题与选题研究，帮助用户选题
version: 3
tools:
  - web_search
  - web_fetch
  - file_write
---
# Topic Researcher

搜索热点话题、评估选题价值、生成写作素材。本 skill 是系统级共享工作流；选题角度和框架遵循分身 system prompt 的 writing-style。

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

你的 system prompt 中的 knowledge 部分包含当前用户的专属写作风格（writing-style）。所有选题、角度、框架选择都必须遵循该风格。如果风格未指定，使用通用新媒体写作风格。

## ⚠️ 工具调用规则

**所有 required 参数必须在一次调用中全部传齐。** 缺任何一个 required 字段都会报错 `missing field xxx`。

## Process

### 1. 多源搜索

用 `web_search` 搜索近 24-48h 热点（行业动态、产品发布、技术突破、争议话题）。

### 2. 写作风格切入点匹配

根据 system prompt 中的 writing-style，匹配最合适的选题角度和框架。

### 3. 筛选评估

时效性 30% + 读者关注度 25% + 主题关联度 25% + 可写性 20%。

### 4. 自动选择最佳话题（流水线模式）

当作为写作流水线的第 1 步触发时，从候选中选出得分最高的 1 个，生成流水线 ID（格式 `pipeline-<YYYYMMDD>-<话题关键词>`），保存素材：

```
file_write(path="output/<流水线ID>/素材.md", content="流水线ID: <流水线ID>\n\n整理后的素材")
```

### 5. 输出

**流水线模式**：告知用户流水线已启动（流水线 ID + 素材路径 + 选定话题），由主控 agent 手动推进到 outline-writer。

**交互模式**（用户只找热点）：输出 3-5 个选题等用户选择，不触发流水线。

## Important Principles

- **流水线 ID 是数据隔离的命脉，每一步都必须透传**
- **所有中间数据存 `output/<流水线ID>/` 目录，不用 knowledge_add**
- 选题角度遵循 system prompt 中的 writing-style
