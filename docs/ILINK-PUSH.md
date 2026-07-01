# iLink 主动推送机制

> 跨渠道 NOTIFY 标记 → fan-out to admins → iLink 直推。解决了 context_token 持久化与 session 查找两个关键问题。

## 完整链路

```
公众号用户发包车咨询（OA webhook）
  → 86bus-assistant 处理（charter-quoter skill）
    → agent 回复中包含 [NOTIFY:charter_lead]...[/NOTIFY]
      → bridge.parse_notify_markers() 解析标记（对用户不可见）
        → notify_routes.json 匹配 type
          → recipients: "admins" → 读 admins.json 获取管理员列表
            → fan-out: 对每个 admin.sender_id 调用 send_fn(channel, bot_id, user_id, msg)
              → weixin channel.send()
                → get_session_for_send(user_id) → BotSession
                → get_context_token(user_id) → 持久化的 token
                  → POST /ilink/bot/sendmessage → iLink Server → 管理员微信
```

## 关键技术问题与解法

### 问题一：context_token 重启丢失

**现象**：服务重启后 iLink 推送失败，日志 `No context_token for user`。

**根因**：context_token 只存在内存 `HashMap` 中，部署重启即清空。iLink sendmessage API 必须携带此 token。

**解法**：context_token 随 BotSession 持久化到 `session.json`。

```
存储时机：poll loop 每次 getUpdates 成功后调用 save_session
          → 合并内存 token + 磁盘已有 token → 写入 session.json
恢复时机：load_new_from_dir 读取 session.json → 注入 BotSession
```

`BotTokenFile` 增加 `context_tokens: HashMap<String, String>` 字段，与 bot_token、baseurl 等凭证一同序列化。

**注意**：save_session 必须先读磁盘已有数据再合并写入（merge-on-save），否则多个 session 并发存储时会互相覆盖。

### 问题二：get_session_for_send 跨账户查找失效

**现象**：iLink API 返回 `{}`（HTTP 200）但不投递消息。

**根因**：`ab7c0d4` 引入的跨账户查找逻辑有缺陷。该逻辑试图找到"接收方 bot 的 session"来发送，但 context_token 只存储于目标用户自己的 BotSession 上，其他 bot session 不持有此 token，导致查找失败后落入错误的 fallback 路径。

**解法**：回归简单的直接查找——`self.bots.get(user_id)`。

```rust
// get_session_for_send：直接按 user_id 查找
fn get_session_for_send(&self, _bot_id: &str, user_id: &str)
    -> Option<Ref<'_, String, BotSession>>
{
    if let Some(state) = self.bots.get(user_id) {
        return Some(state);  // 直接命中，使用目标用户自己的 session
    }
    // fallback: 按 bot_id 查找
    ...
}
```

### 问题三：notify_routes.json 硬编码 user_id

**现象**：管理员变动时需要手动更新 routes 文件。

**解法**：`NotifyTarget` 增加 `recipients: "admins"` 字段。bridge 在 send_response 时动态解析：

```rust
if target.recipients.as_deref() == Some("admins") {
    let agent_id = self.resolve_agent(original);
    let admins = read_admins(&workspace).admins;
    // fan-out to each admin.sender_id
    for admin in &admins {
        send_fn(&channel, &bot_id, &admin.sender_id, &msg);
    }
}
```

管理员加减人只需修改 `admins.json`，推送目标自动跟随。

## 关键文件

| 文件 | 职责 |
|------|------|
| `crates/channels/weixin/src/token.rs` | context_token 持久化（merge-on-save）、get_session_for_send |
| `crates/channels/weixin/src/models.rs` | BotTokenFile 增加 context_tokens 字段 |
| `crates/channels/weixin/src/channel.rs` | send() 函数、poll loop save_session 调用点 |
| `crates/runtime/src/plugin/bridge.rs` | NotifyTarget、parse_notify_markers、fan-out 逻辑 |
| `crates/runtime/src/plugin/admin_store.rs` | read_admins() — 读取管理员列表 |
| `~/.opencarrier/notify_routes.json` | 推送路由配置（channel + recipients） |
| `<workspace>/admins.json` | 管理员列表（creator + approved admins） |
| `senders/<id>/session.json` | iLink 凭证 + context_tokens |

## 配置示例

**notify_routes.json**：
```json
{
  "charter_lead": {
    "channel": "weixin",
    "bot_id": "default",
    "recipients": "admins",
    "prefix": "🚗 新包车咨询"
  }
}
```

**admins.json**：
```json
{
  "admins": [
    {"sender_id": "o9cq80yV...@im.wechat", "role": "admin"},
    {"sender_id": "oOPNNvwimcy...", "role": "admin"}
  ]
}
```

## 限制

- iLink 推送依赖 context_token，需管理员近期给 iLink bot 发过消息（token 由 WeChat 产生，有效期内可用）
- token 持久化到 session.json 后，即使服务重启也可恢复，无需重新交互
- OA openid 类型的管理员收到的是公众号客服消息（48h 窗口），iLink 类型收到的是个人微信 bot 消息
