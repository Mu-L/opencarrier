# OpenCarrier 代码冗余审计报告

审计日期：2026-05-14
更新日期：2026-05-14（驱动层统一重构完成）

## 一、死代码（完全未使用）

| 位置 | 内容 | 行数 | 状态 |
|------|------|------|------|
| `crates/cli/src/progress.rs` | 整个文件未声明为模块，完全不可达 | 322 | **已删除** |
| `crates/cli/src/table.rs` | 整个文件未声明为模块，完全不可达 | 248 | **已删除** |
| `crates/cli/src/main.rs:764` | `launch_desktop_app()` 从未被调用 | ~57 | **已删除** |
| `crates/cli/src/ui.rs:81` | `next_steps()` 从未被调用 | 6 | **已删除** |
| `crates/api/src/routes/bots.rs:197` | `DeviceAuthSession` struct 未使用 | ~12 | 待处理 |

另外 `runtime/Cargo.toml` 依赖了 4 个 channel crate（feishu/wecom/weixin/dingtalk），
但 runtime 代码中零引用 — **已移除**。

## 二、巨型文件需拆分

### 1. `cli/src/main.rs` — 4800 行 → ~4740 行

一个 main.rs 承担了 12+ 种职责，应拆为独立模块：

| 建议模块 | 当前行范围 | 行数 |
|----------|-----------|------|
| `commands.rs` | CLI 结构体定义 | ~112 |
| `daemon.rs` | 守护进程检测/连接 | ~125 |
| `providers.rs` | Provider 检测/测试 | ~115 |
| `cmd_daemon.rs` | start/stop 命令 | ~270 |
| `cmd_agent.rs` | agent 相关命令 | ~440 |
| `cmd_doctor.rs` | 健康检查（最该拆） | ~740 |
| `cmd_dashboard.rs` | dashboard 命令 | ~155 |
| `cmd_config.rs` | config/models/cron 等命令 | ~900 |
| `cmd_hub.rs` | hub/plugin 命令 | ~290 |
| `cmd_uninstall.rs` | 卸载逻辑 | ~350 |

### 2. `runtime/src/agent_loop.rs` — 2853 行

`run_agent_loop` 和 `run_agent_loop_streaming` 几乎相同 — **已合并**。
`run_agent_loop_streaming` 现在是薄转发，核心逻辑统一在 `run_agent_loop` 中。

`run_agent_loop_impl` 单个函数 750 行，三个 StopReason 分支应提取为独立函数。

### 3. `runtime/src/tools/agent.rs` — 1605 行

8 种工具类别挤在一个文件里，应拆为 training.rs、inter_agent.rs、
shared_memory.rs、collaboration.rs、knowledge_graph.rs、scheduling.rs、a2a.rs。
definitions() 方法（436 行纯 JSON schema）应单独放一个文件。

### 4. `api/src/routes/bots.rs` — 1552 行

三个平台的设备认证流程结构完全相同，应拆为 senders/{wecom,feishu,dingtalk}.rs
+ 共享的 senders/device_auth.rs。

## 三、驱动层统一重构（已完成）

### 方案

参考 Python 版 `yingheclient-api/app/api/llm.py` 的 format 分发模式，
将 11 个 HTTP API driver 合并为一个 `UnifiedHttpDriver`，按 `ApiFormat` 分发
到 `complete_FORMAT` 方法。每个 format 只定义请求序列化和响应解析，
HTTP 基础设施（认证、重试、错误分类、异步任务轮询）全部共享。

### 架构

| 文件 | 职责 | 行数 |
|------|------|------|
| `llm_driver.rs` | 接口层：trait + types + Brain + DriverConfig + `create_driver()` 工厂 | ~570 |
| `llm_driver_impl.rs` | 实现层：UnifiedHttpDriver + 11 种 format + 共享 HTTP + 测试 | ~1800 |
| `drivers/claude_code.rs` | CLI 子进程驱动（保留） | ~600 |
| `drivers/qwen_code.rs` | CLI 子进程驱动（保留） | ~500 |
| `drivers/fallback.rs` | 链式降级驱动（保留） | ~200 |

### 删除的文件

| 原文件 | 行数 | ApiFormat |
|--------|------|-----------|
| openai.rs | ~1500 | OpenAI |
| anthropic.rs | ~716 | Anthropic |
| gemini.rs | ~800 | Gemini |
| dashscope_image.rs | ~142 | DashScopeImage |
| dashscope_tts.rs | ~145 | DashScopeTts |
| dashscope_video.rs | ~195 | DashScopeVideo |
| kling.rs | ~283 | Kling |
| minimax_image.rs | ~150 | MiniMaxImage |
| minimax_search.rs | ~139 | MiniMaxSearch |
| glm_search.rs | ~150 | GlmSearch |
| openai_images.rs | ~117 | OpenAIImages |

**合计删除 ~4337 行**，替换为 ~1800 行统一实现，净减 ~2500 行。

### 关键设计决策

1. **SSE 移除**：`UnifiedHttpDriver` 不实现 `stream()`，使用 `LlmDriver` trait
   默认实现（`complete()` 包装为单个 TextDelta + ContentComplete）
2. **错误分类**：复用 `llm_errors.rs::classify_error()` 替代各 driver 硬编码状态码判断
3. **OpenAI 复杂重试**：保留独立 `send_openai_with_retry()` 方法（需修改请求体重试）
4. **异步任务**：dashscope_video / kling 使用共享 `poll_until_complete()` 辅助方法
5. **Kling JWT**：HMAC-SHA256 认证作为 `generate_jwt()` 私有方法
6. **Gemini thoughtSignature**：通过 `ContentBlock::Text`/`ToolUse` 的 `provider_metadata` 往返

## 四、MCP Tools 重复

1. main() 入口样板（8 个工具 × ~15 行 = ~120 行）— 应提供 run_stdio_server! 宏
2. 错误格式化 `format!("{{\"error\": \"{}\"}}", e)` — 200+ 处重复，且有 JSON 注入 bug
3. wechat-oa-mcp/zhihu-mcp 本地重实现 mcp-common 已有函数
4. bilibili-mcp/twitter-mcp/wechat-oa-mcp 各自定义 define_params! 宏绕过 mcp-common

## 五、优化进度

| 优先级 | 改动 | 收益 | 状态 |
|--------|------|------|------|
| P0 | 删除 progress.rs、table.rs、launch_desktop_app()、next_steps() | 减 ~630 行死代码 | **已完成** |
| P0 | 移除 runtime 的 4 个未使用 channel 依赖 | 减少编译时间 | **已完成** |
| P1 | 统一 HTTP driver 层（11 个 → 1 个 UnifiedHttpDriver） | 减 ~2500 行重复 + 统一错误处理 | **已完成** |
| P1 | 合并 run_agent_loop / run_agent_loop_streaming | 消除冗余入口 | **已完成** |
| P2 | 拆分 cli/main.rs | 可维护性大幅提升 | 待处理 |
| P2 | MCP tools 统一 err_json() + run_stdio_server! 宏 | 消除 200+ 处重复 + 修 JSON bug | 待处理 |
| P2 | 拆分 agent.rs、bots.rs、agent_loop.rs | 提升可读性 | 待处理 |
