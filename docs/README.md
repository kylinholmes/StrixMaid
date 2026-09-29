# 项目现状与开发入口

以当前源码为准，最后核对：2026-09-27。本文记录现状，不用历史测试数字代替当前验收。

## 当前实现

StrixMaid 是 Linux、macOS、Windows 的服务器观测与管理平台。前端是
React + TypeScript + Vite；后端是 Rust workspace，主程序默认嵌入 `web/dist`。

| 层 | 职责 |
|---|---|
| `strixmaid-types` | DTO、错误、IPC 与 RPC 契约 |
| `strixmaid-core` | 系统 provider、采集与存储、会话、worker、终端；不依赖 HTTP 框架 |
| `strixmaid-node` | 单机 API、认证、审计、WebSocket 与生命周期编排 |
| `strixmaid` | `serve` / `agent` / `worker` 入口、UI、节点登记与指标汇聚 |
| `strixmaid-helper` | 系统认证、身份切换、会话资源持有；只依赖 types |
| `web/src` | 按功能组织的 React 页面与数据层 |

已实现：概览、实时与历史指标、进程、服务、日志、文件浏览与终端工作区。
文件侧有分页、图标、缩略图、文本/图片/媒体预览与原生 Range 下载；写操作按 B–D 分期推进。
审计有后端 API，正式前端的审计与设置页面仍为占位。

`strixmaid agent` 当前采集、聚合、存储并推送指标，没有装载完整 node 运行时。
多主机管理只完成了 Router 在任意流上服务的基础验证；yamux、远程 API 代理和
SSH 推装未实现。不要把设计目标当成可用功能。

## 阅读入口

- [2026-09-29 后续修复](reviews/2026-09-29-follow-up.md)：连接竞态、日志分页、终端与配置拆分、CLI 校验及剩余验证。
- [2026-09-29 Linux / D-Bus 验证](reviews/2026-09-29-linux-dbus.md)：模块拆分、Fedora ARM 真实总线压测与主任务复核。
- [2026-09-29 会话生命周期](reviews/2026-09-29-session-lifecycle.md)：修复过期管理访问被续活，真实 PAM 与进程回收验证。
- [2026-09-29 文件访问验收](reviews/2026-09-29-files-live.md)：真实 PAM/UID、Range、五轮并发与取消、内存增长修复。
- [2026-09-27 审查报告](reviews/2026-09-27-quality-and-architecture.md)：问题证据、优先级与实测范围。
- [设计与边界](design.md)：进程、权限、存储、API 契约。
- [路线索引](roadmap/README.md)：已落地设计、后续方案与验收。
- [文件模块分期与验收](roadmap/13-files.md)：A 预览/下载已实现，包含字节通道与浏览器文件凭证；后续 B–D 为写操作。
- [后续模块与文件通道历史交接](HANDOFF-2026-09-21.md)：保留决策背景，文件方案以 13 号路线为准。
- [macOS](macos-platform.md) / [Windows](windows-platform.md)：平台差异。
- [D-Bus 事故](incidents/2026-09-25-dbus-wedge.md)：死锁机制与真实系统压测条件。

已删除旧交接、旧差距分析和完成的逐步实施清单；历史内容从 git 查阅。
代码中的旧 roadmap 编号保留为历史设计出处，不表示还有对应待办。

## 本地检查

```sh
cd web
bun install --frozen-lockfile
bun run check
bun run typecheck
bun run test
# 已安装 Edge 时；其他环境见 web/e2e/README.md
PLAYWRIGHT_CHANNEL=msedge bun run test:browser
bun run build
cd ..
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo clippy -p strixmaid --no-default-features --all-targets --locked -- -D warnings
cargo build --workspace --locked
cargo test --workspace --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked
bash scripts/acceptance.sh
```

先构建前端：`rust-embed` 在 debug 编译时也要求 `web/dist` 存在。
先 build 再 test：helper 的部分测试会启动磁盘上的真实主程序。
`acceptance.sh` 不给 `STRIX_TOKEN` 时只执行静态检查，动态项标为未测。
仓库尚非 rustfmt-clean，当前 CI 不把全量格式检查作为门槛。

## 仍有效的工程约束

- Unix 附件通道的每一帧都通过 `recvmsg` 读取；普通 `read` 会丢弃附带的 fd。
- 权限选择在 node 的执行入口完成，worker 以所选身份执行；不要复制一套授权判断。
- 会话拆除先关终端，再关 worker。shell 自退、关闭、登出要共用幂等关闭路径。
- 多线程 worker 在 fork 前计算补充组；fork 后只走适合该阶段的系统调用。
  子进程不得继承主进程的 IPC fd。
- 附件、分块、帧上限与终端退出的回归测试要走真实通道或 PTY，不能只用假 worker。
- 有界事件队列的消费者不能在消费循环内等待同一连接的请求回复；D-Bus 修复需在
  有 `services.changed` 订阅者时施压，空载健康检查不能证明修复有效。
- 平台专属代码在对应平台检查，macOS 本地结果不能替代 Linux root 或 Windows 验收。
- 原生图标素材下载要校验内容，HTTP 200 可能只是软链接目标字符串。
- 注释与文档用中文书面语；依赖通过 `cargo add` 添加。
