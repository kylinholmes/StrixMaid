# macOS 作为交付目标

> 初稿 2026-08-27（那时 macOS 只是开发平台），2026-09-16 按新定位改写。
> **macOS 现在是第三个交付目标**，与 Windows 同级：有 launchd 服务定义、
> 安装与卸载脚本、发布包与 CI 门槛。两处与另外两个平台不同的边界先说在前面：
>
> * **只出 Apple Silicon**（`aarch64-apple-darwin`）。不做 Intel，不做 universal。
> * **暂不做代码签名与公证。** 后果不是「不够体面」而是具体的：从浏览器下载的
>   发布包会被 Gatekeeper 打上隔离标记，直接运行会被拒绝。解法与原因见 §7.5，
>   那一节必须读。
>
> 本文记录三进程结构在这里长什么样、各 provider 的数据从哪来、哪些能力在这个
> 平台上根本不存在，以及交付形态（§7）是什么样。

---

## 1. 这一层是怎么来的，定位变了什么

`design.md` 的每一条采集路径都是 `/proc`、`/sys`、systemd 与 journald。
最初在 macOS 上开发时，若不做适配，只有两种选择：

- 全部 provider 报 unavailable —— 那么 `/processes`、`/services`、`/logs`、`/metrics`
  全是空数组，**对照 API 测试就退化成只能验证鉴权与错误码**；
- 起一台 Linux 虚拟机 —— 编辑 / 编译 / 调试的回路被拉长到不可接受。

因此选择第三条：给每个 provider 补一套 macOS 原生实现，让本机跑起来的服务返回**真实数据**。

**2026-09 起这一层的定位变了**：不再是「为了开发回路顺手做的适配」，而是要交付给
别人跑的东西。技术内容（§2 到 §4）一个字没变，变的是三件事：

1. **质量门槛与 Windows 相同**。取舍标准仍是 `design.md` §1 第 2 条——能力探测而非
   硬依赖，缺什么如实缺席，绝不拿相近的东西冒充。区别在于，此前「这项在 macOS 上
   没做」可以用「反正只是开发平台」结掉；现在必须说清楚是**平台没有**还是**还没做**。
   §3 的表格就是按这个口径重新读过的。
2. **路径默认值单列一组**（§7.2）。此前 macOS 跟着 Linux 走 `#[cfg(not(windows))]`，
   `run_dir` 因此默认指向 `/run/strixmaid`——**macOS 上根本没有 `/run`**。
   在开发平台上它只是一个没人用到的字段，作为交付目标它是 bug。
3. **多了服务定义、安装脚本与发布包**（§7.1、§7.3、§7.4）。

依然**不变**的是：Linux 与 Windows 的实现、取值与文档一个字节没动。

## 2. 平台分界线

分界一律用 `#[cfg(target_os = ...)]`，**Linux 实现的内容逐字节未改**——
`metrics/collect/` 与 `providers/system/` 下的 Linux 文件是纯目录移动（git 记为 rename），
`super::` 引用靠各 `linux/mod.rs` 里的转发声明继续成立。

| 模块 | Linux | macOS |
|---|---|---|
| `metrics/collect/` | `linux/`：`/proc`、`/sys` | `macos/`：mach、`sysctl`、`getfsstat` |
| `providers/system/` | `linux/` | `macos/` |
| `providers/process/` | `linux.rs`：`procfs` | `macos.rs`：`libproc` |
| `providers/service/` | `bus.rs`（zbus）+ `cli.rs` | `launchd.rs`（`launchctl`） |
| `providers/log/` | `journalctl.rs` | `oslog.rs`（`log show` / `log stream`） |
| `platform/` | 无 | `macos.rs`：`sysctl` / `getfsstat` 等多处共用的 FFI |

`procfs` 与 `zbus` 已移进 `[target.'cfg(target_os = "linux")'.dependencies]`，
macOS 侧只多一个 `mach2`（且只用它的 `mach_host_self` / `mach_task_self` 两个符号，
原因见 `metrics/collect/macos/mod.rs` 的模块文档）。

## 3. 覆盖差异

**缺失一律如实上报，不拿相近的东西冒充。** DTO 里的 `Option` 与
`GET /metrics/series` 的实际内容就是「有没有」的唯一事实来源，
这正是 `design.md` §1 第 2 条「能力探测而非硬依赖」在各层的体现。

### 3.1 指标（§7.1 的七项采集器）

