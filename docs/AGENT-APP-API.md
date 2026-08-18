# Agent App API — 产品与接口立法（2026-08-16 起）

> 本文是 **opencarrier 对外 API 产品化**的唯一权威规范：桌面/移动 app 如何消费 opencarrier、
> 分身（clone）如何归属用户、DupHub 如何承担分发。与 `docs/CLONE-FORMAT.md` 同级立法——
> **任何修改对外 API 路由形状、鉴权分层、分身所有权字段的 commit，必须同批修改本文**。
>
> 现状标注：`✅ 已有` / `🔧 需补` / `⏳ 后期`。本文以 2026-08-19 代码为准。

---

## 1. 定位：微信 for agents

```
opencarrier daemon（状态权威：agents/sessions/crons/memory/channels）
   │
   ├─ 用户面 API ──── 用户 token ────→ 外部 app（桌面/移动/第三方）
   ├─ 管理面 API ──── admin key ────→ dup CLI、运维、dashboard
   └─ 渠道（微信OA/企微/iLink）──→ 服务端内置集成，不进公开 API
```

四个角色：

| 角色 | 对应物 | 说明 |
|---|---|---|
| 状态权威 | opencarrier daemon | agents/sessions/crons/memory 全在服务端；客户端无状态 |
| 客户端 | 桌面 app / 移动 app / 第三方 | 只做渲染与输入，经 API 交互 |
| 聊天对象 | 分身（clone） | app 内呈现为微信式会话列表成员 |
| 应用商店 | DupHub | 分身定义层分发：安装=拉定义到本地实例 |

**明确不做**（架构红线）：
- 不做本地↔服务器记忆同步。每套安装是独立状态岛；共享的只有定义层（经 DupHub）。
  桌面 app 要么远程客户端（记忆在服务器），要么后期本地打包（记忆在本地），二者不混。
- 渠道（微信 OA、企微客服、iLink）不暴露为公开 API——它们是服务端集成，不是 API 消费者。
- 客户端不持久化业务状态（localStorage 只放 UI 偏好与凭证）。

### 1.1 iLink 推送模型（2026-08-19 协议立法）

生产四探针实证的送达语义（`ilinkai.weixin.qq.com` 直测），决定产品对定时推送的承诺边界：

**推送能力公式**：`任一活的扫码号（bot_token 鉴权）+ 与收件号有关系 + 收件号活着`。

| 层 | 判定 | 失败形态 |
|---|---|---|
| API 层 | 收件**号**近期在线（iLink 上人人都是扫码账号） | `ret:-2 prepare failed`，直接拒 |
| 送达层 | 发送号与收件号有来往（聊过/即收件号本身） | HTTP 200 + message_id，**静默丢弃** |
| token 层 | context_token **不需要**（请求体可选字段，裸发即达）；带过期 token 反而失败 | 08-13 有过期实录 |

**产品语义**：
- **推送与分身无关**：分身只是内容发起者，号池是系统共享资源；任何分身都可给任何号推送。
  路由 = 收件号自己的 session（池内号，自聊形态）或与它聊过的号（关系路由）。
- **送达是 best-effort，无回执**：`message_id ≠ 送达`。上游不给 delivery receipt，成功回执
  只能靠收件人下次互动间接确认。所有"已推送"语义一律按尽力而为表述。
- **定时任务可用域**（普通用户与 admin 无能力差别）：用户设定任务时必然刚聊过天（关系已
  建立）+ 活跃用户号天然活着 → cron 到点裸发即达。用户号休眠多日 = `prepare failed`，属
  pending_notifications 补投兜底管辖。
- **代码事实**（f4186f7 起）：send 路径 context_token 全部可选化（None 省略字段）+ 过期
  token 自动裸发重试；`No context_token` 失败类别已整体消失，不再是产品缺陷 vocabulary。

## 2. 产品形态：分身 = 聊天对象

app 主界面是微信式会话列表：每个分身一个会话项（头像/名字/最近消息），点开是聊天窗。

两种分身形态，**同一引擎，仅 ACL 不同**：

| 形态 | ACL | 模式 | 例子 |
|---|---|---|---|
| 公共分身（公众号模式） | `*` 任何人可聊 | 一套分身服务所有人，session 按 `user:<sender_id>` 隔离 | 86bus-assistant |
| 私人分身（好友模式） | `{owner}` 仅主人 | 用户创建/安装的，只有本人 token 能投递 | 我的写作助手 |

底层依据（✅ 已有）：session 按 label `user:<sender_id>` 隔离、记忆按 user_id 隔离。
**"分身属于这个 app"= ACL 记账，不需要新引擎。**

