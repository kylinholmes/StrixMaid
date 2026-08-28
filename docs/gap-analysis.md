# StrixMaid 现状与 MVP 目标的差距

> 编写日期：2026-08-27，末次更新 2026-08-28。
> **2026-08-28 现状**：roadmap 01–06 完成、07 验证工装完成（未在 root 实跑）、08 采集侧 + §5 拓扑完成；
> `cargo test --workspace` **463 通过**、clippy 零 warning（含无 UI 变体）；改动约 70 文件**未提交**。
> 逐里程碑状态见下表 §3；**给接手 AI 的总交接见 [`HANDOFF-2026-08-28.md`](./HANDOFF-2026-08-28.md)**。
>
> 原始编写日期：2026-08-27。对应代码状态：Phase 0–3 接线完成，`cargo test --workspace` 273 通过，clippy 零 warning，共 28,147 行 Rust。
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

## 2. 结构性缺口：授权模型未闭环 —— 已闭合（2026-08-28）

> `roadmap/01-worker-execution.md` 已实现，本节描述的是**改造前**的状态，保留作为背景。
> 现状：全部请求经 `auth::exec::call` 派给该会话的 worker，读走 user worker、
> 写走 admin worker（未提权返回 403）；`logs.follow` 经 worker 订阅，可见范围随用户；
> 服务端不再含任何授权判断代码。详见 `roadmap/01` §9「完成状态」。


`design.md` §5 的核心是「以登录用户身份执行操作，由 PAM / polkit / journald ACL / 文件权限裁决」。当前实现只做到了前半段：

- 认证正确地映射到系统用户，helper 也确实以该用户 fork 了 worker；
- 但 `crates/strixmaid-server/src/routes/` 下的全部处理器都在**主进程内直接调用 provider**，没有任何一条路径经过 worker。`grep -rn 'user_worker\|admin_worker' crates/strixmaid-server/src/routes/` 为 0 处。
- worker 的 RPC 分发表（`core/src/worker/mod.rs` 的 `Dispatcher`）只注册了 `ping` 与 `whoami`。

后果：一旦主进程以 root 部署，任何已登录用户的请求都以 root 执行。polkit 看不到该用户，journald ACL 与文件权限全部被绕过，§5 的授权模型形同虚设。当前开发实例未暴露此问题，仅因为它以非 root 身份运行。

该缺口同时阻塞终端（PTY 必须在 worker 内 fork）、`scope=user` 的跨用户支持（须在目标用户的 worker 内连 session bus）、以及日志可见性的正确性。方案见 `roadmap/01-worker-execution.md`。

### 2.1 提权缺少授权检查 —— 已修复（2026-08-28）

原状：`elevate_start` 不检查资格，helper 的 `SpawnWorker { as_root: true }` 只判断
helper 自身是否为 root，`can_elevate` 只是展示字段。后果是 root 部署下任何能通过
PAM 登录的用户重输密码即可获得 root worker。

已按 `roadmap/01-worker-execution.md` §4.8 实现：

- 新增配置 `session.elevate_groups`，默认 `["sudo", "wheel", "admin"]`（覆盖
  Debian / RHEL·Arch / macOS 三种惯例）。**空列表表示禁止任何人提权**，是合法配置。
- **权威判断在 helper 内**：`SpawnWorker { as_root: true }` 时用 `AuthOk` 阶段
  由 NSS 查出的组判断，不满足直接返回 `Error`、**不 fork**。允许列表经 `AuthStart`
  下发，helper 不读配置文件（它是 setuid 组件，少一条读文件路径就少一处风险）。
- `elevate_start` 提前拒绝：不 spawn 第二个 helper、不进入 PAM 对话，403 并说明所需的组。
- `can_elevate` 改用同一份配置与**同一个函数**（`strixmaid_types::auth::may_elevate`），
  杜绝「前端显示按钮、helper 拒绝」的不一致；有专门用例穷举组合比对两者。

注意：这只闭合了「谁能成为 root」。§2 那个更大的缺口——请求仍在主进程内执行、
根本没经过 worker——**依然存在**，仍需 `roadmap/01` 的主体部分。

## 3. P0 范围内尚未完成的项

