# 管理员机制设计（Admin Governance）

> 管理员（admin）不是权限开关，而是分身的「治理者」。本文记录 OpenCarrier 管理员机制的设计共识与落地路线。
>
> 真相源：每个 workspace 的 `admins.json`（已实现）。

## 核心理念

1. **管理员的任务是训练出一个好用的分身**，让它更好地服务用户——不是当权限门卫。
2. **分身持续自进化是常态**，像程序员写代码：遇到不懂的去查、遇到新需求判断该不该满足、能力不够自己加。自进化不是 admin 的特权，是分身的基本能力。
3. **管理员只在分身偏了的时候出手调教**（纠偏）。分身不可能不犯错，人也会犯错。
4. **不分裂分身**：一个客服分身既能对外服务、也能对内做事（写公众号）。靠切行为/模式，不靠拆成两个分身。
5. **不建权限矩阵**：除了少数不可逆动作硬门，其余靠分身的判断力 + 管理员纠偏。

## 三个本质（不是三个硬模式）

| 本质 | 干什么 | 谁触发 |
|------|--------|--------|
| **① 调教 (TUNE)** | 纠偏、改 skill/knowledge/system prompt、定进化方向 | 管理员（纠偏）+ 分身有限自进化 |
| **② 对内 (INTERNAL)** | 为品牌生产内容（写公众号、做短视频、内部运营） | 管理员安排 **或分身自己决定** |
| **③ 对外 (SERVE)** | 客服、发月卡、包车报价 | 普通用户 |

关键认识：这不是三个档位开关，而是**一条连贯的实践流**（serve + learn + judge + evolve）。对外服务的过程本身就在喂进化——serve 和 internal-evolution 不是两件事。admin 的作用是**加宽自主带宽 + 拉回偏离的手**，不是切模式。

## 现状：自进化已经是默认且对所有人开的

`maybe_run_evolution`（kernel.rs:202）在每轮非平凡对话后**后台自动跑**，不分 admin/用户：

1. `should_skip()` 本地过滤（琐碎对话/过短回复跳过）
2. LLM 分析对话，抽取知识（`shared` 所有人受益 / `private` 该用户隔离）
3. `apply_evolution()` 写 knowledge 文件、更新 MEMORY.md、记录知识盲区（gaps）
4. 可选：脱敏后推回 Hub（feedback.rs）

进化方向由每个分身的 **`EVOLUTION.md`** 控制：`Conservative/Moderate/Aggressive/Disabled`、容量上限、bloat 自动清理过期知识、`identity_frozen_files` 冻结 SOUL 等核心文件。

**结论：进化的主体是分身自己，admin 从来不是进化的触发器。** 普通用户的服务对话也会贡献 shared knowledge，所有人受益（知识共建）。

## 管理员的三重职责（共识收敛）

| 职责 | 内容 | 现状 |
|------|------|------|
| **① 能力硬门** | publish/shell/agent 配置等**不可逆**动作，非管理员挡掉 | ❌ 缺（范围很小，就几类工具） |
| **② 进化治理** | 定方向(EVOLUTION.md)、纠偏(删错知识/改规则)、**复盘学到的知识** | 🟡 工具基本齐，缺复盘闭环 |
| **③ 主动推送目标** | 分身需要帮助/通气时，推给谁（包车单、进度、疑问） | ❌ 缺（包车推送卡在这） |

### 为什么③是核心切入点

分身主动推送的本质是：**「我遇到搞不定的（包车单要人对接）、或需要你知道的（我在做什么），我喊管理员。」** 推送目标必须是「当前这个分身的管理员们」——从 `admins.json` 动态解析，而不是 `notify_routes.json` 里写死一个 ID。管理员加了人/撤了人，推送目标自动跟着变。

## 真相源：admins.json（已实现）

`crates/runtime/src/plugin/admin_store.rs` 已实现完整的角色体系：

```json
{
  "admins": [
    { "sender_id": "...", "sender_name": "...", "role": "creator", "approved_at": "..." },
    { "sender_id": "...", "sender_name": "...", "role": "admin",   "approved_at": "..." }
  ],
  "pending": [ { "sender_id": "...", "sender_name": "...", "requested_at": "..." } ]
}
```

- **creator**：第一个绑定分身的人，自动分配（`auto_assign_creator`），不可撤销
- **admin**：发「申请管理权限」→ creator/admin 在后台审批（`approve`/`revoke`）
- `is_admin(workspace, sender_id)` 已可用，但**目前从未在正常消息流里被调用**——这正是要补的