### 2.1 定义层 / 实例分离（分阶段）

- **MVP**：安装 = 完整 workspace 实例化（现有 `clone_install`），ACL 记在实例上。
- **后期**：轻分身（persona 型）= 共享定义 + per-user 状态挂载，安装零拷贝。重分身（带
  scripts/知识库）保持完整 workspace。判据：安装耗时与磁盘成本成为瓶颈时再做。

## 3. 现有 API 盘点（桌面 app MVP 直接消费）

| 能力 | 端点 | 状态 |
|---|---|---|
| 分身列表（=会话列表数据源） | `GET /api/agents` → `[{id,name,display_name,state,ready,model_name,identity{emoji,avatar_url,color},profile}]` | ✅ |
| 发消息（同步） | `POST /api/agents/{id}/message` `{message,sender_id,sender_name,active_flow?}` → `{response,input_tokens,output_tokens,iterations}` | ✅ |
| 发消息（SSE 流式） | `POST /api/agents/{id}/message/stream` | ✅ |
| 实时聊天 | `WS /api/agents/{id}/ws`（见 §3.1） | ✅ |
| 会话历史 | `GET /api/agents/{id}/session`（**仅默认 session**） | 🔧 需补 `?sender_id=` |
| 市场 | `GET /api/hub/templates`、`POST /api/hub/templates/{name}/install` | ✅ |
| 鉴权 | Bearer / X-API-Key（恒时比较）；WS 额外支持 `?token=` query | ✅ |

### 3.1 WS 协议（现有，冻结为 v1 契约）

```
Client → Server: {"type":"message","content":"...","sender_id":"..."}   # sender_id 决定 session 隔离
Server → Client: {"type":"typing","state":"start|tool|stop"}
Server → Client: {"type":"text_delta","content":"..."}
Server → Client: {"type":"response","content":"...","input_tokens":N,"output_tokens":N,"iterations":N}
Server → Client: {"type":"silent_complete"}
Server → Client: {"type":"canvas","canvas_id":"...","html":"...","title":"..."}
Server → Client: {"type":"error","content":"..."}
限流：10 msg/min per WS；5 WS/IP；30min idle 断开
```

### 3.2 已知缺口清单（按消费者需求排序）

| # | 缺口 | 修法 | 状态 |
|---|---|---|---|
| G1 | 按 label 取会话历史（桌面 app 显示自己的聊天记录） | `GET /api/agents/{id}/session?sender_id=X` → `find_session_by_label(agent_name,"user:X")` | 🔧 本次补 |
| G2 | 用户账户与多租户 token（见 §4） | auth 模块扩展 | ⏳ Phase 2 |
| G3 | 分身 ACL/所有权字段（见 §5） | clones 元数据扩展 | ⏳ Phase 2 |
| G4 | app 内创建分身（向导：模板+人设参数） | `POST /api/v1/clones` | ⏳ Phase 2 |
| G5 | DupHub 作者绑定 + private 可见性 | hub 模板元数据 | ⏳ Phase 2 |
| G6 | 审批回路（权限提示推给 app、app 答复） | WS 加 `approval_request`/`approval_answer` | ⏳ Phase 3 |
| G7 | 事件流订阅（turn 生命周期/cron 链进度） | SSE `/api/agents/{id}/events`，投影 session-events JSONL | ⏳ Phase 3 |
| G8 | `/api/v1` 版本前缀 + OpenAPI 契约 | utoipa | ⏳ Phase 3（对外第三方前） |

## 4. 鉴权分层

```
admin key（现 OC_API_KEY）→ 管理面：clones dup/compile/rollback、config、providers、shutdown、
                            全部 agents 管理端点 —— 仅供运维/dup CLI/dashboard
                            WeChat OA 数据面 /api/wechat-oa/{app_id}/*（user/get、draft 盘点、
                            freepublish/get、template/list+send、comment 留言八件套，2026-08-18；
                            凭证服务端解析不回显。同日下线 wechat-oa-mcp——微信能力唯一接口形态
                            即此 API，agent 需要数据走���常轮次或确定性 cron）
user token（🔧 Phase 2）  → 用户面：登录、我的分身、发消息、读自己的会话、hub 安装/发布
```