| 采集项 | macOS | 说明 |
|---|---|---|
| CPU | ⚠️ 5 条 | mach 只统计 user / system / idle / nice 四态。**没有** `cpu.iowait` / `irq` / `softirq` / `steal`；`cpu.usage` 相应地定义为 `100 − idle` 而非 `100 − idle − iowait` |
| 内存 | ⚠️ 部分 | 没有 `mem.buffers` / `mem.dirty` 的对应概念；`mem.available` 是估算值，见下 |
| 负载 | ⚠️ 部分 | `getloadavg(3)`；XNU 不导出运行队列长度，**没有** `procs.running` |
| PSI | ❌ | `/proc/pressure` 是 Linux 独有的内核特性，无任何等价物 |
| 磁盘 IO | ❌ | 逐设备统计要走 IOKit。这一项是**还没做**，不是平台没有——当初按开发平台的收益判断压后了，定位变更之后它就是一项待补的能力 |
| 文件系统 | ✅ | `getfsstat(2)` |
| 网络 | ⚠️ 部分 | `sysctl NET_RT_IFLIST2`（64 位计数，不回绕）；`if_data64` **没有**发送方向的丢包计数，故无 `net.tx_drops` |

**`mem.available` 的口径**是本次适配引入的近似，需要记录在案：

```text
available ≈ (free + purgeable + external) × 页大小
```

Linux 的 `MemAvailable` 是内核算好的、扣除了水位线的值；上式没有扣水位线，
**系统性偏乐观**。看趋势可以，别拿它做容量告警的绝对阈值。

### 3.2 主机信息

`disks`（物理盘枚举）、`hardware.serial`、`hardware.bios_version`、`cpu.numa_nodes`、
`cpu.quota_cores`、以及三个 `ntp_*` 字段在 macOS 上都是空 / `None`，原因见
`providers/system/macos/mod.rs` 与 `time.rs` 的模块文档。健康报告的 `skipped`
相应地报 `["reboot", "launchd", "smart"]` 而不是 Linux 的 `["systemd", "smart"]`。

> 这带来一处**对 `design.md` §8 的补充**：`HealthReport.skipped` 的内容原本写死在
> `build_report` 里，现在改由 `HealthInputs.skipped` 从平台侧传入。
> 理由：在 macOS 上报「systemd 未检查」会让前端把它显示成一项待补的能力，那是误导。

### 3.3 进程

`cgroup` / `unit`（macOS 没有 cgroup）、`cwd`、`fds`、`tty`、`io_*` 为 `None`。
`cmdline` 与 `environ` 走 `sysctl KERN_PROCARGS2`，**只有同 uid 或 root 能读**，
别人的进程会退化成 `None`——这与 Linux 上读不到 `/proc/<pid>/environ` 的表现一致。

### 3.4 服务（launchd）

launchd 与 systemd 的模型只重合一半。完整映射表见
`providers/service/launchd.rs` 的模块文档，这里只记两条会影响 API 使用者的：

1. **unit 名 = launchd label + `.service` 后缀**。`com.apple.Finder` 对外报作
   `com.apple.Finder.service`。不加后缀的话 `unit_type`（取自最后一段）会变成
   `Finder` 这种垃圾值，`?type=service` 过滤随之失效。代价是名字长七个字符，
   换来 API 契约在两个平台上完全一致。
2. **`reload` / `mask` / `unmask` / `unit_deps` 返回 `capability_unavailable`（501）**，
   而不是假装成功或降级成别的操作。launchd 没有这些概念。

### 3.5 日志（统一日志）

字段映射见 `providers/log/oslog.rs`。三点值得注意：

- **游标是自造的** `<unix 微秒>:<整行的 FNV-1a 哈希>`。统一日志没有 journald 那种
  不透明游标；两部分都只依赖日志内容，不依赖服务端状态，因此可跨请求使用。
  一开始用的是 `traceID`，那是错的：同一 activity 下的事件共享它，实测两分钟里
  就有 57 组重复键（单个 traceID 最多重复 1698 次）。翻页按「严格早于游标」筛，
  同键的条目会被**整组丢掉**——是静默漏日志，不只是排序不稳。
- **`log show` 比 `journalctl` 慢一个量级**，且不接受「只要最后 N 条」。
  因此查询强制带时间窗口，调用方没给 `since` 时按 1 小时兜底；
  输出边读边只保留最新的 `limit` 条，内存不随窗口增长。
