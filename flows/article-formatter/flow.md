---
name: article-formatter
description: 将 Markdown 文章转换为微信公众号兼容的内联样式 HTML
version: 5
tools:
  - file_read
  - file_write
---
# Article Formatter

将 Markdown 文章转换为微信公众号兼容的**内联样式 HTML**。本 skill 是系统级共享工作流；配色和组件风格遵循分身 system prompt 的 writing-style（未指定时用默认现代风）。

完整的组件库（HTML 模板）见 `references/components.md`，排版时按需查阅。

## 工具名规范

- 写文件 = `file_write`（不是 ~~mcp__tools__file_write~~）
- 读文件 = `file_read`

## ⚠️ 工具调用规则

**所有 required 参数必须在一次调用中全部传齐。**

## Process

### 1. 读取 Markdown

从 message 提取流水线 ID：

```
file_read(path="output/<pipeline_id>/正文.md")
```

识别文章类型（产品文章 / 行业分析 / 热点评论 / 深度教程）。

### 2. 确定配色方案

从 system prompt 的 writing-style 读取主题色。未指定时用默认现代风（紫渐变 #667eea→#764ba2）。

### 3. 文章类型智能匹配

根据内容特征选择排版模式：

| 文章类型 | 特征识别 | 推荐组件组合 |
|---------|---------|------------|
| **产品文章** | 功能介绍、使用教程、更新日志 | 步骤流程卡 + 对比卡片 + 工具卡片 + 文末行动卡 |
| **行业分析** | 数据对比、趋势判断、多维度评测 | 数据条 + 标签页卡 + 时间线 + 引用高亮卡 |
| **热点评论** | 观点输出、争议话题、快速反应 | 引用高亮卡 + 投票互动卡 + 摘要信息卡 |
| **深度教程** | 代码实践、配置指南、原理讲解 | 代码块 + 步骤流程卡 + 折叠详情卡 + 提示卡 |

### 4. 逐元素转换

按 `references/components.md` 的模板转换 Markdown 元素（标题→胶囊标题、加粗→彩色 strong、代码块→深色块、引用→提示卡等）。配色用第 2 步确定的主题色替换默认色。

### 5. 智能排版节奏

**每 2-3 段纯文字后，必须插入一个视觉组件**，打破"文字墙"。根据文章类型调整密度（产品文密、热点评论疏）。

### 6. 后处理

- 所有 `<div>` 替换为 `<section>`
- 压缩 HTML：标签间不留多余换行/空格
- 确认没有 CSS class、没有 `<style>` 标签
- 确认所有样式内联
- 不使用 `<h1>`（公众号有自己的标题系统）

### 7. 输出

保存到 `output/<pipeline_id>/正文.html`，告知主控 agent 排版完成（流水线 ID + HTML 路径），由主控 agent 调用 draft-publisher 发布。

## Important Principles

### 核心规范
- 严格遵循公众号 HTML 规范，不使用不支持的标签
- 不使用 `<h1>`
- 图片外部 URL 必须可公开访问
- 代码块用深色背景 `<section>` 包裹
- 配色遵循 writing-style 主题色

### 兼容性
- **不用 flex 布局**：公众号对 flex 支持不稳定，对比卡片用纵向堆叠
- **渐变色降级**：`linear-gradient` 同时设 `background-color` 降级色
- **代码块换行**：`white-space:pre` + `overflow-x:auto` 保证长代码正确换行

### 组件库
完整 HTML 模板见 `references/components.md`（渐变胶囊标题、摘要卡、深色代码块、提示卡、对比卡、编号卡、数据条、时间线、引用高亮卡、文末行动卡等 18 个组件）。