- **WeChat OA 端点细则**：只代理服务器绑定账号（`senders/<app_id>/session.json` 存在且
  channel=weixin-oa），响应为微信 JSON 透传，错误归一 `{error, errcode, errmsg}`；路径前缀
  `/api/wechat-oa/` 与公开 webhook 前缀 `/api/weixin-oa/` 刻意区分，不落 is_public 白名单。
  留言端点 `/comment/{open,close,list,markelect,unmarkelect,delete,reply,reply/delete}`
  （POST，body 带 `msg_data_id`，`index` 可选默认 0；`comment/list` 另有 `comment_type`
  0=全部/1=普通/2=精选、`offset`、`count`≤50）。

- user token 绑定 `user_id`，消息管线复用现有 `sender_id → user:<sender_id>` session 隔离与
  user_id 记忆隔离——**token 即身份，不新增隔离机制**。
- 用户账户锚点：微信小程序 login 或手机号（`⏳`；MVP 期间桌面 app 用 admin key + 显式
  `sender_id`（如 `desktop-<name>`）实现会话隔离，等价于单管理员多会话）。
- scope 最小集：`chat`（发消息/读自己的会话）、`hub:install`、`hub:publish`、`clone:create`。
- localhost 免鉴权（loopback 信任）保留现状。

## 5. 分身所有权模型（🔧 Phase 2）

```
clone 元数据新增：
  owner_user_id: Option<UserId>   # None = 平台公共分身
  visibility:   Public | Private  # Private = 仅 owner token 可投递消息
DupHub 模板新增：
  author: UserId                  # push 权限绑定作者（现 Bearer hub key 升级为作者身份）
  visibility: Public | Private    # Private 模板仅作者可 install
```

- 消息路由 gate：`send_message_with_handle` 入口前查 clone ACL——Private 且 sender 非 owner → 403。
- `POST /api/v1/clones/{id}/publish`：把我的分身定义层 push 到 DupHub（复用 dup push 管线）。
- `POST /api/v1/hub/templates/{name}/install`：把模板实例化为"我的"分身（owner=me）。

## 6. 费用模型（产品决策，先立原则）

| 模式 | 适用 | 说明 |
|---|---|---|
| 平台 key + 每用户配额 | MVP | 配额烧尽 → 引导 BYO key；防滥用底线 |
| BYO key（用户自配 LLM key） | 可持续态 | app 设置页配置，per-user 加密存储 |

**红线**：用户私有分身的 LLM 调用不允许无配额地烧平台 key。

## 7. 桌面 app（第一消费者，本次交付）

- **形态**：纯远程客户端（carrier.yinnho.cn + key）。本地打包（app 内嵌 daemon）为后期
  "一起打包的产品"预留，不在本期。
- **技术**：Tauri v2 纯 cargo（无 node 构建链），静态 HTML/JS 前端。位置 `desktop/`（独立
  cargo workspace，不进主 workspace）。
- **API 代理**：HTTP 走 Rust 侧 reqwest（`invoke` 命令）——服务器 CORS 白名单不含
  `tauri://localhost`，且代理层是未来 user token 的统一注入点；WS 走 Rust 侧
  tokio-tungstenite → Tauri events（浏览器 WS 虽有 `?token=` 兜底，统一走代理）。
- **UI**：微信式双栏——左=分身会话列表（identity emoji/头像、状态点、模型标签），右=聊天窗
  （历史 + 流式 text_delta + typing/tool 指示 + canvas 渲染）。设置：服务器地址 + API key +
  sender 身份。
- **会话隔离**：`sender_id = desktop-<配置名>`，与渠道用户互不污染。
- **dogfood 纪律**：桌面 app 只走公开 API，不走 AppState 后门。API 缺什么就补什么（G1 即
  第一例）——这是本 app 的存在意义之一。

## 8. 分阶段落地

| 阶段 | 内容 | 出口判据 |
|---|---|---|
| Phase 1（本次） | G1 补端点；桌面 app MVP（列表/聊天/流式/历史/设置）连生产 | 真人用桌面 app 与生产分身完成一轮流式对话 |
| Phase 2 | G2/G3/G4/G5：账户、ACL、app 内创建、DupHub 所有权 | 两个用户各自 token 互不可见对方分身与会话 |
| Phase 3 | G6/G7/G8：审批回路、事件流、/v1+OpenAPI | 第三方可以只凭公开文档写出一个客户端 |

## 9. 变更纪律

- 对外 API 路由形状、WS 消息类型、鉴权分层、ACL 字段的任何变更 → 同批修改本文。
- 渠道推送语义（iLink 裸发/关系路由/best-effort 边界，§1.1）变更 → 同批修改本文。
- WS 消息类型只增不改语义（客户端版本不可控）。
- 新增公开端点必须写进 §3 表格（含响应形状）。