- **`boots` 只报当前这一次启动**。归档里确实有历史 boot 的日志，但 `log` 没有
  `--list-boots` 那样的枚举接口，要 distinct 出历史 bootUUID 只能全量扫一遍。
- **`log show` 与 `log stream` 的级别开关写法不同**：前者是布尔标志
  `--info` / `--debug`，后者是 `--level info|debug`。给 `log show` 传 `--level`
  会直接 `unrecognized option`。
- **默认窗口 5 分钟**，不是 1 小时：统一日志约 250 行/秒（实测 5 分钟 6.2 万行），
  1 小时就是七十多万行。查询只解析留下的那几十条，不为丢弃而解析全部。

### 3.6 能力位

`SystemCapabilities` 的字段名沿用 Linux 实现的名字，**语义是「这项能力可用」而不是
「装了这个软件」**。因此 macOS 上 `launchd` 点亮 `systemd` 位、`oslog` 点亮 `journal` 位。

> 这是**对 `design.md` §6 的补充**：与其为每个平台在 API 契约里加两个新字段
> （下游代码生成器全要跟着改），不如让「后端具体是谁」留在 `providers` 列表里，
> 那才是它该待的地方。定位变成交付目标之后这个决定更站得住：Windows 后来也走了
> 同一条路（`design.md` §6 的 Windows 补充），三个平台的 API 契约因此完全一致。`polkit` 在 macOS 上恒为 `false`——它的授权走
> Authorization Services / TCC，与 polkit 的「按 action id 询问策略」模型对不上。

### 3.7 PAM 服务配置(认证成功路径的前提)

OpenPAM 在**服务文件不存在**时整体回退到 `/etc/pam.d/other`,而 macOS 的 `other`
是四行 `pam_deny`——表现为「密码正确也认证失败」,helper 日志只有一句
`认证失败: pam_authenticate`,极具迷惑性(2026-09-09 实测踩中)。两种解法:

1. **正规**:装 macOS 模板(骨架取自系统自带 `/etc/pam.d/login`,认证走 `pam_opendirectory`):

   ```sh
   sudo install -m 0644 packaging/pam.d/strixmaid.macos /etc/pam.d/strixmaid
   ```

2. **免 sudo 的开发期权宜**:配置里 `pam_service = "login"`,借用系统自带的
   `login` 服务栈。语义上是冒名,只该出现在本机联调的临时配置里。

**`packaging/macos/install.sh` 会替你做第 1 种**(已存在则不覆盖)。这一步在 macOS 上
不是可选项:Linux 上漏装 pam.d 文件通常还能落到发行版自己的默认栈,macOS 落到的是
全拒的 `other`。Linux 侧的 `packaging/install.sh` 要按 `/etc/os-release` 在两份模板
之间做选择,macOS 只有一份,也没有 `/etc/os-release`,因此 macOS 的安装脚本不做判断。

## 4. 平台 API 差异的几个坑

这几条不是设计选择，是 macOS 内核 / 库与 Linux 的既有差异，**记下来以免重复踩**：

| 差异 | 后果 | 处理 |
|---|---|---|
| **Linux-PAM 与 OpenPAM 的常量数值不同** | `PAM_ESTABLISH_CRED` 在 Linux-PAM 是 2，而 2 在 OpenPAM 里是 `PAM_DELETE_CRED`——认证成功后不但没建立凭据反而把凭据删了，且 `pam_setcred` 照样返回 0 | `helper/src/auth/unix.rs` 按平台分两套 `consts`，每项注明出处 |
| 没有 `SOCK_CLOEXEC` / `MSG_CMSG_CLOEXEC` | 只能事后 `fcntl(FD_CLOEXEC)`，存在极小的竞态窗口 | `session/framing.rs::set_cloexec`，窗口影响见其文档 |
| 没有 `MSG_NOSIGNAL` | 写到已关闭的对端会被 `SIGPIPE` 打死 | 改用 `SO_NOSIGPIPE` 套接字选项（覆盖面反而更广，连普通 `write` 也管） |
| `nix` 在 Apple 上编译掉了 `getgroups` / `getgrouplist` | 编译不过 | 直接调 libc，见 `worker/mod.rs::current_groups` 与 `helper/src/main.rs::getgrouplist` |
| `initgroups` 第二个参数是 `c_int` 而非 `gid_t` | 类型不匹配 | `helper/src/spawn/unix.rs` 按平台取别名 |
| **`scutil --set` 对 `admin` 组成员放行，不需要 root** | 「反正非 root 会失败」这种测试会在开发者自己机器上真的改掉主机名 | 写操作的单测只测错误映射函数，绝不真的调命令 |
| **`log show` 偶尔把同一条事件吐两次**（逐字节相同、时间戳相同） | 游标随之重复，翻页时它的孪生兄弟会被边界一起漏掉 | `oslog::show` 排序后按游标相邻去重——两条本就无从区分，去重不丢信息 |
| macOS 自带 bash 3.2 | 没有 `mapfile` / `declare -A`；`set -u` 下空数组展开报错；**按字节解析变量名，`"$var（中文）"` 会把中文吃进变量名** | `scripts/*.sh` 全部兼容 3.2，细节见脚本头部注释 |

