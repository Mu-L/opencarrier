<h1 align="center">OpenCarrier</h1>
<h3 align="center">扫码即用 — 你的 AI 分身，在微信/飞书/钉钉里等你</h3>

<p align="center">
  开源 Agent 操作系统 | 微信、企微、飞书、钉钉全平台支持<br/>
  <strong>不装 App、不配环境、不跑命令行。扫个码，机器人就是你的 AI 助理。</strong>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/language-Rust-orange?style=flat-square" alt="Rust" />
  <img src="https://img.shields.io/badge/license-MIT-blue?style=flat-square" alt="MIT" />
  <img src="https://img.shields.io/badge/version-0.3.0-blueviolet?style=flat-square" alt="v0.3.0" />
  <img src="https://img.shields.io/badge/tests-1485-brightgreen?style=flat-square" alt="1485 Tests" />
</p>

---

## 怎么用？

打开链接，选一个分身，扫码绑定。**一分钟搞定。**

<p align="center">
  <strong>👉 <a href="https://carrier.yinnho.cn/share">carrier.yinnho.cn/share</a> 👈</strong>
</p>

支持平台：

| 平台 | 入口 | 能力 |
|------|------|------|
| 个人微信 | 扫码绑定 iLink 机器人 | 对话、文章推送 |
| 企业微信 | SmartBot / 应用 / 客服 | 对话、主动推送、群聊 |
| 飞书 | 扫码授权 | 对话、主动推送 |
| 钉钉 | 扫码授权 | 对话、主动推送 |

---

## Installation

### 一键安装（推荐）

```bash
curl -sSf https://carrier.sh | sh
```

### 手动下载

