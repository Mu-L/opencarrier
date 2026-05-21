# OpenCarrier 内置工具参考

## Core 工具（始终自动加载，无需在 skill 中声明）

| 工具 | 说明 |
|------|------|
| `file_read` | 读取文件内容，路径相对于 agent workspace |
| `file_list` | 列出目录文件，路径相对于 agent workspace |
| `tool_search` | 搜索工具目录，按自然语言 query 匹配内置工具 |
| `skill_load` | 按 name 加载 skill 完整内容 |
| `knowledge_read` | 读取 knowledge 文件 |
| `knowledge_list` | 列出可用 knowledge 文件 |
| `session_summarize` | 保存当前对话摘要 |
| `memory_tree` | 查询用户层级记忆树 |
| `cron_create` | 创建定时任务 |
| `cron_list` | 列出定时任务 |
| `cron_cancel` | 取消定时任务 |
| `task_plan` | 将复杂任务拆分为有序步骤 |

## filesystem

| 工具 | 说明 |
|------|------|
| `file_write` | 写入文件 |
| `file_convert` | 用 Pandoc 转换文档格式（markdown, html, docx, pdf 等） |
| `apply_patch` | 应用多段 diff patch，用于精准编辑 |

## shell

| 工具 | 说明 |
|------|------|
| `shell_exec` | 执行 shell 命令并返回输出 |

## knowledge

| 工具 | 说明 |
|------|------|
| `knowledge_add` | 添加 knowledge 条目 |
| `knowledge_remove` | 删除 knowledge 条目 |
| `knowledge_lint` | 检查 knowledge 健康状态 |
| `knowledge_heal` | 自动修复 knowledge 问题 |
| `knowledge_index` | 重建 MEMORY.md 索引 |
| `knowledge_import` | 导入外部数据（FAQ、聊天记录等） |
| `knowledge_extract` | 从对话中提取新 knowledge |
| `skill_create` | 创建新 skill 文件 |
| `skill_update` | 更新 skill body（保留 frontmatter） |
| `clone_evaluate` | 评估 clone 质量（0-100 分） |

## media

| 工具 | 说明 |
|------|------|
| `image_analyze` | 分析图片（格式、尺寸、预览） |
| `image_generate` | 从文字生成图片 |
| `media_describe` | 用视觉模型描述图片内容 |
| `media_transcribe` | 音频转文字 |
| `text_to_speech` | 文字转语音 |
| `speech_to_text` | 语音转文字 |
| `canvas_present` | 展示交互式 HTML 画布 |

## process

| 工具 | 说明 |
|------|------|
| `process_start` | 启动长运行进程（最多 5 个） |
| `process_poll` | 读取进程 stdout/stderr 输出 |
| `process_write` | 向进程 stdin 写入数据 |
| `process_kill` | 终止进程 |
| `process_list` | 列出运行中的进程 |

## web

| 工具 | 说明 |
|------|------|
| `web_fetch` | 抓取 URL（GET/POST），HTML 自动转 Markdown |

## agent

| 工具 | 说明 |
|------|------|
| `agent_send` | 向其他 agent 发送消息并获取回复 |
| `agent_list` | 列出所有运行中的 agent |
| `agent_find` | 按 name/tag/tool 搜索 agent |
| `agent_spawn` | 从 TOML 创建新 agent |
| `agent_kill` | 终止 agent |
| `agent_restart` | 重启 agent |
| `train_write` | 向目标 clone workspace 写文件 |
| `train_read` | 从目标 clone workspace 读文件 |
| `train_list` | 列出目标 clone workspace 文件 |
| `train_evaluate` | 评估目标 clone 质量 |

## misc

| 工具 | 说明 |
|------|------|
| `location_get` | 根据 IP 获取地理位置 |
| `system_time` | 获取当前日期时间和时区 |
| `user_profile` | 读取或更新用户画像 |
| `event_publish` | 发布自定义事件 |
| `schedule_create` | 用自然语言或 cron 创建定时任务 |
| `schedule_list` | 列出定时任务 |
| `schedule_delete` | 删除定时任务 |
| `task_post` | 向共享任务队列发布任务 |
| `task_claim` | 认领任务 |
| `task_complete` | 完成任务 |
| `task_list` | 列出任务队列 |

## sqlite

| 工具 | 说明 |
|------|------|
| `sqlite_query` | 只读 SQL 查询（仅 SELECT/PRAGMA），返回 Markdown 表格 |
| `sqlite_schema` | 列出 SQLite 数据库的表和列 |

## a2a

| 工具 | 说明 |
|------|------|
| `a2a_discover` | 发现外部 A2A agent |
| `a2a_send` | 向外部 A2A agent 发送任务 |