## 5. 本机验证结果（2026-08-27）

环境：Apple Silicon，macOS 26.5.2，rustc 1.98.0，非 root，当前用户在 `admin` 组。

> 这一轮验证的是 §2 到 §4 那些**平台适配**的内容，做在定位变更之前。
> §7 的交付形态（plist、安装脚本、打包脚本、新的路径默认值）**不在其中**，
> 逐条见 §7.6。

| 项 | 结果 |
|---|---|
| `cargo build --workspace` | 三个二进制全部产出；helper 链到 `/usr/lib/libpam.2.dylib` |
| `cargo test --workspace` | 全绿（含各 provider 的「本机采集」用例） |
| `cargo clippy --workspace --all-targets` | 零 warning |
| 四个 provider 探测 | `host` / `proc` / `launchd` / `oslog` 全部 `Available` |
| `scripts/api-smoke.sh`（未认证） | 26 项断言全通过 |
| `/debug` 调试页 | headless 浏览器加载正常：uPlot 就位，九个面板全部渲染，能力面板取到真实数据（`systemd✓ journal✓ helper✓ polkit✗ user_units✓`），受保护面板各自独立显示错误 |
| PAM 链路 | `POST /auth/start` 经 helper → OpenPAM 返回 `Password:` prompt；错误密码 `respond` → 401 `unauthenticated`。**helper spawn、socketpair IPC、PAM 会话回调在 macOS 上均已跑通** |

### 一个测试环境上的坑

**跑 `cargo test` 之前先停掉本机的 `strixmaid serve`。**
`session::tests::完整状态机_登录_提权_超时回收_登出` 会 spawn 真实的 helper 与 worker；
本机同时有一个服务实例在跑时，该用例约半数概率失败在
`Worker("worker 在发出 Hello 之前就退出了")`。停掉服务后连续多轮全绿。

这不是 macOS 特有的（用例与被测代码都是平台无关的），只是在本机联调时格外容易撞上
——写代码、起服务、跑测试往往是同一个终端里的连续动作。
根因未深究（怀疑是两个实例的 helper/worker 之间抢某个共享资源），
记在这里以免下次把它当成偶发 flake 放过去。

### 未验证

- **认证成功路径**：需要真实密码，只能由开发者本人交互执行
  （`scripts/dev-login.sh`，密码走 `read -s`，不进参数 / 环境变量 / 文件）。
  拿到 token 后 `STRIX_TOKEN=... scripts/api-smoke.sh` 可跑完全部只读端点。
- **提权与 setuid 到其他用户**：需要 root，与 `roadmap/07-verification.md` §1 同属一类。
- **WS 频道的实际推送**：冒烟脚本只验证了未认证时握手前被 401 拒绝；
  `/debug` 的实时面板需要登录后在真实浏览器里点开才能验证。
- **`/debug` 登录后的面板**：指标 band 图（min–max 区间带 + avg 实线 + med 虚线，
  §7.5）与各数据表格同上。

## 6. 与 roadmap 的关系

本次适配**不改变** `roadmap/01–07` 的任何结论与优先级：

- `01-worker-execution.md` 的授权模型缺口在 macOS 上同样存在，且性质相同；
- worker 的 RPC 分发表仍然只有 `ping` / `whoami`；
- 新增的 macOS provider 与 Linux provider 走同一套 trait，
  01 把请求改道到 worker 之后，两边一起改道，不需要额外工作。