从 [GitHub Releases](https://github.com/yinnho/opencarrier/releases) 下载对应平台的二进制。

### 从源码编译

```bash
git clone https://github.com/yinnho/opencarrier.git
cd opencarrier
cargo build --release -p cli
cp target/release/opencarrier /usr/local/bin/
```

---

## Quick Start

```bash
# 1. 初始化（生成配置和登录凭据）
opencarrier init

# 2. 启动守护进程
opencarrier start

# 3. 打开 Dashboard
open http://localhost:4200
```

配置文件位于 `~/.opencarrier/config.toml`，数据目录为 `~/.opencarrier/`。

---

## What is OpenCarrier?

OpenCarrier 是一个用 Rust 构建的 **开源 Agent 操作系统**。核心概念是**分身（Clone）**——每个分身是一个独立的 AI 角色，拥有人格、知识、技能和工作空间。

**v0.3.0** 的核心突破：你在微信、飞书、钉钉里扫个码，机器人就是你的 AI 助理。聊天就是交互界面。

---

## 目录结构

```
~/.opencarrier/
├── config.toml              # 主配置
├── brain.json               # LLM 路由配置（热重载）
├── workspaces/              # 分身工作空间
│   └── <agent-name>/
│       ├── SOUL.md          # 人格
│       ├── system_prompt.md # 行为指令
│       ├── MEMORY.md        # 知识索引
│       ├── knowledge/       # 知识库
│       └── agent.toml       # 运行参数
├── senders/                 # 渠道发送者
│   └── <sender_id>/
│       ├── session.json     # 平台凭证 + 路由
│       └── config.json      # sender → agent 绑定
└── sessions/                # 对话历史
```

### sender_id 映射

每个渠道用不同的平台标识作为 `sender_id`（即 `senders/` 下的目录名）：

| 渠道 | sender_id 来源 | session.json 中的 sender_key |
|------|---------------|------------------------------|
| 企微 SmartBot | bot_id | `bot_id` |
| 飞书 | app_id | `app_id` |
| 钉钉 | app_key | `app_key` |
| 微信 | openid | `openid` |

---

## 架构

```
┌──────────────────────────────────────────┐
│  分身 (Clone) — WHO: 身份 + 工作空间     │
├──────────────────────────────────────────┤
│  大脑 (Brain) — THINK: LLM 智能路由      │
├──────────────────────────────────────────┤
│  工具 (Tool) — DO: 内置 + MCP 工具       │
├──────────────────────────────────────────┤
│  渠道 (Channel) — CONNECT: 全平台接入     │
├──────────────────────────────────────────┤
│  记忆 (Memory) — REMEMBER: 生命周期管理   │
└──────────────────────────────────────────┘
```

### 大脑层 (Brain) — LLM 智能路由

三层路由：Provider → Endpoint → Modality，支持熔断器、热重载、20+ Provider（Anthropic, OpenAI, Gemini, DeepSeek, Ollama 等）。

### 渠道层 (Channel) — 全平台接入

内置四个渠道适配器，启动时自动发现并连接：

| 渠道 | 连接方式 | crate |
|------|---------|-------|
| 企微 SmartBot | WebSocket 长连接 | `channel-wecom` |
| 企微 App/Kf | HTTP Webhook | `channel-wecom` |
| 飞书 | WebSocket 长连接 | `channel-feishu` |
| 钉钉 | WebSocket 长连接 | `channel-dingtalk` |
| 微信 | iLink HTTP 长轮询 | `channel-weixin` |

新 bot 扫码注册后立即启动连接（事件驱动），无需重启或等待轮询。

### 工具层 (Tool)

内置工具集 + MCP 扩展：

| 工具 | 功能 |
|------|------|
| `file_read`/`file_write`/`file_list` | 文件读写 |
| `shell_exec` | Shell 命令 |
| `web_fetch` | 网页抓取 |
| `kv_get`/`kv_set`/`kv_list` | 抽屉记忆（用户私有数据） |
| `knowledge_read`/`knowledge_add`/`knowledge_extract` | 知识库（共享知识） |
| `skill_load`/`skill_create`/`skill_update` | 技能管理 |
| `cron_create`/`cron_list`/`cron_delete` | 定时任务 |
| `agent_send`/`agent_spawn`/`agent_list` | 多 Agent 协作 |
| MCP 扩展 | 企微(45)、飞书(73)、公众号、浏览器、搜索等 |

### 记忆层 (Memory)

两层记忆结构 + 知识库：

| 层 | 工具 | 存储 | 注入方式 |
|---|------|------|---------|
| **L0 摘要** | 自动生成 | 最近 N 条对话摘要 | system prompt `[Recent conversations]` |
| **L2 抽屉** | `kv_get`/`kv_set`/`kv_list` | 用户私有数据（偏好、账号、决策） | system prompt `[User profile]`/`[Entities]`/`[Recent events]` 等 |
| **知识库** | `knowledge_read`/`knowledge_add`/`knowledge_extract` | 共享知识文件（knowledge/*.md） | system prompt 知识区块 |

- L0 摘要：每轮对话自动生成（INTENT + OUTCOME + KEY FACTS），保留最近 10 条
- L2 抽屉：按语义 key 组织（`entity.wechat_accounts`、`preference.theme`、`event.2026-05-28.decision`），值是数组，按 `(agent_name, owner_id, user_id)` 隔离
- 知识提取：对话结束后自动从 key_facts 分类写入抽屉（状态型合并去重，时间线型追加）
- LLM 自学习：缺少信息 → 问用户 → 成功后存储 + 更新 SKILL.md

---

## Crate 结构

```
opencarrier (14 crates, 282 source files, 1485 tests)
├── crates/
│   ├── types/          共享类型 + Channel trait + 配置工具
│   ├── memory/         SQLite 记忆层
│   ├── runtime/        Agent loop + LLM drivers + tools + MCP + bridge
│   ├── kernel/         内核: 子系统协调, RBAC, 调度
│   ├── api/            REST/WS API + Dashboard + 渠道注册
│   ├── cli/            CLI (init/start/agent/chat/config)
│   ├── lifecycle/      分身生命周期: 进化, 编译, 健康
│   ├── clone/          分身管理: Hub 下载, workspace 安装
│   └── channels/
│       ├── wecom/      企微渠道 (SmartBot WS + App/Kf Webhook)
│       ├── feishu/     飞书渠道 (WebSocket)
│       ├── dingtalk/   钉钉渠道 (WebSocket)
│       └── weixin/     微信渠道 (iLink 长轮询)
└── tools/
    ├── mcp-common/           MCP 公共库
    ├── wecom-mcp/            企微工具 (45 tools)
    ├── feishu-mcp/           飞书工具 (73 tools)
    ├── wechat-oa-mcp/        公众号工具
    ├── browser-mcp/          浏览器工具
    ├── bilibili-mcp/         B站工具
    ├── xiaohongshu-mcp/      小红书工具
    ├── zhihu-mcp/            知乎工具
    ├── twitter-mcp/          Twitter/X 工具
    └── reddit-mcp/           Reddit 工具
```

---

## Security

- Loop Guard — 工具循环检测 + 熔断器
- SSRF 防护 — 阻断私有 IP、云元数据端点
- Capability Gates — RBAC 能力门控
- Secret Zeroization — API key 自动擦除
- Merkle 哈希链审计 — 每个操作密码学链接

---

## Development

```bash
cargo build --workspace --lib          # 编译
cargo test --workspace                 # 1485 tests
cargo clippy --workspace --all-targets -- -D warnings  # 0 warnings
```

---

## 社区

<p align="center">
  <img src="docs/wechat-group.jpg" width="200" alt="微信群" /><br/>
  <sub>扫码加入微信群，聊聊你的 AI 分身</sub>
</p>

---

## License

MIT
