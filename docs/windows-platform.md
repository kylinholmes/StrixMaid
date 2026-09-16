# Windows 作为正式运行平台

> 编写日期：2026-09-16。
> 与 [`macos-dev-platform.md`](./macos-dev-platform.md) 记录的那一层不同，
> Windows **不是**开发平台而是交付目标：全部 provider、Windows 服务（SCM）宿主、
> 打包安装、CI 门槛都按「要经得起当真」的标准做。
> 本文记录三进程结构在这里长什么样、各 provider 的数据从哪来、
> 以及**哪些能力在这个平台上根本不存在**。

---

## 1. 这一层与 macOS 那一层不是同一件事

macOS 适配的目的是缩短开发回路，取不到的数据可以整项不做（IOKit 的磁盘 IO 就没做，
理由写的是「成本与联调收益不匹配」）。Windows 不能这么算账：它是有人会拿来跑生产的
平台，一项缺失意味着那台机器上的运维少一块视野。

因此 Windows 侧的取舍标准只有一条，就是 `design.md` §1 的第 2 条：
**能力探测而非硬依赖，缺什么如实缺席，绝不拿相近的东西冒充**。这句话在代码里的
原文写在 `crates/strixmaid-core/src/metrics/collect/windows/mod.rs` 的模块文档里，
第 4 节整节都是它的落地。

一个直接后果是：本文第 4 节比 macOS 那篇的「覆盖差异」长得多。缺失越多越要写清楚，
而不是越要藏起来。

## 2. 进程结构

### 2.1 三个进程的身份

`design.md` §2.2 的拓扑在 Windows 上原样成立，只是每一级的身份换了说法：

```
strixmaid（服务进程，SCM 拉起，身份 = LocalSystem）
  │   HTTP/WS · 前端资源 · 全局指标采集 · 存储与聚合 · 会话路由
  └─ strixmaid-helper（同一身份，主进程的子进程，每会话一个）
       │   LogonUserW 认证 · LoadUserProfileW · CreateProcessAsUserW
       └─ worker（身份 = 登录用户的令牌，每会话一个）
            ├─ 文件操作 / 进程信号
            └─ ConPTY 终端
```

| 这一级 | Linux 上的身份 | Windows 上的身份 |
|---|---|---|
| 主进程 | uid 0（root） | 服务账户，默认 `LocalSystem`（`S-1-5-18`） |
| helper | uid 0，主进程 `fork`+`exec` | 与主进程同一令牌，`CreateProcessW` 起的子进程 |
| worker（普通） | `setuid` 到登录用户 | `LogonUserW` 拿到的用户令牌，`CreateProcessAsUserW` |
| worker（提权） | 不切换身份，保持 uid 0 | UAC 的 **linked token**（完整管理员令牌） |

映射表的出处是 `crates/strixmaid-helper/src/spawn/mod.rs` 的模块文档，那里逐行列了
「这一步做什么 → Unix 怎么做 → Windows 怎么做」，包括建通道、只交一个句柄、
切身份、环境块、回收五件事。

**主进程为什么必须是 LocalSystem**：它要拿别的用户的登录令牌去 `CreateProcessAsUserW`，
而那需要 `SeAssignPrimaryTokenPrivilege` 与 `SeIncreaseQuotaPrivilege`；改系统时区
还要 `SeTimeZonePrivilege`，重启要 `SeShutdownPrivilege`
（`providers/system/windows/actions.rs`）。服务账户里只有 LocalSystem 默认带齐这些。
`install.ps1` 的 `-Account` 参数留了 `LocalService` / `NetworkService` 两个选项，
但那两个身份下提权与跨用户操作会失败，属于「只读部署」的用法。

**Unix 与 Windows 在特权模型上的根本差异**：Unix 问的是「你是不是 root」，
Windows 问的是「你的令牌里这一条特权有没有、启没启用」——管理员令牌里的
`SeTimeZonePrivilege` 默认也是**禁用**状态，用之前必须 `AdjustTokenPrivileges`
打开。这句话的原文在 `providers/system/windows/actions.rs` 的模块文档里。

### 2.2 认证链：`LogonUserW` 与 `LoadUserProfileW` 各自对应 PAM 的哪一步

`design.md` §5.4 规定的 PAM 调用序列，在 Windows 上一步一步有对应物：

| PAM（Linux / macOS） | Windows | 语义是否等价 |
|---|---|---|
| `pam_start(service, user)` | 无 | Windows 不读 `/etc/pam.d/<名字>`，没有「服务栈」这一层 |
| `pam_authenticate` | **`LogonUserW`**（`LOGON32_LOGON_INTERACTIVE` + `LOGON32_PROVIDER_DEFAULT`） | 等价：拿用户名 + 明文口令换一张令牌 |
| `pam_acct_mgmt` | **没有单独一步** | LSA 把两件事做在同一次 `LogonUserW` 里：账户禁用、锁定、过期、口令过期、登录时段 / 工作站限制、未授予登录类型，全部表现为 `LogonUserW` 失败加一个特定的 `GetLastError` |
| `pam_setcred(PAM_ESTABLISH_CRED)` | 无 | 凭据就是那张令牌本身，不需要另外「建立」 |
| **`pam_open_session`** | **`LoadUserProfileW`** | 等价：往注册表挂载用户单元（`HKCU`）、准备 `%USERPROFILE%`，让用户级的设置对 worker 可见 |
| `pam_close_session` | `UnloadUserProfile` | 同上，必须与 `LoadUserProfileW` 在同一进程、同一令牌上成对调用 |
| `pam_end` | `CloseHandle(令牌)` | |

`LogonUserW` ↔ `pam_authenticate`、`LoadUserProfileW` ↔ `pam_open_session` 这两对是
理解 Windows 认证链的钥匙。`strixmaid_types::ipc` 里 `SpawnWorker { open_session }`
这个字段的文档就是按这个对应写的：「会话是否已建立（Unix 的 `pam_open_session` /
Windows 的 `LoadUserProfileW`）」。

**为什么是交互式登录类型**：`LOGON32_LOGON_NETWORK` 拿不到能加载配置文件的令牌，
`LOGON32_LOGON_BATCH` 要求另一项用户权限且组成员关系的展开方式不同。worker 要做的是
「以这个人的身份在这台机器上干活」，交互式才是对的语义。helper 自己没有交互式桌面
不影响这一点——登录类型描述的是**被登录的那个账户**要拿一张什么样的令牌，与调用方
是谁无关。

