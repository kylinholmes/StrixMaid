# 浏览器回归

在 `web` 目录执行：

```sh
PLAYWRIGHT_CHANNEL=msedge bun run test:browser
```

本机有 Edge 时直接复用，不需要下载 Chromium。未指定 channel 时使用 Playwright
管理的 Chromium；CI 用 `bun node_modules/playwright/cli.js install --with-deps chromium`
安装运行时，然后执行 `bun run test:browser`。

测试启动独立的 Vite 服务（127.0.0.1:5174），每个用例使用独立浏览器上下文。
API 与 WebSocket 在浏览器网络边界提供固定响应，不连接实际系统账户，也不启动 helper。
会话用例操作完整应用和登录界面；日志用例在 StrictMode 中挂载生产 `useLogs`。
`logs.html` 仅是开发服务器中的测试入口，不进入正式 Vite 构建。

覆盖：

- 注销请求挂起时立即锁定；随后登录另一用户，同路径查询和私有挂载点不复用旧数据。
- 重复游标下切换日志筛选取消旧页，StrictMode 不重复发送分页请求。
- 暂停跟随时缓冲有界，截断后通过保留记录末尾游标恢复分页。
- REST 首页晚到不会覆盖实时推送；重叠游标只展示一次。

失败时 `test-results` 保留 trace，CI 将它上传为 artifact。
这些用例验证浏览器状态与请求时序，不替代 PAM、真实终端握手或 Linux/Windows 系统验收。
