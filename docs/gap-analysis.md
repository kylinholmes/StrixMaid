# StrixMaid 现状与 MVP 目标的差距

> 编写日期：2026-08-27。对应代码状态：Phase 0–3 接线完成，`cargo test --workspace` 273 通过，clippy 零 warning，共 28,147 行 Rust。
> 目标定义见 `design.md`（§13 实现顺序、§14 不做清单）与 `cockpit-feature-inventory.md`（P0 判定）。
> 后续工作的详细方案见 `roadmap/` 目录，本文只回答「差什么、为什么、先做哪个」。

---

## 1. 已完成部分

| 领域 | 内容 | 位置 |
|---|---|---|
| 底座 | workspace、5 个 crate、四层配置、sqlx + migrations、rust-embed、OpenAPI 自动收集（29 个端点）、Scalar 离线文档、`/debug` 调试页 | `crates/*`、`server/src/{apidoc,debug,embed}.rs` |
| 认证 | 自写 PAM FFI 的 helper、socketpair IPC、challenge-response 登录、setuid worker、提权（第二个 helper）、会话与超时回收、Bearer 中间件、WS 子协议鉴权 | `strixmaid-helper/`、`core/src/session/`、`core/src/worker/`、`server/src/auth/` |
| 指标 | 7 类采集器（含 PSI）、环形缓冲、每分钟落盘、五层聚合与保留期清理、自动选层查询、`metrics.live` 频道 | `core/src/metrics/`、`core/src/store/metrics.rs` |
| 主机 | 完整 `SystemInfo`、虚拟化识别、DMI、健康聚合（含需重启检测）、时间与 NTP 状态、主机名 / 时区 / 电源写操作 | `core/src/providers/system/` |
| 进程 | 列表（CPU% 差分）、树、详情、cgroup 到 unit 反查、信号、renice | `core/src/providers/process/` |
| 服务 | zbus 主路径 + systemctl 降级、cgroup 用量直读、unit 文件与依赖、操作、去抖事件流 | `core/src/providers/service/` |
| 日志 | journalctl 查询、游标翻页、单条详情、boot 列表、共享子进程的 follow | `core/src/providers/log/` |
| 能力探测 | system 层六项探测、user 层按组推导、未认证时 `user = null` | `core/src/capability/` |

本机（Ubuntu 24.04，普通用户，无 root）实测通过的路径：PAM 错误密码经 HTTP → helper → `pam_authenticate`；polkit 对 `ssh.service` restart 返回 403；journalctl 翻页不重不漏；`systemd-run --user` 触发 `services.changed`。

## 2. 结构性缺口：授权模型未闭环

`design.md` §5 的核心是「以登录用户身份执行操作，由 PAM / polkit / journald ACL / 文件权限裁决」。当前实现只做到了前半段：

- 认证正确地映射到系统用户，helper 也确实以该用户 fork 了 worker；
- 但 `crates/strixmaid-server/src/routes/` 下的全部处理器都在**主进程内直接调用 provider**，没有任何一条路径经过 worker。`grep -rn 'user_worker\|admin_worker' crates/strixmaid-server/src/routes/` 为 0 处。
- worker 的 RPC 分发表（`core/src/worker/mod.rs` 的 `Dispatcher`）只注册了 `ping` 与 `whoami`。

后果：一旦主进程以 root 部署，任何已登录用户的请求都以 root 执行。polkit 看不到该用户，journald ACL 与文件权限全部被绕过，§5 的授权模型形同虚设。当前开发实例未暴露此问题，仅因为它以非 root 身份运行。

该缺口同时阻塞终端（PTY 必须在 worker 内 fork）、`scope=user` 的跨用户支持（须在目标用户的 worker 内连 session bus）、以及日志可见性的正确性。方案见 `roadmap/01-worker-execution.md`。

### 2.1 提权缺少授权检查

`SessionManager::elevate_start`（`crates/strixmaid-core/src/session/mod.rs`）不检查用户是否有资格提权；helper 的 `SpawnWorker { as_root: true }` 只判断 helper 自身是否为 root（`crates/strixmaid-helper/src/spawn.rs` 第 54 行）。capability 层的 `can_elevate` 只是展示字段，未被强制。

后果：root 部署下，任何能通过 PAM 登录的用户重新输入自己的密码即可获得 root worker。这与 §2 的缺口性质相同——认证正确、授权缺失——但危害更直接。修正方案见 `roadmap/01-worker-execution.md` §4.8。

## 3. P0 范围内尚未完成的项

