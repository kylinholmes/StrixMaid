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
> **已在真实 root 环境跑通（2026-08-29）**，见下方「结果」列。另加了
> `run-in-docker.sh`（只有 docker 的机器用）。**人工项**（§2 浏览器、§3 长时
> 运行、§4 release 性能、§1.2 的空闲超时 / faillock）工装不覆盖，见 README 矩阵。

## 0. 结果说明（2026-08-29）

在两个 systemd 容器里各跑一遍，产物取自 CI 的 `strixmaid-dist-x86_64`
（musl 静态的 `strixmaid` / `strixmaid-agent` + glibc 2.28 基线的 helper）：

| 记号 | 环境 |
|---|---|
| **U** | `ubuntu:24.04` 容器 |
| **R** | `rockylinux:9` 容器 |

现已进 CI（`ci.yml` 的 `verify` job，两发行版矩阵，每次 PR），分期方案见
`09-ci-verification.md`。**「结果」列里 U 与 R 不同的地方都是真实的发行版差异**，
不是偶发——它们正是这份矩阵存在的理由。

本轮由此暴露并修复的产品缺陷：**`/logs` 在 RHEL 系上返回 500**（§1.2 #5）。
`journalctl` 说 `insufficient permissions`，映射没认出这个措辞，落进了 `internal`。
Ubuntu 上永远复现不了——那里 journald 的 ACL 让用户至少打得开自己的日志，返回 200。

## 1. root 环境

### 1.1 环境

任一即可：

- VM：Ubuntu 24.04 或 Rocky 9，有 root，可装 `06-packaging.md` 的产物；
- 容器：`podman run -d --systemd=always --name strix-test docker.io/library/ubuntu:24.04 /sbin/init`（需要 `systemd` 包与 `polkitd`），以 `--privileged` 或至少 `--cap-add SYS_ADMIN`；容器内 `loginctl` 需要 `systemd-logind` 可运行，`pam_systemd` 才能建会话。

准备两个用户：`alice`（普通，不在 `sudo` / `adm` / `systemd-journal` 组）、`bob`（在 `sudo` 组）。

### 1.2 步骤与预期

| # | 操作 | 预期 | 结果 |
|---|---|---|---|
| 1 | `systemctl start strixmaid`；`GET /capabilities` | `helper = true`、`polkit = true` | **U ✓ R ✓** |
| 2 | 以 `alice` 登录（`/auth/start` → `/auth/respond`） | 200 `status = complete`；`ps -o user,cmd -C strixmaid` 出现一个 `alice` 的 `strixmaid worker`；`loginctl list-sessions` 出现 alice 的会话（`pam_open_session` 成功） | **U ✓ R ✓** — helper 以 root `setuid` 到 alice 成功，logind 会话建起 |
| 3 | `GET /auth/session` | `session_opened = true` | **U ✓ R ✓** |
| 4 | `GET /capabilities` | `user.can_read_journal = false`、`can_elevate = false`、`elevated = false` | **U ✓ R ✓** |
| 5 | `GET /logs?limit=50` | 能读到系统日志时 200；一个 journal 文件都打不开时 403 + `can_retry_elevated`（**不是 500**） | **U ✓** 200（只含自己的条目）；**R ✓** 403 + 可提权重试。**修复前 R 是 500** —— `insufficient permissions` 被归成 `internal`，见 §0 |
| 6 | `GET /processes/<worker pid>` | `uid` 为 alice；`cgroup` 在 `user.slice/user-<uid>.slice/session-*.scope` 下 | **U ✓ R ✓**（cgroup 形状属人工核对） |
| 7 | `POST /services/<测试 unit>/action { restart }` | 403 且带 `can_retry_elevated`。**码是 `permission_denied` 而非 `elevation_required`**：服务操作走 `exec::call_escalating_from`，先让 polkit 裁决并返回它的真实理由；`elevation_required` 是 `Privilege::Admin` 那类路由在「压根没有 admin worker」时的回答 | **U ✓ R ✓** `permission_denied` + `Interactive authentication required.` |
| 8 | `POST /auth/elevate/start` | 403 `permission_denied`，helper 进程数不增加（`01` §4.8） | **U ✓ R ✓**。注意 `pgrep -x strixmaid-helper` **永远匹配不到**：Linux 的 `comm` 只有 15 字符，实际是 `strixmaid-helpe` |
| 9 | `GET /services?scope=user` | 列出 alice 的用户级 unit；session bus 连不上时 503 `unavailable`（**不是 501**——501 是「本机没装」，页面该隐藏；503 是暂时不可用，页面不该隐藏） | **R ✓** 200；**U** 503 —— 容器里没起 `user@.service`，属环境。VM 上应为 200 |
| 10 | `POST /auth/logout` | worker 与 helper 进程退出；`loginctl` 中会话消失 | **U ✓ R ✓** |
| 11 | 以 `bob` 登录后 `POST /auth/elevate/start` → `respond` | 200；`ps` 出现 uid 0 的第二个 worker | **U ✓ R ✓** — 实测已提权会话共 **2 个 helper**（会话一个、提权一个）+ 2 个 worker |
| 12 | `GET /capabilities` | `elevated = true`、`can_manage_units = true`、`can_read_journal = true` | **U ✓ R ✓**。`can_read_journal` 靠**升级到 admin worker** 成立（`exec::escalate`），与「bob 以自己身份读不读得到」不是一回事 |
| 13 | `POST /services/<测试 unit>/action { restart }` | 200，返回 `job`；`systemctl status` 显示刚重启 | **U ✓ R ✓** |
| 14 | `GET /logs?limit=50` | 含系统日志 | **U ✓ R ✓** 200（内容属人工核对） |
| 15 | 等待 `elevated_idle_timeout_secs`（默认 300 s）不做任何管理操作 | admin worker 退出；`GET /auth/session` 的 `elevated = false`；user worker 仍在 | **未测** — 耗时，`LONG=1`；宜按 `09` P2 做成定时任务 |
| 16 | 等待 `idle_timeout_secs`（默认 900 s） | 会话失效，下一请求 401；全部进程退出 | **未测** — 同上 |
| 17 | 连续登录失败 5 次 | 每次约 2 s（`pam_unix` 失败延迟）；`faillock`（RHEL）或等效机制生效时第 6 次被锁定，响应为 401 且 detail 含锁定信息 | **未测** — 依赖发行版 faillock 配置，人工 |
| 18 | `PUT /system/hostname`（bob 已提权） | 200；`hostnamectl` 显示新值；`/etc/hostname` 已改 | **U ✓ R ✓** |
| 19 | `kill -9` 主进程后重启 | `sessions` 表为空；旧 token 401；无残留 worker / helper | **U ✓ R ✓** 三项全中 |
| 20 | 以 `alice` 登录 20 次不登出 | 20 对 helper + worker；RSS 增长线性（记录每会话开销）；`idle_timeout` 后全部回收 | **未测** — 属容量观测，人工或长测 |

