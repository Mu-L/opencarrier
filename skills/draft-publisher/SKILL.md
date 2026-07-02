---
name: draft-publisher
description: 将排版好的文章发布到微信公众号(自动封面+建草稿+发布)
version: 10
tools:
  - file_read
---
# Draft Publisher(AI + API 模式)

将排版好的文章发布到微信公众号。**发布动作(生成封面→上传→建草稿→正式发布)由后台确定性 handler 执行,不走 agent 工具链**——你只负责确认文件和发一个标记。这是 2026-07 改造后的稳定方案,取代之前反复失败的 `image_generate`/`mcp_wechat_oa_*` 工具链。

## 你要做的(只有两步)

### 1. 确认文件就位

```
file_read(path="output/<pipeline_id>/正文.html")   // 排版后的 HTML(必须存在)
```

确认 `正文.html` 已由 article-formatter 产出。同时确认同目录有 `正文.md`,且**其首行是文章标题**(handler 据此取标题)。

若 `正文.html` 不存在,先回到 article-formatter 排版,不要发标记。

### 2. 取 app_id 并发 PUBLISH 标记

从 User Profile 的 `preferences.wechat_accounts` 取目标公众号的 `app_id`(用户未指定取**第一个**;用户说"发到XX"按 name 匹配)。

**回复的最后一行**发标记(必须是这个精确格式):

```
[PUBLISH:<app_id>]<正文.html 的路径>[/PUBLISH]
```

例如:
```
[PUBLISH:wx4e35abcebe78a249]output/pipeline-20260702-xxx/正文.html[/PUBLISH]
```

路径用你 `file_read` 时用的同一个路径(相对 `~/.opencarrier` 或绝对路径都行,handler 都认)。

## 标记之后会发生什么(不用你管)

后台 handler 自动:
1. 读 `正文.html` 正文 + 同名 `.md` 首行作标题
2. **生成封面图**(失败则取素材库第一张图,再失败才报错——公众号发布必须有封面)
3. 建草稿 → 正式发布
4. 把结果(`✅ 已发布` + media_id/publish_id,或 `❌ 失败原因`)作为新消息推给用户

**你不需要、也不应该**调用 `image_generate`、`mcp_wechat_oa_upload_media`、`mcp_wechat_oa_create_draft`、`mcp_wechat_oa_publish_draft` 等任何发布相关工具。发了标记就够了。

## 凭证

`preferences.wechat_accounts` JSON 数组:
```json
[{"name": "账号名", "app_id": "wx...", "app_secret": "..."}, ...]
```
你只需要 `app_id` 放进标记;`app_secret` 由 handler 从已注册的 OA 账号自动取,不用你传。

## Important Principles

- 只发**一个** `[PUBLISH:...]` 标记,放回复最后一行
- 发布前用一句话告知用户「正在发布…」,标记会被自动剥离、用户看不到
- 标记里的 app_id 必须正确(发错公众号就发错了)
- `正文.md` 首行必须是标题——这决定发布后的文章标题
- 不要重复发标记;不要在标记里塞多余内容
