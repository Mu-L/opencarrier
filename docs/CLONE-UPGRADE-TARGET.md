# 分身目标架构：双仓分离 · DupHub · Upgrade

> **状态**：目标架构（Target Architecture）  
> **日期**：2026-07-18  
> **范围**：opencarrier（运行时）↔ opencarrier-clones（分身工程）↔ DupHub（制品仓）

本文描述**理想交付链路**，用于对齐产品与实现方向。不等于当前所有命令均已落地；实现时以此为准。

---

## 1. 一句话

**在本地 clones 做好分身 → 上传 DupHub → 线上 OpenCarrier 对已有实例执行 upgrade。**  
共享 flows 与内置工具一样属于平台能力；改分身只在 clones 侧进行，不在服务器上当主编辑场。

---

## 2. 三方职责

| 组件 | 仓库 / 服务 | 职责 |
|------|-------------|------|
| **OpenCarrier** | `opencarrier` | Agent OS：引擎、内置工具、**共享 flows**、已部署分身的运行实例；对外暴露能力目录；对已安装分身执行 **upgrade** |
| **OpenCarrier Clones** | `opencarrier-clones` | 分身工程：生成、编辑、校验、打包（.agx）、**发布到 DupHub**；是分身定义的工程真相源 |
| **DupHub** | duphub.com | 分身制品注册与分发：版本化存储 .agx / 元数据；OpenCarrier **upgrade** 的上游 |

```
┌─────────────────────┐     publish      ┌──────────┐     upgrade      ┌─────────────────────┐
│ opencarrier-clones  │ ───────────────► │  DupHub  │ ◄─────────────── │    opencarrier      │
│ 分身工程 / 打包      │                  │ 制品版本  │                  │ 运行时 / 已部署实例  │
└─────────────────────┘                  └──────────┘                  └─────────────────────┘
         ▲                                                                  │
         │  只在这里改分身定义                                                │  不手改 workspace 当主流程
         └──────────────────────────────────────────────────────────────────┘
```

**分离原则（与现模式一致）**：

- 引擎与平台能力 → 只在 `opencarrier` 演进，经 `git push deploy` 上线。
- 分身定义 → 只在 `opencarrier-clones` 演进，经 DupHub 再 upgrade 到运行时。
- 禁止：以 scp / 直接改线上 `workspaces/` 作为常态发布手段。

---

## 3. 平台能力 vs 分身定义

### 3.1 平台层（OpenCarrier）

与「有哪些工具」同一层级：

| 能力类型 | 运行时位置 | 源码 / 部署 |
|----------|------------|------------|
| 内置工具 / MCP | 进程内 + 配置 | 引擎 + `config.toml` / MCP 二进制 |
| **共享 flows** | `~/.opencarrier/flows/` | 仓库 `opencarrier/flows/`，deploy hook 同步 |
| 能力目录 | `GET /api/v1/capability-catalog` | 工具 + MCP + **declarable 列表** + 共享 flows 摘要 |

共享 flows 示例（写作流水线等）：

```
topic-researcher → outline-writer → article-writer
    → article-formatter → draft-publisher / article-publisher
```

设计约定：

- 共享 flow = **通用工作流**（≥2 个分身会复用），不硬编码人格与语气。
- 风格 / 署名 / 选题偏好从分身的 `system_prompt` / `knowledge` 读取。
- 改共享 flow = 改平台，走 **opencarrier 部署**，不是 upgrade 某个分身。

详见：[SKILL-STANDARD.md](./SKILL-STANDARD.md)（历史文档仍可能写 `skills/`；**运行时目录已为 `flows/`**）。

### 3.2 分身层（Clones → DupHub → 实例）

分身包 / workspace **定义**主要包含：

- 身份：`template.json`、`profile.md`、`SOUL.md`、`system_prompt.md`
- 知识：`knowledge/`（人设、领域规范，非流水线临时产物）
- **私有 flows**：仅该分身独有（如 `preference-learn`、`image-in-article`）
- 配置：`agent.toml` / 插件与 MCP 依赖声明等