**部署时最常踩的一脚**：目标账户必须有本机的「允许本地登录」
（`SeInteractiveLogonRight`），否则 `LogonUserW` 以 `ERROR_LOGON_TYPE_NOT_GRANTED`
失败。域策略把这项权限收紧的机器上，密码完全正确也登不进去，因此这个错误码单独
翻译了一句可操作的提示。

**`LoadUserProfileW` 失败时降级继续**：它另需 `SeBackupPrivilege` 与
`SeRestorePrivilege`（LocalSystem 与管理员都有）。拿不到时按 Unix 侧
`pam_open_session` 的老规矩降级继续，只是用户级的环境变量不到位。

**认证失败的措辞是合并的**：`ERROR_LOGON_FAILURE`（口令错）、`ERROR_NO_SUCH_USER`
（没这个账户）与 `ERROR_NONE_MAPPED`（名字解析不到 SID）翻译成**同一句话**，
且不把错误号带出去——区分它们等于给攻击者一个枚举账户的口子。

**helper 为什么仍然必须是每会话一个、活到登出**：理由与 PAM 时代完全相同。
`LoadUserProfileW` / `UnloadUserProfile` 要在同一进程、同一令牌上成对调用，
登录令牌也要有人一直持有着——持有者退出，令牌就没了，worker 再也换不了身份。

**`pam_service` 配置项在 Windows 上无意义**。字段保留只是为了让配置形状在三个平台上
一致（下游的配置管理不必按平台分叉），helper 收到后直接忽略。这一条写在
`crates/strixmaid-core/src/config.rs` 的 `DEFAULT_PAM_SERVICE` 文档里，
`Config::example_toml()` 生成的 Windows 版示例配置里也有同样的提示行。

**明文口令的处理没有因为换平台而放宽**：`design.md` §5.3 的三条硬约束照旧——
只在认证那一瞬间存在于内存、绝不进日志、绝不入库，类型层面由 `Zeroizing<String>` 强制。

### 2.3 IPC 通道与附件：为什么是「读端去拉」

| | Unix | Windows |
|---|---|---|
| 通道 | `socketpair(AF_UNIX, SOCK_STREAM)` 的一端 | 一条命名管道实例 |
| 附件 | fd，走 `SCM_RIGHTS` 带外传递 | `HANDLE`，走 `DuplicateHandle` |
| 身份证明 | 父子关系（fd 是继承来的） | `GetNamedPipeClientProcessId` 对上子进程 pid |

这张表与下面的论证都出自 `crates/strixmaid-core/src/session/channel.rs` 的模块文档。

Unix 的 `SCM_RIGHTS` 是**写端推**：发送方把 fd 塞进控制消息，内核在接收方建一个新 fd。
Windows 没有这种带外通道，只有 `DuplicateHandle`，而它需要**对另一个进程的
`PROCESS_DUP_HANDLE` 权限**。谁有这个权限，看一眼进程拓扑就清楚：主进程是权限最高的
那个，而**附件永远朝主进程流动**（helper 把 worker 通道交给主进程、worker 把终端通道
交给主进程）。

所以 Windows 上一律由**读端**（主进程）`OpenProcess(PROCESS_DUP_HANDLE)` 打开写端进程、
把句柄拉过来，并用 `DUPLICATE_CLOSE_SOURCE` 顺手关掉源端那一份。

反过来做——让 worker 往主进程里推句柄——需要给 worker 开 `PROCESS_DUP_HANDLE`
到一个 SYSTEM 进程上，**那等于把提权漏洞写进设计**。这是选择读端拉取的全部理由：
不是因为 API 更顺手，而是因为另一个方向在安全上不可接受。

这条不对称性是 Windows 侧唯一与 Unix 语义不同的地方，因此 `PeerProcess` 在 Unix 上是
个零大小的空壳，在 Windows 上才真的持有句柄。

一个随之而来的硬性纪律：**写端发出附件后不能 drop 自己那一份**。句柄已被内核关掉，
值可能已被复用，再 `CloseHandle` 会误关无关对象——用 `session::channel::release_sent`
或 `std::mem::forget`。

帧格式与 `design.md` §10 一致（长度前缀 + 附件计数 + JSON）。附件计数这一字段在
Windows 上同样必要：读端要知道该不该去拉、拉几个，收到的个数与帧头不符即为协议错误。

### 2.4 SID 怎么变成 `uid: u32`

API 契约里的身份字段是 `u32`（`design.md` §5.2、§9.1），那是 Unix 的 uid。
Windows 的身份是 **SID**，一个变长结构，塞不进 32 位。

规则是取 SID 的**最后一段子权威（RID）**，并做一处映射
（`crates/strixmaid-core/src/platform/windows/token.rs` 的模块文档）：

| SID | RID | 本项目的 uid | 理由 |
|---|---|---|---|
| `S-1-5-18`（LocalSystem） | 18 | **0** | 它就是 Windows 的 root：`capability::derive_user_caps` 的 `uid == 0` 判断、审计里的「以最高权限执行」都靠这一条 |
| `S-1-5-21-…-500`（内建 Administrator） | 500 | 500 | 是管理员但不是 SYSTEM，不该冒充 uid 0 |
| 普通本地用户 | 1001+ | 1001+ | 与 Linux 的 1000+ 同一量级 |

**RID 在域环境下不是全局唯一的**（两个域里都可能有 RID 1105）。这不影响本项目：
uid 只用于同一台机器内的展示与比对，跨节点的身份映射按 `design.md` §11 本来就不做。
真正权威的标识是 `TokenIdentity::sid` 里的完整 SID 串，需要精确比对的地方
（如账户缓存键）一律用它。

**组名要规范化**。`may_elevate` 按组名判断提权资格，而 `session.elevate_groups` 是配置
里写死的英文名；可内建组的显示名却是**本地化**的：德文 Windows 上 `S-1-5-32-544` 叫
`Administratoren`。若直接用 `LookupAccountSidW` 的结果，德文机器上提权会静默地对所有人
关闭。因此对已知的内建 SID 额外补一个英文规范名，两个名字都进 `groups`——
本地化名给人看，规范名给判断用。