macOS 侧唯一的额外注意点：`03-terminal.md` 的 fd 传递依赖帧头改造，
而 macOS 的 `SCM_RIGHTS` 已在本次适配中验证可用（`session/framing.rs` 的单测在本机通过）。

---

## 7. 交付形态（2026-09 新增）

> 本节全部内容**没有在真机上跑过**：写它的机器是 Windows，既装不了 plist、
> 也编不出 `aarch64-apple-darwin`（缺 Apple SDK）。能做的静态检查都做了
> （`sh -n`、plist 走 XML 解析器验格式与 `Label` 一致性、路径常量在
> `aarch64-apple-darwin` 目标下单独编过一遍），逐条未验证项列在 §7.6。

### 7.1 产物与发布包

三个产物与 Linux 一一对应，`worker` 同样不是独立二进制而是 `strixmaid` 的子命令：

| 产物 | macOS 上的链接方式 |
|---|---|
| `strixmaid` | 系统 dylib（libSystem 等）；前端由 rust-embed 嵌在里面 |
| `strixmaid-agent` | 同上 |
| `strixmaid-helper` | 同上，外加 `/usr/lib/libpam.2.dylib`（OpenPAM） |

`design.md` §1 第 4 条的「静态单二进制优先」在这里与 Windows 是同一种落地形式：
**macOS 上没有「全静态」这个选项**（Apple 不提供静态的 libSystem），所以标准改成
「不依赖任何需要另外安装的运行时」。SQLite 由 `libsqlite3-sys` 编进二进制，
PAM 用系统自带的那一份。

打包走 `scripts/package-macos.sh`，产出 `strixmaid-<版本>-aarch64-macos.tar.gz`，
内部布局与 Linux 的 tar.gz 同构（根目录三个二进制 + `LICENSE` + `README.md` +
`packaging/`）。脚本自己构建前端：`web/dist` 不在 git 里，而 release 下 rust-embed
在**编译期**就要读它，所以顺序是前端在 `cargo build` 之前，不能反。

出包前有三条自检，对应 Linux 侧「不产出动态链接的静态包」那条断言的意图：

1. `lipo -archs` 必须恰好是 `arm64`——既不是 universal，也没因为漏了 `--target`
   混进本机架构的产物；
2. `otool -L` 里不许出现 `/usr/local` 或 `/opt/homebrew` 下的 dylib——那是构建机
   上装的东西，目标机器没有，症状是一运行就 `dyld: Library not loaded`；
3. `codesign --verify --strict` 必须通过，不通过就 ad-hoc 重签（理由见 §7.5）。

另有一条：`strixmaid-helper` 的 `otool -L` 里必须有 `libpam`，否则认证链整条不可用。

> **名字里的 `-macos` 不是装饰**：`scripts/package.sh aarch64`（Linux 的 aarch64 包）
> 产出的是 `strixmaid-<版本>-aarch64.tar.gz`。不加后缀两者同名——今天撞不上
> （CI 的 Linux 侧只出 x86_64），但补上 Linux arm64 构建的那天，两个包会在发布页
> 上互相覆盖。Windows 侧的 `-x86_64-windows.zip` 本来就带平台名，这里跟的是同一个
> 惯例。CI 里引用该名字的地方用的是宽松通配（`strixmaid-*-aarch64*.tar.gz`），
> 所以后缀变化不需要同步改 workflow。

### 7.2 路径默认值

macOS 此前跟着 Linux 走 `#[cfg(not(windows))]`，其中 `/run/strixmaid` 是一条
**在 macOS 上根本不存在的路径**。现在单列一组（`crates/strixmaid-core/src/config.rs`），
每条的依据都写在常量的文档注释里：

