# searxng-mcp

SearXNG 网页搜索引擎 MCP server。

通过自托管的 SearXNG 实例提供网页搜索能力，支持中英文搜索，返回结构化结果（标题、URL、摘要）。

## 工具

- `web_search` — 网页搜索，支持 language、time_range、categories、engines 等参数

## 配置

```toml
[[mcp_servers]]
name = "searxng"
description = "SearXNG 网页搜索引擎，支持中英文搜索"
timeout_secs = 30
```
