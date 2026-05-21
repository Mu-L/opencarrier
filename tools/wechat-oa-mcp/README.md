# wechat-oa-mcp

微信公众号 MCP server。

提供微信公众号文章发布、素材上传、草稿管理等能力。

## 工具

- `create_draft` — 创建草稿
- `upload_media` — 上传图片/素材
- `get_access_token` — 获取 access token
- 其他公众号管理工具

## 配置

```toml
[[mcp_servers]]
name = "wechat-oa"
description = "微信公众号文章发布和素材管理"
timeout_secs = 60
```