同一个理由也决定了 `install.ps1` 里的 ACL 一律按 SID 授权而不是组名。

### 2.5 提权（`as_root`）= UAC 的 linked token

Unix 上提权是「不切换身份，保持 root」；Windows 上是**换一张令牌**。

UAC 下的管理员登录会产生一对令牌：一张过滤过的（日常使用，`Administrators` 组被标成
`SE_GROUP_USE_FOR_DENY_ONLY`），一张完整的（linked token）。`as_root` 的 worker 用的
就是后者。

两处由此而来的实现约束：

* **判定提权资格时，被标成 `SE_GROUP_USE_FOR_DENY_ONLY` 的组也算数**。提权资格问的是
  「这个账户属不属于管理员组」，而不是「当前这张令牌现在有没有管理员权限」——后者是
  `is_elevated` 的事。漏掉它会让每个管理员都提不了权
  （`platform/windows/token.rs::group_names`）。
* **`uid` 恒等于 `euid`**。Windows 没有「有效用户」这一层：一个进程的令牌就是它的全部
  身份，不存在 setuid 那种「实际身份与有效身份分开」的状态。提权在这里是另一个令牌、
  进而是另一个进程，而不是同一个进程的两套 id（`worker/mod.rs::whoami`）。

会话层的语义不变：提权是独立的、更短的空闲超时（默认 300s，对齐 sudo 的
`timestamp_timeout`），超时只回收 admin worker，会话本身仍然存活。

### 2.6 终端 = ConPTY + 作业对象

`CreatePseudoConsole`（Windows 10 1809 起）给出一个伪控制台，交给它两根管道，
转义序列与控制台缓冲区由它代为处理。细节在
`crates/strixmaid-core/src/worker/terminal/windows.rs` 的模块文档，四条要点：

1. **建完 ConPTY 必须立刻放手自己那两个管道端**。`CreatePseudoConsole` 已经复制了自己
   的一份，再留着的话 shell 退出时读端等不到 EOF。
2. **关终端靠作业对象**。Unix 上是 `killpg` 杀整个进程组；Windows 上每个终端配一个
   **作业对象**，shell 一创建就被塞进去，它此后起的所有子孙进程自动继承，
   `TerminateJobObject` 就是 `killpg` 的等价物。
3. **顺序是先关 ConPTY 再终止作业**。反过来就永远拿不到 shell 的正常退出码。
4. **退出状态里没有 `signal`**。进程终止只有一个 32 位退出码，没有「被哪个信号杀死」
   这一维；被强杀的进程退出码是传进去的那个常量值，如实报在 `code` 里。

**不做跨用户的终端**。admin worker 要以别的用户身份开终端时需要那个用户的令牌，
而拿令牌需要凭据（`LogonUserW`）或 `SeTcbPrivilege`，worker 两样都没有。
`TermOpenParams::user` 与自身不符时返回 `PermissionDenied` 并说清原因。
这不是缺功能：正确做法是让主进程走完整的认证链（helper `LogonUserW` → 新 worker），
而那条路已经存在——主进程按 `session.elevated` 派给哪个 worker，派过来的 worker
本身就是目标身份。与 Unix 上 admin worker 靠 root 特权直接切的区别，只是切换发生在
更早的一步。

---

## 3. 数据源对照表

`design.md` §1 的第一条原则是「优先从 `/proc`、`/sys`、netlink、`/etc` 直读」。
Windows 上没有这些，对应的直读入口是：`NtQuerySystemInformation`（进程与处理器）、
注册表（静态描述信息）、IOCTL（块设备）、IP Helper（网络）、SCM（服务）、
事件日志 API（日志）。

**除 GPU 外一律不走 PDH**（性能计数器）：它要两轮采集、每轮要走一遍计数器名解析，
而 CPU / 内存 / 磁盘 / 网络都有更直接、更便宜、且与内核数据结构同源的入口。
GPU 是唯一没有别的路的那一项。理由原文在 `platform/windows/pdh.rs` 的模块文档。

**一律不走 WMI**：`Win32_LogicalDisk` / `Win32_DiskDrive` 要起 COM、连 WMI 服务，
而 WMI 在负载高时可能几秒才应答——一个每 2 秒采集一轮的指标管线不能赌这个
（`platform/windows/volume.rs`）。

### 3.1 指标采集器（`metrics/collect/`）

| 采集项 | Linux | Windows |
|---|---|---|
| CPU | `/proc/stat` 的 `cpu` / `cpuN` 行 | `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`，逐核 idle / kernel / user / dpc / interrupt 五项 |
| 内存 | `/proc/meminfo` | `GlobalMemoryStatusEx`（总量 / 可用）+ `GetPerformanceInfo`（`SystemCache × PageSize`）+ `NtQuerySystemInformation(SystemPagefileInformation)`（页面文件） |
| 负载 / 进程数 | `/proc/loadavg` | 进程表里的**线程状态**（`SystemProcessInformation`）：`Running` + `Ready`/`Standby` → `procs.running` |
| PSI | `/proc/pressure/*` | 无 |
| 磁盘 IO | `/proc/diskstats` + `/sys/block` | `IOCTL_DISK_PERFORMANCE` 的 `DISK_PERFORMANCE`；盘按 `\\.\PhysicalDriveN` 枚举 |
| 文件系统 | `/proc/self/mounts` + `statvfs` | `FindFirstVolumeW` + `GetVolumePathNamesForVolumeNameW` + `GetDiskFreeSpaceExW` + `GetDriveTypeW` |
| 网络 | `/proc/net/dev` | `GetIfTable2` 的 `MIB_IF_ROW2` |
| GPU | `/sys/class/drm/card*/device/*` | PDH：`\GPU Engine(*)\Utilization Percentage`、`\GPU Adapter Memory(*)\Dedicated Usage`（经 `PdhAddEnglishCounterW`）；显存总量读注册表显示类键的 `HardwareInformation.qwMemorySize` |

### 3.2 主机信息（`providers/system/`）

