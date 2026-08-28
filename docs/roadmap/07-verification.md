# 07 验证清单

本文件列出当前代码已实现但未在目标环境验证的部分（`gap-analysis.md` §4），给出环境要求、步骤与预期结果。每项验证的结果应回写到本文件的「结果」列。

> **验证工装（2026-08-28）**：本清单能自动化的部分已做成脚本，在 `scripts/verify/`：
>
> - `run-in-podman.sh` —— 一键：起 systemd 容器（polkit/PAM/sudo + alice/bob +
>   一个只睡觉的 `strixtest.service`）→ `install.sh` 装 → 起服务 → 跑下面两个
>   检查 → 拆。宿主用 rootless podman 即可，root 全在容器内。
> - `root-checks.sh` —— §1.2 的 #1–#14、#18、#19 与 §5 的 #1–#3、#5–#7，全自动
>   打 ok/bad/skip 并给出退出码。
> - `agent-checks.sh` —— 05 §5.2 的双进程 Agent 测试（登记 / 上线 / 补发 /
>   重连无空洞）。
> - `login.sh` —— 无人值守登录 / 提权助手（密码走环境变量，**仅限一次性测试容器**）。
> - `README.md` —— 运行方式与**覆盖矩阵**（哪些自动、哪些人工 / 长测）。
>
> **这套工装尚未在真实 root 环境跑通**（开发机无 root，也无法保证 rootless
> systemd 容器）。脚本已 `bash -n` 全部通过；首次在 VM / 容器里运行时按各条
> ok/bad 的具体消息微调（unit 名、发行版差异）。**人工项**（§2 浏览器、§3 长时
> 运行、§4 release 性能、§1.2 的空闲超时 / faillock）工装不覆盖，见 README 矩阵。
>
> 下面「结果」列在真实环境跑过后回填。

## 1. root 环境

### 1.1 环境

任一即可：

- VM：Ubuntu 24.04 或 Rocky 9，有 root，可装 `06-packaging.md` 的产物；
- 容器：`podman run -d --systemd=always --name strix-test docker.io/library/ubuntu:24.04 /sbin/init`（需要 `systemd` 包与 `polkitd`），以 `--privileged` 或至少 `--cap-add SYS_ADMIN`；容器内 `loginctl` 需要 `systemd-logind` 可运行，`pam_systemd` 才能建会话。

准备两个用户：`alice`（普通，不在 `sudo` / `adm` / `systemd-journal` 组）、`bob`（在 `sudo` 组）。

### 1.2 步骤与预期

| # | 操作 | 预期 | 结果 |
|---|---|---|---|
| 1 | `systemctl start strixmaid`；`GET /capabilities` | `helper = true`、`polkit = true` | |
| 2 | 以 `alice` 登录（`/auth/start` → `/auth/respond`） | 200 `status = complete`；`ps -o user,cmd -C strixmaid` 出现一个 `alice` 的 `strixmaid worker`；`loginctl list-sessions` 出现 alice 的会话（`pam_open_session` 成功） | |
| 3 | `GET /auth/session` | `session_opened = true` | |
| 4 | `GET /capabilities` | `user.can_read_journal = false`、`can_elevate = false`、`elevated = false` | |
| 5 | `GET /logs?limit=50` | 只含 alice 自己的日志（`_UID = alice 的 uid`），无 `sshd` / `kernel` 条目 | |
| 6 | `GET /processes/<worker pid>` | `uid` 为 alice；`cgroup` 在 `user.slice/user-<uid>.slice/session-*.scope` 下 | |
| 7 | `POST /services/cron.service/action { restart }` | 403 `elevation_required`（未提权） | |
| 8 | `POST /auth/elevate/start` | 403 `permission_denied`，`pgrep -c strixmaid-helper` 不增加（依赖 `01` §4.8；修正前当前代码会进入 PAM 对话并在重输密码后给出 root worker，属已知缺陷） | |
| 9 | `GET /services?scope=user` | 列出 alice 的用户级 unit（至少 `dbus.service` 等默认项） | |
| 10 | `POST /auth/logout` | worker 与 helper 进程退出；`loginctl` 中会话消失；`pgrep -c strixmaid-helper` 为 0 | |
| 11 | 以 `bob` 登录后 `POST /auth/elevate/start` → `respond` | 200；`ps` 出现 uid 0 的第二个 worker | |
| 12 | `GET /capabilities` | `elevated = true`、`can_manage_units = true`、`can_read_journal = true` | |
| 13 | `POST /services/cron.service/action { restart }` | 200，返回 `job`；`systemctl status cron` 显示刚重启 | |
| 14 | `GET /logs?limit=50` | 含系统日志 | |
| 15 | 等待 `elevated_idle_timeout_secs`（默认 300 s）不做任何管理操作 | admin worker 退出；`GET /auth/session` 的 `elevated = false`；user worker 仍在 | |
| 16 | 等待 `idle_timeout_secs`（默认 900 s） | 会话失效，下一请求 401；全部进程退出 | |
| 17 | 连续登录失败 5 次 | 每次约 2 s（`pam_unix` 失败延迟）；`faillock`（RHEL）或等效机制生效时第 6 次被锁定，响应为 401 且 detail 含锁定信息 | |
| 18 | `PUT /system/hostname`（bob 已提权） | 200；`hostnamectl` 显示新值；`/etc/hostname` 已改 | |
| 19 | `kill -9` 主进程后重启 | `sessions` 表为空；旧 token 401；无残留 worker / helper | |
| 20 | 以 `alice` 登录 20 次不登出 | 20 对 helper + worker；RSS 增长线性（记录每会话开销）；`idle_timeout` 后全部回收 | |

