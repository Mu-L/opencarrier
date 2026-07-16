---
name: article-formatter
description: 将 Markdown 文章转换为微信公众号兼容的内联样式 HTML
version: 6
tools:
  - file_read
  - file_write
---
# Article Formatter

将 Markdown 文章转换为微信公众号兼容的**内联样式 HTML**。本 skill 是系统级共享工作流；配色和组件风格遵循分身 system prompt 的 writing-style（未指定时用默认现代风）。

完整的组件库（18 个 HTML 模板）已**内联在本文末尾《组件库》章节**，排版时直接查阅。

⚠️ **绝对不要 `file_read("references/components.md")` 或任何 `references/...` 路径**——这些文件不在你的工作目录下，读不到。读不到就跳过、用本文末尾的模板，**切勿反复重试同一路径**（会耗尽迭代上限导致整篇排版失败）。

## 工具名规范

- 写文件 = `file_write`（不是 ~~mcp__tools__file_write~~）
- 读文件 = `file_read`

## ⚠️ 工具调用规则

**所有 required 参数必须在一次调用中全部传齐。**

**`file_read` 报 "No such file" 时，绝不反复重试同一路径**——换正确路径或跳过继续。

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

按本文末尾《组件库》的模板转换 Markdown 元素（标题→胶囊标题、加粗→彩色 strong、代码块→深色块、引用→提示卡等）。配色用第 2 步确定的主题色替换默认色。

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

## 组件库

article-formatter 排版时使用的 18 个内联样式 HTML 组件模板。所有组件用 `<section>`（不用 `<div>`），样式全内联，不用 flex 布局。

**配色**：默认现代风（紫渐变 `#667eea→#764ba2`）。若分身 system prompt 的 writing-style 指定了主题色，把下文模板里的 `#667eea` / `#764ba2` / `#1a1a2e` 等替换成 writing-style 的主题色。

---

### 基础组件

#### 1. 渐变胶囊标题

```html
<section style="margin-top:2em;margin-bottom:1em;"><section style="display:inline-block;padding:4px 16px;background:linear-gradient(135deg,#667eea 0%,#764ba2 100%);border-radius:4px;"><span style="font-size:18px;font-weight:bold;color:#fff;">标题文字</span></section></section>
```

#### 2. 摘要信息卡

```html
<section style="margin:1.5em 0;padding:20px;background:linear-gradient(135deg,#f5f7fa 0%,#c3cfe2 100%);border-radius:8px;"><section style="font-size:14px;color:#666;margin-bottom:8px;">📌 核心数据</section><section style="font-size:24px;font-weight:bold;color:#1a1a2e;">85.5k Stars</section><section style="font-size:14px;color:#666;margin-top:4px;">补充说明</section></section>
```

#### 3. 深色代码块（修复换行）

`white-space:pre` + `overflow-x:auto` 保证长代码正确换行。

```html
<section style="background:#1e1e2e;border-radius:8px;padding:16px 20px;margin:1em 0;overflow-x:auto;"><section style="font-size:12px;color:#6c7086;margin-bottom:8px;">Python</section><pre style="margin:0;white-space:pre;word-wrap:normal;overflow-x:auto;"><code style="color:#cdd6f4;font-family:'Courier New',monospace;font-size:14px;line-height:1.6;">代码内容</code></pre></section>
```

手动语法高亮色值（Catppuccin Mocha）：
- 关键字：`#cba6f7`（紫）
- 函数名：`#89b4fa`（蓝）
- 字符串：`#a6e3a1`（绿）
- 类型：`#f9e2af`（黄）
- 注释：`#6c7086`（灰）

#### 4. 提示卡（三种类型）

**💡 信息提示（蓝色）**
```html
<section style="margin:1em 0;padding:16px;background:#e8f4fd;border-left:4px solid #1890ff;border-radius:0 8px 8px 0;"><section style="font-size:14px;color:#1890ff;font-weight:bold;margin-bottom:4px;">💡 提示</section><section style="font-size:15px;color:#333;line-height:1.8;">内容</section></section>
```

**⚠️ 警告提示（橙色）**
```html
<section style="margin:1em 0;padding:16px;background:#fff7e6;border-left:4px solid #fa8c16;border-radius:0 8px 8px 0;"><section style="font-size:14px;color:#fa8c16;font-weight:bold;margin-bottom:4px;">⚠️ 注意</section><section style="font-size:15px;color:#333;line-height:1.8;">内容</section></section>
```