**不应**把整条通用写作流水线复制进每个分身；应依赖共享 flows + 分类器 / `flow_load`。

### 3.3 运行时解析顺序

```
flow 命中 / flow_load(name):
  1. workspaces/{agent}/flows/     ← 私有优先
  2. ~/.opencarrier/flows/         ← 共享兜底

flow_update 若改到共享 flow:
  → copy-on-write 写入私有 flows/，共享原件不变
```

私有同名会盖住共享——分身工程中应避免用空 stub 误覆盖共享 flow。

---

## 4. 目标交付链路

### 4.1 主路径（迭代已有分身）

```text
1. 本地 opencarrier-clones
   - 编辑分身 workspace
   - 可选：对照 capability-catalog（工具 / 共享 flows / MCP）校验
   - pack → .agx

2. 上传 DupHub
   - publish（版本、分类、可见性）
   - Hub 上出现可 upgrade 的制品版本

3. 线上 OpenCarrier
   - upgrade <name>          # 例如 upgrade ai-writer
   - 从 DupHub 拉取该分身定义并应用到已有实例
   - 保留运行时状态（见 §5）
```

### 4.2 与 install 的区分

| 操作 | 语义 | 典型场景 |
|------|------|----------|
| **install**（若保留） | 新装一个实例 / 首次落地 | 机器上还没有该分身 |
| **upgrade** | **更新已有实例的定义层** | 日常迭代（目标主路径） |

目标强调：

- 日常上线是 **`upgrade ai-writer`** 这种形态，**不是**「重新 download + 走一遍安装逻辑」。
- upgrade 的上游是 **DupHub 上的版本**，不是运维手工 scp 目录。

### 4.3 平台自身上线（与分身无关）

```text
改引擎 / 共享 flows / 工具
  → opencarrier 仓 commit
  → git push deploy main
  → 构建二进制 + 同步 ~/.opencarrier/flows/ + restart
```

分身定义 **不**随每次引擎 deploy 从 git 覆盖 workspaces（避免与 upgrade 双通道冲突）。

---

## 5. Upgrade 语义（目标）

### 5.1 应更新（定义层）

- `template.json`、`agent.toml`（manifest 相关字段）
- `SOUL.md`、`system_prompt.md`、`profile.md`、`EVOLUTION.md` 等身份与指令
- `knowledge/` 中属制品的内容
- 私有 `flows/`
- 分身声明的 MCP / plugins 依赖元数据（以制品为准）

### 5.2 应保留（运行时层）

- `sessions/`
- `senders/`（渠道用户数据、per-user 输出）
- `output/` 等运行产物（除非策略明确清理）
- 渠道绑定、路由、admins 等**环境绑定**（若与制品冲突，策略需另定：默认保留线上绑定）
- 数据库 / 本地状态文件（若有）

### 5.3 不应作为 upgrade 常态的手段

- 直接 SSH 改 `~/.opencarrier/workspaces/{name}/`
- 用「删实例 + 全量 install」代替 upgrade
- 把 sessions/senders 打进 .agx 再覆盖上去

---

## 6. 目录心智模型（线上）

```text
~/.opencarrier/
├── flows/                         # 平台共享 flows（随 opencarrier deploy）
│   ├── article-writer/flow.md
│   ├── article-formatter/flow.md
│   ├── draft-publisher/flow.md
│   └── …
├── workspaces/
│   ├── ai-writer/                 # 实例：定义来自 upgrade；运行时本地生长
│   │   ├── system_prompt.md       # ← upgrade 更新
│   │   ├── knowledge/             # ← upgrade 更新（制品部分）
│   │   ├── flows/                 # ← 私有 flows，upgrade 更新
│   │   ├── senders/               # ← 保留
│   │   └── sessions/              # ← 保留
│   └── wechat-writer/
└── config.toml / brain.json / …   # 平台配置
```

