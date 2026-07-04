# Adaptive Modality — 三层模型动态切换

> ⚠️ **已废弃（2026-07-03）**：本文描述的"按轮次切换 modality"方案经生产验证导致
> 能力参差不齐、工具执行不可靠（弱模型拿到为强模型设计的工具 schema），已弃用。
> 替代方案��� [`docs/AGENT-LOOP-REMEDIATION.md`](AGENT-LOOP-REMEDIATION.md)：去掉轮次切换，
> 全程使用单一 `reasoning` 模型。注意：落地实现 `pick_modality` 早已偏离本文的原始设计意图
> （本文原意是事件���动——首轮与最终回答用 reasoning——但实现简化成了纯奇偶切换）。
> 若未来要恢复多模型，应走"任务级 routing"（分类一次定模型），而非轮次切换。

## 背景

LLM 调用是 OpenCarrier 最大的运营成本。当前 agent loop 每一轮都用同一个 modality（通常是 `chat`），但不同轮次对模型能力的需求差异很大：

- **第一轮**需要深度理解用户意图、规划步骤 → 需要强推理模型
- **中间轮**只是看工具返回结果、决定调下一个工具 → 普通模型就够
- **摘要/快速提取** → 最便宜的模型就行

如果全程用贵模型，大部分中间轮次的推理能力被浪费了。

## 方案

复用 brain.json 现有的 modality 机制，在 agent loop 中根据轮次性质动态切换 modality：

| modality | 定位 | 典型模型 | 用在哪 |
|----------|------|---------|--------|
| `reasoning` | 最贵，强推理 | Claude Opus / Sonnet thinking | 意图理解、策略规划、最终综合 |
| `chat` | 普通，日常对话 | Claude Sonnet / Kimi K2 | 看结果→调工具、一般对话 |
| `fast` | 最便宜，快速 | Haiku / GLM Flash | 摘要、简单提取（已有） |

## 切换策略

```
Turn 1 → reasoning（理解意图，发出第一个 tool_use）
Turn 2 → chat（看工具结果，决定下一步）
Turn 3 → chat（继续执行）
...
Turn N（连续N轮chat无策略变化）→ reasoning（审视全局，是否需要调整方向）
最终回答 → reasoning（综合所有结果，输出高质量回答）
摘要 → fast（已有，不变）
```

### 核心规则

```rust
fn pick_modality(ctx: &TurnContext) -> &str {
    // 1. 第一轮：必须 reasoning 理解意图
    if ctx.iteration == 0 { return "reasoning" }

    // 2. 最终回答（无 tool_use）：reasoning 综合输出
    if ctx.last_was_text_only { return "reasoning" }

    // 3. 中间执行轮：chat
    "chat"
}
```

### 降级机制

```
reasoning 不可用 → 降级到 chat
chat 不可用 → 降级到 fast
```

不配 `reasoning` modality 就跟现在一样全用 `chat`，完全向后兼容。

## 配置

brain.json 中配置三个 modality，复用现有 endpoint 和 fallback 链：

```json
{
  "endpoints": {
    "opus":   { "provider": "anthropic", "model": "claude-opus-4",       "base_url": "...", "format": "anthropic" },
    "sonnet": { "provider": "anthropic", "model": "claude-sonnet-4-6",   "base_url": "...", "format": "anthropic" },
    "haiku":  { "provider": "anthropic", "model": "claude-haiku-4-5",    "base_url": "...", "format": "anthropic" }
  },
  "modalities": {
    "reasoning": { "primary": "opus",   "fallbacks": ["sonnet"] },
    "chat":      { "primary": "sonnet", "fallbacks": ["kimi", "deepseek"] },
    "fast":      { "primary": "haiku",  "fallbacks": ["zhipu_flash"] }
  },
  "default_modality": "chat"
}
```

## 流程示例

用户问："帮我调研一下 Rust async runtime 对比 tokio 和 glommio"

```
Turn 1 [reasoning - Claude Opus]
  → 理解：这是技术调研任务，需要搜索+对比分析
  → tool_use: web_search("tokio vs glommio async runtime comparison")

Turn 2 [chat - Claude Sonnet]
  → 看搜索结果：有3篇相关文章
  → tool_use: web_fetch(url=第一篇)

Turn 3 [chat - Claude Sonnet]
  → 看文章内容：提取了 tokio 的特点
  → tool_use: web_fetch(url=第二篇)

Turn 4 [chat - Claude Sonnet]
  → 看文章内容：提取了 glommio 的特点
  → tool_use: web_search("tokio glommio benchmark performance")

Turn 5 [chat - Claude Sonnet]
  → 看 benchmark 结果
  → 返回纯文本回答

Turn 6 [reasoning - Claude Opus]
  → 审核回答，补充分析深度，输出最终结果
```

7轮中仅2轮用 reasoning，5轮用 chat。相比全用 reasoning，成本降低约 60-70%。

## 成本估算

假设一个典型任务：8轮 agent loop，1轮意图理解 + 5轮工具执行 + 1轮审视 + 1轮最终回答。

| 方案 | reasoning 轮次 | chat 轮次 | fast 轮次 | 相对成本 |
|------|---------------|-----------|-----------|---------|
| 全 reasoning | 8 | 0 | 0 | 100% |
| 全 chat | 0 | 8 | 0 | ~20% |
| 自适应切换 | 2-3 | 5-6 | 0 | ~35-45% |

以 Claude 定价为参考（Opus $15/M input, Sonnet $3/M input, Haiku $0.80/M input），
自适应切换相比全 reasoning 节省 55-65%，同时保持关键步骤的推理质量。

## 实现要点

### 改动范围

1. **`agent_loop/mod.rs`** — 核心改动，第386-394行的 modality 选择逻辑
   - 当前：写死用 `manifest.model.modality` 或 `"chat"`
   - 改后：根据 `iteration` 和上下文动态选 `reasoning` / `chat` / `fast`

2. **`agent_loop/helpers.rs`** — `SUMMARY_MODALITY` 已有，不变

3. **降级逻辑** — brain 查询 modality 时，如果 `reasoning` 不存在则回退到 `chat`

### 不需要改的

- `brain.json` 格式 — 完全复用现有 modality 机制
- `Brain` trait — 不需要新增接口
- `ModalityConfig` — 不需要新增字段
- `KernelConfig` — 不需要新增配置项

### 向后兼容

- 没有 `reasoning` modality → 全用 `chat`，跟现在一样
- 没有 `fast` modality → 摘要用 `chat`，跟现在一样
- 只配了 `chat` → 行为完全不变

## 未来扩展

- **智能切换**：根据 tool_use 复杂度决定下一轮用 `reasoning` 还是 `chat`（比如发现新信息需要重新规划时升级）
- **成本统计**：按 modality 统计 token 用量，方便运营分析
- **Per-agent 覆盖**：agent.toml 里指定默认 modality 偏好（比如简单 agent 全用 chat）
