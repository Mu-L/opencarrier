# AginxBrower

轻量级服务端浏览器引擎，基于 [Obscura](https://github.com/h4ckf0r0day/obscura) 构建，用于快速页面抓取和普通 JS 点击。

## 定位

- **Obscura**：负责快速 scraping + 普通 JS click，体积小、启动快。
- **复杂场景 fallback 到 Chromium**：本 PoC 不实现 Chromium fallback，后续可在调度层根据失败类型切换。

## 目录结构

```
docs/aginx/aginxbrower/
├── Cargo.toml
├── .gitignore
├── README.md
└── src
    ├── main.rs      # HTTP 服务入口与路由
    └── browser.rs   # Obscura 浏览器操作封装
```

## 依赖

- Rust 1.78+
- Obscura 会自动下载预编译 V8 静态库（首次编译较慢）
- 如需代理，设置环境变量 `OBSCURA_PROXY`，例如：
  ```bash
  export OBSCURA_PROXY=socks5://127.0.0.1:8800
  ```

## 构建

```bash
cd docs/aginx/aginxbrower
cargo build --release
```

release 二进制预计在 70MB 左右。

## 运行

```bash
export OBSCURA_PROXY=socks5://127.0.0.1:8800   # 可选
./target/release/aginxbrower
```

默认监听 `0.0.0.0:8089`，可通过 `AGINXBROWER_BIND` 修改：

```bash
AGINXBROWER_BIND=0.0.0.0:8090 ./target/release/aginxbrower
```

## HTTP API

### GET /health

```bash
curl http://127.0.0.1:8089/health
```

响应：

```json
{"status":"ok","engine":"obscura"}
```

### POST /fetch

抓取页面并返回内容。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 目标 URL |
| format | string | 否 | `markdown` / `html` / `text`，默认 `markdown` |
| selector | string | 否 | CSS 选择器，仅提取选中区域 |
| wait_secs | u64 | 否 | 页面加载后额外等待秒数 |

示例：

```bash
cat <<EOF | curl -sS -X POST http://127.0.0.1:8089/fetch \
  -H "Content-Type: application/json" -d @-
{"url":"https://github.com/trending","format":"text","selector":"article"}
EOF
```

响应：

```json
{
  "url": "https://github.com/trending",
  "title": "Trending  repositories on GitHub today · GitHub",
  "content": "..."
}
```

### POST /click

使用 JS `element.click()` 点击指定元素。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 目标 URL |
| selector | string | 是 | CSS 选择器 |
| wait_secs | u64 | 否 | 页面加载后额外等待秒数 |

示例：

```bash
cat <<EOF | curl -sS -X POST http://127.0.0.1:8089/click \
  -H "Content-Type: application/json" -d @-
{"url":"https://github.com/trending","selector":"article:first-of-type h2 a"}
EOF
```

响应：

```json
{
  "url": "https://github.com/trending/",
  "selector": "article:first-of-type h2 a",
  "clicked": true,
  "text_after": "..."
}
```

### POST /eval

在页面上执行任意 JavaScript 并返回结果。

请求字段：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| url | string | 是 | 目标 URL |
| script | string | 是 | JS 表达式或 IIFE |
| wait_secs | u64 | 否 | 页面加载后额外等待秒数 |

示例：

```bash
cat <<EOF | curl -sS -X POST http://127.0.0.1:8089/eval \
  -H "Content-Type: application/json" -d @-
{"url":"https://example.com","script":"document.title"}
EOF
```

响应：

```json
{
  "url": "https://example.com/",
  "result": "Example Domain"
}
```

## 已知限制

1. **无法截图**：Obscura 没有 layout/paint 引擎，不支持截图。
2. **无元素坐标**：只能做 JS click，不能做基于屏幕坐标的点击。
3. **导航等待不可调**：Obscura 的 `goto` 固定等待 `load` 事件，对重页面较慢。
4. **JS 复杂组件可能失败**：React/Vue 等框架的事件委托可能不响应原生 `click()`，需要针对具体站点写 JS。
5. **代理支持**：Obscura 仅支持 HTTP/HTTPS/SOCKS5 代理，通过 `OBSCURA_PROXY` 传入。

## 后续优化方向

1. 增加 `POST /session` 会话保持，复用浏览器实例，减少启动开销。
2. 增加 `POST /form`：自动填充 input 并 submit。
3. 增加失败重试 + 超时细粒度控制。
4. 在 OpenCarrier 调度层实现 Chromium fallback 策略。
5. 暴露 Prometheus /healthz 等运维端点。

## 与 Chromium 对比

| 项目 | AginxBrower (Obscura) | Chromium |
|------|----------------------|----------|
| 二进制体积 | ~70MB | ~256MB+ |
| 启动速度 | 快 | 慢 |
| 截图 | ❌ | ✅ |
| 坐标点击 | ❌ | ✅ |
| JS click / scraping | ✅ | ✅ |
| 复杂 SPA 兼容 | 中等 | 高 |

## 许可证

与 OpenCarrier 主项目保持一致。