| 项 | Linux | macOS | macOS 侧的依据 |
|---|---|---|---|
| 配置 | `/etc/strixmaid/config.toml` | 同左 | hier(7)：`/etc` 是 system configuration files；而且 OpenPAM 只认 `/etc/pam.d/<服务名>`，装这个软件本来就要往 `/etc` 下放东西 |
| 数据 | `/var/lib/strixmaid` | `/var/db/strixmaid` | **macOS 没有 `/var/lib`**（FHS 的东西）。hier(7) 对 `/var/db` 的定义是「misc. automatically generated system-specific database files」，这里放的正是程序生成的 SQLite 库 |
| 运行 | `/run/strixmaid` | `/var/run/strixmaid` | **macOS 没有 `/run`**。hier(7)：`/var/run` 是「system information files describing various info about system since it was booted」 |
| 二进制 | `/usr/bin` | `/usr/local/bin` | `/usr` 在只读系统卷上并受 SIP 保护，写不进去；`/usr/local` 是 SIP 明确放行给第三方的目录，默认就在 `/etc/paths` 里 |
| 服务定义 | `/etc/systemd/system/*.service` | `/Library/LaunchDaemons/*.plist` | Apple 的 Daemons and Services 指南 |
| 日志 | stderr → journald | `/var/log/strixmaid/{server,agent}.log` | launchd **不会**把作业的 stdout / stderr 送进统一日志，不设 `StandardOutPath` 就直接丢进 `/dev/null`，只能自己指定文件；`/var/log` 见 hier(7) |
| PAM 模板 | `/etc/pam.d/strixmaid` | 同左 | 见 §3.7 |

两处选择需要说明：

* **数据目录不用 `/Library/Application Support/StrixMaid`。** 那是给「应用程序」的
  支持文件准备的，Finder 里可见、会被迁移助理一并搬走；而这里是只有 root 能读的
  指标与审计库（目录 0700）。`/var/db` 更贴合它的性质，也与同在 `/var` 下的
  运行目录、日志目录保持同一套命名。
* **`/var/run` 的内容每次开机重置**，其中的子目录不能假定跨重启存在，而 launchd
  又没有 systemd `RuntimeDirectory=` 的对应物（不会替进程建目录）。当前代码路径
  其实还用不到它——helper 走 socketpair，不落文件系统 socket——保留该项只为配置
  形状三平台一致。真要用它的那天，得由进程自己 `mkdir`。

### 7.3 launchd：选 LaunchDaemon 而不是 LaunchAgent

`packaging/macos/io.strixmaid.server.plist` 与 `io.strixmaid.agent.plist`，
装到 `/Library/LaunchDaemons/`，属于**系统域**。`Label` 用反向域名式并与文件名一致
（launchd.plist(5)：「it is the expected convention for launchd property list files
to be named `<Label>.plist`」）。

选 Daemon 的三条理由：

1. **LaunchAgent 只在某个用户登录之后才存在**，注销即消失。一台没人登录的服务器上
   Agent 什么都不做——而那正是本软件要工作的场景（`design.md` §2.2：全局指标采集
   必须在无人登录时持续运行）。
2. **主进程需要 root**：spawn helper 做 PAM 认证与 setuid、读全量进程信息、改主机名
   与时区。Agent 拿到的是登录用户的权限。
3. Apple 的 Daemons and Services 指南把这条线划在「是不是特定于某个已登录用户」。

代价是 Daemon 没有用户 GUI 会话（不能弹窗、不能访问用户钥匙串），本项目都不需要。

与 `packaging/strixmaid.service` 的对应关系：

| systemd | launchd | 说明 |
|---|---|---|
| `ExecStart=` | `ProgramArguments` | 必须写绝对路径，launchd 不给继承交互 shell 的 `PATH` |
| `Restart=on-failure` | `KeepAlive = {SuccessfulExit: false}` | 退出码非零才重启；写成布尔 `true` 会连正常退出也拉起来 |
| `RestartSec=2` | `ThrottleInterval = 10` | **故意不取 2**：systemd 还有 `StartLimitBurst` 兜底，launchd 没有这一档，配置写错会永远重试，10 秒把刷日志的速度压到五分之一 |
| `TimeoutStopSec=20` | `ExitTimeOut = 20` | |
| `Environment=RUST_LOG=info` | `EnvironmentVariables` | |
| `WantedBy=multi-user.target` | 放进 `/Library/LaunchDaemons` 即等价 | |
| `ConditionPathExists=`（agent） | `KeepAlive = {PathState: ...}` | 见下 |
| `StateDirectory` / `RuntimeDirectory` | **无对应物** | 目录由 `install.sh` 建 |
| `ExecStartPre` 的配置校验 | **无对应物** | 不用 `sh -c` 绕：那样 launchd 监督的成了 sh，退出码与信号隔了一层，`KeepAlive` 的判断随之失真。改为在 `install.sh` 里校验一次 |
| `KillMode=control-group` | **无对应物** | launchd 不做 cgroup。worker 与 helper 是主进程的子进程，主进程收到 SIGTERM 后按序拆会话，路径与 Linux 相同，只是少了一层兜底清场 |
| `StartLimitIntervalSec` / `StartLimitBurst` | **无对应物** | launchd 不限重启次数，只有 `ThrottleInterval` 限频率 |

