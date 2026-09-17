# StrixMaid 设计文档

> 轻量、通用、现代化的服务器观测与管理平台。
> 定位参照 Cockpit（见 `cockpit-feature-inventory.md`），但去除其对 DBus 系统服务的重度依赖、补上历史指标、重做多节点模型。
> 本文档为第 1 版设计基线，全部条目均已逐项确认。

---

## 1. 核心原则

1. **除 systemd（及其自带的 journalctl）外，默认假设系统里什么都没有。** 信息优先从 `/proc`、`/sys`、netlink、`/etc` 直读，不依赖 udisks2、NetworkManager、PackageKit、firewalld、libvirt、PCP。
2. **能力探测而非硬依赖。** 每个 provider 启动时 `probe()`，探测不到的能力在 API 中标为 unavailable，前端隐藏对应页面——优雅降级，而非报错。
3. **授权外包给操作系统。** 以登录用户身份执行操作，由 PAM / polkit / journald ACL / 文件权限裁决。不自建 RBAC。
4. **静态单二进制优先。** 需要动态链接的部分全部隔离进 `strixmaid-helper`。
5. **AgentCore 是唯一的业务逻辑所在地。** Server 与 Agent 都只是它的宿主。

> **第 1 条在 Windows 上的等价写法**：那里没有 `/proc`、`/sys`、netlink，
> 「直读」对应的入口是 `NtQuerySystemInformation`（进程与处理器）、注册表
> （静态描述信息）、IOCTL（块设备）、IP Helper（网络）、SCM（服务）、
> 事件日志 API（日志）。
>
> 判据与第 1 条同源：**不依赖需要额外守护进程的中间层**。因此
> **一律不走 WMI**——它要起 COM、连 WMI 服务，负载高时可能几秒才应答，
> 一个每 2 秒采集一轮的指标管线不能赌这个；**除 GPU 外也不走 PDH**（性能计数器），
> 它要两轮采集、每轮走一遍计数器名解析，而 CPU / 内存 / 磁盘 / 网络都有更直接、
> 更便宜、且与内核数据结构同源的入口。GPU 是唯一没有别的路的那一项。
>
> 第 2 条在 Windows 上格外吃重：拿不到的东西比 Linux 多，逐条清单见
> [`windows-platform.md`](./windows-platform.md) §4。

---

## 2. 产物与进程模型

### 2.1 三个产物

| 产物 | 链接方式 | 内容 |
|---|---|---|
| `strixmaid` | 静态 musl | UI + AgentCore + Server 与 Agent 两种模式 + worker 模式 |
| `strixmaid-helper` | 动态 glibc | PAM 认证、setuid fork、NSS 代理 |

`worker` 不是独立二进制，而是主二进制的子命令（`strixmaid worker`）。helper 只负责「PAM 认证 → setuid → exec strixmaid worker」，因此可以做到极小。

> **第一个交付目标是 Linux**，上面这张表说的就是它。macOS 与 Windows 后来各自
> 成为交付目标，形态见下面两节补充。每个 provider 都补了一套 macOS 原生实现
> （launchd / 统一日志 / libproc / mach），覆盖差异与平台 API 的坑逐条记在
> [`macos-platform.md`](./macos-platform.md)。Linux 实现的内容未因此改动一个字节。

#### Windows 补充（2026-09）

**Windows 后来成为第二个交付目标**：有人会拿它跑生产，缺一项就少一块视野，
因此它享受与 Linux 相同的质量门槛（CI 里 `cargo clippy --workspace --all-targets
-- -D warnings` 与 `cargo test --workspace` 各跑一遍），取舍标准仍是 §1 的第 2 条。
完整说明见 [`windows-platform.md`](./windows-platform.md)。

三个产物在 Windows 上的形态：

| 产物 | 链接方式 | 与 Linux 的差别 |
|---|---|---|
| `strixmaid.exe` | MSVC 动态（系统 DLL） | 多一组 `service` 子命令：注册 / 注销 / 启停 / 查询，以及被 SCM 拉起的 `service run` 入口 |
| `strixmaid-agent.exe` | 同上 | 暂不随发布包分发——还没有被 SCM 托管的入口 |
| `strixmaid-helper.exe` | 同上 | 没有 PAM：认证走 `LogonUserW`，会话走 `LoadUserProfileW`，身份切换走 `CreateProcessAsUserW` |

「静态单二进制优先」（§1 第 4 条）在 Windows 上表现为另一种形式：不依赖任何需要单独
安装的运行时，全部走系统 DLL。helper 之所以仍然独立存在，理由从「隔离动态链接」变成了
「持有登录令牌与用户配置文件直到登出」——见 §5.4 的 Windows 补充。

`worker` 同样不是独立二进制，而是 `strixmaid.exe` 的子命令。这一点对 Windows 的
安装脚本有直接影响：worker 以**登录用户**的身份运行，所以安装目录必须给 Users
读 + 执行权限，否则登录之后什么都干不了。

Linux 与 macOS 实现的内容未因此改动一个字节。

#### macOS 补充（2026-09）

**macOS 也提升为交付目标**，与 Windows 同级：有 launchd 服务定义、安装与卸载脚本、
发布包与 CI 门槛。此前它只是开发平台（「取不到的数据可以整项不做」），provider 那一层
的实现未变，变的是「缺项要说清楚是平台没有还是还没做」以及多了一整套交付物。
完整说明见 [`macos-platform.md`](./macos-platform.md)。

三个产物在 macOS 上的形态：

| 产物 | 链接方式 | 与 Linux 的差别 |
|---|---|---|
| `strixmaid` | 系统 dylib（libSystem 等） | 无额外子命令；服务宿主是 launchd，不需要 Windows 那种 `service` 子命令族 |
| `strixmaid-agent` | 同上 | 随发布包分发 |
| `strixmaid-helper` | 同上，外加 `/usr/lib/libpam.2.dylib` | PAM 是 **OpenPAM**，常量数值与 Linux-PAM 不同；服务文件缺失时整体回退到全拒的 `/etc/pam.d/other`，所以模板必须装 |

两条边界写在这里，免得下游误以为漏了：

* **只出 Apple Silicon**（`aarch64-apple-darwin`），不做 Intel，不做 universal。
* **暂不做代码签名与公证**。后果是浏览器下载的包会被 Gatekeeper 打上隔离标记，
  用户需要 `xattr -dr com.apple.quarantine` 或在系统设置里放行一次
  （`macos-platform.md` §7.5）。这是已知取舍，不是缺陷。

「静态单二进制优先」（§1 第 4 条）在 macOS 上与 Windows 同理：Apple 不提供静态的
libSystem，所以标准同样改成「不依赖任何需要另外安装的运行时」。

### 2.2 进程拓扑

```
strixmaid                (root, 常驻)
  │   HTTP/WS · 前端资源 · 全局指标采集 · 存储与聚合 · 会话路由
  └─ strixmaid-helper    (root, 按需 spawn)
       │   PAM 会话 · setuid · exec worker
       └─ worker         (uid = 登录用户, 每会话一个)
            ├─ system bus 连接      → polkit 按该用户裁决
            ├─ PTY 会话
            └─ 文件操作 / 进程信号
```

**全局指标采集留在主进程（root）**——它与登录用户无关，且必须在无人登录时持续运行。