| # | 项 | 现状 | 方案文件 |
|---|---|---|---|
| 1 | 请求经 worker 执行：读走 user worker，写走 admin worker，未提权返回 403 | 见 §2 | `01-worker-execution.md` |
| 2 | 审计日志 | `Store::audit_write` / `audit_query` 已实现；服务端零处写入；`GET /audit` 端点不存在（`/debug` 页已在调用，返回 404） | `02-audit.md` |
| 3 | 终端 | 未开始。types 已有 `CreateTerminalReq` / `TerminalInfo` / `ResizeReq` | `03-terminal.md` |
| 4 | 文件管理壳：`GET /files`、`GET /files/content` | types 已有 `DirListing` / `FileContent`；无 provider、无路由 | `04-files-and-ws-channels.md` |
| 5 | WS 频道 `system.health`、`processes.live` | hub 与注册接口就绪，源未实现 | `04-files-and-ws-channels.md` |
| 6 | `strixmaid-agent`：推送、断连补发、Server 端汇聚 | `crates/strixmaid-agent/src/main.rs` 为 3 行占位 | `05-agent.md` |
| 7 | 打包：musl 静态构建、`strixmaid.service`、pam.d 安装、`ui` feature | musl target 已安装但无构建配置；pam.d 模板已有（`strixmaid-helper/pam.d/`）但无安装步骤；`ui` feature 未实现，前端始终嵌入 | `06-packaging.md` |
| 1a | 提权授权检查：仅允许 `sudo` / `wheel` / `admin` 组成员提权，在 helper 内强制 | 见 §2.1，无任何检查 | `01-worker-execution.md` §4.8 |
| 8 | `scope=user` 跨用户 | `SystemdBus::with_user_uid` 已留接口；依赖 #1 | `01-worker-execution.md` |
| 9 | capability `user` 层的实测探测 | 当前按组名推导（`derive_user_caps`）；§6 要求试读 journal 等实测 | `01-worker-execution.md` |

## 4. 已实现但未经验证的部分

以下内容有代码、有单元测试，但从未在其目标环境中运行过。它们构成当前最大的风险面。

### 4.1 root 运行路径

本机无 root，以下路径一行都没有执行过：

- helper 以 root 运行时的 `initgroups` / `setgid` / `setuid` 到**其他**用户；
- `pam_open_session` 成功路径：logind 会话创建、`XDG_RUNTIME_DIR`、用户级 systemd 实例的拉起；
- 提权：`SpawnWorker { as_root: true }` 与 admin worker；
- polkit 对非 root worker 的实际裁决（当前只验证了主进程非 root 时的拒绝）；
- `pam.d/strixmaid.{debian,rhel}` 模板是否能通过各发行版的 PAM 栈；
- 会话回收时 helper / worker 进程是否干净退出、有无残留。

### 4.2 浏览器

本机无浏览器。Scalar、`/debug` 全部面板、WebSocket 子协议握手、uPlot band 渲染只经 curl 验证了 HTTP 状态与资源完整性。

### 4.3 长时间运行

指标落盘、五层聚合、保留期清理只运行了数分钟。7 天以上的清理、`m_1d` 层的生成、数据库体积增长曲线、RSS 稳定性均无实测数据。

### 4.4 release 构建的性能

进程列表 3036 个进程 156 ms、unit 列表 711 个 610 ms 均为 debug 构建的数字。

验证方案见 `roadmap/07-verification.md`。

## 5. 明确不在 MVP 范围的项

以下由 `design.md` §14 及各轮决策排除，不计入差距：TLS（走反代）、告警系统、多节点管理操作（MVP 中 Agent 只读）、网络 / 存储 / 账户 / 容器 / 软件更新（P1–P2）、插件机制、正式前端（另行开发）、虚拟机、SELinux、kdump、sosreport。

## 6. 建议顺序与依赖

```
01 worker 执行路径 ──┬── 02 审计（在 01 的调用汇聚点写入）
                     ├── 03 终端（PTY 在 worker 内）
                     └── 04 文件壳与 WS 频道（fs 在 worker 内；processes.live 复用 01 的流式 RPC）
06 打包 ─────────────── 07 验证（root 环境需要可安装的产物）
05 Agent ─────────────── 独立，优先级最低
```

01 决定安全模型，且是 03 / 04 / 08 / 09 的前置，应最先做。02 很小，紧随 01。03 是剩余工作中最大的独立块。06 与 07 应并行准备：没有可安装的产物就无法在 root 环境做 07。

## 7. 未决事项

1. **root 测试环境。** §4.1 列出的路径必须在有 root 的 VM 或支持 systemd 的容器（`podman run --systemd=always`）中验证。没有该环境，认证与提权链路无法离开「代码看起来正确」的状态。
2. **`/debug` 页在浏览器中的实际表现。** 首次打开后如有问题，优先怀疑 §4.2。
3. **commit。** 当前全部代码未提交。建议 01 开始前先提交一次。