**示例（写作类分身）**：

| 分身 | 平台共享 | 分身私有（制品内） |
|------|----------|-------------------|
| ai-writer | topic / outline / writer / formatter / publisher… | 人设「小载」、AI 向 knowledge、如 `image-in-article` |
| wechat-writer | 同上 | 多风格 knowledge、`preference-learn`、渠道侧运行时数据 |

两者共用共享写作流水线，靠 system_prompt / knowledge 区分风格与业务。

---

## 7. 能力目录与工程校验

目标上，**capability-catalog** 应让 clones 工程侧知道目标运行时「有什么」：

- 内置 / 可声明工具
- 已连接 MCP
- **共享 flows 列表**（与工具并列的平台能力）
- 废弃项与替换建议

本地生成 / 评估分身时：

```text
OPENCARRIER_URL + OPENCARRIER_API_KEY
  → GET /api/v1/capability-catalog
  → 约束 tools: 与 MCP 声明，避免声明目标环境不存在的能力
```

默认生产 runtime：`https://carrier.yinnho.cn`（可配置）。

---

## 8. 明确不做 / 避免的模式

| 模式 | 原因 |
|------|------|
| 线上手改 workspace 当发布 | 与 clones / Hub 真相源分叉 |
| 每个分身复制完整共享流水线 | 升级共享逻辑要改 N 份；应用共享 flows |
| upgrade = 整目录 rsync 含 sessions | 毁掉用户状态 |
| 只同步 workspaces、忽略共享 flows | 写作类分身「半残」（历史踩坑） |
| 私有 stub 同名盖住完整共享 flow | 运行时静默用坏定义 |

---

## 9. 与现状的关系（简要）

| 已有基础 | 状态 |
|----------|------|
| `opencarrier/flows/` + deploy 同步共享 flows | 已有 |
| DupHub publish（clones 侧） | 已有 |
| capability-catalog | 已有 |
| **`opencarrier hub upgrade <name>`** | **已实现（Phase 1）** — 定义层替换 + Bearer 下载 + `hub_template_id` |
| `POST /api/clones/{name}/upgrade` | 已接同一逻辑；`?version=` 可选 |
| `opencarrier hub link <name>` | 给存量 workspace 补 `hub_template_id` |
| install 写 `hub_template_id` | 已实现（= template name） |

### 9.1 操作速查（Phase 1）

```bash
# 本地 clones：改分身 → pack → publish 到 DupHub
clone-creator publish ./ai-writer.agx

# 服务器 / 已装实例
opencarrier hub link ai-writer          # 存量若无 hub_template_id 时先做一次
opencarrier hub upgrade ai-writer       # 从 DupHub 拉定义层，保留 sessions/senders
opencarrier hub upgrade ai-writer -v 2  # 指定版本（若 Hub 支持）
```

---

## 10. 相关文档

| 文档 | 说明 |
|------|------|
| [SKILL-STANDARD.md](./SKILL-STANDARD.md) | 共享 skill/flow 设计原则（目录名以运行时 `flows/` 为准） |
| [CLONE-STRUCTURE.md](./CLONE-STRUCTURE.md) | Workspace 结构与 senders 隐私 |
| [TOOL-RULES.md](./TOOL-RULES.md) | 工具声明规则 |
| opencarrier-clones `docs/clone-upgrade-target.md` | 本文在 clones 仓的副本，便于工程侧只读该仓时查阅 |

---

## 11. 术语

| 术语 | 含义 |
|------|------|
| **共享 flow** | 平台级流程定义，所有分身可分类命中 / `flow_load` |
| **私有 flow** | 分身 workspace 内流程，可覆盖同名共享 |
| **定义层** | 制品内、可版本化、应被 upgrade 的内容 |
| **运行时层** | 实例上产生的会话、用户数据、渠道状态，upgrade 默认保留 |
| **upgrade** | 从 DupHub 将新版本定义应用到已有实例 |
| **DupHub** | 分身制品托管（duphub.com） |