一个会话最多两个 worker：`user worker`（登录即建，uid = 登录用户）与 `admin worker`（提权后才建，uid = 0）。API 层按操作类型路由；未提权的写操作返回 403 + 需要管理访问。

#### Windows 补充：同一张拓扑，换一套身份

```
strixmaid                (服务进程, SCM 拉起, 身份 = LocalSystem)
  │   HTTP/WS · 前端资源 · 全局指标采集 · 存储与聚合 · 会话路由
  └─ strixmaid-helper    (同一令牌, 按需 spawn)
       │   LogonUserW · LoadUserProfileW · CreateProcessAsUserW
       └─ worker         (身份 = 登录用户的令牌, 每会话一个)
            ├─ ConPTY 终端 + 作业对象
            └─ 文件操作 / 进程终止
```

逐级对应：

| 这一级 | Linux | Windows |
|---|---|---|
| 主进程 | uid 0 | 服务账户，默认 `LocalSystem`（`S-1-5-18`，映射成 uid 0） |
| helper | uid 0，`fork` + `exec` | 与主进程同一令牌，`CreateProcessW` |
| user worker | `setuid` 到登录用户 | `LogonUserW` 拿到的用户令牌 + `CreateProcessAsUserW` |
| admin worker | 不切换身份，保持 uid 0 | **UAC 的 linked token**（完整管理员令牌） |
| 会话 bus | system bus，polkit 按该用户裁决 | 无对应物——授权由令牌里的组与特权直接裁决 |

三处语义差异值得记在设计层：

1. **提权是换一张令牌，不是保持身份**。Windows 没有「有效用户」这一层，一个进程的
   令牌就是它的全部身份，因此 `uid` 恒等于 `euid`。提权在这里是另一个令牌、进而是
   另一个进程。
2. **helper 的生命周期理由换了，结论没换**。Linux 上它必须活到登出，是因为
   `pam_open_session` / `pam_close_session` 要在同一进程同一句柄上成对调用；
   Windows 上是因为 `LoadUserProfileW` / `UnloadUserProfile` 同样要成对，
   且登录令牌需要有人一直持有。
3. **polkit 恒为不可用**。Windows 没有「按 action id 询问策略」的模型，
   `SystemCapabilities.polkit` 在这里永远是 `false`——与 macOS 同理。

由第 1 条还推出一个容易踩的结论：**Windows 上的 `admin worker` 的 uid 不是 0**。
uid 取自令牌里用户 SID 的 RID，而 linked token 的用户 SID 与过滤过的那张完全相同，
所以提权前后 uid 不变；`uid == 0` 在这个平台上只留给 `S-1-5-18`（LocalSystem）。
判断「这个会话有没有管理访问」要看 `session.elevated`，不要看 uid。

---

## 3. Crate 结构

```
strixmaid/
├─ Cargo.toml                       workspace
├─ crates/
│  ├─ strixmaid-types/              纯 serde 类型：DTO / WS envelope / IPC 消息 / 错误
│  │                              ★ 同时拥有 MetricLayer / RetentionPreset（API 契约，core 复用）
│  ├─ strixmaid-core/               ★ AgentCore —— 全部业务逻辑
│  │   ├─ providers/
│  │   │   ├─ mod.rs                Provider trait + Registry + probe()
│  │   │   ├─ service/              ServiceProvider: systemd(zbus) → systemctl(降级)
│  │   │   ├─ log/                  LogProvider: journalctl
│  │   │   ├─ process/              ProcessProvider: /proc
│  │   │   ├─ system/              主机信息 / DMI / 虚拟化识别 / 重启检测 / 时间
│  │   │   ├─ fs/                   FsProvider（P0 仅留壳）
│  │   │   └─ net/                  netlink 只读（P1）
│  │   ├─ metrics/
│  │   │   ├─ collect/              各采集器
│  │   │   ├─ ring.rs               内存环形缓冲
│  │   │   ├─ rollup.rs             分层聚合与保留期清理
│  │   │   └─ scheduler.rs          采集调度
│  │   ├─ store/                    sqlx + migrations（Agent / Server 共用）
│  │   ├─ session/                  会话与 worker 生命周期、提权状态
│  │   ├─ worker/                   worker 模式的 RPC 服务端
│  │   └─ capability/               两层能力探测
│  ├─ strixmaid-node/               ★ 一台主机的完整 API：axum Router + 认证与会话
│  │                                  + 审计 + WS 频道 + Windows 服务托管。
│  │                                  core 是「能力库」，node 是「把能力做成 API」，
│  │                                  两个宿主装载同一个 node（§11）
│  ├─ strixmaid/                    薄：node + 前端嵌入 + 节点目录 + 管道 + 拨号回上级。
│  │                                  产物就叫 `strixmaid`，`serve` 与 `agent`
│  │                                  是它的两种模式（2026-09-17 合并，见 §11）
│  └─ strixmaid-helper/             独立二进制，动态链接
└─ web/                             React 前端，构建产物由 rust-embed 嵌入
```

---

## 4. 技术选型

| 领域 | 选型 | 理由 |
|---|---|---|
| HTTP | `axum 0.8` + `tower-http` | 生态标准 |
| 异步运行时 | `tokio 1.53` | — |
| 数据库 | `sqlx 0.9`（SQLite，bundled） | 编译期 SQL 校验 + 内建 migration，比 sea-orm 轻 60%、编译快 2 倍 |
| systemd | `zbus 5.19` 走 `org.freedesktop.systemd1` | 官方 API，有属性信号与 job 事件；连不上 bus 时降级 `systemctl` 子进程 |
| journald | `journalctl -o json` 子进程 | libsystemd FFI 会毁掉静态构建；纯 Rust 解析 `.journal` 格式不成熟 |
| /proc | `procfs 0.18` | — |
| netlink | `rtnetlink 0.23` | P1 |
| PTY | `portable-pty 0.9` | — |
| 前端嵌入 | `rust-embed 8.12` | 编译期嵌入 + 预 gzip；debug 模式改为磁盘读，热更新不受影响 |
| OpenAPI | `utoipa 5.5` | — |
| 敏感数据 | `zeroize` | 明文密码用完立即擦除 |
| 前端 | React 19 + TS + Vite + Tailwind + shadcn/ui + Tremor | shadcn 是当下管理面板视觉标杆；Tremor 补数据面板组件 |
| 图表 | `uPlot` | ~45KB，10 万点不掉帧；Recharts 几千点即卡 |
| 终端 | `xterm.js` | — |

---

## 5. 认证与授权

### 5.1 模型

认证走 **PAM，映射到系统用户**。不自建用户体系、不自建 RBAC、无 setup token、无默认密码。
权限完全由操作系统裁决：systemd 操作由 polkit 裁决，日志可见性由 journald ACL 裁决，文件访问由文件权限裁决。

**多节点时认证发生在目标节点上，而非 Server 上。** Server 只是转发凭据，因此不存在跨节点身份映射问题。登录会话天然是「针对某个节点」的，切换节点需要重新认证。

### 5.2 登录协议：challenge-response

PAM 是对话式的（2FA 追问验证码、密码过期要求改密）。登录 API 从第一版就设计成多轮，MVP 只有一轮但结构留好：

