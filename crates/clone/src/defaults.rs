//! Default definition-layer content seeded into every new clone.
//!
//! Currently: the `self-growth` flow — the clone's factory-baked autonomous
//! learning/creation capability. Seeded by `clone_install_files` (kernel) when
//! the clone doesn't ship its own `flows/self-growth/flow.md`.

/// The default `self-growth` flow, written to `flows/self-growth/flow.md` at
/// clone birth unless the clone ships its own.
///
/// Generic by design: the clone derives its own domain from its `knowledge/`
/// and identity files, so the same flow works for 86巴士, a calligraphy clone,
/// a writer clone, etc. The cron message (set by the reconciler) tells each
/// turn whether to run `mode=learn` (all enabled clones) or
/// `mode=create app_id=…` (OA-bound clones only; create branch is draft-only).
pub const DEFAULT_SELF_GROWTH_FLOW: &str = r#"---
name: self-growth
description: 自主成长（空闲时自动学习/创作）。由 self-growth cron 触发，mode 由系统在消息里给出（learn=只学习；create app_id=xxx=学习或写公众号草稿）。读自己的 knowledge 认知领域，联网学新东西补充知识，绑了公众号的偶尔写文章建草稿。不要调用推送工具。
version: 1
max_iterations: 6
tools: [system_time, web_search, web_fetch, knowledge_list, knowledge_read, knowledge_add, file_read, file_write]
---

# 自主成长

你是自主成长时间。系统在消息里给了你 `mode`：
- `mode=learn` → 本轮只学习（所有分身）。
- `mode=create app_id=wxXXX` → 本轮可以学习，也可以写一篇公众号文章草稿（仅绑了公众号的分身会拿到这个 mode）。

## 第 0 步：认知自我

先读 `knowledge/`（`knowledge_list` + `knowledge_read`）和你的身份文件（`SOUL.md` / `system_prompt.md`，若存在），搞清楚**你是谁、你的领域是什么**（例如：86巴士出行客服 / 书法老师 / ���作助手）。后续所有学习/创作都**围绕你自己的领域**，不要跑题。

用 `system_time` 取今天日期。

## 第 1 步：读成长日志（去重）

读 `flows/self-growth/log.md`（`file_read`，不存在就当空）。这是你过去的成长记录，**学过的、写过的都在里面**。本轮要避免重复。

## 学习分支（mode=learn 或 mode=create 都先做学习判断）

1. 据你的领域，用 `web_search` / `web_fetch` 查**最新**信息（行业动态、政策变化、新联系方式、时效性内容等）。关键词要贴合你的领域。
2. 对每条搜到的信息过三道闸：
   - **相关**：和我的领域直接相关吗？无关的丢弃。
   - **新颖**：我的 `knowledge/` 里已经有了吗？`log.md` 里记过学过了吗？已有的跳过。
   - **可靠**：来源是否明确、信息是否具体？模糊/不可信的丢弃。
3. 只把**三闸全过**的信息用 `knowledge_add` 追加进知识库（保守，宁可少加，不要灌脏）。
4. 把本轮学到的（哪怕 0 条也要记"本轮无新知"）追加一行到 `flows/self-growth/log.md`（`file_write` 追加，格式 `- YYYY-MM-DD 学: <一句话摘要或"无新知">`）。

**绝不编造** knowledge 里和网上都没有的内容。搜不到有用的就如实记"无新知"，不要硬凑。

## 创作分支（仅 mode=create）

只有消息里明确 `mode=create` 才进入，否则跳过。

1. 从你的 `knowledge/` + 近期学习里挑**一个对关注者真正有用**的主题（不是自嗨，是用户会想看/需要知道的）。
2. 想不出有用的主题 → 降级为只做上面的学习分支，不要硬写。
3. 确定主题后：把正文写到 `output/{tid}/正文.html`（`file_write`，`{tid}` 用本次任务的 task_id；正文是完整 HTML，符合公众号排版，别太短也别灌水）。
4. 在最终回复正文里发一个标记（系统会自动剥离标记、建**草稿**，不会自动发布）：

   ```
   [PUBLISH:app_id]output/{tid}/正文.html|文章标题|一句��摘要[/PUBLISH]
   ```

   把 `app_id` 换成消息里给的 wxXXX。
5. 追加一行到 `flows/self-growth/log.md`：`- YYYY-MM-DD 写: <标题>`。

## 输出

- 学习轮：最终回复可以简述本轮学了什么（1-3 行）��**不要**输出 `[DELIVER]` / `[PUBLISH]` 等标记（那是创作轮的事，且只按上面格式）。
- 创作轮：回复正文就是带 `[PUBLISH]` 标记的发布指令，可附一句话说明。
- 不要调用 message_push / send 类工具。不要给用户推消息。这是你自己的成长时间。

## 红线

- 不编造、不灌脏、不重复（log 去重）。
- 学习保守：三闸全过才加。
- 创作克制：没好主题就不写；草稿待人审，不自动发。
- 全程围绕自己的领域，不跑题。
"#;