agent 那份的 `KeepAlive` 只写 `PathState`，不与 `SuccessfulExit` 并列：按
launchd.plist(5)，字典里的多个条件是**或**的关系，两个写在一起等于把「缺配置也照样
重启」那一档放了回来，而 `ConditionPathExists=` 的用意恰恰是不让它在缺配置时崩个不停。

`ProcessType`：主进程取 `Interactive`（man 页原话是这类作业「run with the same
resource limitations as apps, that is to say, none」），因为它要服务浏览器 UI 与 PTY，
还要按固定节拍采指标，被 `Background` 的 CPU / I/O 降级打到会直接表现为卡顿与采样
漂移；agent 取 `Standard`。主进程另外把打开文件数的**软**上限提到 4096——macOS 给
launchd 作业的默认值很低（`launchctl limit maxfiles` 报的第一个数，通常是 256），
几十个会话就能撞上。

### 7.4 安装与卸载

```sh
tar xzf strixmaid-<版本>-aarch64-macos.tar.gz
cd strixmaid-<版本>-aarch64
sudo packaging/install.sh            # 只注册，不启动
sudo packaging/install.sh --start    # 装完立即启动
```

脚本一律**用新式的 `launchctl bootstrap`，不用已过时的 `launchctl load`**：
后者在 plist 有问题时经常退出 0 却什么也没做，bootstrap 会把失败报出来。
注意两条命令的参数形状不同——`bootstrap` 收「域 + plist 路径」，
`bootout` 收「域/标签」：

```sh
sudo launchctl bootstrap system /Library/LaunchDaemons/io.strixmaid.server.plist
sudo launchctl bootout   system/io.strixmaid.server
sudo launchctl print     system/io.strixmaid.server      # 查状态
sudo launchctl kickstart -k system/io.strixmaid.server   # 改完配置重启
```

装完的目录与权限：

| 路径 | 权限 | 为什么 |
|---|---|---|
| `/usr/local/bin/strixmaid{,-agent,-helper}` | 0755 root:wheel | helper 由主进程（root）spawn，不需要 setuid 位。注意 macOS 上 root 的主组是 `wheel` |
| `/etc/strixmaid/` | 0755 root:wheel | 配置 0644 |
| `/var/db/strixmaid/` | 0700 root:wheel | 指标、会话与审计的 SQLite，只有主进程该碰 |
| `/var/log/strixmaid/` | 0750 root:admin | 文件由 launchd 以 0644 建出来，靠目录权限把非管理员挡在外面；管理员不必 sudo 就能 tail，与 Linux 上 journald 开放给 `adm` 组是同一个取舍 |
| `/var/run/strixmaid/` | 0700 root:wheel | 开机即清空，建出来只保证「刚装完就能用」 |

**卸载默认保留配置与数据**，与 Windows 侧的 `-Purge`、Linux 侧 deb 的
`remove` / `purge` 语义一字不差：

```sh
sudo packaging/uninstall.sh            # 停服务、注销、删二进制；配置与数据保留
sudo packaging/uninstall.sh --purge    # 连 /etc/strixmaid、/var/db、/var/log 一起删
```

理由与 Windows 侧相同：数据库里是几个月的指标历史与审计记录，配置里是运维改过的
监听地址、保留期与提权组。把「先卸掉再装个新版本」变成一次数据丢失，代价远大于
目录残留。

有一处与 Linux 不同必须说清楚：**macOS 没有「装好了但开机不自启」这一档**。
plist 一旦放进 `/Library/LaunchDaemons`，下次开机 launchd 就会装载它——这更接近
Windows 服务的「自动启动」，而不是 Linux 上「装了 unit 但没 `systemctl enable`」。
真要装而不启用，执行 `sudo launchctl disable system/io.strixmaid.server`；
那条记录写在 launchd 的覆盖数据库里，卸载不会清掉它，所以 `install.sh` 每次都会
无条件 `launchctl enable` 一次，保证「重装一遍」能修好这种状态。

### 7.5 隔离标记与代码签名（必读）

这一节说的是**两件不同的事**，经常被混在一起。

#### 隔离标记（`com.apple.quarantine`）