**🔑 关键要点（绿色）**
```html
<section style="margin:1em 0;padding:16px;background:#f6ffed;border-left:4px solid #52c41a;border-radius:0 8px 8px 0;"><section style="font-size:14px;color:#52c41a;font-weight:bold;margin-bottom:4px;">🔑 要点</section><section style="font-size:15px;color:#333;line-height:1.8;">内容</section></section>
```

#### 5. 对比卡片（纵向堆叠，不用 flex）

```html
<section style="margin:1.5em 0;"><section style="padding:16px;background:#fff1f0;border-radius:8px 8px 0 0;"><section style="font-size:14px;color:#ff4d4f;font-weight:bold;margin-bottom:8px;">❌ 之前</section><section style="font-size:15px;color:#333;line-height:1.8;">内容A</section></section><section style="padding:16px;background:#f6ffed;border-radius:0 0 8px 8px;"><section style="font-size:14px;color:#52c41a;font-weight:bold;margin-bottom:8px;">✅ 之后</section><section style="font-size:15px;color:#333;line-height:1.8;">内容B</section></section></section>
```

#### 6. 编号列表卡片（纵向堆叠版）

```html
<section style="margin:1em 0;padding-left:40px;position:relative;margin-bottom:16px;"><section style="position:absolute;left:0;top:0;width:28px;height:28px;background:linear-gradient(135deg,#667eea,#764ba2);border-radius:50%;text-align:center;line-height:28px;color:#fff;font-size:14px;font-weight:bold;">1</section><section style="font-size:16px;font-weight:bold;color:#1a1a2e;">标题</section><section style="font-size:15px;color:#666;line-height:1.8;margin-top:4px;">描述</section></section>
```

---

### 进阶组件

#### 7. 标签页卡

```html
<section style="margin:1.5em 0;border:1px solid #e8e8e8;border-radius:8px;overflow:hidden;">
  <section style="background:#f5f7fa;padding:12px 16px;border-bottom:1px solid #e8e8e8;">
    <section style="display:inline-block;padding:4px 12px;background:linear-gradient(135deg,#667eea,#764ba2);border-radius:4px;color:#fff;font-size:14px;font-weight:bold;">开源模型</section>
    <section style="display:inline-block;padding:4px 12px;color:#666;font-size:14px;margin-left:8px;">闭源模型</section>
  </section>
  <section style="padding:16px;">
    <section style="font-size:15px;color:#333;line-height:1.8;">内容...</section>
  </section>
</section>
```

#### 8. 时间线卡片

```html
<section style="margin:1.5em 0;padding-left:24px;border-left:2px solid #e8e8e8;">
  <section style="margin-bottom:20px;position:relative;">
    <section style="position:absolute;left:-29px;top:4px;width:12px;height:12px;background:linear-gradient(135deg,#667eea,#764ba2);border-radius:50%;"></section>
    <section style="font-size:14px;color:#667eea;font-weight:bold;">2024.03</section>
    <section style="font-size:15px;color:#333;line-height:1.8;margin-top:4px;">MCP 协议首次发布</section>
  </section>
</section>
```

#### 9. 数据条（Data Bar）

```html
<section style="margin:1em 0;">
  <section style="margin-bottom:12px;">
    <section style="font-size:14px;color:#333;margin-bottom:4px;">DeepSeek-V4 <span style="color:#999;">92.5%</span></section>
    <section style="height:8px;background:#f0f0f0;border-radius:4px;overflow:hidden;">
      <section style="height:100%;width:92.5%;background:linear-gradient(90deg,#667eea,#764ba2);border-radius:4px;"></section>
    </section>
  </section>
</section>
```

#### 10. 工具/产品卡片

```html
<section style="margin:1em 0;padding:16px;background:#f8f9fa;border-radius:8px;border:1px solid #e8e8e8;">
  <section style="font-size:16px;font-weight:bold;color:#1a1a2e;margin-bottom:8px;">🚀 claude-mem</section>
  <section style="font-size:14px;color:#666;line-height:1.6;margin-bottom:8px;">为 Claude 提供跨会话记忆层</section>
  <section style="font-size:13px;color:#999;">⭐ 78.3k Stars · Python · MIT</section>
</section>
```

#### 11. 引用高亮卡

```html
<section style="margin:1.5em 0;padding:20px;background:linear-gradient(135deg,#667eea 0%,#764ba2 100%);border-radius:8px;">
  <section style="font-size:18px;color:#fff;font-weight:bold;line-height:1.6;font-style:italic;">"Agent 的记忆层，正在成为新的基础设施。"</section>
  <section style="font-size:14px;color:rgba(255,255,255,0.8);margin-top:12px;text-align:right;">—— 出处</section>
</section>
```

#### 12. 步骤流程卡

