# browser-mcp

浏览器自动化 MCP server。

通过 Playwright 控制浏览器，支持导航、点击、截图、页面内容抓取等操作。

## 工具

- `browser_navigate` — 导航到 URL
- `browser_click` — 点击坐标
- `browser_type` — 输入文本
- `browser_screenshot` — 截图
- `browser_read_page` — 读取页面 HTML
- `browser_scroll` — 滚动页面
- `browser_run_js` — 执行 JavaScript
- `browser_back` — 后退
- `browser_wait` — 等待
- `browser_close` — 关闭浏览器

## 配置

```toml
[[mcp_servers]]
name = "browser"
description = "浏览器自动化，支持截图、点击、页面抓取"
timeout_secs = 60
```