**本版本不做代码签名与公证**，因此：用浏览器（Safari / Chrome）下载的发布包会被打上
`com.apple.quarantine` 扩展属性，解压出来的文件继承它；Gatekeeper 在 exec 时看到这个
属性又找不到可信签名，就**直接拒绝执行**。

这是「暂不做签名公证」的直接后果，**不是 bug，也不是包坏了**。三种解法：

1. 解压后清掉整个目录的标记（推荐，一条命令）：

   ```sh
   xattr -dr com.apple.quarantine strixmaid-<版本>-aarch64
   ```

   `-d` 是删除该属性，`-r` 递归。属性本来就不存在时会报错，无妨。

2. 用 `curl` / `wget` 下载。标记是**下载它的那个 App** 打的，命令行工具不打，
   所以这条路根本不会产生隔离标记——「同一个包别人能跑我不能跑」多半就是这个原因。

3. 让系统放行一次：**系统设置 → 隐私与安全性**，在下方「安全性」一栏点「仍要打开」。
   注意 **macOS 15 (Sequoia) 起 Apple 去掉了「在访达里按住 Control 点按再打开」那条
   老捷径**，只能走系统设置。

`packaging/install.sh` 会对**装到 `/usr/local/bin` 的那三个副本**清一次标记，所以
装完之后的服务与命令行都不受影响；上面说的是「还没装、先想跑一下解压出来的二进制」
那一步。

要根除这件事只有一条路：拿 Apple Developer ID 证书签名，再用 `notarytool` 公证、
`stapler` 装订。那需要付费的开发者账号与一套 CI 上的密钥管理，本版本明确不做。

#### arm64 上的 ad-hoc 签名（与上面无关）

Apple Silicon 的内核**拒绝执行完全没有签名的二进制**，所以链接器会自动给产物打一个
ad-hoc 签名（linker-signed）。这不是身份背书，只是「让它能被执行」的最低要求。

与本项目相关的坑：release profile 开了 `strip = true`，而 `strip` 会改动 Mach-O，
**可能让那个签名失效**——症状是运行即 `Killed: 9`，且看不出任何原因。
`scripts/package-macos.sh` 因此在出包前逐个 `codesign --verify --strict`，
验不过就 `codesign --force --sign -` 重签一次（`-` 就是 ad-hoc 的意思，不涉及证书），
再验一遍，还不过就停止出包。

### 7.6 本节里没有实机验证的部分

写这一版的机器是 Windows，以下内容**只按官方文档写成，没有在 macOS 上跑过**，
第一次真机安装时要逐条确认：

- **两份 plist 能否被 launchd 接受**。做过的检查是：XML 良构（走 XML 解析器，
  顺带发现并修掉了「注释里不能出现两个连写的短横」这个会让整份 plist 解析失败的
  问题）、plist 的 dict 结构 key/value 配对、`Label` 与文件名一致。
  **没做**的是 `plutil -lint` 与真正的 `launchctl bootstrap`。
- **`KeepAlive` 的实际行为**：`SuccessfulExit: false` 对应 `Restart=on-failure`、
  `PathState` 在配置文件出现之后会不会自动把 agent 拉起来，都按 launchd.plist(5)
  推断，未实测。
- **`ThrottleInterval` / `ExitTimeOut` / `ProcessType` / `SoftResourceLimits`
  的实际效果**，同上。
- **`install.sh` / `uninstall.sh` 的运行时行为**。做过的是 `sh -n` 与 `bash -n`
  语法检查，以及「变量展开后紧跟中文」这个 bash 3.2 坑的扫描。
  `install`(1) 的 `-o root -g wheel`、`launchctl enable` 在未装载作业上的行为、
  `xattr -d` 的退出码，都没有实测。
- **`scripts/package-macos.sh` 整条链路**：`aarch64-apple-darwin` 在本机编不了
  （没有 Apple SDK），因此 `lipo` / `otool` / `codesign` 三条自检、
  `COPYFILE_DISABLE=1 tar` 的实际产物都没验证过。
- **`strip` 是否真的会打断 linker-signed 签名**：按公开资料它可能发生，
  打包脚本按「可能发生」来防。真机上第一次出包时看一眼 `codesign --verify`
  是不是真的报错，就能知道这条防护有没有触发。
- **路径常量的落地**：`/var/db/strixmaid` 与 `/var/run/strixmaid` 只在
  `aarch64-apple-darwin` 目标下单独编译验证了常量本身，没有在真机上建过这两个目录。