```jsonc
POST /api/v1/auth/start     { username }
  → { session, prompts: [ { id, style: "prompt"|"prompt_echo"|"info"|"error", text } ] }

POST /api/v1/auth/respond   { session, responses: [ { id, value } ] }
  → 200 { "status": "complete", "token": "...", "user": { uid, gid, username, groups } }
  | 200 { "status": "more",     "session": "...", "prompts": [ ... ] }
  | 401 ApiError
```

响应用**内部判别字段** `status`（`#[serde(tag = "status")]`），不用 `untagged`。理由有二：untagged 在服务端加字段时会静默匹配到错误分支；且 OpenAPI 里生成的是无 discriminator 的 `oneOf`，多数代码生成器处理不好——而 OpenAPI 导出是本项目的 P0 硬要求（§12.1），不能在契约层给下游埋坑。

`responses[].value` 在类型层就是 `Zeroizing<String>`：不实现 `Serialize`、不实现 `Clone`、`Debug` 脱敏为 `<redacted>`。§5.3 的三条约束中，这一条由类型系统强制，不依赖 code review。

提权（`/api/v1/auth/elevate/*`）复用完全相同的协议。

### 5.3 凭据处理硬约束

PAM 需要明文密码，因此某个进程必然短暂持有它。MVP 采用「Server 可信」模型（浏览器经 TLS → Server → helper / 远程 Agent），但以下三条为强制要求，需进 code review checklist：

1. 明文密码只在认证的那一瞬间存在于内存；
2. **绝不进日志**，包括 debug 级；
3. **绝不入库**，`sessions` 表只存 token 的 hash；用完立即 `zeroize`。

端到端加密（每 Agent 一对密钥，浏览器加密、Server 只搬运密文）留到真正做多节点时再上。

### 5.4 PAM 集成

- PAM 服务名 `strixmaid`，安装时按发行版选择模板：Debian 系 `@include common-auth`，RHEL 系 `include system-auth`。
- **调用 `pam_open_session`** —— 这是 `--user` unit 支持的前提（创建 loginctl 会话、`XDG_RUNTIME_DIR`、启动 user manager）。
- 用户级 systemd 的 session bus 走 EXTERNAL(uid) 认证，root 直连会被拒，必须 setuid 后再连——这项工作归 helper。

#### Windows 补充：`LogonUserW` 与 `LoadUserProfileW`

Windows 上没有 PAM，但本节规定的每一步都有对应物：

| 本节的这一步 | Windows |
|---|---|
| `pam_start(service, user)` | 无。不读 `/etc/pam.d/<名字>`，没有「服务栈」这一层 |
| `pam_authenticate` | **`LogonUserW`**（`LOGON32_LOGON_INTERACTIVE`）：用户名 + 明文口令换一张令牌 |
| `pam_acct_mgmt` | **没有单独一步**。LSA 把两件事做在同一次 `LogonUserW` 里：账户禁用、锁定、过期、口令过期、登录时段限制、未授予登录类型，全部表现为它失败加一个特定的 `GetLastError` |
| `pam_setcred(PAM_ESTABLISH_CRED)` | 无。凭据就是那张令牌本身 |
| **`pam_open_session`** | **`LoadUserProfileW`**：往注册表挂载用户单元（`HKCU`）、准备 `%USERPROFILE%` |
| `pam_close_session` | `UnloadUserProfile`，必须与上一步在同一进程、同一令牌上成对调用 |
| `pam_end` | `CloseHandle(令牌)` |

因此 `pam_service` 配置项在 Windows 上没有意义，字段保留只为配置形状三平台一致，
helper 收到后直接忽略。§5.3 的三条凭据硬约束一字不改地继续成立。

**部署时最常踩的一脚**：目标账户必须有本机的「允许本地登录」
（`SeInteractiveLogonRight`），否则口令完全正确 `LogonUserW` 也会以
`ERROR_LOGON_TYPE_NOT_GRANTED` 失败。

「用户级 unit」那一条在 Windows 上整项不存在：SCM 只有一张全机服务表，
收到 `scope=user` 时如实返回 `capability_unavailable`。理由见
[`windows-platform.md`](./windows-platform.md) §4.4。

---

## 6. 能力探测（两层）

```jsonc
GET /api/v1/capabilities
{
  "system": {                       // 启动时探测一次
    "systemd": true, "journal": true, "helper": true,
    "polkit": true, "user_units": true, "podman": false
  },
  "user": {                         // 会话建立时探测
    "uid": 1000, "name": "alice", "groups": ["alice", "sudo"],
    "can_read_journal": false, "can_manage_units": false,
    "can_elevate": true, "elevated": false
  }
}
```

`user` 可为 `null`——**未认证时本端点也返回 200，只带 `system` 层**。这不是为了方便，而是必需：若 `strixmaid-helper` 未安装或启动失败，登录根本不可能成功，登录页必须能据此显示「PAM helper 不可用」，而不是让用户对着一个神秘失败的登录框反复重试。

前端据此区分两种状态，二者体验不同、不可混淆：
- **能力不存在** → 隐藏页面；
- **能力存在但当前用户无权** → 显示但禁用，并给出提权入口与可操作的说明（例如「你的账户不在 `systemd-journal` 或 `adm` 组，因此只能看到自己的日志。启用管理访问后可查看全部。」）。

#### Windows 补充：字段名不变，语义是「这项能力可用」

与 macOS 同一个决定（见 [`macos-platform.md`](./macos-platform.md) §3.6）：
`SystemCapabilities` 的字段名沿用 Linux 实现的名字，**语义是「这项能力可用」而不是
「装了这个软件」**。与其为第二、第三个平台在 API 契约里加字段（下游代码生成器全要
跟着改），不如让「后端具体是谁」留在 `providers` 列表里。

| 字段 | Windows 上的取值与判据 |
|---|---|
| `systemd` | SCM 是系统组件，恒 `true`；真连不上会由 service provider 的 `probe()` 覆盖 |
| `journal` | 事件日志同理，恒 `true`；真查不了会由 log provider 的 `probe()` 覆盖 |
| `polkit` | 恒 `false`。Windows 的授权走 UAC 与服务的 DACL，与 polkit 的「按 action id 询问策略」模型对不上。这不影响提权——提权的权威判定在 helper 内部（§5） |
| `user_units` | 恒 `false`。SCM 只有一张全机服务表，没有 `systemctl --user` 那一层 |
| `helper` | 同 Linux：`helper_path` 能否解析到一个可执行文件 |

两层探测的「第二层」（`user`）在 Windows 上换成了读令牌：组来自令牌里的组 SID，
`can_elevate` 按**英文规范组名**判断——本地化系统上内建组的显示名不是英文，
helper 会额外补一个规范名，配置里照写 `Administrators` 即可。

---

## 7. 指标：采集、聚合、存储

> 本节对应 [`roadmap/08-metrics-and-panel.md`](./roadmap/08-metrics-and-panel.md)。
> 其**采集侧已实施**（2026-08-28）：7.1 的采集项从 58 种裁到 34 种（含新增的
> GPU 四条），实施状态与决策记录见该文件开头。面板重做与静态拓扑部分仍为提案。
> 7.2 的分层聚合、7.3 的 median 选型、7.5 的 band 展示形式**不受影响**。

### 7.1 采集项（P0）

共 34 项（`roadmap/08` §4.2 的口径，权威定义在 `metrics/catalog.rs` 的 `CATALOG`）。
裁剪的三条规则：派生量不存、同向计数器合成一条异常信号、慢变量进健康检查。