| # | 项 | 现状 | 方案文件 |
|---|---|---|---|
| ~~1~~ | ~~请求经 worker 执行~~ | **已完成**（2026-08-28），见 §2 与 `01` §9 | `01-worker-execution.md` |
| ~~2~~ | ~~审计日志~~ | **已完成**（2026-08-28）：写入点设在 `exec` 的调用出口与认证路由，`GET /audit` 需管理访问，保留期每小时清理，`/debug` 有审计面板 | `02-audit.md` §8 |
| ~~3~~ | ~~终端~~ | **主体已完成**（2026-08-28）：PTY 在 worker 内，附着 / 回看 / resize / 空闲回收齐备；`{"t":"exit"}` 帧带真实退出码；空闲、shell 自退、登出三种关闭经观察者写审计。剩余为浏览器实测与 root 环境验收，见 `HANDOFF.md` | `03-terminal.md` |
| ~~4~~ | ~~文件管理壳~~ | **已完成**（2026-08-28）：fs provider 在 user worker 内执行，`allowed_roots` 随调用下发，两个只读路由 + `/debug` 文件面板 | `04-files-and-ws-channels.md` |
| ~~5~~ | ~~WS 频道 `system.health`、`processes.live`~~ | **已完成**（2026-08-28）：health 主进程 30s 重算并并入 failed units、变更才广播；processes.live 经会话 worker 的流式 RPC | `04-files-and-ws-channels.md` |
| ~~6~~ | ~~`strixmaid-agent`~~ | **已完成**（2026-08-28）：本地采集落盘 + WS 推送 + `(ts,series)` 键集补发 + `/ws/agent` 汇聚与 `/nodes` 管理；TLS 留待 06，见 `05-agent.md` 头部状态块 | `05-agent.md` |
| ~~7~~ | ~~打包~~ | **仓库侧已完成**（2026-08-28）：`.cargo` 配置、`ui` feature、service/install.sh/package.sh、CI（musl 构建与体积断言在 CI 跑——本机无 musl 工具链），见 `06-packaging.md` 头部状态块 | `06-packaging.md` |
| ~~1a~~ | ~~提权授权检查~~ | **已完成**（2026-08-28）：`session.elevate_groups` 配置项，helper 内权威判断 + `elevate_start` 提前拒绝 + `can_elevate` 同源，见 §2.1 | `01-worker-execution.md` §4.8 |
| ~~8~~ | ~~`scope=user` 跨用户~~ | **随 #1 一并解决**：worker 以登录用户身份运行，`scope=user` 连的就是它自己的 session bus，不需要额外机制 | `01-worker-execution.md` §4.2 |
| ~~9~~ | ~~capability `user` 层的实测探测~~ | **已完成**：`caps.probe_user` 在 user worker 内实测，结果覆盖推导值，按「会话+提权状态」缓存 60 秒 | `01-worker-execution.md` §4.6 |

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

验证方案见 `roadmap/07-verification.md`；可自动化的部分已做成工装 `scripts/verify/`（一键 podman + systemd 容器跑 §1.2 / §5 / 05 §5.2，尚未在真实 root 环境执行）。

## 4a. 剩余 gap 一览（2026-08-28，给接手者）

**后端 P0 无已知缺口。** 剩下两件都非本机能做的编码：

1. **07 验证实跑**（最高优先，决定上线信心）——授权脊椎从没在 root 下跑过。工装 `scripts/verify/` 已备，换 root 的 VM/容器一条命令跑，回填 §07 结果列。
2. **正式前端**（产品可用性）——框架待项目负责人定（别擅自选栈）。后端接口齐全；`/debug` 有原生实时性能面板可参考；样稿 `roadmap/08 §6–§8` + `.mockup.html` 是设计真相。`/perf` 页是一次实时化尝试但**浏览器里空白、未修**，建议照样稿在正式框架重写。

已界定**不做**（design.md §14）：TLS（反代）、告警、虚拟机/SELinux/高级存储/插件、NVIDIA 利用率（Q1(c)）、非 Linux 作管理端。

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

## 7. 开发平台

工程现在同时能在 macOS 上编译、测试、运行（三个二进制齐全，测试全绿，clippy 零 warning，
四个 provider 全部探测可用）。这**不改变本文的任何结论**——授权模型缺口、P0 未完成项、
未验证项在两个平台上完全相同。详见 [`macos-dev-platform.md`](./macos-dev-platform.md)。

顺带修掉的一个历史问题：`.gitignore` 里 Cargo 模板自带的裸 `debug` 规则在任意层级生效，
把 `crates/strixmaid-server/src/debug/`（§12.1 的调试页）整个吞掉了，导致该目录**从未进入
版本库**，而 `main.rs` 仍声明 `mod debug;`——**仓库此前在任何平台的 debug 构建都编译不过**。
规则已改为锚定的 `/target/` 与 `/debug/`，调试页已按 §12.1 重写（九个面板、vendored uPlot、
每面板独立容错）。API 的可复现验证另有 `scripts/api-smoke.sh`，对照 openapi.json 逐端点跑。

## 8. 未决事项

1. **root 测试环境。** §4.1 列出的路径必须在有 root 的 VM 或支持 systemd 的容器（`podman run --systemd=always`）中验证。没有该环境，认证与提权链路无法离开「代码看起来正确」的状态。
2. **`/debug` 页在浏览器中的实际表现。** 首次打开后如有问题，优先怀疑 §4.2。
3. **`/debug` 调试页的认证后面板未在真实浏览器里验证过**。页面已按 §12.1 重写并在
   headless 浏览器里确认「加载、uPlot 就位、九个面板各自独立容错、未认证数据正确渲染」，
   但登录后的指标 band 图与各数据表格需要真实密码，只能由开发者本人打开浏览器确认。