### 1.3 pam.d 模板

在 Ubuntu 24.04 与 Rocky 9 上分别用对应模板完成 1.2 的 #2、#11；记录 PAM 栈输出中的 warning。helper 的 stderr 继承自主进程，journald 按 unit 归类：`journalctl -u strixmaid | grep 'strixmaid-helper\['`。

## 2. 浏览器

环境：任一现代浏览器，SSH 隧道到 9700。

| # | 检查 | 预期 | 结果 |
|---|---|---|---|
| 1 | `/api/docs` | Scalar 渲染，左侧 29 个端点；「Try it」对 `/health` 成功；开发者工具 Network 面板无外部域名请求 | |
| 2 | `/` | 302 到 `/debug` | |
| 3 | `/debug` 登录 | prompts 渲染为密码框；错误密码显示 401 detail；正确密码后顶部显示用户名，`sessionStorage` 有 token | |
| 4 | 指标面板 | 勾选 `cpu.usage` 后 uPlot 显示 band（min–max 区间）、avg 实线、med 虚线；切换 1h / 1d 曲线刷新 | |
| 5 | 实时开关 | Network 面板中 `/ws` 握手请求头含 `Sec-WebSocket-Protocol: bearer, …`，响应头含 `Sec-WebSocket-Protocol: bearer`；每 2 s 收到 `metrics.live` 帧 | |
| 6 | 关闭实时后等待 60 s | WS 控制台只出现一对 ping | |
| 7 | 进程面板 | 3000 进程渲染不卡顿；排序、搜索、tree 开关有效 | |
| 8 | 服务面板 | 点击行展开详情，`cgroup.memory_current` 有值；对 `ssh.service` restart 显示 403 与「需要管理访问」 | |
| 9 | 日志面板 | 翻页、follow 开关（`logger test` 后 1 s 内出现） | |
| 10 | 深色 / 浅色 | 跟随系统切换 | |

## 3. 长时间运行

环境：release 构建，`Normal` 保留期，本机 128 核或任一 16 核以上机器。运行 ≥ 8 天。

| # | 指标 | 采集方式 | 预期 | 结果 |
|---|---|---|---|---|
| 1 | RSS | 每小时 `ps -o rss -C strixmaid` | 稳定，波动 < 10%；16 核机器 < 20 MiB + 环形缓冲 | |
| 2 | 数据库体积 | 每天 `du -b strixmaid.db` | 第 1 天后增长率下降；第 8 天 ≈ `design.md` §7.4 估算的 Normal 值 ± 30% | |
| 3 | `m_1m` 行数 | `SELECT COUNT(*) FROM m_1m` | 第 2 天起稳定在 `series 数 × 1440` 附近（保留 1 天） | |
| 4 | 聚合正确性 | 任取一个 series、一个整点，`m_5m` 的 `sum/cnt` 与对应 5 行 `m_1m` 加权平均一致；`min`/`max` 一致 | 完全一致 | |
| 5 | `m_1d` 生成 | 第 2 天 `SELECT COUNT(*) FROM m_1d WHERE ts = <昨天 0 点>` | 等于 series 数 | |
| 6 | 采集失败 | `grep WARN` 日志 | 无持续性 warning | |
| 7 | 时钟回拨 | 第 3 天手动 `date -s` 回拨 10 分钟 | 采集不中断；`m_1m` 无负时间差；恢复后无重复桶 | |

## 4. release 性能

环境：`cargo build --release`，本机。

| # | 操作 | 预期 | 结果 |
|---|---|---|---|
| 1 | `GET /processes`（3000 进程，经 worker） | < 80 ms | |
| 2 | `GET /services` | 与 `busctl call … ListUnitFiles` 单独耗时之差 < 50 ms | |
| 3 | `GET /logs?limit=100` | < 50 ms | |
| 4 | `GET /metrics/query` 7 天、1 个 series | < 30 ms | |
| 5 | 一轮采集（`engine.last_round()`） | 16 核 < 3 ms；128 核 < 10 ms | |
| 6 | 空闲 CPU（无登录、无 WS） | `top` 中 < 0.5% | |

## 5. 安全检查

| # | 检查 | 方法 | 预期 | 结果 |
|---|---|---|---|---|
| 1 | 密码不进日志 | `RUST_LOG=trace` 下登录一次，`grep` 密码明文 | 0 处 | |
| 2 | 密码不入库 | `strings strixmaid.db \| grep` 密码 | 0 处 | |
| 3 | token 只存 hash | `SELECT id FROM sessions` 与响应中的 token 比较 | 不相等，`id` 为 64 位 hex | |
| 4 | worker uid 校验 | 修改测试用 helper 使 `Hello.uid` 与声称不符 | 主进程拒绝并 kill worker | |
| 5 | helper 的 fd 3 | 直接从终端运行 `strixmaid-helper` | 立即退出并报「fd 3 不是 socket」 | |
| 6 | WS 无 token | 见 `gap-analysis.md` 已验证 | 401 | 已通过 |
| 7 | 反代头伪造 | 无 `trusted_proxies` 时带 `X-Forwarded-For` | 审计中 `remote_addr` 为直连地址 | |