| 类别 | 内容 |
|---|---|
| CPU | 总量：usage / system / iowait / irq（含软中断）/ steal；每核仅 `cpu.core.usage` |
| GPU | 每卡：usage / mem_used / mem_total / temp（Linux sysfs，`gpu_busy_percent` 可读才采；NVIDIA 见 roadmap/08 §12 Q1） |
| 内存 | total / used / available / cached（含 Buffers）/ swap_total / swap_used |
| 负载 | `load.1m`、运行队列长度、进程总数（5m / 15m 是 1m 的移动平均，不入库） |
| **PSI** | `/proc/pressure/{cpu,memory,io}` 的 avg10 —— 差异化项，见下；cpu 无 full（整机层面恒 0） |
| 磁盘 | 每设备：read/write bytes、iops（读写合计）、util%、await |
| 文件系统 | 每挂载点：used / total（使用率由前端做除法；inode 走健康检查 `disk.inodes`） |
| 网络 | 每接口：rx/tx bytes、errors（收发错误 + 丢包合计） |

**PSI 是关键差异化。** `/proc/pressure/*` 直接给出「系统有多少时间因等内存 / 等 IO 而停滞」。CPU 20% 但 `io.pressure` 60% 的机器体感是卡死的，而所有传统面板（含 Cockpit）都会显示「一切正常」。采集成本仅为读三个小文件。

⭕ 可选：TCP 连接数按状态分布、全局 fd 使用量、上下文切换、中断、`hwmon` 温度。

### 7.2 分层聚合

内存环形缓冲：默认 2s 采集，保留 1 小时，采集间隔可配 1–60s。

**每核只有 `cpu.core.usage` 一条曲线**。原先按需展开全部 8 态的 `metrics.per_core_detail` 配置项已随 roadmap/08 的裁剪删除——面板上没人看第 97 核的 softirq 历史，那个粒度的排查该上 perf；128 核机器的每核 series 数因此恒为 128。

落盘为五层，每个桶存 **cnt / min / max / sum / med** 五个字段：

| 层 | 桶宽 | Less 保留 | **Normal 保留（默认）** | 聚合来源 |
|---|---|---|---|---|
| `m_1m` | 60s | 6h | 1d | 内存环形缓冲 |
| `m_5m` | 300s | 3d | 7d | `m_1m` |
| `m_15m` | 900s | 14d | 30d | `m_5m` |
| `m_12h` | 43200s | 90d | 90d | `m_15m` |
| `m_1d` | 86400s | 1y | 1y | `m_12h` |

聚合规则：

```
min = MIN(min)
max = MAX(max)
sum = SUM(sum)          精确，无浮点累积误差
cnt = SUM(cnt)
med = MEDIAN(med)       子桶中位数的中位数，近似，见 7.3
```

展示时 `avg = sum / cnt`。存 `sum` 而非 `avg` 是为了让逐级聚合完全精确——存 avg 则每一级都要做一次乘除，五层下来会累积浮点误差。

后台任务每分钟运行一次，先聚合再清理。聚合语句为 UPSERT，**重复执行幂等**，且下界含目标表已有的最大桶，以吸收迟到数据。

**查询自动选层规则**（三条，顺序应用）：

1. 在「桶宽 ≤ 请求 step」的层中取最粗的一层；
2. 若结果点数超过上限（4000）则逐级升粗；
3. **若该层保留期覆盖不住请求跨度则继续升粗。**

第 3 条不可省略：没有它，1 年跨度会选中 `m_12h`（Normal 下只保留 90 天），曲线前 275 天会凭空缺失。因此选层函数必须知道当前的保留期预设。

时间区间为左闭右开 `[from, to)`；桶以起始时间标识，闭区间会把恰好落在 `to` 上的下一个桶带进来。所有层按 unix epoch 对齐（`m_1d` 落在 UTC 00:00，不随本地时区偏移）。偶数个子桶的中位数取中间两位的平均。

### 7.3 为什么是 median 而不是 p95

**p95 在这个采样密度下是冗余的。** 1 分钟桶按 2s 采集只有 30 个点，p95 落在第 28.5 位，即「第二大的值」——与 `max` 几乎重合，不提供独立信息。样本量太小，高分位数会退化成极值的近似。

**median 提供了 min/max/avg 都没有的信息：分布中心。** 更重要的是 **`avg` 与 `med` 的差值本身即为偏斜度指标**：

- `avg ≈ med` → 负载平稳；
- `avg > med` → 平时空闲但存在少数尖峰。

这正是排障时最想区分的两种状态，而 min/max/avg 组合无法回答。

**可合并性**：median 与 p95 同为分位数，逐级聚合仍是近似（`median of medians`）。但其误差性质显著更好——p95 of p95s 每级都取更高分位，误差单向累积、系统性偏高；中位数是稳健统计量，在子分布相似时近似良好且误差无方向性。因此无需像 `p95max` 那样重命名语义，直接标为 `med` 即可。

精确分位数需要 t-digest / DDSketch 一类可合并草图，每点数百字节、存储涨约 20 倍，收益不足以覆盖成本。

### 7.4 数据量

一台 16 核 / 4 盘 / 2 网卡 / 3 挂载点 / 1 GPU 的机器约 71 个 series（roadmap/08 §4.5）：Less 约 12MB，Normal 约 35MB（含索引）。多节点线性叠加。仅提供 Less 与 Normal 两套预设。

### 7.5 展示形式

**uPlot band 系列**，而非 K 线：min–max 画半透明区间带，avg 画实线，med 画虚线。**avg 与 med 两条线的分离程度直接可视化了负载的偏斜**——贴合即平稳，分离即尖峰型。信息量与 K 线相同，但几十条曲线叠加时可读性显著更好——K 线在时序监控场景下会糊成一片。

---

## 8. 数据模型（SQLite）

```sql
-- ================= 时序 =================
CREATE TABLE series (
  id      INTEGER PRIMARY KEY,
  node    TEXT    NOT NULL,               -- 'local' 或节点 ID
  metric  TEXT    NOT NULL,               -- 'cpu.usage' / 'mem.available' / 'disk.read_bytes'
  labels  TEXT    NOT NULL DEFAULT '',    -- 'dev=sda' / 'iface=eth0'，k=v 按键排序后拼接
  unit    TEXT,
  UNIQUE(node, metric, labels)
);

-- m_1m / m_5m / m_15m / m_12h / m_1d 五张同构表
CREATE TABLE m_1m (
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  ts        INTEGER NOT NULL,             -- 桶起始时间，unix 秒
  cnt       INTEGER NOT NULL,             -- 实际采样点数，用于加权聚合与缺失检测
  min       REAL    NOT NULL,
  max       REAL    NOT NULL,
  sum       REAL    NOT NULL,             -- avg = sum / cnt；存 sum 使逐级聚合精确无累积误差
  med       REAL    NOT NULL,             -- 1m 层为真中位数，粗粒度层为 median of medians
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;
```

`WITHOUT ROWID` + 复合主键 `(series_id, ts)` 使数据按 series 聚簇，查询单条曲线的时间范围是一次顺序扫描。

