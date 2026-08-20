# 安全政策 / Security Policy

## 报告漏洞 / Reporting a Vulnerability

**请勿通过公开 issue、PR、讨论区或社交渠道报告安全漏洞。**

请使用 GitHub 的私密漏洞报告通道（Security 标签页 → "Report a vulnerability"，或直接访问
[github.com/yinnho/opencarrier/security/advisories/new](https://github.com/yinnho/opencarrier/security/advisories/new)）。
提交后只有维护者可见，支持在披露前协作修复与协调 CVE。

Please report security vulnerabilities privately via GitHub's **Private Vulnerability Reporting**
(Security tab → "Report a vulnerability"). Do NOT open public issues or PRs for security bugs.
Only maintainers can see your report, and we can coordinate a fix and credit before any disclosure.

## 处理时限 / Response Expectations

- **确认收到**：3 个工作日内
- **初步评估**：7 个工作日内
- 修复发布后会在 Security Advisories 中公布，并致谢报告人（除非要求匿名）

## 范围 / Scope

欢迎报告的问题包括但不限于：

- 任何 crate 中的内存安全、注入、路径穿越、SSRF 问题
- API / WebSocket 端点的鉴权绕过、越权访问（见 `docs/AGENT-APP-API.md`）
- 各渠道接入（微信/企微/飞书/钉钉）的消息伪造或回调伪造
- cron / flow 执行链的提权（如 `shell_allow` 匹配绕过、flow 自提权路径）
- 供应链：依赖中的已知漏洞（Dependabot 已启用，但人工分析同样欢迎）

**不在范围内**：

- 没有具体影响的纯理论问题、需要服务器端已沦陷为前提的问题
- 对我们线上生产服务的扫描、DoS、暴力测试
- 本仓库未部署的配置错误

## 披露政策 / Disclosure

修复发布后，我们会通过 GitHub Security Advisories 发布公告；若报告人希望申请 CVE，我们会在公告中配合。
