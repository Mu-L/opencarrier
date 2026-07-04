# Agent Loop 设计文档

> 状态：草案
> 关联：取代 `AGENT-LOOP-REMEDIATION.md`（Phase 1 已落地）和 `LOOP-ENGINEERING.md`（优化方案合并入本文）
> 日期：2026-07-04

---

## 目录

1. [架构全景](#1-架构全景)
2. [当前实现详解](#2-当前实现详解)
3. [问题清单](#3-问题清单)
4. [设计原则](#4-设计原则)
5. [重新设计](#5-重新设计)
6. [组件契约](#6-组件契约)
7. [优化条目与优先级](#7-优化条目与优先级)
8. [实施路线](#8-实施路线)
9. [验证与度量](#9-验证与度量)

---

## 1. 架构全景

### 1.1 Agent Loop 在系统中的位置

```
Channel (weixin/feishu/wecom/api)
  │ 入站消息
  ▼
PluginBridgeManager → SenderRouter → agent_name
  │
  ▼
CarrierKernel::send_message()
  │ 加载 session、记忆、工具、skills、subagents
  ▼
┌─────────────────────────────────────────────┐
│              run_agent_loop()               │  ← 本文档的讨论范围
│                                             │
│  Setup → Loop(0..25) → Teardown             │
│    │         │                                │
│    │         ├── pick_modality               │
│    │         ├── call_with_fallback          │
│    │         └── dispatch StopReason         │
│    │              ├── EndTurn                │
│    │              ├── ToolUse                │
│    │              └── MaxTokens              │
│                                             │
│  输出: AgentLoopResult { response, usage }  │
└─────────────────────────────────────────────┘
  │
  ▼
Session 持久化 → TurnSummary → Knowledge Drawer
```

### 1.2 核心约束

| 约束 | 值 | 说明 |
|------|-----|------|
| 最大迭代 | 25 (可配) | 防止无限循环 |
| 整体超时 | 600s | tokio::time::timeout 包裹 |
| LLM 调用超时 | 180s/次, 300s 流式墙钟 | 防止 API 挂死 |
| 上下文窗口 | 128K (默认) | 触发 context guard |
| 历史消息上限 | 30 | 安全阀，超限裁剪 |
| LLM 重试 | 3 次 | 指数退避 |
| 工具执行超时 | 120s/300s | 分短/长工具 |

### 1.3 代码布局

```
crates/runtime/src/agent_loop/
├── mod.rs          (760 行) 主循环编排
├── helpers.rs      (617 行) LLM 调用/重试/fallback、循环检测、turn summary
├── end_turn.rs     (441 行) EndTurn/StopSequence 处理
├── tool_use.rs     (571 行) ToolUse 处理
├── max_tokens.rs   (107 行) MaxTokens 处理
└── tests.rs        (1811 行) 测试
```

---

## 2. 当前实现详解

### 2.1 主循环流程 (mod.rs)

```
run_agent_loop(600s timeout)
  └── run_agent_loop_impl
        │
        ├── [Setup]
        │   ├── 计算 loop_deadline
        │   ├── 提取 hand_allowed_env (manifest metadata)
        │   ├── 触发 BeforePromptBuild hook
        │   ├── 构建 system_prompt
        │   ├── 添加 user message 到 session.messages
        │   ├── 从 session.messages 构建 llm_messages (过滤 System)
        │   ├── validate_and_repair(messages)
        │   ├── 注入 canonical_context_msg (如果有)
        │   ├── 裁剪超长历史 (MAX_HISTORY_MESSAGES = 30)
        │   └── 初始化 loop 状态变量
        │
        ├── [Loop: for iteration in 0..max_iterations]
        │   │
        │   ├── 1. 上下文溢出恢复 recover_from_overflow()
        │   ├── 2. 上下文防护 apply_context_guard()
        │   ├── 3. 构建 CompletionRequest { messages, tools, system, ... }
        │   ├── 4. 阶段回调 (Streaming/Thinking) — 仅首轮 Streaming
        │   │
        │   ├── 5. Modality 选择
        │   │   ├── pick_modality() → 总是 reasoning (Phase 1 改动)
        │   │   └── remaining_secs < 120 时强制 reasoning (事件驱动)
        │   │
        │   ├── 6. Loop status 注入 (每 2 轮，含错误工具历史)
        │   │
        │   ├── 7. call_with_fallback → LLM 调用
        │   │   ├── 遍历 Brain endpoints
        │   │   ├── call_with_retry (最多 3 次，指数退避)
        │   │   ├── circuit breaker 检查
        │   │   └── LLM 并发信号量
        │   │
        │   ├── 8. 预算耗尽补救 (time budget exhausted)
        │   │   └── 最后一次 reasoning 调用 (无工具，强制收尾)
        │   │
        │   ├── 9. 累加 token usage
        │   │
        │   ├── 10. 文本工具调用恢复 (streaming 路径)
        │   │   └── 最多 2 次重试
        │   │
        │   └── 11. dispatch by StopReason:
        │       ├── EndTurn/StopSequence → handle_end_turn()
        │       │   ├── 解析指令 (NO_REPLY, silent, reply_to)
        │       │   ├── 空响应重试 (含 sustained-failure 保护)
        │       │   ├── 空响应 guard (fallback 中文提示)
        │       │   ├── 保存 session
        │       │   ├── 生成 TurnSummary
        │       │   ├── 知识提取 → knowledge drawer
        │       │   └── 裁剪旧消息 (MAX_RETAINED_MESSAGES = 12)
        │       │
        │       ├── ToolUse → handle_tool_use()
        │       │   ├── 重置连续 MaxTokens 计数
        │       │   ├── 记录工具调用 (用于 loop 检测)
        │       │   ├── loop 检测 → 移除死循环工具
        │       │   ├── 逐个执行工具调用:
        │       │   │   ├── BeforeToolCall hook (可阻止)
        │       │   │   ├── 超时包裹执行 (120s/300s)
        │       │   │   ├── AfterToolCall hook
        │       │   │   ├── skill_load 去重
        │       │   │   └── 动态截断工具结果
        │       │   ├── 工具错误跟踪 (累计 + 升级警告)
        │       │   ├── 动态工具刷新 (tool_search/skill_load)
        │       │   └── task_plan 检测 → break
        │       │
        │       └── MaxTokens → handle_max_tokens()
        │           ├── 累计连续计数
        │           ├── < 5 次: "Please continue" 续写
        │           └── >= 5 次: 返回 partial response
        │
        └── [Teardown — max iterations exceeded]
            ├── 保存 user message + 错误摘要 (丢弃工具噪音)
            ├── 触发 AgentLoopEnd hook
            └── 返回 MaxIterationsExceeded 错误
```

### 2.2 LLM 调用链 (helpers.rs)

```
call_with_fallback(brain, modality, request)
  │
  ├── brain.endpoints_for(modality)  → Vec<Endpoint>
  │
  └── for ep in endpoints:
        ├── 跳过剩余时间不足的 endpoint (< 30s)
        ├── req.model = ep.model
        └── call_with_retry(driver, req, stream_tx, provider)
              │
              ├── circuit breaker 检查 (ProviderCooldown)
              ├── 计算 per-call timeout
              ├── 获取 LLM 并发信号量 (30s 超时)
              └── for attempt in 0..=3:
                    ├── tokio::time::timeout 包裹 stream 调用
                    ├── 成功 → 记录成功，返回
                    ├── RateLimited → 等待 retry_after，继续
                    ├── Overloaded  → 等待 retry_after，继续
                    └── 其他错误  → 分类 (llm_errors::classify)，记录失败，返回
```

### 2.3 双轨消息维护

Loop 维护两份消息列表:

| 列表 | 用途 | 生命周期 |
|------|------|----------|
| `session.messages` | 持久化到 SQLite | 跨 loop 运行 |
| `messages` | 发给 LLM 的 working copy | 单次 loop |

**同步规则**:
- Setup: `messages` 从 `session.messages` 拷贝（过滤 System role）
- ToolUse/MaxTokens: 两边同时 push
- EndTurn: `session` push → 裁剪 → 持久化；`messages` 作为 working copy 被丢弃
- 失败时: `session.messages` 被部分裁剪（只保留 user + 错误摘要）

**已知不一致**:
- ToolUse 中 assistant blocks 推了两遍（line 88-91 和 line 92-95 分别推 session 和 messages）
- MaxTokens 中 "Please continue" 也推了两遍

### 2.4 Loop 检测机制

```
recent_tool_calls: Vec<(tool_name, input_hash)>

每次 ToolUse:
  1. 将本轮所有 tool_call 追加到 recent_tool_calls
  2. 裁剪到 LOOP_DETECTION_WINDOW * 2 (12)
  3. 检测尾部 6 个是否完全相同
  4. 如果死循环 → 移除该工具 + 注入系统消息
```

特点:
- 只检测"相同工具 + 相同输入哈希"的完全重复
- "相同工具 + 不同输入"(如分页搜索) 不算死循环
- 被动检测——发现前已浪费 6 次调用

---

## 3. 问题清单

### 3.1 架构设计问题

#### P1: Modality 选择代码冗余
**位置**: `mod.rs:392-470`
**问题**: `pick_modality` 已恒返回 reasoning，但调用处还有一个 `remaining_secs < 120` 分支也返回 reasoning。两个分支结果相同，`pick_modality` 的 `brain` 和 `_iteration` 参数事实上已无用。
**影响**: 代码混淆、维护负担。新读者会困惑"到底哪个分支生效"。

#### P2: Loop status 注入频率与模型能力不匹配
**位置**: `mod.rs:409`
**问题**: 注入条件是 `is_reasoning && iteration.is_multiple_of(2)`。Phase 1 后每轮都是 reasoning，所以 gating 退化为纯每 2 轮注入。但每次 LLM 调用是无状态的——模型在奇数轮看不到剩余时间和错误工具历史。尤其在 budget tight (< 120s) 阶段，模型可能对时间压力毫不知情。
**影响**: 模型决策信息不完整，可能在不该调工具的时候继续调工具。

#### P3: 缺少主动上下文预算预警
**位置**: `mod.rs:371` 的 `apply_context_guard` 和 `recover_from_overflow`
**问题**: 两者都是被动的——超限后才裁剪/压缩。没有在接近阈值时给模型发送预警。模型可能在不知情的情况下继续大量工具调用，导致上下文在下一轮被强制裁剪，丢失关键信息。
**影响**: 工具结果被静默截断或丢弃，模型基于不完整信息做决策。

#### P4: Loop 检测窗口过大
**位置**: `helpers.rs:102` 的 `LOOP_DETECTION_WINDOW = 6`
**问题**: 需要 6 次完全相同的(工具名, 输入哈希)才触发。这意味着在检测到死循环前，已浪费 6 次 LLM 调用 + 6 次工具执行。生产环境下，6 轮 × 每轮 5-15s ≈ 30-90s 的浪费。
**影响**: 延迟高、token 浪费。且窗口设为 6 的注释说"Below 4 risks blocking legitimate retries"——但没有数据支撑 4 不够、6 刚好。

#### P5: 文本工具调用恢复失败后的静默丢弃
**位置**: `mod.rs:585-609`
**问题**: 2 次 text recovery 重试后，LLM 输出的 `[Called tool_xxx]` 文本被当作普通文本返回给用户。用户看到的是原始工具调用文本，而非工具执行结果。
**影响**: 用户体验差——看到乱码般的 `[Called tool_xxx with {...}]`。

### 3.2 工程实现问题

#### P6: 双轨消息维护的不一致风险
**位置**: 多处
**问题**: `session.messages` 和 `messages` 处处手工同步，没有类型系统保证一致性。ToolUse 和 MaxTokens 中各有一处推了两遍，但这是否是有意行为？没有注释说明。
**影响**: 潜在的 session 持久化不一致 bug。未来修改时容易忘记同步两边。

#### P7: 工具发现的易失性
**位置**: `tool_use.rs:474-509`
**问题**: 每次新的 `tool_search` 会驱逐上一次发现的工具。如果 LLM 在同一次对话中需要用到两个不同场景的工具集，第二次 tool_search 会导致第一个场景的工具丢失。
**影响**: LLM 需要重复 tool_search 来找回丢失的工具，浪费迭代。

#### P8: 错误跟踪的重置逻辑过于乐观
**位置**: `tool_use.rs:334-348`
**问题**: 工具只要成功一次就立即重置 `consecutive_tool_errors` 计数器。如果工具在场景 A 出错 4 次、场景 B 成功 1 次、场景 A 再出错，计数器从 0 重新开始——模型看不到该工具在场景 A 有 4 次失败的"前科"。
**影响**: 错误升级警告 (≥3 次) 可能永远不会触发，模型反复在危险路径上尝试。

#### P9: 缺乏结构化的 Loop State 持久化
**位置**: 全局
**问题**: 如 `LOOP-ENGINEERING.md` 分析，agent loop 每次启动都是"冷启动"——不知道上次跑到哪、卡在哪、上次预算耗尽的原因是什么。TurnSummary 只记录了用户意图和结果，没有 loop 级别的运行状态。
**影响**: 跨 session 无记忆，agent 可能重复做同样的事、掉进同样的坑。

### 3.3 缺少的能力

#### P10: 没有 Agent 自身可读的状态暴露
Loop 在运行中积累了丰富的状态信息（iteration, error history, tool call history, context pressure），但这些信息只在日志和 loop status 注入中存在。没有统一的"LoopState"结构让模型、supervisor、dashboard 都能消费。

#### P11: 没有成本/预算的可观测性
每次 loop 跑完只有 `total_usage` 被返回给调用方，但没有累积/聚合。无法回答"ai-writer 这个 agent 今天花了多少 token？""哪个 agent 的 loop 最长？"。

#### P12: MCP 没有健康检查/退避
3 个 MCP server 连不上时每 60s 无脑重试，产生日志噪音。

---

## 4. 设计原则

### 原则 0: 简单优先 (Anthropic 第一原则)
> 复杂度只在"被证明能改善结果"时才加。

落实到 Loop:
- 能用一个模型跑完的，不加模型切换
- 能用一个 Vec<Message> 维护的，不加第二个
- 能在 setup 做一次的检查，不放到每轮循环里

### 原则 1: 信息完整原则
每轮 LLM 调用都应获得做出正确决策所需的所有信息:
- 当前是第几轮 / 还剩几轮
- 还剩多少时间
- 上下文用了多少 / 还剩多少
- 哪些工具连续失败 / 哪些工具刚成功
- 上次 loop 运行的结果（如果有）

**当前违反**: loop status 每 2 轮注入一次，奇数轮信息不完整。

### 原则 2: 显式状态原则
Loop 的内部状态应该结构化、可序列化、可被外部消费:
- 模型能读到（system message 注入）
- Supervisor 能读到（监控/决策）
- Dashboard 能读到（可观测性）

**当前违反**: 状态散落在日志、局部变量、双轨消息中。

### 原则 3: 早期预警原则
在问题发生前通知模型，而非发生后补救:
- 上下文接近阈值 → 提前告知，而非超限后裁剪
- 时间紧张 → 提前告知，而非超时后补救
- 工具连续失败 → 提前警告，而非等 6 次死循环

**当前违反**: context guard 和 overflow recovery 都是被动反应。

### 原则 4: 渐进式降级原则
当资源（时间、上下文、迭代次数）紧张时，Loop 应逐步从"尝试更多工具"降级到"尽快给出最佳答案":
1. 正常 → 自由调用工具
2. 上下文接近阈值 → 告知模型，减少非必要工具
3. 时间紧张 → 强制 reasoning，减少工具并发
4. 时间耗尽 → 最后一次无工具调用，强制收尾

**当前**: 第 3 步已实现（budget < 120s），第 1/2/4 步已实现。缺少第 1→2 的平滑过渡。

### 原则 5: 可观测性内建原则
Loop 不是黑盒。关键指标应在运行时和运行后可见:
- 每轮 token 消耗
- 工具调用成功/失败率
- 迭代次数分布
- 上下文压力变化

**当前违反**: token 只在结束时累加，无逐轮粒度。

---

## 5. 重新设计

### 5.1 Loop 状态机

将当前"一个大 for 循环 + 分支分发"重构为显式状态机:

```
                    ┌─────────┐
                    │  INIT   │ Setup: 加载 session、记忆、
                    └────┬────┘ 构建 prompt、初始化 LoopState
                         │
                         ▼
              ┌─────────────────────┐
         ┌───▶│     PREPARE_TURN    │ 每轮开始:
         │    └──────────┬──────────┘ - 上下文检查 + 预警
         │               │            - 注入 loop status
         │               │            - 选择 modality
         │               ▼
         │    ┌─────────────────────┐
         │    │     LLM_CALL        │ call_with_fallback
         │    └──────────┬──────────┘ 超时/重试/fallback
         │               │
         │               ▼
         │    ┌─────────────────────┐
         │    │   DISPATCH          │ 按 StopReason 分发
         │    └───┬─────┬─────┬─────┘
         │        │     │     │
         │   ┌────▼──┐ ┌▼───┐┌▼──────┐
         │   │END_TURN│ │TOOL││MAX_TOK│
         │   └────┬───┘ └┬───┘└───┬───┘
         │        │       │        │
         │        │  ┌────┘        │
         │        │  │  (continue) │
         │        └──┼─────────────┘
         │           │
         │           ▼
         │    ┌─────────────┐
         └────│  NEXT_TURN  │ 迭代数+1，判断是否超出
              └──────┬──────┘
                     │ (max iterations 或
                     │  上下文不可恢复)
                     ▼
              ┌─────────────┐
              │  TEARDOWN   │ 保存 session、LoopState
              └─────────────┘ 触发 hooks、返回结果
```

### 5.2 核心结构: LoopState

引入统一的 LoopState 结构，贯穿整个 loop 生命周期:

```rust
/// LoopState — loop 运行状态的完整快照。
/// 在整个 loop 执行期间由主循环维护，结束后通过 MemoryHandle 持久化。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopState {
    // ---- 运行计数 ----
    pub iteration: u32,
    pub max_iterations: u32,

    // ---- 时间预算 ----
    pub started_at: String,         // ISO 8601
    pub deadline_at: String,        // ISO 8601
    pub state: LoopBudgetState,

    // ---- 上下文预算 ----
    pub context_tokens_used_estimate: usize,
    pub context_tokens_max: usize,
    pub context_pressure: ContextPressure,

    // ---- 工具追踪 ----
    pub tools_executed: u32,
    pub tools_failed: u32,
    pub recent_tool_calls: Vec<ToolCallRecord>,
    pub consecutive_errors: HashMap<String, u32>,

    // ---- 跨 session 持久化字段 ----
    /// 上次运行的摘要（跨 session 可见）
    pub last_run: Option<LastRunSummary>,
    /// 上次 loop 的迭代次数
    pub last_iteration: u32,
    /// 上次的停止原因
    pub last_stop_reason: String,

    // ---- 运行日志 ----
    pub turn_log: Vec<TurnLogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoopBudgetState {
    Comfortable,   // > 5min remaining
    Moderate,      // 2-5min
    Tight,         // 1-2min
    Critical,      // < 1min
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextPressure {
    Normal,        // < 60%
    Elevated,      // 60-80%
    High,          // 80-95%
    Critical,      // > 95%
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub input_hash: u64,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastRunSummary {
    pub timestamp: String,
    pub iterations: u32,
    pub stop_reason: String,
    pub tokens_used: u64,
    pub outcome: RunOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunOutcome {
    Complete,
    BudgetExhausted,
    MaxIterations,
    ContextOverflow,
    Error(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnLogEntry {
    pub iteration: u32,
    pub modality: String,
    pub stop_reason: String,
    pub tokens_in: u32,
    pub tokens_out: u32,
    pub tools_called: Vec<String>,
    pub tool_errors: u32,
    pub context_pressure: ContextPressure,
}
```

**对比现状**:
- 当前: 状态散落在 10+ 个局部变量中（`iteration`, `consecutive_tool_errors`, `recent_tool_calls`, `total_usage`, `budget_warning_sent`...）
- 新设计: 一个 LoopState 统一持有，任何地方都能读取完整状态

### 5.3 Loop status 注入重新设计

**当前**: `is_reasoning && iteration.is_multiple_of(2)` → 每 2 轮注一次，含错误历史。

**重新设计**: 每轮都注入，但内容根据 `context_pressure` 和 `loop_budget_state` 动态调整:

```rust
fn build_status_message(state: &LoopState) -> String {
    let mut msg = format!(
        "📊 Turn {}/{} | ⏱️ ~{}s remaining | 📐 context: {} ({}%)",
        state.iteration + 1,
        state.max_iterations,
        state.remaining_secs(),
        state.context_pressure.as_label(),
        state.context_usage_pct(),
    );

    // 仅在有问题时追加详细信息（节省 token）
    if !state.consecutive_errors.is_empty() {
        let errors: Vec<String> = state.consecutive_errors.iter()
            .map(|(name, count)| format!("{name}(×{count})"))
            .collect();
        msg.push_str(&format!("\n⚠️ 连续出错: {}", errors.join(", ")));
    }

    // 上下文压力高时追加建议
    match state.context_pressure {
        ContextPressure::High | ContextPressure::Critical => {
            msg.push_str("\n⚠️ 上下文即将耗尽，优先输出最终答案，减少工具调用。");
        }
        _ => {}
    }

    // 时间紧张时追加建议
    match state.budget_state {
        LoopBudgetState::Tight | LoopBudgetState::Critical => {
            msg.push_str(&format!(
                "\n⏱️ 剩余 {}s，如果工具调用可能超时，直接给出当前最佳答案。",
                state.remaining_secs()
            ));
        }
        _ => {}
    }

    msg
}
```

**关键变化**:
- 每轮注入，模型始终拥有完整信息
- 上下文压力在 `High/Critical` 时主动建议收尾（原则 3: 早期预警）
- 内容自适应——问题少时消息短，节省 token

### 5.4 上下文预警系统

在 `PREPARE_TURN` 阶段（每轮循环开始），计算上下文压力等级并采取对应行动:

```rust
fn prepare_turn(state: &mut LoopState, messages: &mut Vec<Message>) {
    // 1. 估算当前上下文 token 用量
    let estimated = estimate_token_count(messages, &system_prompt, tools);
    state.context_tokens_used_estimate = estimated;
    state.context_pressure = classify_pressure(estimated, state.context_tokens_max);

    // 2. 按压力等级行动
    match state.context_pressure {
        ContextPressure::Critical => {
            // > 95%: 立即裁剪最旧的非必要消息
            compact_oldest_tool_results(messages);
        }
        ContextPressure::High => {
            // > 80%: 不裁剪，但通过 status message 告知模型
            // (status message 已在 build_status_message 中处理)
        }
        _ => {}
    }

    // 3. 仍然做原有的 overflow recovery 作为安全网
    // (这是 reactive 的 last resort，与上述 proactive 预警互补)
}
```

### 5.5 简化 Modality 选择

```rust
/// 选择当前轮次的 modality。
///
/// 策略: 全程使用 reasoning，仅在时间紧张时保持不变（已全程 reasoning）。
/// 保留函数签名以备 Phase 3 的事件驱动升级逻辑重新启用。
fn pick_modality(
    brain: Option<&Arc<dyn Brain>>,
    _iteration: u32,
    default_modality: &str,
) -> String {
    // 全程单一模型。多模型优化改走任务级 routing。
    if let Some(brain) = brain {
        if brain.has_modality(REASONING_MODALITY) {
            return REASONING_MODALITY.to_string();
        }
    }
    default_modality.to_string()
}
```

**同时清理 `mod.rs:456-470`**: `remaining_secs < 120` 分支与 `pick_modality` 结果相同，删除冗余分支。预算压力警告走 `build_status_message`。

### 5.6 Loop 检测优化

**当前**: `LOOP_DETECTION_WINDOW = 6`，被动检测。

**优化**:
1. 将窗口降到 4: "Below 4 risks blocking legitimate retries" 的担忧可以通过更精细的判断缓解
2. 增加"软检测": 相同工具连续调用 2 次（不限输入）→ 在 status message 中追加温和提醒
3. 保留"硬检测": 相同工具+相同输入连续 4 次 → 移除工具

```rust
const SOFT_LOOP_WINDOW: usize = 2;  // 相同工具连续调用 → 提醒
const HARD_LOOP_WINDOW: usize = 4;  // 相同工具+相同输入 → 移除

fn check_tool_loop(
    recent: &[(String, u64)],
    state: &mut LoopState,
    tools_owned: &mut Vec<ToolDefinition>,
) -> Option<String> {
    // 硬检测: 相同(工具, 输入哈希)连续 HARD_LOOP_WINDOW 次
    if let Some((name, _)) = detect_exact_loop(recent, HARD_LOOP_WINDOW) {
        tools_owned.retain(|t| t.name != name);
        return Some(format!(
            "工具 `{name}` 连续{HARD_LOOP_WINDOW}次返回相同结果，已被移除。请换其他方式。"
        ));
    }

    // 软检测: 相同工具名连续 SOFT_LOOP_WINDOW 次 → 仅提醒
    if recent.len() >= SOFT_LOOP_WINDOW {
        let tail = &recent[recent.len() - SOFT_LOOP_WINDOW..];
        let first_name = &tail[0].0;
        if tail.iter().all(|(n, _)| n == first_name) {
            // 不注入系统消息——由 build_status_message 在 loop status 中追加提醒
        }
    }

    None
}
```

### 5.7 错误跟踪优化

**当前**: 工具成功一次就重置计数器，场景相关的失败历史丢失。

**优化**: 引入滑动窗口计数，而非"成功就归零":

```rust
/// 最近 N 次调用的成功/失败窗口
struct ToolErrorTracker {
    window_size: usize,  // 默认 5
    history: HashMap<String, VecDeque<bool>>,  // true = success
}

impl ToolErrorTracker {
    fn record(&mut self, tool_name: &str, success: bool) {
        let entry = self.history.entry(tool_name.to_string()).or_default();
        entry.push_back(success);
        if entry.len() > self.window_size {
            entry.pop_front();
        }
    }

    fn consecutive_failures(&self, tool_name: &str) -> u32 {
        let history = match self.history.get(tool_name) {
            Some(h) => h,
            None => return 0,
        };
        // 从尾部向前数连续失败次数
        let mut count = 0;
        for success in history.iter().rev() {
            if *success { break; }
            count += 1;
        }
        count
    }
}
```

这样工具在场景 A 出错 4 次 → 场景 B 成功 1 次 → 场景 A 再出错 1 次，`consecutive_failures` 是 1 而非 0，如果场景 A 再出错 2 次，会重新达到 3 次阈值并触发升级警告。

### 5.8 单轨消息 + 同步策略

**当前**: `session.messages` 和 `messages` 双轨手工同步。

**重新设计**: 单一 `messages: Vec<Message>` 贯穿 loop，loop 结束后统一持久化:

```rust
// 主循环中只维护一份 messages
let mut loop_messages: Vec<Message> = /* 从 session 加载 */ ;

// 每轮 push assistant/tool_result 只写 loop_messages
// 不再同步写 session.messages

// Teardown 时一次性持久化
session.messages = loop_messages;  // 或合并
memory.save_session(session).await?;
```

**收益**: 消除双轨不一致 bug 源，减少 push 操作数量。

**风险**: ToolUse 中当前写 session 是为了在 loop 中途崩溃时仍有部分持久化。改为 loop 结束统一写入会丢失"中途崩溃的部分结果"——但是这个 tradeoff 是值得的，因为:
- Loop 本身有 600s timeout，中途崩溃的概率低
- 如果真崩溃，LoopState 的 turn_log 能记录已完成的工具调用
- Supervisor 的 panic/recover 机制是另一层保护

### 5.9 LoopState 持久化

Loop 结束时通过 MemoryHandle 持久化 LoopState:

```rust
// Teardown 阶段
let loop_state = state.to_persistable();  // 只保留跨 session 需要的字段

if let Some(mh) = memory_handle {
    let key = format!("loop_state:{}", session.agent_name);
    let value = serde_json::to_value(&loop_state)?;
    mh.kv_set(&session.agent_name, owner_id, sender_id, &key, value).ok();
}
```

下次 loop 启动时恢复:

```rust
// Setup 阶段
let last_state: Option<LoopState> = memory_handle
    .and_then(|mh| mh.kv_get(agent_name, owner_id, sender_id, "loop_state").ok().flatten())
    .and_then(|v| serde_json::from_value(v).ok());

// 如果有上次状态，在 system prompt 中注入上下文
if let Some(last) = &last_state {
    if let Some(run) = &last.last_run {
        system_prompt_extra = format!(
            "📋 上次运行: {} 轮, 原因: {}, 结果: {:?}",
            run.iterations, run.stop_reason, run.outcome
        );
    }
}
```

---

## 6. 组件契约

### 6.1 主循环 (mod.rs) — 编排者

**职责**:
- 生命周期管理: Setup → Loop → Teardown
- LoopState 的唯一持有者和更新者
- 调用 helper 函数完成具体操作，不做细节

**不应做的**:
- 不直接操作 session 持久化（交给 Teardown 阶段统一处理）
- 不直接处理 LLM 错误（交给 call_with_fallback）
- 不直接执行工具（交给 handle_tool_use）

**对外接口** (不变):
```rust
pub async fn run_agent_loop(...) -> CarrierResult<AgentLoopResult>
```

### 6.2 Helpers (helpers.rs) — 工具箱

**职责**:
- LLM 调用: call_with_fallback, call_with_retry
- 通信: build_status_message, classify_pressure, estimate_token_count
- 检测: detect_tool_loop, detect_exact_loop
- 持久化: generate_turn_summary, trim_oldest_turns

**不应做的**:
- 不持有可变状态（纯函数或只读）
- 不修改 messages/session（由主循环控制）

### 6.3 EndTurn (end_turn.rs) — 收尾者

**职责**:
- 解析指令 (NO_REPLY, silent, reply_to)
- 空响应处理: 重试带 sustained-failure 保护
- 生成 TurnSummary
- 知识提取 → drawer

**不应做的**:
- 不保存 session（交给 Teardown 统一处理）
- 不触发 AgentLoopEnd hook（交给 Teardown）

### 6.4 ToolUse (tool_use.rs) — 执行者

**职责**:
- 工具执行: 超时包裹、hooks、结果截断
- Loop 检测: 软检测 + 硬检测
- 错误跟踪: 滑动窗口计数
- 工具发现: tool_search/skill_load 后刷新工具列表
- task_plan 检测

**不应做的**:
- 不保存 session（交给 Teardown）
- 不决定 modality（由主循环在 PREPARE_TURN 中决定）

### 6.5 MaxTokens (max_tokens.rs) — 续写者

**职责**:
- 处理 MaxTokens stop reason
- 追加 "Please continue" 或返回 partial response

### 6.6 LoopState — 状态持有者

**职责**:
- 封装所有 loop 运行状态
- 提供 `build_status_message()` 给 LLM
- 提供 `to_persistable()` 给持久化
- 提供 `turn_log` 给可观测性

**不应做的**:
- 不包含 LLM driver/brain 引用（那是依赖，不是状态）
- 不包含 session 引用

---

## 7. 优化条目与优先级

| # | 条目 | 优先级 | 改动量 | 收益 | 对应问题 |
|---|------|--------|--------|------|----------|
| O1 | 引入 LoopState 统一状态管理 | 🔴 P0 | 中 | 消除状态散落，为后续优化打基础 | P9, P10 |
| O2 | 每轮注入 loop status（含上下文/时间预警） | 🔴 P0 | 小 | 模型决策信息完整，早期预警 | P2, P3 |
| O3 | 清理 modality 冗余代码 | 🟡 P1 | 极小 | 代码清晰度 | P1 |
| O4 | Loop 检测窗口 6→4 + 软检测 | 🟡 P1 | 小 | 减少死循环浪费 | P4 |
| O5 | 错误跟踪改为滑动窗口 | 🟡 P1 | 小 | 更准确的升级警告 | P8 |
| O6 | 单轨消息 (消除双轨) | 🟡 P1 | 中 | 消除不一致 bug 源 | P6 |
| O7 | Loop 状态机重构 | 🟢 P2 | 大 | 清晰的执行流，便于扩展 | 架构 |
| O8 | LoopState 跨 session 持久化 | 🟢 P2 | 小 | 解决冷启动 | P9 |
| O9 | Turn log 可观测性 (逐轮 token) | 🟢 P2 | 小 | 成本分析、性能调优 | P11 |
| O10 | 文本恢复失败后 fallback 文本优化 | 🔵 P3 | 极小 | UX 改善 | P5 |
| O11 | 工具发现改为追加而非驱逐 | 🔵 P3 | 小 | 减少重复 tool_search | P7 |
| O12 | MCP 健康检查 + 指数退避 | 🔵 P3 | 中 | 日志噪音消除 | P12 |

### 优先级说明

- **P0**: 阻塞性——不做则后续优化没有基础
- **P1**: 高收益——独立可做，改动可控，ROI 高
- **P2**: 重要但不紧急——在 P0/P1 稳定后做
- **P3**: 锦上添花——UX/日志改善

---

## 8. 实施路线

### Sprint 1: 基础重构 (O1, O2, O3) — 目标: 状态统一 + 信息完整

```
Day 1-2: O1 — 引入 LoopState
  ├── 定义 LoopState 结构体 (crates/runtime/src/agent_loop/state.rs)
  ├── 将现有局部变量迁移到 LoopState 字段
  ├── 实现 build_status_message()
  └── 更新现有代码引用 (iteration → state.iteration, etc.)

Day 2-3: O2 — 每轮注入 loop status
  ├── 修改 mod.rs 中的注入逻辑 (去掉 is_multiple_of(2) 条件)
  ├── 加入上下文压力预警 (ContextPressure::High/Critical → 收尾建议)
  ├── 加入时间压力预警 (LoopBudgetState::Tight/Critical → 降级建议)
  └── 更新 LoopState 中的 context_pressure 计算

Day 3: O3 — 清理 modality 冗余
  ├── 合并 mod.rs:456-470 的 budget tight 分支
  ├── 简化 pick_modality (去掉 _iteration 参数)
  └── 清理调用点
```

### Sprint 2: 可靠性提升 (O4, O5) — 目标: 减少无意义迭代

```
Day 4: O4 — Loop 检测窗口优化
  ├── HARD_LOOP_WINDOW 6 → 4
  ├── 新增软检测: 相同工具连续 2 次 → build_status_message 追加提醒
  └── 更新 LOOP_DETECTION_WINDOW 常量和引用

Day 5: O5 — 错误跟踪滑动窗口
  ├── 实现 ToolErrorTracker (5 窗口滑动)
  ├── 替换 HashMap<String, u32> 为 ToolErrorTracker
  └── 更新 consecutive_tool_errors 的读写点
```

### Sprint 3: 结构优化 (O6, O7, O8) — 目标: 干净架构 + 跨 session 记忆

```
Day 6-8: O6 — 单轨消息
  ├── 去掉 session.messages 在循环中的同步写入
  ├── ToolUse: 只写 loop_messages
  ├── MaxTokens: 只写 loop_messages
  ├── EndTurn: 去掉 session push，保留 session 用于 Teardown
  └── Teardown: 统一持久化

Day 9-12: O7 — 状态机重构 (可选，取决于 O6 后代码质量)
  ├── 提取 PREPARE_TURN / LLM_CALL / DISPATCH / NEXT_TURN 为独立阶段
  ├── 每个阶段为独立函数
  └── 主循环变成清晰的状态转换

Day 13: O8 — LoopState 持久化
  ├── LoopState::to_persistable()
  ├── Teardown 时 kv_set
  └── Setup 时 kv_get + 注入 system prompt
```

### Sprint 4: 可观测性 + 边角改善 (O9-O12)

```
O9:  Turn log 输出到日志/metrics
O10: 文本恢复失败后的 fallback 文案优化
O11: 工具发现改为增量追加
O12: MCP 健康检查 + 退避
```

---

## 9. 验证与度量

### 9.1 每 Sprint 后的验证

```bash
cargo build --workspace --lib
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

### 9.2 行为验证

部署到 staging 后验证关键 agent:

| Agent | 验证场景 | 观察指标 |
|-------|---------|---------|
| ai-writer | 选题 → 写作 → 发布 | loop status 每轮注入、上下文预警触发、end_turn 正常 |
| wechat-writer | 选题 → 写作 → 发布 | 同上 |
| 通用 agent | 工具调用密集任务 | 软/硬 loop 检测触发、错误跟踪滑动窗口计数准确 |

### 9.3 度量指标

| 指标 | 当前基线 | 目标 | 测量方式 |
|------|---------|------|----------|
| 平均迭代次数 | ? | 下降 10-20% | LoopState.turn_log.len() |
| MaxIterations 触发率 | ? | 下降 | 统计 StopReason |
| 工具错误导致的额外迭代 | ? | 下降 | 统计连续错误 → 升级警告触发频率 |
| 空响应重试率 | ? | 不变或下降 | 统计 end_turn 中 Retry 次数 |
| 上下文溢出率 | ? | 下降 | 统计 ContextPressure::Critical 触发频率 |

### 9.4 回滚策略

每个 Sprint 产出独立 commit，可用 `git revert` 单 commit 回滚。Sprint 间不耦合——Sprint 1 可独立上线，Sprint 2 不依赖 Sprint 1 的持久化改动。

---

## 附录 A: 与现有文档的关系

| 文档 | 关系 |
|------|------|
| `AGENT-LOOP-REMEDIATION.md` | 本文档**取代**它。Phase 1 已落地，其余内容合并入本文 §5.5 |
| `LOOP-ENGINEERING.md` | 本文档**取代**它。LoopState 设计、分诊、成本可观测等内容已合并入本文 |
| `ADAPTIVE-MODALITY.md` | 已标记 deprecated，保留作为历史参考 |
| `BRAIN-ARCHITECTURE.md` | 正交——本文不涉及 Brain 内部设计，只引用 modality 概念 |
| `STREAMING-UNIFICATION.md` | 正交——本文不涉及流式统一，但 loop 的 LLM 调用统一用 streaming |

## 附录 B: 关键常数速查

| 常数 | 当前值 | 新设计值 | 位置 |
|------|--------|---------|------|
| MAX_ITERATIONS | 25 | 不变 | mod.rs |
| AGENT_LOOP_TIMEOUT_SECS | 600 | 不变 | mod.rs |
| MAX_RETRIES | 3 | 不变 | helpers.rs |
| LOOP_DETECTION_WINDOW | 6 | 4 (hard) + 2 (soft) | helpers.rs |
| MAX_CONTINUATIONS | 5 | 不变 | max_tokens.rs |
| ERROR_ESCALATION_THRESHOLD | 3 | 不变，但用滑动窗口计算 | tool_use.rs |
| MAX_HISTORY_MESSAGES | 30 | 不变，但 context guard 提前预警 | helpers.rs |
| MAX_RETAINED_MESSAGES | 12 | 不变 | end_turn.rs |
| TOOL_TIMEOUT_SECS | 120 | 不变 | helpers.rs |
| TOOL_TIMEOUT_LONG_SECS | 300 | 不变 | helpers.rs |
| STREAM_WALL_CLOCK_TIMEOUT_SECS | 300 | 不变 | helpers.rs |
| PER_LLM_CALL_TIMEOUT_SECS | 180 | 不变 | helpers.rs |
| TOOL_SEARCH_RECALL_LIMIT | 10 | 不变 | helpers.rs |
| MAX_TEXT_RECOVERY_RETRIES | 2 | 不变 | mod.rs |
| MAX_SILENT_RETRIES | 2 | 不变 | end_turn.rs |
| DEFAULT_CONTEXT_WINDOW | 128_000 | 不变 | helpers.rs |
| MAX_TOTAL_TOOLS | 32 | 不变 | tool_use.rs |