```sql
-- ================= 节点与会话（仅 Server） =================
CREATE TABLE nodes (
  id         TEXT PRIMARY KEY,            -- 'local' 或 uuid
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL,               -- 'local' | 'agent'
  token_hash TEXT,                        -- Agent 预共享 token 的 hash
  last_seen  INTEGER,
  created_at INTEGER NOT NULL
);

CREATE TABLE sessions (                   -- 浏览器会话
  id          TEXT PRIMARY KEY,           -- token 的 hash，绝不存明文
  created_at  INTEGER NOT NULL,
  last_active INTEGER NOT NULL,
  user_agent  TEXT,
  remote_addr TEXT
);

CREATE TABLE node_sessions (              -- 某会话在某节点上的认证状态
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  node_id     TEXT NOT NULL REFERENCES nodes(id)    ON DELETE CASCADE,
  uid         INTEGER NOT NULL,
  username    TEXT NOT NULL,
  elevated    INTEGER NOT NULL DEFAULT 0,
  elevated_at INTEGER,
  authed_at   INTEGER NOT NULL,
  last_active INTEGER NOT NULL,
  PRIMARY KEY (session_id, node_id)
);

CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          INTEGER NOT NULL,
  node_id     TEXT    NOT NULL,
  username    TEXT    NOT NULL,
  uid         INTEGER,
  elevated    INTEGER NOT NULL,
  action      TEXT    NOT NULL,           -- 'service.start' / 'process.kill' / 'file.write'
  target      TEXT,                       -- 'nginx.service' / '1234' / '/etc/hosts'
  params      TEXT,                       -- JSON
  result      TEXT    NOT NULL,           -- 'ok' | 'denied' | 'error'
  detail      TEXT,
  remote_addr TEXT
);
CREATE INDEX idx_audit_ts ON audit_log(ts DESC);

CREATE TABLE settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
```

**重新认证会把 `elevated` 重置为 0**：重新登录视为放弃管理访问，提权必须显式再走一次 `/auth/elevate/*`。

审计日志分页按 `id DESC` 而非 `ts DESC`——`id` 是 AUTOINCREMENT，与写入顺序严格一致，同一秒内的多条记录翻页不会重复或遗漏；`idx_audit_ts` 仍服务于时间范围过滤。

会话模型分两层（`sessions` + `node_sessions`）是有意为之：MVP 中 `node_sessions` 永远只有 `local` 一行，但将来从「一会话一身份」改成「一会话多身份」会波及每一条鉴权路径，属于核心重构。现在多一张表成本接近于零。

---

## 9. API

### 9.1 REST（P0）

```
认证
  POST   /api/v1/auth/start                开始认证 → prompts
  POST   /api/v1/auth/respond              回应 → token 或更多 prompts
  POST   /api/v1/auth/elevate/start        提权，同一协议
  POST   /api/v1/auth/elevate/respond
  POST   /api/v1/auth/logout
  GET    /api/v1/auth/session              当前会话信息

能力
  GET    /api/v1/capabilities              两层能力

系统
  GET    /api/v1/system/info               主机名 / 发行版 / 内核 / 架构 / 虚拟化 / CPU / 内存 / 磁盘
  GET    /api/v1/system/health             健康聚合：failed units / 磁盘超阈值 / 需重启 / SMART
  GET    /api/v1/system/time               时间 / 时区 / NTP 同步状态
  PUT    /api/v1/system/hostname           ⭕
  PUT    /api/v1/system/timezone           ⭕
  POST   /api/v1/system/power              { action: "reboot" | "poweroff" }

服务
  GET    /api/v1/services                  ?type=&state=&enabled=&q=&scope=system|user
  GET    /api/v1/services/{unit}           详情，含 cgroup CPU / 内存占用
  GET    /api/v1/services/{unit}/file      unit 文件原文
  GET    /api/v1/services/{unit}/deps      ⭕ 依赖关系
  POST   /api/v1/services/{unit}/action    { action: start|stop|restart|reload|enable|disable|mask|unmask }

日志
  GET    /api/v1/logs                      ?priority=&since=&until=&unit=&boot=&q=&cursor=&limit=
  GET    /api/v1/logs/entry/{cursor}       单条全字段详情
  GET    /api/v1/logs/boots                boot 列表

进程
  GET    /api/v1/processes                 ?sort=&order=&user=&q=&tree=
  GET    /api/v1/processes/{pid}           cmdline / cwd / exe / env⭕ / fd⭕ / 所属 unit
  POST   /api/v1/processes/{pid}/signal    { signal: "TERM"|"KILL"|"HUP" }
  POST   /api/v1/processes/{pid}/renice    ⭕

指标
  GET    /api/v1/metrics/series            可用 series 列表
  GET    /api/v1/metrics/query             ?series=&from=&to=&step=  自动选层
  GET    /api/v1/metrics/current           实时快照

终端
  POST   /api/v1/terminals                 { shell?, user? } → { id }
  GET    /api/v1/terminals                 本会话的终端列表
  DELETE /api/v1/terminals/{id}
  POST   /api/v1/terminals/{id}/resize     { cols, rows }

文件（P0 仅留壳，后续专门打磨）
  GET    /api/v1/files?path=
  GET    /api/v1/files/content?path=

审计
  GET    /api/v1/audit                     需管理访问
```

写操作一律走 REST（幂等、易调试、好审计）。`curl` 可以完成全部管理操作，只有实时流才需要 WS 客户端。

### 9.2 WebSocket

```
WS /ws                     控制面，多路复用
WS /ws/terminal/{id}       终端专用连接
```

终端单独开连接：它是纯二进制流、延迟敏感、生命周期独立于页面，塞进多路复用会让流控与背压难以处理。

控制面 envelope：

```jsonc
{ "v": 1,
  "t": "sub" | "unsub" | "data" | "req" | "resp" | "err" | "ping",
  "ch": "metrics.live",       // 频道
  "id": 42,                   // 关联请求与响应
  "d": { }                    // payload
}
```

频道：`metrics.live`（2s 推送）、`logs.follow`（带过滤参数）、`services.changed`（zbus 信号驱动）、`system.health`、`processes.live`。

**保活策略**：服务端不主动 ping、无空闲超时（连接由 TCP 与反代管理）。客户端的应用层 `ping` 只为穿过反代 / LB 的空闲超时（nginx 默认 60s），因此应**自适应**：仅在连续 45s 没收到任何帧时发一次；有订阅数据流动时不发。固定周期的 ping 是浪费。

**WS 鉴权走子协议，不走 URL。** 浏览器不允许给 WebSocket 握手设置 `Authorization` 头，而 `?token=` 会把 token 泄进访问日志与反代日志。约定：`new WebSocket(url, ["bearer", token])` → 握手携带 `Sec-WebSocket-Protocol: bearer, <token>`；鉴权中间件从该头回退提取，未认证在**升级前**即以 401 拒绝；服务端升级时必须回 `bearer` 子协议，否则浏览器会中止握手。

---

## 10. IPC 协议

### 进程关系与通道

**helper 由主进程按会话 spawn，通过继承的 socketpair 通信，不使用文件系统 socket。**

```
主进程 ──socketpair──▶ strixmaid-helper（root，每会话一个，持有 PAM 句柄）
                            │ fork + setuid + exec
                            └──socketpair──▶ strixmaid worker（uid = 登录用户）
                                 └─ 其一端经 SCM_RIGHTS 传回主进程
```