关键约束：**两条消息路径都汇聚在 `messaging.rs prepare_context`**（DirectBind 公众号渠道 vs SenderBased 个人微信渠道）。而且 is_admin 必须在 DirectBind 上也检查——因为 86bus 的 creator 就是个公众号 openid。所以 admin 判断不能放在 bridge 的 `!direct_bind` 块里，要在 prepare_context 算。

## admins.json 驱动三件事

```
admins.json（唯一真相源）
   │
   ├─① is_admin → 少数硬门（tool_runner / skill_load 对 publish/shell/配置）
   ├─② admins 列表 → 推送目标解析（[NOTIFY:...] target 从写死 user_id → 该 agent 的所有 admin）
   └─③ admin 身份信号 → system prompt 注入（告诉分身「现在跟 trainer 对话、可接受直接纠偏」）
```

## 关键设计决策

### 不用权限清单，用判断 + 极少硬门

- **硬门只留给不可逆/影响品牌的动作**：`publish_draft`（公开发布）、`shell_exec`（系统）、agent 配置改动。��� is_admin（或自主执行上下文）挡。就这几类。
- **其余「限制」是策略不是锁**。例如「公众号用户不能点单写文章」——`article-writer`（写一篇 md）本身可逆、无害，用户点了顶多浪费 token，**靠分身判断拒绝**就够了；只有 `publish_draft`（公开发布）不可逆，才硬门。
- **自进化工具（knowledge_add / skill_create / web_search / preference-learn）保持广泛可用**——分身在自己的判断循环里用它们。

### is_admin 怎么传

在 `prepare_context`（messaging.rs，workspace + sender_id 第一次同时出现处）算一次，作为 `bool` 跟 `sender_id / owner_id / channel_type` 同族传下去。每个请求读一次 `admins.json`（OS 缓存，很便宜）。使用点：

| 层 | 位置 | 作用 |
|----|------|------|
| 硬门兜底 | `tool_runner` | publish/shell/配置类工具非 admin 拒绝 |
| 硬门兜底 | `skill_load` | （备用） |
| 推送目标 | bridge `send_response` | `[NOTIFY:...]` 解析 admin 列表 |
| 身份信号 | prompt builder | 注入「当前用户角色」 |

### 二元 vs RBAC

现在是二元（creator/admin 都算 `is_admin=true`）。字段已叫 `role`，留扩展余地——未来要细分（如「writer 能写不能发」）再扩，不影响现有设计。

## 落地路线（按可见性排序，最小步进）

### Phase 1：让主动推送活起来（③，最小可见）

把 `notify_routes.json` 的 target 从硬编码 `user_id`，升级成能解析「该 agent 的 admins」。

- 新增 target 形式：`{ "target": "admins" }`（或保留 user_id 作为兜底/定向）
- bridge `send_response` 解析 NOTIFY 时，若 target=admins，从该 agent 的 `admins.json` 取所有 admin 的 `(channel, sender_id)`，逐个推送
- **立刻解决包车推送卡住的问题**：管理员加人/撤人，推送目标自动跟着变

### Phase 2：硬门（①）

- `prepare_context` 算 `is_admin`
- `tool_runner` 对 `publish_draft` / `shell_exec` / 配置类工具检查 is_admin

### Phase 3：进化治理闭环（②的复盘）

分身自动学了一堆知识，但管理员看不到「最近学了什么、哪条可能学错了」。补：

- 管理员能查「最近 N 条进化记录」
- 能标记某条「学错了」→ 删除 + 加规则「别再学这个」

### Phase 4：身份信号（prompt）

is_admin 时注入「当前用户：管理员」，让分身优雅地接受纠偏指令（而非提示词里写死 sender_id）。

## 已知的待清理项（86bus 实例）

- `notify-charter` skill 是 `charter-quoter` 的重复简化版，且用了错误类型 `charter_order`（routes 里没有）→ 删除，统一用 `charter-quoter`
- `charter-quoter` 的 `[NOTIFY:charter_timeout]` 在 routes 里无对应 key → 补路由或等 Phase 1 的 admins target 一并处理
- `charter-quoter` 用的 `charter_lead/charter_confirmed/charter_fleet` 类型正确，与 routes 对得上 ✓

## 不在本次范围

- 完整 RBAC（creator/admin 之外的角色）
- 每个 skill 的权限清单（用判断 + 策略取代）
- MemorySubstrate 重构为独立服务（见 MEMORY-SYSTEM.md「不在本次范围」）
