# OpenCarrier 内置共享 Skill

> 这些 skill 随主项目分发，部署到 `~/.opencarrier/skills/`，所有分身默认可用。通过 LLM 语义匹配自动激活（无需 import 声明），命中后加载全文并注入其声明的 tools。详见 [docs/SKILL-STANDARD.md](../docs/SKILL-STANDARD.md) 的「系统级共享 Skill」章节。

## 写作流水线

| Skill | 说明 |
|-------|------|
| `topic-researcher` | 搜索热点话题、评估选题价值、生成写作素材 |
| `outline-writer` | 根据素材撰写文章大纲 |
| `article-writer` | 根据大纲撰写完整 Markdown 文章正文 |
| `article-formatter` | 将 Markdown 转换为微信公众号兼容的内联样式 HTML |
| `draft-publisher` | 将排版好的文章发布到微信公众号草稿箱 |

## 流水线顺序

```
topic-researcher → outline-writer → article-writer → article-formatter → draft-publisher
   (选题/素材)        (大纲)            (正文)           (排版)              (发布)
```

每步的中间产物存 `output/<pipeline_id>/` 目录，由主控 agent 手动推进下一步（不再用 cron 链式触发）。

## 包车业务

| Skill | 说明 |
|-------|------|
| `charter-quoter` | 包车报价与订单处理（收集信息→查表报价→推送通知→找车队） |

## 餐饮运营

| Skill | 说明 |
|-------|------|
| `schedule-chart` | 生成排班表和全天在岗覆盖图，多班次多人可视化 |

## 设计原则

- **系统 skill = 通用工作流**：≥2 个分身会重复用的纯流程，与具体人格/语气无关
- **风格从 system prompt 读**：写作风格、配色、署名等个性化信息不硬编码在系统 skill 里，而是从分身的 writing-style 注入
- **分身只保留私有 skill**：独特的人格、领域逻辑、语气定制定义在 `workspaces/{agent}/skills/`