理由：helper 不是全局守护进程，而是**每个会话一个、生命周期等于会话**——PAM 要求 `pam_open_session` / `pam_close_session` 在同一进程、同一句柄上成对调用，所以持有 PAM 句柄的进程必须活到登出。既然是主进程 spawn 的子进程，父子关系本身就是身份证明，socketpair 比文件系统 socket + `SO_PEERCRED` 更简单、更不易出错，也不在文件系统留痕。

worker 的 socketpair 一端由 helper 经 `SCM_RIGHTS` 传回主进程；此后主进程与 worker 直接通信，不再经过 helper。

#### Windows 补充：命名管道，且附件由**读端**去拉

| | Unix | Windows |
|---|---|---|
| 通道 | `socketpair(AF_UNIX, SOCK_STREAM)` 的一端 | 一条命名管道实例 |
| 附件 | fd，`SCM_RIGHTS` 带外传递 | `HANDLE`，`DuplicateHandle` |
| 身份证明 | 父子关系（fd 是继承来的） | `GetNamedPipeClientProcessId` 对上子进程 pid |

「不走文件系统 socket」这条结论在 Windows 上更彻底：命名管道根本不落文件系统，
`run_dir` 因此在这个平台上用不到（配置项保留只为形状一致）。

**附件的方向是反的，而且必须是反的。** `SCM_RIGHTS` 是写端推；Windows 只有
`DuplicateHandle`，而它需要对另一个进程的 `PROCESS_DUP_HANDLE` 权限。本项目里附件
永远朝主进程流动（helper 把 worker 通道交给主进程、worker 把终端通道交给主进程），
而主进程恰好权限最高——所以一律由**读端**`OpenProcess(PROCESS_DUP_HANDLE)` 打开写端
进程、把句柄拉过来，并用 `DUPLICATE_CLOSE_SOURCE` 关掉源端那一份。

反过来做需要给低权限的 worker 开 `PROCESS_DUP_HANDLE` 到一个 SYSTEM 进程上，
那等于把提权漏洞写进设计。完整论证在
`crates/strixmaid-core/src/session/channel.rs` 的模块文档。

随之而来的一条纪律：**写端发出附件后不能 drop 自己那一份**——句柄已被内核关掉，
值可能已被复用，再 `CloseHandle` 会误关无关对象。

### 帧格式

**长度前缀（u32 大端）+ fd 计数（u8）+ JSON。** 不用 bincode：IPC 消息量极小（每会话几十条），性能无关紧要，而 JSON 用 `socat` 就能调试、且 helper 少一个依赖。

fd 本身走 `SCM_RIGHTS` 带外通道、不在帧里，但**帧头要记这一帧附带几个 fd**（`roadmap/03-terminal.md` §4.1）。原因是 `SOCK_STREAM` 上的一条硬性质：用普通 `read()` 读过附着了 fd 的那些字节，**内核会把 fd 直接丢掉**——不报错、无痕迹。因此可能收到 fd 的那一侧必须每一帧都走 `recvmsg`，而它需要提前知道该不该去控制缓冲里取。收到的个数与帧头不符即为协议错误。

```rust
// 主进程 → helper
AuthStart   { service: "strixmaid", username }
AuthRespond { responses: Vec<(PromptId, Zeroizing<String>)> }
SpawnWorker { open_session: bool }   // 用已认证的身份
CloseSession
GetPasswd   { uid }        // NSS 代理，P1
GetGroups   { uid }

// helper → 主进程
Prompts       { prompts: Vec<Prompt> }
AuthOk        { uid, gid, username, groups }
AuthFail      { reason }
WorkerSpawned { pid }      // 随后一帧 SCM_RIGHTS 传 fd
Error         { message }
```

### PAM 接入方式

**自写 FFI，不用 `pam-client` / `pam` crate**——二者分别停更于 2022 / 2023，且都要求 `libpam0g-dev` 才能链接。PAM 应用侧 API 只有 `pam_start / pam_authenticate / pam_acct_mgmt / pam_setcred / pam_open_session / pam_close_session / pam_end / pam_get_item` 八个函数，二十年未变；自行声明 `extern "C"` 并在 `build.rs` 里 `-l:libpam.so.0` 直接链接运行时库，**不需要 dev 包**，与「自包含」原则一致。

### helper 的职责边界

helper 是「需要动态链接或需要切换身份的操作」的唯一出口：

| 职责 | 为什么必须在 helper | 阶段 |
|---|---|---|
| PAM 认证 | libpam 只能动态链接 | P0 |
| 以指定用户身份 fork PTY / worker | 需要 setuid | P0 |
| `--user` unit 访问 | setuid 后才能连 session bus | P1 |
| NSS 用户 / 组解析 | 静态 musl 的 `getpwnam` 不走 NSS，接 LDAP/SSSD 的机器会静默漏用户 | P1 |

---

## 11. 节点模型

- Server = 转发层 + API 提供者 + 中心存储；**业务逻辑全在 AgentCore**。
- Server 内含一个 AgentCore 实例，即 `local` 节点，与远程节点走完全相同的代码路径。
- **Agent 与 Server 是同一个二进制的两种模式**（2026-09-17 合并）。`strixmaid serve` 监听端口、带前端、管下级；`strixmaid agent` 拨号回上级、不监听。此前是两个二进制，而 4.18 MB 与 10.5 MB 的差距几乎全是 node 层——Agent 补全管理能力之后本来就要拿到那一层，那个体积躲不掉，合并真正多花的只有前端资源 0.83 MB。换来的是产物矩阵减半、远程推装时推的就是自己这份二进制，以及「级联」与「就地提升成 Server」从换二进制变成改一个参数。Windows 上两种模式的服务名不同（`StrixMaid` / `StrixMaidAgent`），可以装在同一台机器上。
- **Agent 也有完整的存储与分层聚合能力**，本地保留自己的历史数据。
- Agent 主动连 Server，**一条双向复用的 WS 同时承载「Agent → Server 指标推送」与「Server → Agent 管理请求」**。好处：NAT 后的 Agent 可用，Server 不需要维护 N 个拉取定时器。
- Server 重启或网络中断后，凭时间戳游标向 Agent 请求补发**整个断连期间**的数据，曲线不留洞。
- HTTP 路径带节点标识：`/nodes/<id>/api/v1/**`，`local` 即 Server 自身那个实例。`/api/v1/**` 保留为 `local` 的别名。
- **Agent 接受远程管理操作**（2026-09-17 修订，方案见 `roadmap/11-multi-host.md`）。此前写的是「MVP 仅只读，有真实场景再设计」——场景出现了：在面板里添加一台空白主机，输入其本地账户口令，自动装好 agent，此后该主机与 Server 本机完全等价。
- 跨节点**不做身份映射**。操作者用的是目标主机的本地账户，认证对话经 Server 透传到该主机，PAM / `LogonUserW` 在那一侧发生，Server 只搬字节、不持有可复用的凭据。因此每个节点一套会话。
- Server 对远程节点是**管道而非翻译器**：不为任何端点写转发代码。业务逻辑装在 `strixmaid-node`（见下面的目录树），Agent 与 Server 提供同一个 `axum::Router`；Server 把浏览器的请求原样送进那条 WS 上的一条多路复用流，由对侧的 `Router` 直接处理。

---