### 1.3 pam.d 模板

在 Ubuntu 24.04 与 Rocky 9 上分别用对应模板完成 1.2 的 #2、#11；记录 PAM 栈输出中的 warning。helper 的 stderr 继承自主进程，journald 按 unit 归类：`journalctl -u strixmaid | grep 'strixmaid-helper\['`。

**结果（2026-08-29）：两份模板都通过。** `install.sh` 按 `/etc/os-release` 选模板
（U → `strixmaid.debian`，R → `strixmaid.rhel`），#2 与 #11 在两个环境都成功，
`pam_open_session` 均返回成功并建起 logind 会话：

```
strixmaid-helper[620]: 开始 PAM 认证，service=strixmaid
strixmaid-helper[620]: PAM 需要 1 项输入（本轮共 1 条消息），转发主进程
strixmaid-helper[620]: 认证通过，uid=1002
pam_unix(strixmaid:session): session opened for user bob(uid=1002) by bob(uid=0)
strixmaid-helper[620]: pam_open_session 成功
```

**warning 的人工核对尚未做**：上面只确认了「能通过」，没有逐条看 PAM 栈有没有吐
warning（比如缺模块、次序可疑）。这条仍算未完成。

## 2. 浏览器

> **未测（2026-08-29）**：需要无头浏览器驱动真实渲染，且正式前端框架未定
> （`HANDOFF-2026-08-28.md` §2.2 —— 属架构决定）。方案见 `09-ci-verification.md` P4。

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

> **未测（2026-08-29）**：这一节要的是时间不是算力，不适合放进 CI。
> 方案见 `09-ci-verification.md` P3 —— 起一台常驻实例定期抓取，产出是一条曲线。

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

> **未测（2026-08-29）**：GitHub runner 的性能批次间波动大，在那种机器上定基线
> 只会制造噪声告警（`09-ci-verification.md` P5）。应在固定硬件上跑。
> 另注意：07 验收用的发布物由 CI 按**默认 release profile**（fat LTO）构建，
> 本节计时可直接用同一份产物。

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
| 1 | 密码不进日志 | `RUST_LOG=trace` 下登录一次，`grep` 密码明文 | 0 处 | **U ✓ R ✓**（0 处）。**但工装的 unit 用 `RUST_LOG=info`，这只是弱检验**——严格版要 `trace`，未做 |
| 2 | 密码不入库 | `strings strixmaid.db \| grep` 密码 | 0 处 | **R ✓**（alice / bob 两个密码各 0 处）；**U 未测** —— `ubuntu:24.04` 里没有 `strings` |
| 3 | token 只存 hash | `SELECT id FROM sessions` 与响应中的 token 比较 | 不相等，`id` 为 64 位 hex | **U ✓ R ✓** |
| 4 | worker uid 校验 | 修改测试用 helper 使 `Hello.uid` 与声称不符 | 主进程拒绝并 kill worker | **未测** — 需篡改过的 helper；core 单测已覆盖，工装不重复 |
| 5 | helper 的 fd 3 | 直接从终端运行 `strixmaid-helper` | 立即退出并报「fd 3 不是 socket」 | **U ✓ R ✓** |
| 6 | WS 无 token | 见 `gap-analysis.md` 已验证 | 401 | **U ✓ R ✓** |
| 7 | 反代头伪造 | 无 `trusted_proxies` 时带 `X-Forwarded-For` | 审计中 `remote_addr` 为直连地址 | **U ✓ R ✓** 审计记的是 `127.0.0.1:<port>`，未采信伪造头 |