```html
<section style="margin:1em 0;">
  <section style="margin-bottom:16px;">
    <section style="display:inline-block;padding:2px 8px;background:#1890ff;color:#fff;font-size:12px;border-radius:4px;margin-bottom:6px;">STEP 1</section>
    <section style="font-size:15px;color:#333;line-height:1.8;">安装 MCP SDK</section>
  </section>
</section>
```

#### 13. 投票/互动卡

```html
<section style="margin:1.5em 0;padding:16px;background:#fff7e6;border-radius:8px;border:1px dashed #fa8c16;">
  <section style="font-size:15px;color:#333;font-weight:bold;margin-bottom:12px;">🤔 你觉得 Agent 记忆层会成为标配吗？</section>
  <section style="font-size:14px;color:#666;line-height:1.8;">A. 必然趋势</section>
  <section style="font-size:14px;color:#666;line-height:1.8;">B. 还有很长的路要走</section>
</section>
```

---

### v3 新增组件

#### 14. 阅读进度条（长文顶部）

```html
<section style="margin:0 0 1.5em 0;padding:12px 16px;background:#f5f7fa;border-radius:8px;">
  <section style="font-size:13px;color:#999;margin-bottom:8px;">📖 阅读进度</section>
  <section style="height:4px;background:#e8e8e8;border-radius:2px;overflow:hidden;">
    <section style="height:100%;width:0%;background:linear-gradient(90deg,#667eea,#764ba2);border-radius:2px;transition:width 0.3s;"></section>
  </section>
  <section style="font-size:12px;color:#bbb;margin-top:6px;text-align:right;">预计阅读 5 分钟</section>
</section>
```

#### 15. 折叠详情卡

```html
<section style="margin:1em 0;border:1px solid #e8e8e8;border-radius:8px;overflow:hidden;">
  <section style="padding:12px 16px;background:#f5f7fa;">
    <section style="font-size:14px;color:#667eea;font-weight:bold;">📎 展开查看技术细节</section>
  </section>
  <section style="padding:16px;">
    <section style="font-size:14px;color:#666;line-height:1.8;">折叠的内容...</section>
  </section>
</section>
```

#### 16. 标签云

```html
<section style="margin:1.5em 0;">
  <section style="font-size:14px;color:#999;margin-bottom:12px;">🏷️ 相关标签</section>
  <section style="display:inline-block;padding:4px 12px;background:#f0f0f0;border-radius:12px;font-size:13px;color:#666;margin-right:8px;margin-bottom:8px;">MCP</section>
  <section style="display:inline-block;padding:4px 12px;background:#f0f0f0;border-radius:12px;font-size:13px;color:#666;margin-right:8px;margin-bottom:8px;">AI Agent</section>
</section>
```

#### 17. 多维度数据对比表

```html
<section style="margin:1.5em 0;border:1px solid #e8e8e8;border-radius:8px;overflow:hidden;">
  <section style="padding:12px 16px;background:#f5f7fa;font-size:14px;font-weight:bold;color:#333;">多维度评测对比</section>
  <section style="padding:16px;">
    <section style="margin-bottom:16px;">
      <section style="font-size:13px;color:#999;margin-bottom:4px;">Agentic Coding</section>
      <section style="height:6px;background:#f0f0f0;border-radius:3px;overflow:hidden;margin-bottom:4px;">
        <section style="height:100%;width:92.5%;background:linear-gradient(90deg,#667eea,#764ba2);border-radius:3px;"></section>
      </section>
      <section style="font-size:12px;color:#666;">DeepSeek-V4: 92.5% | Claude 4: 89.3%</section>
    </section>
  </section>
</section>
```

#### 18. 文末行动卡

署名/账号名按分身实际信息替换（不要硬编码"小载"）。

```html
<section style="margin-top:2em;padding:24px 20px;background:linear-gradient(135deg,#f5f7fa 0%,#e8e8f0 100%);border-radius:8px;">
  <section style="text-align:center;margin-bottom:16px;">
    <section style="font-size:16px;font-weight:bold;color:#1a1a2e;margin-bottom:4px;">觉得有用？分享给需要的人</section>
    <section style="font-size:13px;color:#999;">关注本号获取更多内容</section>
  </section>
  <section style="border-top:1px solid #e0e0e0;padding-top:16px;margin-top:16px;">
    <section style="font-size:13px;color:#999;margin-bottom:8px;">📚 相关阅读</section>
    <section style="font-size:14px;color:#667eea;line-height:1.8;">• 相关文章一</section>
    <section style="font-size:14px;color:#667eea;line-height:1.8;">• 相关文章二</section>
  </section>
</section>
```