## 12. 配置与部署

| 项 | 值 |
|---|---|
| 配置格式 | TOML |
| 优先级 | 内置默认 < `/etc/strixmaid/config.toml` < 环境变量 `STRIXMAID_*` < 命令行 |
| 数据目录 | `/var/lib/strixmaid/`（SQLite）。**本表是 Linux 的取值**，macOS 与 Windows 各见下面的补充 |
| 运行目录 | `/run/strixmaid/`（helper socket） |
| 日志 | stderr，交由 journald 收集，不自写日志文件 |
| 默认监听 | `127.0.0.1:9700`（端口待最终确认） |
| TLS | MVP 不做，对外暴露走反向代理；P1 支持自带证书 |
| 首次启动 | 无 setup token、无默认密码——直接用系统账户登录 |
| 配置文件路径覆盖 | `STRIXMAID_CONFIG` 环境变量 / `--config`。路径必须在构建 Figment **之前**确定，故不走普通的合并层 |
| 时长字段格式 | 一律整数秒，字段名带 `_secs` 后缀。理由：环境变量层必须能表达同样的值，而 `"2s"` 这类人类可读形式在 figment 的宽松解析下会与 TOML 层产生表示分歧 |
| 未知配置项 | `deny_unknown_fields` —— 配置项拼错必须报错，不可静默忽略 |

**会话超时语义**：`session.idle_timeout_secs`（默认 900s）是浏览器会话的空闲超时；`session.elevated_idle_timeout_secs`（默认 300s，对齐 sudo 的 `timestamp_timeout`）是**提权状态独立的、更短的**空闲超时——超时后回收 admin worker、`elevated` 降回 false，会话本身仍然存活，需要重新提权。提权必须比会话更早过期，而非更晚。
| 安装物 | 二进制 + `strixmaid.service` + `/etc/pam.d/strixmaid`（按发行版模板）+ 默认 config.toml |

MVP 不做 TLS 的理由：自签证书会给每个用户制造浏览器警告，正经用法是放在 nginx / Caddy 之后。

#### Windows 补充：路径、服务宿主与安装物

Windows 上没有 FHS，等价物是 `%ProgramData%\StrixMaid`——那正是「机器范围、非用户、
可写」的系统目录。**这些默认值在代码里是编译期常量而不是运行时展开 `%ProgramData%`**：
配置的默认值必须在 `Config::default()` 里是确定的（示例配置、错误信息、测试都要引用它），
而 `%ProgramData%` 在实际部署里几乎总是 `C:\ProgramData`；被改掉的机器上用 `--config`
或 `STRIXMAID_DATA_DIR` 显式指定即可。

| 项 | Linux | Windows |
|---|---|---|
| 配置文件 | `/etc/strixmaid/config.toml` | `C:\ProgramData\StrixMaid\config.toml` |
| 数据目录 | `/var/lib/strixmaid/` | `C:\ProgramData\StrixMaid\data\` |
| 运行目录 | `/run/strixmaid/`（helper socket） | `C:\ProgramData\StrixMaid\run\`，**实际用不到**（IPC 走命名管道） |
| 二进制 | `/usr/bin/` | `%ProgramFiles%\StrixMaid\` |
| 服务宿主 | systemd unit | SCM，服务名 `StrixMaid`，`ImagePath` 是「绝对路径 + `service run`」 |
| 日志 | stderr → journald | stderr → 服务宿主；服务模式下另写 Windows 事件日志 |
| PAM 模板 | `/etc/pam.d/strixmaid` | 无 |
| 安装物 | tar.gz + `install.sh` | zip + `install.ps1` / `uninstall.ps1` |

三条与 Linux 不同的部署约束：

1. **服务注册走 `strixmaid.exe service install`，不用 `New-Service`。** 那条子命令还要
   写恢复策略（`SERVICE_CONFIG_FAILURE_ACTIONS`）与服务描述，并把 `ImagePath` 写成
   绝对路径——服务由 `services.exe` 拉起，工作目录是 `%SystemRoot%\system32`，
   相对路径解析不到。这些 `New-Service` 都给不了。
2. **`helper_path` 必须写成绝对路径。** 默认值是裸名，要靠 `PATH` 查找，而服务继承的是
   系统 `PATH`，`%ProgramFiles%\StrixMaid` 不在其中。安装脚本因此把这一项改写掉。
3. **目录 ACL 要显式设置，不能靠继承。** `%ProgramData%` 的默认 ACL 给 Users 一条带
   继承的「创建文件 / 写入数据」，不断掉的话任何登录用户都能往配置目录里写文件——
   而服务以 LocalSystem 读这里的 `config.toml`，`helper_path` 是一条会被执行的路径。
   安装目录反过来**必须**给 Users 读 + 执行：worker 以登录用户身份运行，而 worker 是
   `strixmaid.exe` 的子命令。

卸载的语义与 deb 一致：**默认保留配置与数据**，`-Purge` 才删。

详见 [`windows-platform.md`](./windows-platform.md) 与
[`packaging/windows/README.md`](../packaging/windows/README.md)。

#### macOS 补充：路径、服务宿主与安装物

macOS 既不是 FHS 也不是 Windows，而是 BSD 的 hier(7) 布局：有 `/etc`、有 `/var`，
但**没有 `/run`**，也**没有 `/var/lib`**。此前 macOS 跟着 Linux 走
`#[cfg(not(windows))]`，`run_dir` 因此指向一条本机根本不存在的路径；现在单列一组。

| 项 | Linux | macOS |
|---|---|---|
| 配置文件 | `/etc/strixmaid/config.toml` | 同左（hier(7) 的 `/etc`；而且 OpenPAM 只认 `/etc/pam.d/<服务名>`） |
| 数据目录 | `/var/lib/strixmaid/` | `/var/db/strixmaid/`（hier(7)：自动生成的系统数据库文件） |
| 运行目录 | `/run/strixmaid/` | `/var/run/strixmaid/`（**没有 `/run`**；内容每次开机重置） |
| 二进制 | `/usr/bin/` | `/usr/local/bin/`（`/usr` 只读并受 SIP 保护，`/usr/local` 是放行给第三方的） |
| 服务宿主 | systemd unit | launchd，**LaunchDaemon**（`/Library/LaunchDaemons/io.strixmaid.{server,agent}.plist`） |
| 日志 | stderr → journald | `/var/log/strixmaid/{server,agent}.log`（launchd 不把 stdout 送进统一日志，不设就丢 `/dev/null`） |
| PAM 模板 | `/etc/pam.d/strixmaid` | 同左，且**必须装**：缺文件时 OpenPAM 回退到全拒的 `other` |
| 安装物 | tar.gz + `install.sh` | tar.gz + `packaging/macos/install.sh` / `uninstall.sh` |

三条与 Linux 不同的部署约束：

1. **选 LaunchDaemon 而不是 LaunchAgent。** Agent 只在某个用户登录之后才存在，
   而全局指标采集必须在无人登录时持续运行，主进程也需要 root。
2. **装载用 `launchctl bootstrap`，不用已过时的 `launchctl load`。** 后者在 plist
   有问题时经常退出 0 却什么也没做。
3. **没有「装好但开机不自启」这一档。** plist 一进 `/Library/LaunchDaemons`，
   下次开机就会被装载；要装而不启用得显式 `launchctl disable`。