| 字段 | Linux | Windows |
|---|---|---|
| 系统版本 | `/etc/os-release` | 注册表 `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion` |
| 主机名 | — | `GetComputerNameExW(ComputerNamePhysicalDnsHostname)` |
| `machine_id` | `/etc/machine-id` | 注册表 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` |
| CPU 型号 | `/proc/cpuinfo` | 注册表 `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0` |
| CPU 拓扑 | `/sys/devices/system/{cpu,node}/*/topology` | `GetLogicalProcessorInformationEx(RelationAll)` |
| 机型 / BIOS / 序列号 | `/sys/class/dmi/id/*` | 注册表 `HKLM\HARDWARE\DESCRIPTION\System\BIOS` |
| 虚拟化识别 | `/proc/1/cgroup`、`/sys/hypervisor/type`、DMI… | **CPUID**：`CPUID.1:ECX[31]` + `CPUID.0x40000000` 的厂商串；退回 SMBIOS 字段 |
| 物理盘 | `/sys/block` | `\\.\PhysicalDriveN` + `IOCTL_STORAGE_QUERY_PROPERTY` + `IOCTL_DISK_GET_LENGTH_INFO` |
| 卷 ↔ 物理盘 | `backing_dev` | `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` |
| GPU 拓扑 | `/sys/class/drm/card*` | 注册表显示适配器类 `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-…}\NNNN` |
| 网卡拓扑 | `/sys/class/net/*` + `getifaddrs` | `GetAdaptersAddresses` + `GetIfTable2`，按接口索引关联 |
| 开机时刻 | `/proc/stat` 的 btime | `NtQuerySystemInformation(SystemTimeOfDayInformation)` |
| 时区 | `/etc/localtime` | `GetDynamicTimeZoneInformation` |
| NTP 状态 | `adjtimex(2)` + 配置文件 | 注册表 `…\Services\W32Time\Parameters` 的 `Type` |
| 需要重启 | `/run/reboot-required` + 内核版本比较 | 两个注册表键是否存在：CBS 的 `RebootPending`、Windows Update 的 `RebootRequired` |
| 改主机名 / 时区 / 重启 | 直写 / 换软链 / `systemctl` | `SetComputerNameExW` / `SetDynamicTimeZoneInformation` / `InitiateSystemShutdownExW` |

### 3.3 进程（`providers/process/`）

Linux 遍历 `/proc` 逐个读 `stat`；Windows 是**一次** `NtQuerySystemInformation(SystemProcessInformation)`
拿到全表（pid、ppid、名字、线程数与状态、启动时刻、CPU 时间、工作集、虚拟大小、
基础优先级、IO 计数）。

| 字段 | Linux | Windows |
|---|---|---|
| `state` | `/proc/<pid>/stat` 第 3 字段 | 由**线程**状态推导 |
| `nice` | 内核 nice 值 | 由基础优先级反映射 |
| `uid` / `user` | `/proc/<pid>` 的属主 | 逐进程 `OpenProcess` → `TokenUser` 的 SID → RID；名字走 `LookupAccountSidW`（按 SID 缓存） |
| `cmdline` | `/proc/<pid>/cmdline` | `NtQueryInformationProcess(ProcessCommandLineInformation)` |
| `cwd` / `environ` | `/proc/<pid>/{cwd,environ}` | 读目标进程 **PEB**（`PROCESS_VM_READ` + `ReadProcessMemory`） |
| `unit` | `/proc/<pid>/cgroup` → systemd unit | SCM 的服务宿主进程 pid 反查 |
| 发信号 | `kill(2)` | `TerminateProcess`（见 §4.3） |

### 3.4 服务（`providers/service/`）

| systemd | Windows SCM |
|---|---|
| `ListUnits` / `ListUnitFiles`（zbus 或 `systemctl`） | `EnumServicesStatusExW` |
| unit 名 | 服务名 + `.service` 后缀 |
| `Description` | `QueryServiceConfig2W(SERVICE_CONFIG_DESCRIPTION)`，退回 `DisplayName` |
| active / inactive / failed | `dwCurrentState` + `dwWin32ExitCode` |
| enabled / disabled / static | `dwStartType`；列表路径改从注册表 `…\Services\<名字>\Start` 批量读（不是近似，是同一份数据的另一个读法） |
| 依赖图 | `QueryServiceConfigW` 的 `lpDependencies` + `EnumDependentServicesW` |
| unit 文件 | 注册表键 `HKLM\SYSTEM\CurrentControlSet\Services\<名字>` 渲染成 ini 形态 |
| `PropertiesChanged` 信号 | 没有总线，改轮询 + 差分（见 §4.4） |
| 启停 / 改启动类型 | `StartServiceW` / `ControlService` / `ChangeServiceConfigW` |

**为什么服务名要加 `.service` 后缀**：Windows 服务名里带点的一大把
（`NVDisplay.ContainerLocalSystem`），直接用会让取自最后一段的 `unit_type` 变成
`ContainerLocalSystem` 这种垃圾值，`?type=service` 过滤随之报废。
代价是名字长七个字符，换来 API 契约在三个平台上完全一致——与 macOS 侧
launchd label 的处理是同一个决定。

**服务句柄按需申请最小权限**：一上来就申请 `SERVICE_ALL_ACCESS` 会让非管理员
**连服务列表都拉不出来**。

### 3.5 日志（`providers/log/`）

| journald | Windows 事件日志 |
|---|---|
| `journalctl --output=json` | `EvtQuery`（`EvtQueryChannelPath` + XPath）+ `EvtNext`，反向读 |
| `journalctl -f` | `EvtSubscribe(EvtSubscribeToFutureEvents)`，内核推送 |
| `MESSAGE` | `EvtOpenPublisherMetadata` + `EvtFormatMessage`，渲染不出时退回 `EventData` 拼接 |
| 原始记录 | `EvtRender(EvtRenderEventXml)` |
| `PRIORITY` | `Level`（0→Notice、1→Crit、2→Err、3→Warning、4→Info、5→Debug） |
| `_SYSTEMD_UNIT` / `SYSLOG_IDENTIFIER` | `Provider@Name` |
| `_PID` / `_UID` | `Execution@ProcessID` / `Security@UserID` 的 SID→RID |
| `_TRANSPORT` | 通道名（`System` / `Application` / `Security`） |
| `_HOSTNAME` | `Computer` 元素 |
| `journalctl --vacuum-*` | `EvtClearLog`（只有整清一档，见 §4.5） |

### 3.6 文件（`providers/fs/`）

| 字段 | Unix | Windows |
|---|---|---|
| 目录枚举 | `read_dir` | `read_dir` / `FindFirstFileW` |
| `mode` | `st_mode` 低 12 位 | **合成值**，只从「是不是目录」与「有没有只读属性」两位合成（见 §4.6） |
| `uid` / `gid` | 真实 uid / gid | 属主 / 属组 SID 的 RID |
| `user` / `group` | NSS | `GetNamedSecurityInfoW` + `LookupAccountSidW` |
| `mtime_ts` | `st_mtime` | 最后写入的 `FILETIME` |
| 根 | 唯一的 `/` | 每个驱动器一个根；裸 `\` 是「全部驱动器」这个**虚拟根** |

---

## 4. Windows 上拿不到 / 不适用的能力

**本节的每一条都以代码里已经写明的理由为准。** 缺失一律如实上报（`None` / 空列表 /
`capability_unavailable`），不拿相近的东西冒充；`GET /metrics/series` 与各 DTO 里的
`Option` 就是「有没有」的唯一事实来源。

指标层还有一道机械化检查：`metrics/collect/windows/mod.rs` 里有单测钉死
「本平台没有的那几项，**任何**采集器都不该产出」，名单是 `cpu.iowait`、`cpu.steal`、
`load.1m`、`gpu.temp`、`gpu.mem_alloc` 以及五条 PSI。

### 4.1 指标

| 能力 | 状态 | 原因 |
|---|---|---|
| `cpu.iowait` | **没有** | Windows 不把「等 IO 的那段空闲时间」与普通空闲分开统计：线程等 IO 时被挂起，那颗核上跑的是空闲线程，内核只记 `IdleTime`。拿 `\LogicalDisk(_Total)\% Disk Time` 冒充是磁盘忙碌度而不是 CPU 时间构成，两者不是一回事 |
| `cpu.steal` | **没有** | 「被宿主机偷走的时间」需要 guest 内核与 hypervisor 协作记账（Linux 的 `steal` 来自 paravirt 时钟）。Windows guest 没有对应概念，Hyper-V 的宿主机侧计数器 guest 里读不到 |
| PSI（五条） | **没有** | `/proc/pressure` 是 Linux 独有的内核特性，无任何等价物 |
| `load.1m` | **没有** | NT 内核根本不维护负载均值那样的累加器。`\System\Processor Queue Length` 是**瞬时**队列长度，既不是均值也不含等待 IO 的线程；自己在用户态维护一个 EMA 同样不行——那只反映采集器自己的采样节奏，重启即断档，且与任何系统工具都对不上 |
| `gpu.temp` | **没有** | 没有统一的 GPU 温度接口：NVIDIA 要 NVML、AMD 要 ADL、Intel 要 IGCL，三家各一套私有 DLL 且不保证装了驱动就有；WDDM 的性能计数器里没有温度项。与其「装了某家驱动才有」，不如统一缺席 |
| `gpu.mem_alloc` | **不适用** | 那是统一内存架构（Apple Silicon）下「显存条的分母」，独显有真正的 `mem_total` |
| `gpu.mem_total`（多卡时） | **可能整条不产出** | 注册表里的适配器子键号与 PDH 的 `phys_N` 之间没有任何官方对应关系。只有两边数目**相等**时才按排序位置配对，对不上就一条都不产出 |
| 分卡 GPU 曲线 | **可能被合并** | 两块独立适配器的 PDH 实例名里都是 `phys_0`（那是同一适配器内的物理 GPU 序号，不是全局序号），会合并成一条 `gpu=0` 曲线。要分卡需要一张 LUID → 稳定编号的持久映射表，不在 P0 范围 |
| GPU 曲线的时间分辨率 | **降级** | PDH 每 10 秒才真查一次，中间复用上次样本，曲线在刷新间隔内是阶梯状 |
| 超过 64 逻辑处理器 | **只覆盖一个处理器组** | `SystemProcessorPerformanceInformation` 只覆盖调用线程所在的处理器组。少报的核**不产出样本**，而不是报 0——缺席是缺席，不是「这颗核 100% 空闲」 |
| `mem.swap_*`（无页面文件时） | **两条都不产出** | 与 Linux 上没有 swap 分区一样，不是错误 |
| 非固定盘的 `fs.*` | **不采** | 网络盘：对断开的 SMB 挂载做 `GetDiskFreeSpaceExW` 会阻塞到超时，把整轮采集卡死（与 Linux 排除 `nfs`/`cifs` 同因）；光驱：容量随碟片跳变；可移动盘：插拔即整条 series 生灭；RAM 盘：容量恒定 |
| `DiskInfo` 的容量来源 | 非管理员下改走几何信息 | `IOCTL_DISK_GET_LENGTH_INFO` 要 `FILE_READ_ACCESS`，非管理员打得开设备却过不了这个 IOCTL。因此容量优先问 `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX`（`FILE_ANY_ACCESS`），两者能取到结果时给的是同一个数。这不是优化而是功能正确性：顺序颠倒会让非管理员下 `physical_disks()` 恒为空表 |
| `disk.await`（无 IO 的轮次） | **不产出** | `Δ操作数 == 0` 时平均等待时间没有定义。产出 0 会把「这一秒没有 IO」画成「这一秒的 IO 零延迟」 |

### 4.2 主机信息与健康

| 能力 | 状态 | 原因 |
|---|---|---|
| `cpu.quota_cores` | **没有** | 没有 cgroup。作业对象的 `CpuRateControl` 只约束本进程树，且以「周期百分比」而非「几个核」计量 |
| CPU 封装 id | **只能按枚举顺序编号** | `PROCESSOR_RELATIONSHIP` 不带封装 id，没有 `physical_package_id` 的对应物 |
| `NetInfo::duplex` | **没有** | NDIS 的 `OID_GEN_LINK_STATE` 是驱动级 OID，要发 IOCTL 且需要管理员；`MIB_IF_ROW2` 与 `IP_ADAPTER_ADDRESSES` 都没有这一项。不拿「速率大于 0 就算全双工」冒充 |
| `NetInfo::driver` | **语义不同** | Windows 没有用户可见的「内核驱动名」，填的是适配器描述串（`Intel(R) Ethernet Connection I219-V`） |
| `GpuInfo::bus`（PCI 地址） | **没有** | 显示类键里只有 `MatchingDeviceId`（`VEN:DEV`），那是「厂商 id + 设备 id」不是总线位置，同型号两张卡完全一样。`LocationInformation` 是一句本地化的自然语言，凑成 BDF 得先假定语言 |
| GPU 的内核模块名 | **没有** | Windows 没有 `amdgpu` / `i915` 那种东西 |
| `DiskInfo::smart_healthy` | **P0 不做** | 读 SMART 要管理员级别打开设备，而健康报告在非特权下也必须能出。`skipped` 里如实标出 `smart` |
| `inodes_total` / `inodes_used` | **概念不存在** | NTFS / ReFS / exFAT 没有 inode。MFT 记录数随卷动态扩张，没有「用完就建不了文件」的固定上限，拿它冒充只会让前端算出一个没有意义的百分比 |
| `timezone` 为 IANA 名 | **做不到** | Windows 有自己一套时区标识（`China Standard Time`），与 IANA **不是**一一对应；注册表里没有任何 IANA 标识，ICU 的转换函数要链 `icu.dll`。这是本项目与 DTO 注释的一处明确偏离 |
| `ntp_synchronized` | **没有** | W32Time 没有可读的「已同步」位；`w32tm /query /status` 的时刻是服务经 RPC 现算的，不落注册表。读不到就报 `None`，**不编 `false`**——那会被显示成「时钟未同步」 |
| 健康证据 `load1` | **恒为 `None`** | 同 `load.1m`。`build_report` 因此跳过 `load.high` 这一条 |
| `RebootReason::NewerKernel` | **不适用** | Windows 上不存在「装了新内核但还没重启到它」这种可独立观测的状态，系统更新是原子的 |
| `pretty_hostname` | **概念不存在** | 计算机描述（`srvcomment`）只在网上邻居里显示，语义与生命周期都对不上。传了这一项时返回 `capability_unavailable`，**而且在改主机名之前就返回**——半成功比整个失败更难解释 |
| 独立的「内核版本」 | **不存在** | NT 内核与系统同版本发布 |
| 健康报告 `skipped` | `["scm", "smart"]` | SCM 的失败服务归 service provider 报，这里重复检查只会让两处口径打架 |

改主机名还有一处**生效时机**的差异：改完之后 `hostname` 报的仍然是旧名字
（读的是活动名），直到机器重启。这与 Linux 的 `sethostname(2)` 立即生效完全不同，
前端应在改名成功后提示「重启后生效」。

### 4.3 进程

| 能力 | 状态 | 原因 |
|---|---|---|
| `tty` | **没有** | Windows 的控制台是内核对象，不是 `/dev/pts/0` 那样的设备文件，没有可展示的等价名字 |
| `cgroup` | **没有** | 资源限制的对应物是作业对象，但它不是一棵可反查的层级路径 |
| `fds` | **不做** | 句柄表只能靠 `NtQuerySystemInformation(SystemHandleInformation)` **全局**枚举再按 pid 过滤，一次要拉几十万条记录，代价与收益不匹配 |
| `cwd` / `environ`（WOW64 / 受保护进程） | **`None`** | 32 位目标的 PEB 布局与 64 位不同，偏移不能通用。位数不一致时直接报 `None`，绝不按猜来的偏移读一段内存当结果 |
| `euid` / `egid` | **概念不存在** | 只有一张主令牌，`euid` 恒等于 `uid`（不是拿相近的东西冒充，这个平台就只有一个身份） |
| `SIGHUP`（重载配置） | **501** | 对应物是 `ControlService(SERVICE_CONTROL_PARAMCHANGE)`，那属于 service provider 且只对声明接受该控制码的服务有效。进程侧没有任何东西能对应，如实报 `capability_unavailable` 并在 `detail` 里指路 |
| **优雅终止** | **没有** | `term` 与 `kill` **都是**强制终止。`GenerateConsoleCtrlEvent` 只对同一个控制台里的进程有效，`WM_CLOSE` 只对有窗口的进程有效，`SetEvent` 要双方事先约好一个具名事件——三者都不是「对任意 pid 都成立」的通用机制。**不要以为 `term` 更温柔**：目标连一行清理代码都跑不到，与 `SIGKILL` 完全一样 |

两处**部分可得**：

* **进程身份的可读率**。实测 301 个进程里 204 个的身份读得出来（非提升会话），
  其余是受保护进程与别的用户的进程，`OpenProcess` 直接被拒。默认值是
  `uid = 0` 且 `user = None`，**两者要一起看**——`user = Some("SYSTEM")` 才表示真的是
  LocalSystem，`user = None` 表示身份读不出来。前端应把 `user == None` 画成「—」
  而不是把 `uid == 0` 当成 SYSTEM。
* **`unit`（进程归属服务）**。有等价物，但 `svchost.exe` 可以同时托管若干服务，
  这时 pid 无法唯一对应到一个服务，报 `None`；只有「这个进程就是那一个服务」时才给出。

一处**语义不同**值得单独记：**IO 计数的口径**。Linux 的 `/proc/<pid>/io` 区分
`rchar`（所有读）与 `read_bytes`（实际发往块设备的字节），provider 取的是后者；
Windows 的 `ReadTransferCount` / `WriteTransferCount` **把文件、管道、设备 IO 算在一起**，
没有「只算磁盘」的那一档。后果是：同一个进程做同样的事，Windows 报出的数字通常
**大于** Linux；一个只读写命名管道、完全不碰磁盘的进程在 Linux 上速率是 0，
在 Windows 上不是。这是内核只记这一种计数的结果，不是实现上的取舍。

还有一条纪律性的细节：**pid 0（System Idle Process）必须排除**。它在进程表里有
每颗逻辑处理器一个空闲线程，而空闲线程在那颗核空着时状态正是 `Running`。
不滤掉它，一台 16 核的闲置机器会报出 `procs.running ≈ 16`——看上去像满载，实际满闲。

### 4.4 服务

| 能力 | 状态 | 原因 |
|---|---|---|
| drop-in | **没有** | Windows 没有「同一份定义被多个文件分层覆盖」的机制，`drop_ins` 恒为空 |
| cgroup 用量 | **没有** | 没有 cgroup 这一层，`UnitDetail::cgroup` 为 `None` |
| `NRestarts` / `ActiveEnterTimestamp` / `StateChangeTimestamp` | **没有** | SCM 不保存「上次进入运行态的时刻」，也不保存「累计重启了几次」。`SERVICE_CONFIG_FAILURE_ACTIONS` 只描述**将来**失败时怎么办，不是已发生次数的计数器。能凑的近似都不诚实：主进程创建时刻是「进程起来的时刻」而非「服务进入运行态的时刻」 |
| `Documentation=` | **没有** | 恒为空数组 |
| `--user` 作用域 | **不存在** | SCM 只有一张全机服务表。那些名字带随机后缀的「每用户服务实例」不是等价物——由系统按模板自动派生，用户既不能自己装一个也没有管理入口。收到 `scope=user` 时返回 `capability_unavailable`，不拿系统服务改个标签冒充 |
| `*.timer`（定时任务） | **返回空表** | 对应物是计划任务（Task Scheduler），确实读得到，但 `TimerSource` 枚举目前只有 `SystemdTimer` 与 `Launchd`，**没有 Windows 的**。塞进那两个里的任何一个都是撒谎，加变体是 API 契约改动，契约补齐后再实现 |
| 变更事件的推送 | **没有总线** | `NotifyServiceStatusChangeW` 要对**每个服务**单独注册 APC 回调（三百个服务就是三百次注册、三百个常驻句柄，且每次回调后必须重新注册）；SCM 级的通知只报服务的创建与删除，不报状态变化，正好不是要的。因此改用轮询 + 差分 |
| 操作的异步追踪句柄 | **没有** | SCM 没有 job 对象，控制码是同步下发的 |
| 依赖图的边类型 | **只有一种** | SCM 只有「启动前需要谁」这一种边 |

两处口径：`exit_code` 只在**已停止**时才给——服务运行中 `dwWin32ExitCode` 恒为 0，
那 0 是「当前没有错误」而不是「上次以 0 退出」，报成 `success` 就是编数据；
从未启动过同样给 `None`。`enable_state` 读不到时是 `None`——那是「不知道」，
不是「未启用」。

### 4.5 日志

| 能力 | 状态 | 原因 |
|---|---|---|
| `Security` 通道 | **需要管理员** | 读不到是 Windows 的常态而不是故障，只在 debug 日志里记一笔、跳过它继续。反过来，若**一个通道都打不开**就是真出问题了，如实报错——绝不把读取失败伪装成「这段时间没有日志」 |
| 历史 boot 的日志 | **做不到** | 事件日志里没有 boot id，每条事件只知道自己的时刻。`LogQuery::boot` 只接受 `"0"` 或当前这次启动的 id，其余如实返回 `capability_unavailable`，不偷偷返回全部日志 |
| `_BOOT_ID` 的语义 | **派生值** | 由开机时刻派生出一个 32 位 hex 串，它**不是** Windows 提供的标识，只是一个「同一次启动内稳定、重启后必然不同」的占位值，跨机器比对没有意义。开机时刻之前的事件 `boot_id` 留 `None` |
| vacuum 的模式 | **只有 `EraseAll`** | 可以按通道清空（`EvtClearLog`），但没有「收缩到 N 字节」或「只保留最近 N 天」——通道的 `MaxSize` / `Retention` 是**写入策略**，改了不会立刻回收空间，拿它冒充 vacuum 是谎报 |
| `priority=emerg` / `alert` | **必然为空** | 事件日志最严重的一档就是 Critical，syslog 的 Emerg / Alert 没有对应物。由 `LevelFilter::Impossible` 如实表达，不偷偷放宽成 Critical |
| `q` 全文关键字下推 | **下推不了** | XPath 表达不了任意子串匹配，只能拉回来逐条比（与 journalctl 没有 `--grep` 时的处境相同） |
| 含引号的 provider 名下推 | **下推不了** | 事件日志的 XPath 子集不支持 `concat()`，而 XPath 1.0 的字符串字面量本身没有转义机制，`'` 无法表示。含引号的值一律退回进程内过滤，不拼进 XPath |
| 按 id 取一条 | **没有这个接口** | `EventRecordID` 只在单通道内唯一 |

`EventRecordID` 之所以不能当游标，有两条硬理由：一是只在单通道内唯一，撞了之后翻页按
「严格早于游标」裁剪会把同键的条目**整组丢掉**——静默漏日志；二是通道被 `EvtClearLog`
清空后 RecordID 回到 1，比清空前的游标都小，翻页会直接翻到「没有更多」。

正文渲染不出来时**退化成 `EventData` 拼接**：发布者没在本机注册（软件被卸载但日志还在）、
消息 DLL 缺失、当前语言没有对应消息，这三种都是正常情况。退化后的正文只有裸参数，
但参数本身是真的——比空白强，也不是编出来的。

### 4.6 文件

| 能力 | 状态 | 原因 |
|---|---|---|
| `mode`（权限位） | **合成值** | Windows 的访问控制是 ACL，没有 `rwxrwxrwx` 这三组九位。近似在三处：与 ACL 无关（一个 `0o644` 的文件可能因 ACL 连属主自己都读不了）；组位与其他位是**编出来给格式化用的**；目录的只读属性在 Windows 上并**不**表示不可写（资源管理器用它标记自定义图标/视图）。**要不要能读，唯一的答案是真去读一次** |
| `kind` | **只有四种** | `Dir` / `File` / `Symlink` / `Unknown`，Linux 侧七种齐全 |
| 符号链接自身的 `size_bytes` | **恒为 0** | |
| `user` / `group`（超预算或查询失败） | **`None`** | 逐条查属主要开安全描述符，设了一道预算上限；超出的与查询失败用同一种表示——「不知道」，不猜 |
| 驱动器根条目的属主与容量 | **不冒充** | 卷本身没有属主这个概念，`uid`/`gid` 报 0、`user`/`group` 报 `None`、`mtime_ts` 为 0，不拿根目录的属主冒充 |

`allowed_roots` 在 NTFS 的大小写敏感目录下比较偏宽松。可以接受：它按模块文档本来就
**不是安全边界**，真正的边界是文件权限。

---

## 5. 最低系统要求

| 项 | 要求 | 依据 |
|---|---|---|
| **Windows 10 1809（build 17763）/ Server 2019** | 硬性 | 终端走 ConPTY，`CreatePseudoConsole` 自 1809 才有（`worker/terminal/windows.rs`）。这是全项目最高的一条版本门槛，`platform/windows/ntdll.rs` 的模块文档也以它为准说明「本项目的最低目标是 Windows 10」 |
| Windows 10 1709 以上 | GPU 指标 | WDDM 的 `GPU Engine` / `GPU Adapter Memory` 性能计数器要 1709 之后才有。更老的系统上 GPU 指标如实缺席，其余功能不受影响 |
| x86_64 | 发布包 | 只构建 `x86_64-pc-windows-msvc` |
| 无运行时依赖 | | 全部走系统 DLL；没有 PAM，认证用 `LogonUserW` |

`NtQuerySystemInformation` 与 `NtQueryInformationProcess` 的结构布局按 Win7+ 写，
但那不是实际下限——ConPTY 那一条更高，已经把 Win7/8 排除在外。

**服务器版本（Server 2019 / 2022 / 2025）与桌面版的差别**只在两处：没有 WDDM GPU 的机器
（Server Core、虚拟机）GPU 计数器根本不存在，采集器如实返回空表、不报错也不记日志——
缺席是常态。

## 6. 打包与 CI

打包脚本与安装说明见 [`packaging/windows/README.md`](../packaging/windows/README.md)，
构建入口是 `scripts/package-windows.ps1`，产物是与 Linux 侧 tar.gz 同构的 zip。
安装脚本要求管理员权限，注册服务走 `strixmaid.exe service install`
（而不是 `New-Service`——那条子命令还会写恢复策略与服务描述）。

CI 里 Windows 有两个 job（`.github/workflows/ci.yml`）：

* `windows`：`cargo build --workspace` → `cargo clippy --workspace --all-targets -- -D warnings`
  → `cargo test --workspace`，外加安装脚本在 **PowerShell 5.1** 下的语法检查。
  质量门槛与 Linux 完全相同，Windows 不享受任何豁免。
* `package-windows`：现场构建前端、跑打包脚本、检查 zip 内容与示例配置的编码，
  上传 artifact。

需要真实系统能力的测试（ConPTY、事件日志、管理员权限）一律写成**运行时探测 +
`eprintln!` 跳过**，不用 `#[ignore]`。理由见项目约定：`#[ignore]` 是永久性的，
而「本机有没有这项能力」是每次运行都该重新问的问题。

## 7. 平台 API 差异的几个坑

这几条不是设计选择，是既有差异，记下来以免重复踩。

| 差异 | 后果 | 处理 |
|---|---|---|
| `ProductName` 在 Windows 11 上写的仍是 `Windows 10` | 版本号整个报错 | 官方判定方式是看内部版本号，`CurrentBuildNumber >= 22000` 即 Windows 11，`fix_product_name` 据此改写（`providers/system/windows/os_version.rs`） |
| 本地化 Windows 上性能计数器的对象名与计数器名是**翻译过的** | 写死英文路径在德文机器上找不到 | 用 `Pdh*EnglishCounter*` 那一族，它固定按英文名解析 |
| 内建组的显示名是本地化的 | 按名字判断提权资格会在非英文系统上静默关闭所有人的提权 | 对已知内建 SID 额外补英文规范名；ACL 一律按 SID 授权 |
| NDIS 过滤层接口会重复出现 | 一块网卡画出四条一模一样的曲线，求和虚报四倍（实测：不过滤 54 个接口，过滤后 22 个） | 按 `InterfaceAndOperStatusFlags.FilterInterface` 排除 |
| `MEMORYSTATUSEX` 的 `ullTotalPageFile` 是**提交上限**而非页面文件大小 | 页面文件用量报成物理内存 + 页面文件 | 走 `SystemPagefileInformation` |
| `PendingFileRenameOperations` 在装过任何用 `MoveFileEx(DELAY_UNTIL_REBOOT)` 的软件后都存在 | 「需要重启」长期常亮 | 刻意不把它算进重启判据 |
| **Windows PowerShell 5.1 读没有 BOM 的 `.ps1` 时按系统 ANSI 代码页解码** | 中文注释变乱码，乱码字节吞掉引号导致语法错误 | `packaging/windows/*.ps1` 一律存成 UTF-8 **with BOM**；CI 用 5.1 的解析器钉住这一点 |
| **PowerShell 5.1 捕获子进程输出时按 `[Console]::OutputEncoding` 解码**（中文机器上是 GBK） | `strixmaid config example` 的中文注释整片乱码 | 安装与打包脚本改用 `Start-Process -RedirectStandardOutput`，落的是原始字节 |
| **`Set-Content -Encoding UTF8` 在 5.1 上会写 BOM**，而 TOML 规范不允许文件以 BOM 开头 | 配置文件看着好好的却解析失败 | 用 `System.Text.UTF8Encoding($false)` 写回 |
| rustc 在 MSVC 目标上默认嵌一份清单，**自带清单会整个取代它而不是合并** | 自带清单会丢掉 `longPathAware` 与 `activeCodePage=UTF-8` | `crates/*/[crate].exe.manifest` 里把那两项原样带上 |
| `requireAdministrator` 的清单会让 `CreateProcess` 在未提权上下文里直接失败（`ERROR_ELEVATION_REQUIRED`），**不会**自动弹 UAC | helper 起不来，症状离原因很远 | helper 与主进程的清单都明确写 `asInvoker`，理由记在各自的 `build.rs` |
| 运行中的 `.exe` 在 Windows 上是被锁住的 | 升级时覆盖二进制失败 | `install.ps1` 先停服务再覆盖 |

---

## 8. 与其它文档的关系

* `design.md` §2.1 / §2.2 已按本文补上 Windows 一列，Linux 的叙述一字未改。
* `macos-dev-platform.md` 记的是**开发平台**那一层，两者的取舍标准不同（见 §1）。
* 各 provider 的模块级 `//!` 文档是本文第 3、4 节的唯一事实来源。
  代码里写明的限制若与本文不符，以代码为准，并回来改本文。
