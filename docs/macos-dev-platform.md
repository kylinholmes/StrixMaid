# macOS 作为开发与联调平台

> 编写日期：2026-08-27。
> 目标平台仍然只有 Linux（`design.md` §2.1 的三个产物都是 Linux 二进制）。
> 本文记录「让工程能在 macOS 上编译、运行、联调」这件事做了什么、假设了什么、缺什么。

---

## 1. 为什么要有这一层

`design.md` 的每一条采集路径都是 `/proc`、`/sys`、systemd 与 journald。
在 macOS 上开发时，若不做适配，只有两种选择：

- 全部 provider 报 unavailable —— 那么 `/processes`、`/services`、`/logs`、`/metrics`
  全是空数组，**对照 API 测试就退化成只能验证鉴权与错误码**；
- 起一台 Linux 虚拟机 —— 编辑 / 编译 / 调试的回路被拉长到不可接受。

因此选择第三条：给每个 provider 补一套 macOS 原生实现，让本机跑起来的服务返回**真实数据**。
这不改变交付目标，只改变开发回路。

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
| 磁盘 IO | ❌ | 逐设备统计要走 IOKit，成本与联调收益不匹配 |
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

> 这是**对 `design.md` §6 的补充**：与其为一个开发平台在 API 契约里加两个新字段
> （下游代码生成器全要跟着改），不如让「后端具体是谁」留在 `providers` 列表里，
> 那才是它该待的地方。`polkit` 在 macOS 上恒为 `false`——它的授权走
> Authorization Services / TCC，与 polkit 的「按 action id 询问策略」模型对不上。

## 4. 平台 API 差异的几个坑

这几条不是设计选择，是 macOS 内核 / 库与 Linux 的既有差异，**记下来以免重复踩**：

| 差异 | 后果 | 处理 |
|---|---|---|
| **Linux-PAM 与 OpenPAM 的常量数值不同** | `PAM_ESTABLISH_CRED` 在 Linux-PAM 是 2，而 2 在 OpenPAM 里是 `PAM_DELETE_CRED`——认证成功后不但没建立凭据反而把凭据删了，且 `pam_setcred` 照样返回 0 | `helper/src/pam.rs` 按平台分两套 `consts`，每项注明出处 |
| 没有 `SOCK_CLOEXEC` / `MSG_CMSG_CLOEXEC` | 只能事后 `fcntl(FD_CLOEXEC)`，存在极小的竞态窗口 | `session/framing.rs::set_cloexec`，窗口影响见其文档 |
| 没有 `MSG_NOSIGNAL` | 写到已关闭的对端会被 `SIGPIPE` 打死 | 改用 `SO_NOSIGPIPE` 套接字选项（覆盖面反而更广，连普通 `write` 也管） |
| `nix` 在 Apple 上编译掉了 `getgroups` / `getgrouplist` | 编译不过 | 直接调 libc，见 `worker/mod.rs::current_groups` 与 `helper/src/main.rs::getgrouplist` |
| `initgroups` 第二个参数是 `c_int` 而非 `gid_t` | 类型不匹配 | `helper/src/spawn.rs` 按平台取别名 |
| **`scutil --set` 对 `admin` 组成员放行，不需要 root** | 「反正非 root 会失败」这种测试会在开发者自己机器上真的改掉主机名 | 写操作的单测只测错误映射函数，绝不真的调命令 |
| **`log show` 偶尔把同一条事件吐两次**（逐字节相同、时间戳相同） | 游标随之重复，翻页时它的孪生兄弟会被边界一起漏掉 | `oslog::show` 排序后按游标相邻去重——两条本就无从区分，去重不丢信息 |
| macOS 自带 bash 3.2 | 没有 `mapfile` / `declare -A`；`set -u` 下空数组展开报错；**按字节解析变量名，`"$var（中文）"` 会把中文吃进变量名** | `scripts/*.sh` 全部兼容 3.2，细节见脚本头部注释 |

## 5. 本机验证结果（2026-08-27）

环境：Apple Silicon，macOS 26.5.2，rustc 1.98.0，非 root，当前用户在 `admin` 组。

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