卸载语义与 Windows、deb 一致：**默认保留配置与数据**，`--purge` 才删。

详见 [`macos-platform.md`](./macos-platform.md) §7 与
[`packaging/macos/README.md`](../packaging/macos/README.md)。

### 12.1 OpenAPI 导出

**OpenAPI spec 的完整性是 P0 硬要求**——前端可以随时重构，但必须始终能照着 spec 走。因此：

- 路由用 `utoipa-axum` 的 `OpenApiRouter` **自动收集**，不手写 `paths(...)` 清单，避免新增端点时遗漏；
- 所有 DTO 无条件 `#[derive(ToSchema)]`；
- 可视化文档用 **Scalar**（`utoipa-scalar`），非 Swagger UI。Scalar 的 JS 需**本地嵌入**，不可依赖 CDN——目标机器可能没有外网。

**文档端点仅在 debug 构建暴露**：

```rust
#[cfg(any(debug_assertions, feature = "apidoc"))]
```

被 gate 的有：`GET /api/v1/openapi.json`、`GET /api/docs`（含其 JS 资源）、以及 **`GET /debug` 开发调试页**；业务 API 端点在 release 下照常工作。

**`/debug` 调试页**：开发期用来验证各接口的单文件页面（内联 JS/CSS，vendored uPlot），按模块分面板直接调 API 并展示结果，图表类数据可绘图；不做刻意的 UI 设计。每个面板独立容错——某个端点未实现或返回错误时只影响该面板。正式前端到位前 `/` 重定向到 `/debug`（同样受 cfg 门控），之后删除。`debug_assertions` 对应默认行为，额外的 `apidoc` feature（默认关闭）保留「构建一个带文档的 release 版」这条路。

**关于 `oneOf` 的判别方式**：utoipa 5.5 的 `#[schema(discriminator = ...)]` 只支持 `#[serde(untagged)]` + 单字段 `$ref` 变体——想拿到 OpenAPI 的 `discriminator` 关键字就必须退回 untagged，正是本项目要避开的东西。因此 `AuthOutcome` 这类多形状响应改用 **JSON Schema 原生判别**：每个 `oneOf` 分支把判别字段声明为必填的单值枚举（`{"status": {"type":"string","enum":["complete"]}}`）。openapi-generator / openapi-typescript / orval 均支持，且**比 `discriminator` 更严格**——校验器会真的拒绝判别字段与内容不匹配的响应，而 `discriminator` 仅是提示。

理由：生产环境不应对外暴露完整的 API 表面地图，同时省下 Scalar 资源的体积。`ToSchema` derive 不随之 cfg 化——它们只生成 schema 函数，release 下无引用，由 LTO 剥除；逐个 DTO 加 cfg 属性会显著损害可读性。

---

## 13. 实现顺序

### Phase 0 — 骨架 ✅ 已完成（2026-08-27）

实际产出：5 个 crate、64 个测试、clippy 零 warning。release 二进制 2.86 MiB（不含文档）/ 4.00 MiB（含 Scalar）。

Phase 0 期间固化的几条实现约定：
- **静态资源 SPA fallback 按扩展名白名单判断**：只有已知静态资源扩展名（js/css/png/woff2…）未命中时才 404，其余路径一律回退 `index.html`。不能用「含点即文件」的启发式——本应用的前端路由天然带点（`/services/nginx.service`）。
- **命令行参数除 `--config` 外不声明 `env`**，环境变量统一由 figment 处理。否则同一设置会有两个变量名（`STRIXMAID_LOG_LEVEL` vs `STRIXMAID_LOG__LEVEL`）、两种优先级。
- **配置文件路径确定、不向上搜索、允许缺失**：`is_file()` 判断后才 merge `Toml::file_exact`；文件存在但语法错必须报错并带路径。
- **`ApiError` 归 types，server 用 newtype `ApiErr` 适配 `IntoResponse`**（孤儿规则，且 types 不能依赖 axum）。
- **不建空壳路由**：未实现的端点不注册——空壳会进 OpenAPI，给调用方错误的可用性信号。
- Scalar JS 以 gzip 形态入库与嵌入（1.04 MiB），`Content-Encoding: gzip` 原样发出；`withDefaultFonts=false`、`proxyUrl=""` 关闭其两处运行时外部请求。

原计划：
1. workspace + 5 个 crate 骨架
2. `strixmaid-types` 基础 DTO
3. axum server + rust-embed + 一个静态页面跑通
4. sqlx + migrations + 基础 schema
5. 配置加载（TOML + env + CLI）

### Phase 1–3 ✅ 已完成（2026-08-27）

实际产出：认证链路（自写 PAM FFI、socketpair IPC、setuid worker、提权）、7 类采集器 + 环形缓冲 + 落盘、主机/进程/能力 provider、systemd（zbus + systemctl 降级）与 journald provider、WS hub 三个频道、`/debug` 调试页。29 个 REST 端点，273 个测试，clippy 零 warning。

本机（无 root）实测：PAM 错误密码路径经 HTTP → helper → `pam_authenticate` 走通；`polkit` 对 `ssh.service` restart 返回 403 `permission_denied` + `can_retry_elevated=true`。

### Phase 1 — 认证链路（最难的部分最先做）
6. `strixmaid-helper`：PAM challenge-response
7. IPC 协议 + `socketpair` fd 传递 + `SO_PEERCRED` 校验
8. worker fork + setuid + exec
9. 会话管理 + `node_sessions`
10. 提权流程（admin worker）
11. 前端登录页

> 认证放最前，因为它牵涉三个进程、特权切换与协议设计，是**整个系统最难返工的部分**。链路打通后，其余功能都只是往上挂。

### Phase 2 — 只读观测
12. capability 两层探测
13. system provider：主机信息 / 健康聚合 / 时间
14. metrics 采集器 + 环形缓冲 + WS 推送
15. 分层聚合 + 保留期清理
16. process provider + 进程页（含 cgroup → unit 反查）
17. 概览页

### Phase 3 — 服务与日志
18. service provider（zbus + systemctl 降级）
19. zbus 信号 → `services.changed` 推送
20. log provider（journalctl）+ 游标分页 + follow
21. 服务页 + 日志页（**必须虚拟滚动**）

### Phase 4 — 终端
22. worker 内 PTY + WS 桥接
23. 会话保持 + 回看环形缓冲
24. xterm.js 前端

### Phase 5 — 收尾
25. 审计日志
26. 文件管理壳
27. `strixmaid-agent` + WS 同步 + 断连补发
28. 打包：musl 静态构建、systemd unit、pam.d 模板

---

## 14. 明确不做（MVP）

虚拟机管理、SELinux、kdump、sosreport、会话录制、SCAP、高级存储（LVM / RAID / LUKS / iSCSI / Stratis / btrfs）、firewalld、完整告警系统（规则引擎 + 通知渠道）、插件机制、发行版特有功能、MOTD、TLS。

理由见 `cockpit-feature-inventory.md` §0 与 §14——这些恰是 Cockpit 投入最大、使用频率最低、且移植性最差的部分。

---

> 现状与目标的差距分析见 [`gap-analysis.md`](./gap-analysis.md)，后续工作方案见 [`roadmap/`](./roadmap/README.md)。
> macOS 开发平台的适配说明见 [`macos-platform.md`](./macos-platform.md)。
