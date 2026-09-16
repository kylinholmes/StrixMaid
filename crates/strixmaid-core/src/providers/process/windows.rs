//! Windows 进程枚举后端：`NtQuerySystemInformation(SystemProcessInformation)`
//! 取全表，外加每进程一次 `OpenProcess` 取身份与命令行。
//!
//! # 两级取数
//!
//! | 阶段 | 调用 | 拿到的字段 |
//! |---|---|---|
//! | 全表（**一次**调用） | [`ntdll::system_processes`] | pid / ppid / 名字 / 线程数与线程状态 / 启动时刻 / CPU 时间 / 工作集 / 虚拟大小 / 基础优先级 / IO 计数 |
//! | 逐进程（一次 `OpenProcess`） | [`facts_of_process`] | uid / 用户名 / 命令行 |
//!
//! 第一级是 Linux 侧「遍历 `/proc` 读 `stat`」的等价物，代价是**常数**而非线性；
//! 第二级才是线性的那部分。Windows 的身份不在进程结构体里（它在**令牌**里，
//! 而令牌只能通过进程句柄取），命令行同理，所以这一步无法省掉。
//!
//! 实测（本机 301 个进程，release）：**整轮列表 5.4–6.2 ms**，即每进程
//! 18–20 µs；其中「`OpenProcess` + 读令牌」那一段约 1.1 ms（首轮，含
//! [`token::account_by_sid`] 的冷缓存），之后约 0.45 ms。既然整轮只有几毫秒，
//! 就**不再**加一层「pid → (启动时刻, uid, 用户名)」的缓存：多一份会过期、
//! 要跟着 pid 复用一起失效的状态，换不到可观测的收益。
//!
//! 同一次实测里，301 个进程有 **204 个**的身份读得出来（非提升会话）。
//! 剩下的是受保护进程与 SYSTEM / 其它用户的进程——`OpenProcess` 直接被拒，
//! 它们的 `user` 为 `None`，见 [`ProcFacts`]。
//!
//! # 与 Linux / macOS 的字段对照
//!
//! | 字段 | Linux | Windows | 差异 |
//! |---|---|---|---|
//! | `state` | `/proc/<pid>/stat` 第 3 字段 | 由**线程**状态推导，见 [`state_from_threads`] | Windows 的进程没有状态，只有线程有 |
//! | `nice` | 内核 nice 值 | 由基础优先级反映射，见 [`nice_from_base_priority`] | 两套刻度不同，只在优先级类的锚点上对得齐 |
//! | `rss_bytes` | `stat.rss × 页大小` | 工作集 | 口径一致（都是「当前驻留物理内存的量」） |
//! | `vms_bytes` | `stat.vsize` | 虚拟大小 | 口径一致 |
//! | `io_*_rate` | `/proc/<pid>/io` 的 `read_bytes`（**只算磁盘**） | `ReadTransferCount`（**文件 + 管道 + 设备**） | 见下「IO 计数的口径」 |
//! | `uid` / `user` | `/proc/<pid>` 的属主 | 进程令牌里 `TokenUser` 的 SID → RID | 映射规则见 [`crate::platform::windows::token`] |
//! | `cmdline` | `/proc/<pid>/cmdline` | `ProcessCommandLineInformation` | 打不开的进程为 `None`，与 Linux 上内核线程表现一致 |
//!
//! # IO 计数的口径
//!
//! Linux 的 `/proc/<pid>/io` 区分 `rchar`（所有读）与 `read_bytes`（**实际发往块设备**
//! 的字节），provider 取的是后者。Windows 的 `SYSTEM_PROCESS_INFORMATION` 只有
//! `ReadTransferCount` / `WriteTransferCount`，它**把文件、管道、设备 IO 算在一起**，
//! 没有「只算磁盘」的那一档。也就是说：
//!
//! - 同一个进程做同样的事，Windows 报出的数字通常**大于** Linux；
//! - 一个只读写命名管道、完全不碰磁盘的进程，在 Linux 上速率是 0，在 Windows 上不是。
//!
//! 这是 Windows 内核只记这一种计数的结果，不是本实现的取舍。**不拿它冒充磁盘 IO**：
//! 字段照常产出（它确实是这个进程的 IO 量），口径差异写在这里。
//!
//! # 拿不到的字段
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `tty` | Windows 的控制台是内核对象（`ConsoleHandle`），不是 `/dev/pts/0` 那样的设备文件，没有可展示的等价名字 |
//! | `cgroup` | 没有 cgroup。资源限制的对应物是作业对象（Job），但它不是一棵可反查的层级路径 |
//! | `fds` | 句柄表只能靠 `NtQuerySystemInformation(SystemHandleInformation)` **全局**枚举再按 pid 过滤，一次要拉几十万条记录，代价与收益不匹配 |
//! | `cwd` / `environ` | 要读目标进程的 PEB，见下。WOW64（32 位）目标与受保护进程读不到，为 `None` |
//!
//! `unit` **有**等价物：服务的宿主进程 pid 能反查出服务名，见 [`ServicePids`]。
//!
//! # cwd 与 environ 为什么要读 PEB
//!
//! 这两样东西内核不提供查询接口，它们住在目标进程的用户态内存里
//! （`PEB.ProcessParameters` 指向的 `RTL_USER_PROCESS_PARAMETERS`）。
//! 因此要 `PROCESS_VM_READ` + `ReadProcessMemory`。**32 位（WOW64）目标的 PEB
//! 布局与 64 位不同**，偏移不能通用——本实现用
//! `NtQueryInformationProcess(ProcessWow64Information)` 探测位数，与自身位数
//! 不一致时这两个字段直接报 `None`，绝不按猜来的偏移读一段内存当结果。
//!
//! # 信号
//!
//! Windows 没有信号。映射与其语义差异见 [`send_signal`]——**`term` 与 `kill`
//! 都是强制终止**，这是本 provider 最大的一处行为差异。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use strixmaid_types::process::{ProcessDetail, ProcessState, ProcessSummary, SignalName};
use strixmaid_types::{ApiError, ApiResult};

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::{TOKEN_PRIMARY_GROUP, TOKEN_USER, TokenPrimaryGroup, TokenUser};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, ENUM_SERVICE_STATUS_PROCESSW, EnumServicesStatusExW, OpenSCManagerW,
    SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE,
    SERVICE_STATE_ALL, SERVICE_WIN32,
};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetCurrentProcess,
    HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, OpenProcess,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_TERMINATE, PROCESS_VM_READ, QueryFullProcessImageNameW, REALTIME_PRIORITY_CLASS,
    SetPriorityClass, TerminateProcess,
};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

use super::super::Probe;
use super::Context;
use super::cpu::CpuSamples;
use super::io::IoSamples;
use crate::platform::windows::handle::Owned;
use crate::platform::windows::ntdll::{self, ProcessEntry, ThreadStates};
use crate::platform::windows::{error_from_code, last_error, token, wide};

/// `cpu_100ns` 的单位是 100 纳秒，把它当 tick 时对应的「每秒 tick 数」。
/// 与 macOS 把纳秒当 tick（`hz = 10⁹`）是同一手法，[`super::cpu`] 不需要任何改动。
const HUNDRED_NANOS_PER_SEC: u64 = 10_000_000;

/// System Idle Process。它不是可操作的进程（没有名字、没有令牌、杀不掉），
/// 与 macOS 后端滤掉 pid 0 同理。
const PID_IDLE: u32 = 0;
/// `System`（内核本体）。终止它等于让机器当场蓝屏，改它的优先级同样致命，
/// 因此本 provider 的两个写操作都直接拒绝它，见 [`reject_system_pid`]。
const PID_SYSTEM: u32 = 4;

/// 能枚举出进程即可用。判据与 macOS 侧一致：系统 API 不存在「探测不到」，
/// 真读不出任何进程才算不可用。
pub fn probe() -> Probe {
    match ntdll::system_processes() {
        Ok(procs) if !procs.is_empty() => Probe::Available,
        Ok(_) => Probe::unavailable("NtQuerySystemInformation 未返回任何进程"),
        Err(e) => Probe::unavailable(format!("NtQuerySystemInformation 失败：{e}")),
    }
}

// ===========================================================================
// 后端
// ===========================================================================

/// Windows 后端。构造时读一次运行期恒定的常量。
pub struct Backend {
    /// `GlobalMemoryStatusEx().ullTotalPhys`，物理内存总量。运行期恒定。
    mem_total: u64,
    /// pid → 服务名的映射，带 TTL。见 [`ServicePids`]。
    services: ServicePids,
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            mem_total: total_physical_memory(),
            services: ServicePids::new(),
        }
    }

    /// 物理内存总量。构造时读一次，之后直接返回缓存。
    pub fn mem_total(&self) -> u64 {
        self.mem_total
    }

    /// 取全表并逐进程补齐身份与命令行，顺带更新 CPU / IO 快照、清理已消失的 pid。
    pub fn list(
        &self,
        cpu: &mut CpuSamples,
        io: &mut IoSamples,
        ctx: &Context,
    ) -> Vec<ProcessSummary> {
        let Ok(procs) = ntdll::system_processes() else {
            return Vec::new();
        };
        let mut all = Vec::with_capacity(procs.len());
        let mut seen: HashSet<u32> = HashSet::with_capacity(procs.len());
        for entry in &procs {
            if entry.pid == PID_IDLE {
                continue;
            }
            // 遍历期间进程随时可能退出：打不开就退化成「身份未知」，不是错误。
            let facts = facts_of_process(entry.pid);
            let summary = summarize(entry, facts, cpu, io, ctx);
            seen.insert(summary.pid);
            all.push(summary);
        }
        cpu.retain_seen(&seen);
        io.retain_seen(&seen);
        all
    }

    /// 单个进程的详情。
    pub fn detail(
        &self,
        raw_pid: i32,
        cpu: &mut CpuSamples,
        io: &mut IoSamples,
        ctx: &Context,
    ) -> ApiResult<ProcessDetail> {
        let pid = raw_pid as u32;
        let procs = ntdll::system_processes()
            .map_err(|e| ApiError::internal("无法枚举进程表").with_detail(e.to_string()))?;
        let entry = procs
            .iter()
            .find(|p| p.pid == pid && p.pid != PID_IDLE)
            .ok_or_else(|| ApiError::not_found(format!("进程 {pid} 不存在")))?;

        // 详情比列表多要一个 PROCESS_VM_READ（读 PEB 拿 cwd / environ）。
        // 受保护进程给不了这个权限，此时退回只读基本信息的句柄。
        let opened = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ)
            .ok()
            .map(|h| (h, true))
            .or_else(|| {
                open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION)
                    .ok()
                    .map(|h| (h, false))
            });

        let mut facts = ProcFacts::default();
        let mut exe = None;
        let mut cwd = None;
        let mut environ = None;
        let mut gid = None;
        if let Some((handle, vm_read)) = &opened {
            let h = handle.raw();
            // SAFETY: h 由 OpenProcess 刚返回，带 PROCESS_QUERY_LIMITED_INFORMATION；
            // vm_read 为真时还带 PROCESS_VM_READ。
            unsafe {
                facts = facts_of_handle(h);
                exe = image_path(h);
                gid = primary_group_of(h);
                if *vm_read {
                    let (c, e) = peb_strings(h);
                    cwd = c;
                    environ = e;
                }
            }
        }

        let cmdline_args = facts
            .cmdline
            .as_deref()
            .map(split_command_line)
            .unwrap_or_default();
        let unit = self.services.unit_of(pid);
        // Windows 只有一张主令牌，没有「实际 uid / 有效 uid」之分：
        // euid 恒等于 summary.uid（不是拿相近的东西冒充，这个平台就只有一个身份）。
        let euid = Some(facts.uid);
        let (io_read_bytes, io_write_bytes) = (entry.read_bytes, entry.write_bytes);
        let summary = summarize(entry, facts, cpu, io, ctx);

        Ok(ProcessDetail {
            summary,
            cmdline_args,
            exe,
            cwd,
            euid,
            gid,
            // 见模块文档「拿不到的字段」
            tty: None,
            cgroup: None,
            unit,
            environ,
            fds: None,
            io_read_bytes: Some(io_read_bytes),
            io_write_bytes: Some(io_write_bytes),
        })
    }
}

/// 把一条进程表记录转成 [`ProcessSummary`]，同时更新 CPU / IO 快照。
///
/// `facts`（身份与命令行）由调用方给出：列表与详情对同一个进程只开一次句柄。
fn summarize(
    entry: &ProcessEntry,
    facts: ProcFacts,
    cpu: &mut CpuSamples,
    io: &mut IoSamples,
    ctx: &Context,
) -> ProcessSummary {
    // 启动时刻同时充当 pid 复用的判别键，与 macOS 用 pbi_start_tvsec 同理。
    let started = entry.start_ts.max(0) as u64;
    let cpu_percent = cpu
        .observe(
            entry.pid,
            started,
            entry.cpu_100ns,
            ctx.now,
            HUNDRED_NANOS_PER_SEC,
        )
        .unwrap_or(0.0);

    // IO 计数随进程表一起来，不存在「无权限读不到」的情况，因此恒为 Some；
    // 首轮没基线 → Some(0.0)，与 types 文档一致。
    let (read_rate, write_rate) = io
        .observe(
            entry.pid,
            started,
            entry.read_bytes,
            entry.write_bytes,
            ctx.now,
        )
        .unwrap_or((0.0, 0.0));

    let rss_bytes = entry.working_set;
    ProcessSummary {
        pid: entry.pid,
        ppid: entry.ppid,
        name: entry.name.clone(),
        cmdline: facts.cmdline,
        uid: facts.uid,
        user: facts.user,
        state: state_from_threads(&entry.thread_states),
        cpu_percent,
        rss_bytes,
        vms_bytes: entry.virtual_size,
        mem_percent: ctx.mem_percent(rss_bytes),
        threads: entry.threads,
        start_ts: entry.start_ts,
        nice: nice_from_base_priority(entry.base_priority),
        io_read_rate: Some(read_rate),
        io_write_rate: Some(write_rate),
    }
}

/// 物理内存总量。读不到返回 0，此时 `mem_percent` 恒为 0（[`Context::mem_percent`] 的约定）。
fn total_physical_memory() -> u64 {
    // SAFETY: MEMORYSTATUSEX 是纯 POD（全是整数字段），全 0 是合法初值。
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: 指针指向一整个 MEMORYSTATUSEX，且 dwLength 已按文档要求填成结构体大小。
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    if ok == 0 { 0 } else { status.ullTotalPhys }
}

// ===========================================================================
// 进程状态：由线程状态推导
// ===========================================================================

/// 线程状态 → 进程状态。
///
/// **Windows 的进程没有状态**：`EPROCESS` 里没有 Linux `task_struct::state` 的对应物，
/// 可运行 / 睡眠是**线程**的属性。任务管理器显示的「正在运行 / 已挂起」同样是
/// 对线程状态的归纳。本函数就是把这套归纳写下来：
///
/// | 条件 | 结果 | 理由 |
/// |---|---|---|
/// | 线程数为 0 | [`ProcessState::Zombie`] | 进程对象还在（父进程或调试器还持着句柄），但已经没有执行体 |
/// | 有 Running 线程 | [`ProcessState::Running`] | 正占着某个逻辑处理器 |
/// | 有 Ready / Standby 线程 | [`ProcessState::Running`] | 在就绪队列里，等于 Linux 的 `R`（`R` 同时含「正在跑」与「可运行」） |
/// | 全部 Terminated | [`ProcessState::Zombie`] | 线程都退完了，进程对象尚未销毁 |
/// | 其余且有 Waiting | [`ProcessState::Sleeping`] | 绝大多数进程的常态 |
/// | 其余 | [`ProcessState::Unknown`] | 只剩 Initialized / Transition 这类过渡态，不猜 |
///
/// # Linux 有而 Windows 没有的状态
///
/// - `DiskSleep`（`D`，不可中断睡眠）：Windows 的线程等 IO 时状态仍是 `Waiting`，
///   只是 `WaitReason` 不同。`WaitReason` 有十几种（`Executive` / `PageIn` /
///   `FreePage`…），**没有一种能干净地对应「等块设备且不可中断」**，
///   硬把某几种映射成 `DiskSleep` 就是编数据，因此不做。
/// - `TracingStop`（`t`）：调试器挂起在 Windows 上表现为线程被 suspend，
///   而 `SYSTEM_THREAD_INFORMATION` 不暴露挂起计数，区分不出「被调试器停住」
///   与「自己调了 `SuspendThread`」。
/// - `Stopped`（`T`，被信号停止）：Windows 没有信号，也就没有 `SIGSTOP`。
/// - `Idle`（`I`，空闲内核线程）：Windows 的内核工作线程都挂在 `System`（pid 4）
///   这一个进程下，不作为独立进程出现，没有对应物。
pub fn state_from_threads(t: &ThreadStates) -> ProcessState {
    if t.total == 0 {
        return ProcessState::Zombie;
    }
    if t.running > 0 || t.ready > 0 {
        return ProcessState::Running;
    }
    if t.terminated == t.total {
        return ProcessState::Zombie;
    }
    if t.waiting > 0 {
        return ProcessState::Sleeping;
    }
    ProcessState::Unknown
}

// ===========================================================================
// nice ↔ 优先级类
// ===========================================================================

/// 优先级类的锚点：`(基础优先级, nice)`。
///
/// 基础优先级取自 Windows 对各优先级类的定义（`SetPriorityClass` 文档），
/// nice 一侧取 Unix 上「语义相当」的值：
///
/// | 优先级类 | 基础优先级 | nice | 对应的 Unix 直觉 |
/// |---|---|---|---|
/// | `IDLE_PRIORITY_CLASS` | 4 | 19 | 「只在系统闲着时跑」 |
/// | `BELOW_NORMAL_PRIORITY_CLASS` | 6 | 10 | 后台批处理 |
/// | `NORMAL_PRIORITY_CLASS` | 8 | 0 | 默认 |
/// | `ABOVE_NORMAL_PRIORITY_CLASS` | 10 | -5 | 稍微优先 |
/// | `HIGH_PRIORITY_CLASS` | 13 | -10 | 需要及时响应 |
/// | `REALTIME_PRIORITY_CLASS` | 24 | -20 | 抢在系统线程之前（要特权） |
///
/// **两套刻度只在这六个点上对得齐**，中间用分段线性插值。这是个约定而不是等式：
/// Windows 的调度器按优先级类 + 线程相对优先级 + 动态提升工作，没有「nice 值」
/// 这个量；反过来 Linux 也没有「优先级类」。
const PRIORITY_ANCHORS: [(i32, i32); 6] =
    [(4, 19), (6, 10), (8, 0), (10, -5), (13, -10), (24, -20)];

/// 基础优先级（0–31）→ nice（-20..=19）。锚点之间分段线性插值，锚点之外夹住。
pub fn nice_from_base_priority(base: i32) -> i32 {
    let (first_base, first_nice) = PRIORITY_ANCHORS[0];
    if base <= first_base {
        return first_nice;
    }
    let (last_base, last_nice) = PRIORITY_ANCHORS[PRIORITY_ANCHORS.len() - 1];
    if base >= last_base {
        return last_nice;
    }
    for pair in PRIORITY_ANCHORS.windows(2) {
        let (b0, n0) = pair[0];
        let (b1, n1) = pair[1];
        if base >= b0 && base <= b1 {
            return (n0 + div_round((n1 - n0) * (base - b0), b1 - b0)).clamp(-20, 19);
        }
    }
    // 锚点覆盖 4..=24 的全部整数，走不到这里；真走到了也不编数据，报「普通」。
    0
}

/// nice（-20..=19）→ 优先级类常量。
///
/// 分界点取相邻锚点的中点，**平局向低优先级一侧**（宁可把进程放慢，
/// 也不要凭一次取整把它抬到 `HIGH` 去抢系统线程的 CPU）。
pub fn priority_class_from_nice(nice: i32) -> u32 {
    match nice {
        n if n >= 15 => IDLE_PRIORITY_CLASS,
        n if n >= 5 => BELOW_NORMAL_PRIORITY_CLASS,
        n if n >= -2 => NORMAL_PRIORITY_CLASS,
        n if n >= -7 => ABOVE_NORMAL_PRIORITY_CLASS,
        n if n >= -15 => HIGH_PRIORITY_CLASS,
        _ => REALTIME_PRIORITY_CLASS,
    }
}

/// 四舍五入的整数除法（`den > 0`，`num` 可正可负，平局远离零）。
const fn div_round(num: i32, den: i32) -> i32 {
    let half = den / 2;
    if num >= 0 {
        (num + half) / den
    } else {
        (num - half) / den
    }
}

// ===========================================================================
// 逐进程的身份与命令行
// ===========================================================================

/// 需要开句柄才能拿到的那几个字段。
///
/// 打不开进程（受保护进程、`System`、别的用户或更高完整性级别的进程）时全部为
/// 默认值：`uid = 0` 且 `user = None`。**这两者要一起看**——`user = Some("SYSTEM")`
/// 才表示「真的是 LocalSystem」，`user = None` 表示「身份读不出来」。
/// 前端应把 `user == None` 画成「—」而不是把 `uid == 0` 当成 SYSTEM。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ProcFacts {
    uid: u32,
    user: Option<String>,
    cmdline: Option<String>,
}

/// 开一次句柄，取身份与命令行。
fn facts_of_process(pid: u32) -> ProcFacts {
    let Ok(handle) = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION) else {
        return ProcFacts::default();
    };
    // SAFETY: 句柄刚由 OpenProcess 返回，带 PROCESS_QUERY_LIMITED_INFORMATION。
    unsafe { facts_of_handle(handle.raw()) }
}

/// 从一个已打开的进程句柄取身份与命令行。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION` 的有效进程句柄。
unsafe fn facts_of_handle(process: HANDLE) -> ProcFacts {
    // SAFETY: 调用方保证句柄有效且带足够访问权。
    let cmdline = unsafe { ntdll::process_command_line(process) };
    // SAFETY: 同上。
    let (uid, user) = unsafe { user_of_process(process) }.unwrap_or((0, None));
    ProcFacts { uid, user, cmdline }
}

/// 进程令牌里的用户身份。
///
/// 只读 `TokenUser` 一项：一轮列表要对几百个进程做这件事，而
/// [`token::identity_of_token`] 还会枚举全部组（每个组一次 `LookupAccountSidW`），
/// 那是登录鉴权路径才需要的东西。uid 的映射规则与它保持一致
/// （`S-1-5-18` → 0，其余取 RID）。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION` 的有效进程句柄。
unsafe fn user_of_process(process: HANDLE) -> Option<(u32, Option<String>)> {
    // SAFETY: 调用方保证句柄有效。
    let tok = unsafe { token::open_process_token(process) }.ok()?;
    // SAFETY: tok 刚打开，带 TOKEN_QUERY。
    let buf = unsafe { token::token_info(tok.raw(), TokenUser) }.ok()?;
    if buf.len() < std::mem::size_of::<TOKEN_USER>() {
        return None;
    }
    // SAFETY: TokenUser 的返回布局就是 TOKEN_USER，长度刚校验过。
    let user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    let sid = user.User.Sid;
    // SAFETY: sid 指向 buf 内部，该缓冲在本函数返回前一直存活。
    let uid = if unsafe { token::sid_to_string(sid) }.as_deref() == Some(token::SID_LOCAL_SYSTEM) {
        0
    } else {
        // SAFETY: 同上。
        unsafe { token::rid_of_sid(sid) }
    };
    // account_by_sid 自带进程内缓存：一轮列表里几百个进程往往只有三五个不同的 SID。
    // SAFETY: 同上。
    Some((uid, unsafe { token::account_by_sid(sid) }.map(|a| a.name)))
}

/// 进程令牌的主组 RID（详情里的 `gid`）。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION` 的有效进程句柄。
unsafe fn primary_group_of(process: HANDLE) -> Option<u32> {
    // SAFETY: 调用方保证句柄有效。
    let tok = unsafe { token::open_process_token(process) }.ok()?;
    // SAFETY: tok 刚打开，带 TOKEN_QUERY。
    let buf = unsafe { token::token_info(tok.raw(), TokenPrimaryGroup) }.ok()?;
    if buf.len() < std::mem::size_of::<TOKEN_PRIMARY_GROUP>() {
        return None;
    }
    // SAFETY: TokenPrimaryGroup 的返回布局就是 TOKEN_PRIMARY_GROUP，长度刚校验过。
    let pg = unsafe { &*(buf.as_ptr().cast::<TOKEN_PRIMARY_GROUP>()) };
    // SAFETY: PrimaryGroup 指向 buf 内部，该缓冲在本函数返回前一直存活。
    Some(unsafe { token::rid_of_sid(pg.PrimaryGroup) })
}

/// 可执行文件的完整路径（`QueryFullProcessImageNameW`）。
///
/// 用 `PROCESS_NAME_WIN32` 而不是 `PROCESS_NAME_NATIVE`：前者给
/// `C:\Windows\explorer.exe`，后者给 `\Device\HarddiskVolume3\Windows\explorer.exe`。
/// DTO 里的 `exe` 是给人看的路径。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION` 的有效进程句柄。
unsafe fn image_path(process: HANDLE) -> Option<String> {
    // 路径可能超过 MAX_PATH（长路径），一次给足 32767 个字符。
    let mut buf = vec![0u16; 32768];
    let mut len = buf.len() as u32;
    // SAFETY: buf 有 len 个 u16 可写，len 如实描述其容量；成功时 len 被回填成实际长度。
    let ok = unsafe {
        QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &raw mut len)
    };
    if ok == 0 {
        return None;
    }
    buf.truncate(len as usize);
    let s = wide::from_wide_nul(&buf);
    (!s.is_empty()).then_some(s)
}

/// 按 Windows 的规则把一整条命令行拆成 argv。
///
/// 用 `CommandLineToArgvW`（shell32）而不是自己按空格切：Windows 的引号与
/// 反斜杠转义规则相当刁钻（`\\"` 是一个字面反斜杠加一个引号起止符，
/// `""` 在引号内表示一个字面引号），自己实现只会得到一个「大部分时候对」的版本。
///
/// 空串会被该函数解释成「取当前进程的映像路径」——那是它的历史行为，
/// 这里直接短路掉，免得把**别的进程**的空命令行报成本进程的路径。
fn split_command_line(cmdline: &str) -> Vec<String> {
    if cmdline.trim().is_empty() {
        return Vec::new();
    }
    let wide_cmd = wide::to_wide(cmdline);
    let mut argc: i32 = 0;
    // SAFETY: wide_cmd 是以 NUL 结尾的 UTF-16；argc 是输出参数。
    // 成功时返回一块 LocalAlloc 的数组，需由我们 LocalFree。
    let argv = unsafe { CommandLineToArgvW(wide_cmd.as_ptr(), &raw mut argc) };
    if argv.is_null() || argc <= 0 {
        return Vec::new();
    }
    // SAFETY: 调用成功即 argv 指向 argc 个以 NUL 结尾的宽串指针。
    let out = unsafe {
        (0..argc as usize)
            .map(|i| wide::from_wide_ptr(*argv.add(i)))
            .collect()
    };
    // SAFETY: argv 由 CommandLineToArgvW 用 LocalAlloc 分配，按文档用 LocalFree 归还。
    unsafe {
        LocalFree(argv.cast());
    }
    out
}

/// `OpenProcess` 的薄封装：失败时带上 Win32 错误码，供 [`write_error`] 分类。
fn open_process(pid: u32, access: u32) -> io::Result<Owned> {
    // SAFETY: OpenProcess 不解引用任何指针；失败时返回空句柄，由 Owned::new 挡下。
    let h = unsafe { OpenProcess(access, 0, pid) };
    // SAFETY: h 刚由 OpenProcess 返回，尚无其它持有者。
    unsafe { Owned::new(h) }
}

// ===========================================================================
// PEB：cwd 与 environ
// ===========================================================================

#[link(name = "kernel32")]
unsafe extern "system" {
    /// `ReadProcessMemory` 在 `windows-sys` 里属于 `Win32_System_Diagnostics_Debug`
    /// feature，而本 crate 没有开那个 feature（`Cargo.toml` 是共享文件，不擅自改）。
    /// 它只是 kernel32 里一个普通的 C 函数，手写声明即可，代价与 ntdll 那两个一样。
    fn ReadProcessMemory(
        process: HANDLE,
        base: *const core::ffi::c_void,
        buffer: *mut core::ffi::c_void,
        size: usize,
        read: *mut usize,
    ) -> i32;
}

/// `PROCESS_BASIC_INFORMATION`（`winternl.h`）。只用到 `peb_base_address`，
/// 但前面的字段必须逐一列出才能对上偏移。
#[repr(C)]
#[derive(Clone, Copy)]
struct ProcessBasicInformation {
    exit_status: i32,
    peb_base_address: *mut core::ffi::c_void,
    affinity_mask: usize,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
}

/// PEB 的开头几个字段。只需要 `process_parameters`，填充交给 `repr(C)` 自己算
/// ——32 位与 64 位都对（指针宽度变了，布局跟着变）。
#[repr(C)]
#[derive(Clone, Copy)]
struct PebHead {
    inherited_address_space: u8,
    read_image_file_exec_options: u8,
    being_debugged: u8,
    bit_field: u8,
    mutant: *mut core::ffi::c_void,
    image_base_address: *mut core::ffi::c_void,
    ldr: *mut core::ffi::c_void,
    process_parameters: *mut core::ffi::c_void,
}

/// 目标进程内的 `UNICODE_STRING`：`buffer` 是**对方**地址空间里的指针，
/// 本进程不能解引用，只能交给 `ReadProcessMemory`。
#[repr(C)]
#[derive(Clone, Copy)]
struct RemoteUnicodeString {
    /// 字节数（不是字符数），不含结尾 NUL。
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

/// `CURDIR`：`RTL_USER_PROCESS_PARAMETERS.CurrentDirectory` 的类型。
#[repr(C)]
#[derive(Clone, Copy)]
struct CurDir {
    dos_path: RemoteUnicodeString,
    handle: *mut core::ffi::c_void,
}

/// `RTL_USER_PROCESS_PARAMETERS` 的前半段，到 `Environment` 为止。
///
/// 后面还有窗口位置、标题、`CurrentDirectories[32]`、`EnvironmentSize` 等，
/// **故意不写**：那些字段的偏移随 Windows 版本变过，而我们一个都不需要。
/// 只读前半段就不会被尾部的变化牵连。
#[repr(C)]
#[derive(Clone, Copy)]
struct RtlUserProcessParameters {
    maximum_length: u32,
    length: u32,
    flags: u32,
    debug_flags: u32,
    console_handle: *mut core::ffi::c_void,
    console_flags: u32,
    standard_input: *mut core::ffi::c_void,
    standard_output: *mut core::ffi::c_void,
    standard_error: *mut core::ffi::c_void,
    current_directory: CurDir,
    dll_path: RemoteUnicodeString,
    image_path_name: RemoteUnicodeString,
    command_line: RemoteUnicodeString,
    environment: *mut core::ffi::c_void,
}

/// 环境块一次读多少字节。按一页取，正好避开「一次跨太多页、
/// 其中一页不可读就整段失败」。
const ENV_CHUNK: usize = 4096;
/// 环境块的上限。超过就放弃（返回 `None`），不无限读下去。
/// 正常进程的环境是几 KiB，256 KiB 已经是极端值。
const ENV_MAX: usize = 256 * 1024;
/// 单个 `UNICODE_STRING` 的上限，防止字段错位时申请一大块内存。
const MAX_UNICODE_BYTES: usize = 64 * 1024;

/// 读目标进程的 `cwd` 与 `environ`。任何一步失败都返回 `None`，绝不猜。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`
/// 的有效进程句柄。
unsafe fn peb_strings(process: HANDLE) -> (Option<String>, Option<BTreeMap<String, String>>) {
    // 位数不一致 → PEB 布局不同 → 直接放弃。见模块文档。
    // SAFETY: 调用方保证句柄有效。
    let target_wow64 = unsafe { is_wow64(process) };
    if target_wow64.is_none() || target_wow64 != Some(self_is_wow64()) {
        return (None, None);
    }

    let mut basic = std::mem::MaybeUninit::<ProcessBasicInformation>::zeroed();
    // SAFETY: 调用方保证句柄有效；buffer 指向一整个 ProcessBasicInformation，
    // length 如实描述其大小。
    let queried = unsafe {
        ntdll::query_process(
            process,
            ntdll::PROCESS_BASIC_INFORMATION,
            basic.as_mut_ptr().cast::<core::ffi::c_void>(),
            std::mem::size_of::<ProcessBasicInformation>() as u32,
        )
    };
    if queried.is_err() {
        return (None, None);
    }
    // SAFETY: 调用成功即内核已写满结构体。
    let basic = unsafe { basic.assume_init() };
    if basic.peb_base_address.is_null() {
        return (None, None);
    }

    // SAFETY: peb_base_address 是目标进程里的地址，只交给 ReadProcessMemory；
    // 不可读时只是返回 None，不会影响本进程。
    let Some(peb) = (unsafe { read_remote::<PebHead>(process, basic.peb_base_address) }) else {
        return (None, None);
    };
    if peb.process_parameters.is_null() {
        return (None, None);
    }
    // SAFETY: 同上。
    let Some(params) =
        (unsafe { read_remote::<RtlUserProcessParameters>(process, peb.process_parameters) })
    else {
        return (None, None);
    };

    // SAFETY: params 里的指针都是目标进程里的地址，只交给 ReadProcessMemory。
    let cwd = unsafe { read_remote_string(process, params.current_directory.dos_path) };
    // SAFETY: 同上。
    let environ =
        unsafe { read_environment(process, params.environment) }.map(|b| parse_environment_block(&b));
    (cwd, environ)
}

/// 目标进程是否是 WOW64 下的 32 位进程。查不出来返回 `None`。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION` 的有效进程句柄。
unsafe fn is_wow64(process: HANDLE) -> Option<bool> {
    let mut peb32: usize = 0;
    // SAFETY: 调用方保证句柄有效；buffer 指向一个 usize，length 如实描述其大小。
    let queried = unsafe {
        ntdll::query_process(
            process,
            ntdll::PROCESS_WOW64_INFORMATION,
            (&raw mut peb32).cast::<core::ffi::c_void>(),
            std::mem::size_of::<usize>() as u32,
        )
    };
    queried.ok().map(|_| peb32 != 0)
}

/// 本进程是否运行在 WOW64 下。进程生命周期内不变，只查一次。
fn self_is_wow64() -> bool {
    static SELF: OnceLock<bool> = OnceLock::new();
    *SELF.get_or_init(|| {
        // SAFETY: GetCurrentProcess 返回伪句柄，永远有效且带全部访问权。
        unsafe { is_wow64(GetCurrentProcess()) }.unwrap_or(false)
    })
}

/// 从目标进程读一个 `T`。读不全就当读不到。
///
/// # Safety
///
/// `process` 必须带 `PROCESS_VM_READ`；`T` 必须是没有内部不变量的 POD
/// （本模块里全是纯字段的 `repr(C)` 结构体）。
unsafe fn read_remote<T>(process: HANDLE, base: *const core::ffi::c_void) -> Option<T> {
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut got: usize = 0;
    let want = std::mem::size_of::<T>();
    // SAFETY: buffer 指向一整个 T，size 如实描述其大小；base 由调用方保证是
    // 目标进程里的地址（不可读时函数只是失败，不会破坏本进程的内存）。
    let ok = unsafe {
        ReadProcessMemory(
            process,
            base,
            value.as_mut_ptr().cast::<core::ffi::c_void>(),
            want,
            &raw mut got,
        )
    };
    if ok == 0 || got != want {
        return None;
    }
    // SAFETY: 刚确认读满了 size_of::<T>() 字节。
    Some(unsafe { value.assume_init() })
}

/// 读一个目标进程里的 `UNICODE_STRING`。
///
/// # Safety
///
/// `process` 必须带 `PROCESS_VM_READ`；`us` 必须来自同一个进程的内存。
unsafe fn read_remote_string(process: HANDLE, us: RemoteUnicodeString) -> Option<String> {
    let bytes = us.length as usize;
    if us.buffer.is_null() || bytes == 0 || bytes > MAX_UNICODE_BYTES {
        return None;
    }
    let mut buf = vec![0u16; bytes / 2];
    let mut got: usize = 0;
    // SAFETY: buf 有 bytes 字节可写；us.buffer 是目标进程里的地址。
    let ok = unsafe {
        ReadProcessMemory(
            process,
            us.buffer.cast::<core::ffi::c_void>(),
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            bytes,
            &raw mut got,
        )
    };
    if ok == 0 || got != bytes {
        return None;
    }
    let s = wide::from_wide_nul(&buf);
    (!s.is_empty()).then_some(s)
}

/// 读目标进程的环境块。
///
/// `RTL_USER_PROCESS_PARAMETERS` 里确实有 `EnvironmentSize`，但它在结构体
/// **尾部**（`CurrentDirectories[32]` 之后），偏移随版本变过。与其为一个长度
/// 去抄一整串版本相关的布局，不如按页读到**双 NUL** 为止——环境块的终止符是
/// 格式本身的一部分，不依赖任何结构体偏移。
///
/// # Safety
///
/// `process` 必须带 `PROCESS_VM_READ`；`base` 必须是目标进程里的地址。
unsafe fn read_environment(process: HANDLE, base: *mut core::ffi::c_void) -> Option<Vec<u16>> {
    if base.is_null() {
        return None;
    }
    let mut raw: Vec<u8> = Vec::with_capacity(ENV_CHUNK * 2);
    while raw.len() < ENV_MAX {
        let at = raw.len();
        let mut chunk = vec![0u8; ENV_CHUNK];
        let mut got: usize = 0;
        // SAFETY: chunk 有 ENV_CHUNK 字节可写；base+at 是目标进程里的地址，
        // 不可读时函数失败并回填实际读到的字节数。
        let ok = unsafe {
            ReadProcessMemory(
                process,
                base.cast::<u8>().add(at).cast::<core::ffi::c_void>(),
                chunk.as_mut_ptr().cast::<core::ffi::c_void>(),
                ENV_CHUNK,
                &raw mut got,
            )
        };
        if got == 0 {
            break;
        }
        chunk.truncate(got);
        raw.extend_from_slice(&chunk);
        // 双 NUL（四个 0 字节，且落在 u16 边界上）表示环境块结束。
        if let Some(end) = find_double_nul(&raw) {
            raw.truncate(end);
            return Some(bytes_to_wide(&raw));
        }
        if ok == 0 || got < ENV_CHUNK {
            break;
        }
    }
    // 没找到终止符：宁可报「读不到」，也不把一段可能被截断的内存当成完整环境。
    None
}

/// 在 UTF-16 字节流里找连续两个 NUL 字符（环境块的结束），返回**它之前**的字节数。
fn find_double_nul(raw: &[u8]) -> Option<usize> {
    let mut i = 0usize;
    while i + 4 <= raw.len() {
        if raw[i] == 0 && raw[i + 1] == 0 && raw[i + 2] == 0 && raw[i + 3] == 0 {
            return Some(i);
        }
        // 步长必须是 2：四个零字节只有落在 u16 边界上才是两个 NUL 字符。
        i += 2;
    }
    None
}

/// 小端字节流 → `u16` 序列。奇数个字节时丢掉最后一个（正常不会出现，防御性）。
fn bytes_to_wide(raw: &[u8]) -> Vec<u16> {
    raw.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect()
}

/// 环境块 → `KEY=VALUE` 映射。纯函数，可单测。
///
/// 块的形状是「若干以 NUL 分隔的串，以空串结束」，正好是
/// [`wide::from_wide_multi`] 处理的那种表，直接复用。
///
/// **跳过键为空的项**：Windows 用 `=C:=C:\some\dir` 这种隐藏条目记录每个盘符的
/// 当前目录，它们不是真的环境变量，`GetEnvironmentVariableW` 也查不到。
pub fn parse_environment_block(block: &[u16]) -> BTreeMap<String, String> {
    wide::from_wide_multi(block)
        .into_iter()
        .filter_map(|item| {
            let (k, v) = item.split_once('=')?;
            (!k.is_empty()).then(|| (k.to_owned(), v.to_owned()))
        })
        .collect()
}

// ===========================================================================
// unit：服务的宿主进程
// ===========================================================================

/// 服务枚举结果的有效期。进程详情可能被前端按秒刷新，而服务的 pid
/// 只在服务重启时才变，几秒的陈旧完全可接受；重点是**不要每次详情都枚举一遍
/// 全部服务**（一台普通机器有三百多个服务）。
const SERVICE_TTL: Duration = Duration::from_secs(5);

/// 一份服务快照：pid → unit 名。值为 `None` 表示**这个 pid 托管了多个服务**，
/// 无法唯一归属，见 [`ServicePids`]。用 `Arc` 是为了让读者拿走快照之后
/// 立刻放开锁。
type ServiceSnapshot = Arc<HashMap<u32, Option<String>>>;

/// pid → 服务名的映射，带 TTL 缓存。
///
/// 这是 Windows 上 `ProcessDetail::unit` 的唯一来源：systemd 靠 cgroup 路径反查
/// unit，Windows 没有 cgroup，但 SCM 会告诉我们每个服务的宿主进程 pid，
/// 反过来建索引即可。unit 名沿用 service provider 的命名（`<服务名>.service`），
/// 前端才能直接拿它去链服务详情页。
///
/// # 一个 pid 托管多个服务时报 `None`
///
/// `svchost.exe` 可以同时托管若干服务。这时候 pid **无法**唯一对应到一个服务，
/// 而 `unit` 是单个值。从几个里挑一个填上去就是编数据，因此这种 pid 一律报
/// `None`；只有「这个进程就是那一个服务」时才给出 unit 名。
pub struct ServicePids {
    cache: Mutex<Option<(Instant, ServiceSnapshot)>>,
}

impl Default for ServicePids {
    fn default() -> Self {
        Self::new()
    }
}

impl ServicePids {
    pub fn new() -> Self {
        ServicePids {
            cache: Mutex::new(None),
        }
    }

    /// 该 pid 所属的 unit 名（`<服务名>.service`）。不是服务宿主、
    /// 或托管了多个服务时为 `None`。
    pub fn unit_of(&self, pid: u32) -> Option<String> {
        self.snapshot().get(&pid).cloned().flatten()
    }

    /// 取当前快照，过期就重新枚举。枚举失败时返回空表（不是错误：
    /// 连不上 SCM 只意味着这一轮没有 unit 信息）。
    fn snapshot(&self) -> ServiceSnapshot {
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, table)) = guard.as_ref()
            && at.elapsed() < SERVICE_TTL
        {
            return Arc::clone(table);
        }
        let table = Arc::new(enumerate_service_pids().unwrap_or_default());
        *guard = Some((Instant::now(), Arc::clone(&table)));
        table
    }
}

/// `SC_HANDLE` 的 RAII。**不能**用 [`Owned`]：SCM 句柄要 `CloseServiceHandle`
/// 关，拿 `CloseHandle` 关它是错的。
struct ScHandle(SC_HANDLE);

impl Drop for ScHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 构造时已排除空句柄，且本类型独占所有权，只关这一次。
            unsafe { CloseServiceHandle(self.0) };
        }
    }
}

/// 枚举 SCM 里全部 Win32 服务，建 pid → 服务名的映射。
///
/// 只要 `SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE`，这两项对
/// 「已认证用户」默认开放，**不需要管理员**。
///
/// # 与 service provider 的重复
///
/// `providers/service/scm.rs` 也会枚举 SCM。这里刻意只写最小的「pid → 名字」
/// 一张表，不去依赖它——进程 provider 不该为了一个可选字段反向依赖服务
/// provider。两边合并的时机见交付说明。
fn enumerate_service_pids() -> io::Result<HashMap<u32, Option<String>>> {
    // SAFETY: 两个名字为空表示「本机 + 活动数据库」，这是文档规定的用法。
    let scm = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE,
        )
    };
    if scm.is_null() {
        return Err(last_error());
    }
    let scm = ScHandle(scm);

    let mut out: HashMap<u32, Option<String>> = HashMap::with_capacity(256);
    let mut resume: u32 = 0;
    loop {
        let mut needed: u32 = 0;
        let mut returned: u32 = 0;
        // 先问长度：缓冲为空必然失败于 ERROR_MORE_DATA。
        // SAFETY: 缓冲指针为空、容量为 0 时函数只回填 needed。
        unsafe {
            EnumServicesStatusExW(
                scm.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                std::ptr::null_mut(),
                0,
                &raw mut needed,
                &raw mut returned,
                &raw mut resume,
                std::ptr::null(),
            );
        }
        if needed == 0 {
            break;
        }
        if last_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
            return Err(error_from_code(ERROR_MORE_DATA));
        }

        // ENUM_SERVICE_STATUS_PROCESSW 里有指针，缓冲必须按指针对齐。
        // Vec<u8> 只保证 1 字节对齐，因此用 Vec<u64> 兜住 8 字节对齐。
        let words = (needed as usize).div_ceil(8);
        let mut buf = vec![0u64; words];
        // SAFETY: buf 有 words*8 >= needed 字节可写，容量如实给出。
        let ok = unsafe {
            EnumServicesStatusExW(
                scm.0,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                buf.as_mut_ptr().cast::<u8>(),
                (words * 8) as u32,
                &raw mut needed,
                &raw mut returned,
                &raw mut resume,
                std::ptr::null(),
            )
        };
        if ok == 0 && returned == 0 {
            return Err(last_error());
        }

        // SAFETY: 函数保证缓冲前 returned 项是 ENUM_SERVICE_STATUS_PROCESSW，
        // 而 buf 由 Vec<u64> 保证了 8 字节对齐。
        let items = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                returned as usize,
            )
        };
        for item in items {
            let pid = item.ServiceStatusProcess.dwProcessId;
            if pid == 0 || item.lpServiceName.is_null() {
                continue;
            }
            // SAFETY: lpServiceName 指向缓冲内部、以 NUL 结尾的宽串，buf 尚未释放。
            let name = unsafe { wide::from_wide_ptr(item.lpServiceName) };
            if name.is_empty() {
                continue;
            }
            let unit = format!("{name}.service");
            out.entry(pid)
                .and_modify(|slot| {
                    // 同一个 pid 上的第二个服务 → 无法唯一归属，见类型文档。
                    if slot.as_deref() != Some(unit.as_str()) {
                        *slot = None;
                    }
                })
                .or_insert(Some(unit));
        }

        // ok != 0 表示这一轮已经取完；否则 resume 已被回填，继续下一轮。
        if ok != 0 {
            break;
        }
    }
    Ok(out)
}

// ===========================================================================
// 写操作：终止进程与调整优先级
// ===========================================================================

/// 「发信号」在 Windows 上的映射。
///
/// # Windows 没有信号
///
/// 内核里没有可以投递给任意进程的异步通知。最接近的三样东西各有各的不适用：
/// 控制台的 `GenerateConsoleCtrlEvent` 只对**同一个控制台**里的进程有效
/// （服务、GUI 程序、别的会话一律收不到）；窗口消息 `WM_CLOSE` 只对有窗口的
/// 进程有效；`SetEvent` 要双方事先约好一个具名事件。三者都不是「对任意 pid
/// 都成立」的通用机制，因此本 provider 不拿它们冒充信号。
///
/// | `SignalName` | Windows 行为 | 差异 |
/// |---|---|---|
/// | `kill` | `TerminateProcess(h, 9)` | 语义一致：强制终止，进程没有清理机会 |
/// | `term` | `TerminateProcess(h, 15)` | **语义不同**，见下 |
/// | `hup` | 拒绝，`CapabilityUnavailable`（501） | 见下 |
///
/// ## `term` 与 `kill` 的差别只有退出码
///
/// Unix 的 `SIGTERM` 是「请你自己退出」，进程可以捕获它、落盘、关连接再退。
/// **Windows 没有进程级的优雅终止信号**：`TerminateProcess` 一旦发出，目标连一行
/// 清理代码都跑不到，与 `SIGKILL` 完全一样。这里仍然把退出码分别设成 15 与 9，
/// 好让「按了哪个按钮」在事件日志里还能区分，但**不要以为 `term` 更温柔**。
///
/// 真正对应「优雅停止」的东西在 Windows 上是**服务控制**
/// （`ControlService(SERVICE_CONTROL_STOP)`），那是 service provider 的职责。
/// 前端对 Windows 节点应当优先引导用户去停服务，而不是终止进程。
///
/// ## `hup` 为什么直接拒绝
///
/// `SIGHUP` 在 Unix 上是守护进程约定俗成的「重载配置」。Windows 上的对应物是
/// `ControlService(SERVICE_CONTROL_PARAMCHANGE)`——它同样属于 service provider，
/// 而且只对声明接受该控制码的服务有效。进程侧没有任何东西能对应它，
/// 于是如实报 501 并在 `detail` 里指路，而不是拿 `TerminateProcess` 凑数。
/// 做法与 launchd 后端对 `reload` / `mask` 的处理一致。
///
/// # 被拒绝的 pid
///
/// 外壳只挡了 pid 1（Unix 的 init）。Windows 上要挡的是 [`PID_IDLE`] 与
/// [`PID_SYSTEM`]，见 [`reject_system_pid`]。
pub fn send_signal(pid: u32, raw_pid: i32, signal: SignalName) -> ApiResult<()> {
    let _ = raw_pid;
    reject_system_pid(pid)?;
    let exit_code = match signal {
        SignalName::Kill => 9u32,
        SignalName::Term => 15u32,
        SignalName::Hup => {
            return Err(
                ApiError::capability_unavailable("proc", "Windows 没有 SIGHUP").with_detail(
                    "「重载配置」在 Windows 上是服务控制码 SERVICE_CONTROL_PARAMCHANGE，\
                     属于 service provider；进程层面没有可以投递给任意 pid 的信号。",
                ),
            );
        }
    };

    let handle = open_process(pid, PROCESS_TERMINATE).map_err(|e| write_error(pid, "终止", e))?;
    // SAFETY: 句柄刚由 OpenProcess 返回，带 PROCESS_TERMINATE。
    let ok = unsafe { TerminateProcess(handle.raw(), exit_code) };
    if ok == 0 {
        return Err(write_error(pid, "终止", last_error()));
    }
    Ok(())
}

/// 「renice」在 Windows 上的映射：`SetPriorityClass`。
///
/// nice → 优先级类的换算见 [`priority_class_from_nice`]。刻度不是一一对应的，
/// 落进同一个类里的 nice 值效果相同——这是 Windows 只有六档优先级类的结果，
/// 前端不该期待「nice 从 3 调到 4」会有任何可观测的变化。
///
/// `REALTIME_PRIORITY_CLASS`（nice ≤ -16）需要 `SeIncreaseBasePriorityPrivilege`，
/// 普通用户会得到 `ERROR_ACCESS_DENIED` → `PermissionDenied` + 可提权重试，
/// 与 Linux 上非 root 调低 nice 被 `EACCES` 拒绝是同一个形状。
pub fn set_nice(pid: u32, raw_pid: i32, nice: i32) -> ApiResult<()> {
    let _ = raw_pid;
    reject_system_pid(pid)?;

    let handle =
        open_process(pid, PROCESS_SET_INFORMATION).map_err(|e| write_error(pid, "调整优先级", e))?;
    // SAFETY: 句柄刚由 OpenProcess 返回，带 PROCESS_SET_INFORMATION。
    let ok = unsafe { SetPriorityClass(handle.raw(), priority_class_from_nice(nice)) };
    if ok == 0 {
        return Err(write_error(pid, "调整优先级", last_error()));
    }
    Ok(())
}

/// 内核自己的两个 pid 一律拒绝。
///
/// - pid 0 是 System Idle Process，它不是进程，只是「空闲时间」的记账对象；
/// - pid 4 是 `System`，内核本体。终止它当场蓝屏，改它的优先级同样致命。
///
/// 外壳的 `checked_pid` 已经挡掉 0，这里再挡一次是因为这两个函数是 `pub` 的，
/// 不该依赖调用方的前置检查。
fn reject_system_pid(pid: u32) -> ApiResult<()> {
    match pid {
        PID_IDLE => Err(ApiError::invalid_request(
            "不允许操作 PID 0（System Idle Process）",
        )),
        PID_SYSTEM => Err(ApiError::invalid_request(
            "不允许操作 PID 4（System，即 Windows 内核本体）",
        )),
        _ => Ok(()),
    }
}

/// 写操作的错误映射。
///
/// 与 POSIX 侧（[`super::posix`](../posix/index.html)）同一形状：系统拒绝 →
/// `PermissionDenied` + `can_retry_elevated`，由上层决定要不要换 admin worker 重试。
fn write_error(pid: u32, what: &str, e: io::Error) -> ApiError {
    match e.raw_os_error().map(|c| c as u32) {
        // OpenProcess 对不存在的 pid 报 ERROR_INVALID_PARAMETER，
        // 那不是「调用方参数写错了」，而是「这个 pid 没有对应的进程」。
        Some(ERROR_INVALID_PARAMETER) => ApiError::not_found(format!("进程 {pid} 不存在")),
        Some(ERROR_ACCESS_DENIED) => ApiError::permission_denied(format!(
            "系统拒绝对进程 {pid} {what}：需要对该进程的相应访问权\
             （跨用户、受保护进程或更高完整性级别的进程需要管理员令牌）"
        ))
        .with_detail(e.to_string())
        .retry_elevated(),
        _ => ApiError::internal(format!("对进程 {pid} {what}失败")).with_detail(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::process::ProcProvider;
    use strixmaid_types::ErrorCode;
    use strixmaid_types::process::ProcessListQuery;

    fn states(running: u32, ready: u32, waiting: u32, terminated: u32, total: u32) -> ThreadStates {
        ThreadStates {
            running,
            ready,
            waiting,
            terminated,
            total,
        }
    }

    #[test]
    fn 线程状态推导进程状态() {
        // 线程数为 0：进程壳还在，但没有执行体
        assert_eq!(
            state_from_threads(&states(0, 0, 0, 0, 0)),
            ProcessState::Zombie
        );
        // 有线程在跑
        assert_eq!(
            state_from_threads(&states(1, 0, 3, 0, 4)),
            ProcessState::Running
        );
        // 没在跑但排着队，等价于 Linux 的 R
        assert_eq!(
            state_from_threads(&states(0, 2, 3, 0, 5)),
            ProcessState::Running
        );
        // 全在等
        assert_eq!(
            state_from_threads(&states(0, 0, 7, 0, 7)),
            ProcessState::Sleeping
        );
        // 全退完了
        assert_eq!(
            state_from_threads(&states(0, 0, 0, 3, 3)),
            ProcessState::Zombie
        );
        // 一部分退了，剩下的在睡 → 主体还活着
        assert_eq!(
            state_from_threads(&states(0, 0, 1, 2, 3)),
            ProcessState::Sleeping
        );
        // 只剩未分类的过渡态：不猜
        assert_eq!(
            state_from_threads(&states(0, 0, 0, 0, 2)),
            ProcessState::Unknown
        );
    }

    #[test]
    fn nice_与优先级类在锚点上互逆() {
        let expect = [
            (4, 19, IDLE_PRIORITY_CLASS),
            (6, 10, BELOW_NORMAL_PRIORITY_CLASS),
            (8, 0, NORMAL_PRIORITY_CLASS),
            (10, -5, ABOVE_NORMAL_PRIORITY_CLASS),
            (13, -10, HIGH_PRIORITY_CLASS),
            (24, -20, REALTIME_PRIORITY_CLASS),
        ];
        for (base, nice, class) in expect {
            assert_eq!(
                nice_from_base_priority(base),
                nice,
                "基础优先级 {base} 应当映射成 nice {nice}"
            );
            assert_eq!(
                priority_class_from_nice(nice),
                class,
                "nice {nice} 应当映射回同一个优先级类"
            );
        }
    }

    #[test]
    fn nice_映射单调且不越界() {
        let mut prev = nice_from_base_priority(0);
        assert_eq!(prev, 19, "低于 IDLE 的基础优先级夹到 19");
        for base in 1..=40 {
            let nice = nice_from_base_priority(base);
            assert!(
                (-20..=19).contains(&nice),
                "base={base} 算出越界的 nice={nice}"
            );
            assert!(
                nice <= prev,
                "基础优先级越高 nice 应当越小：base={base} nice={nice} 上一个={prev}"
            );
            prev = nice;
        }
        assert_eq!(nice_from_base_priority(31), -20, "最高优先级夹到 -20");
    }

    #[test]
    fn 优先级类覆盖整个_nice_区间() {
        const ALL: [u32; 6] = [
            IDLE_PRIORITY_CLASS,
            BELOW_NORMAL_PRIORITY_CLASS,
            NORMAL_PRIORITY_CLASS,
            ABOVE_NORMAL_PRIORITY_CLASS,
            HIGH_PRIORITY_CLASS,
            REALTIME_PRIORITY_CLASS,
        ];
        for nice in -20..=19 {
            let class = priority_class_from_nice(nice);
            assert!(
                ALL.contains(&class),
                "nice {nice} 映射出未知的优先级类 {class}"
            );
        }
        // 只有最负的那一档才会去碰实时优先级
        assert_ne!(priority_class_from_nice(-15), REALTIME_PRIORITY_CLASS);
        assert_eq!(priority_class_from_nice(-16), REALTIME_PRIORITY_CLASS);
    }

    #[test]
    fn 环境块解析() {
        let mut block: Vec<u16> = Vec::new();
        for item in ["PATH=C:\\Windows", "=C:=C:\\work", "EMPTY=", "中文=值"] {
            block.extend(item.encode_utf16());
            block.push(0);
        }
        block.push(0);

        let env = parse_environment_block(&block);
        assert_eq!(env.get("PATH").map(String::as_str), Some("C:\\Windows"));
        assert_eq!(env.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(env.get("中文").map(String::as_str), Some("值"));
        assert!(
            !env.keys().any(String::is_empty),
            "盘符当前目录那种隐藏项不该混进来：{:?}",
            env.keys().collect::<Vec<_>>()
        );
        assert_eq!(env.len(), 3);
        assert!(parse_environment_block(&[0, 0]).is_empty());
        assert!(parse_environment_block(&[]).is_empty());
    }

    #[test]
    fn 双_nul_定位() {
        // "AB\0\0" 的 UTF-16 小端字节
        let raw = [b'A', 0, b'B', 0, 0, 0, 0, 0];
        assert_eq!(find_double_nul(&raw), Some(4));
        assert_eq!(find_double_nul(&[b'A', 0, b'B', 0]), None);
        // 跨 u16 边界的四个零字节不是终止符：0x0001, 0x0000, 0x0100
        let raw = [1u8, 0, 0, 0, 0, 1];
        assert_eq!(find_double_nul(&raw), None);
    }

    #[test]
    fn 命令行按_windows_规则拆分() {
        let argv = split_command_line(r#""C:\Program Files\app.exe" --flag "a b" c"#);
        assert_eq!(argv, vec![r"C:\Program Files\app.exe", "--flag", "a b", "c"]);
        assert!(
            split_command_line("").is_empty(),
            "空串不该退化成本进程路径"
        );
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn 本机列表_找到自己且字段合理() {
        let provider = ProcProvider::new();
        let me = std::process::id();

        let t0 = Instant::now();
        let first = provider.list_blocking(&ProcessListQuery::default());
        let first_elapsed = t0.elapsed();
        assert!(first.len() > 10, "只枚举到 {} 个进程", first.len());
        assert!(
            !first.iter().any(|p| p.pid == PID_IDLE),
            "System Idle Process 不该出现在列表里"
        );

        let mine = first
            .iter()
            .find(|p| p.pid == me)
            .expect("列表里必须有本进程");
        assert!(mine.cmdline.is_some(), "自己的命令行必须读得到");
        assert!(mine.user.is_some(), "自己的用户名必须解析得出");
        assert_eq!(mine.cpu_percent, 0.0, "首次调用没有基线");
        assert!(mine.threads >= 1);
        assert!(mine.rss_bytes > 0);
        assert!(mine.vms_bytes >= mine.rss_bytes);
        assert!(mine.mem_percent > 0.0, "算不出内存占比说明 mem_total 为 0");
        assert!(
            mine.start_ts > 1_577_836_800,
            "启动时刻不对：{}",
            mine.start_ts
        );
        assert!((-20..=19).contains(&mine.nice));
        assert_eq!(mine.state, ProcessState::Running, "本进程正在跑");
        assert_eq!(mine.io_read_rate, Some(0.0), "首轮没基线，但计数本身测得到");

        std::thread::sleep(super::super::cpu::MIN_INTERVAL);
        let t1 = Instant::now();
        let second = provider.list_blocking(&ProcessListQuery::default());
        let second_elapsed = t1.elapsed();
        assert!(second.iter().any(|p| p.pid == me));

        let named = second.iter().filter(|p| p.user.is_some()).count();
        eprintln!(
            "进程列表：{} 个进程，首轮 {:?}（每进程 {:.1}µs），次轮 {:?}（每进程 {:.1}µs）；\
             身份可读 {named}/{}",
            second.len(),
            first_elapsed,
            first_elapsed.as_secs_f64() / first.len().max(1) as f64 * 1e6,
            second_elapsed,
            second_elapsed.as_secs_f64() / second.len().max(1) as f64 * 1e6,
            second.len(),
        );
        assert!(
            second_elapsed.as_secs() < 2,
            "进程列表耗时异常：{second_elapsed:?}"
        );
    }

    #[test]
    fn 本进程详情() {
        let provider = ProcProvider::new();
        let me = std::process::id();
        let d = provider.detail_blocking(me).unwrap();

        assert_eq!(d.summary.pid, me);
        assert!(!d.cmdline_args.is_empty(), "自己的 argv 必须拆得出");
        let exe = d.exe.as_deref().expect("自己的 exe 总该读得到");
        assert!(exe.contains(':'), "应为 Win32 路径而不是 \\Device\\…：{exe}");
        let cwd = d.cwd.as_deref().expect("自己的 cwd 必须读得到");
        assert!(cwd.contains(':'), "cwd 形状不对：{cwd}");
        let env = d.environ.as_ref().expect("自己的环境变量必须读得到");
        assert!(
            env.keys().any(|k| k.eq_ignore_ascii_case("PATH")),
            "环境变量里必须有 PATH：{:?}",
            env.keys().take(10).collect::<Vec<_>>()
        );
        assert!(d.euid.is_some());
        assert_eq!(d.euid, Some(d.summary.uid), "Windows 只有一张主令牌");
        assert!(d.gid.is_some());
        assert!(d.io_read_bytes.is_some(), "IO 计数随进程表一起来，恒可读");

        // 如实缺席的那几个
        assert_eq!(d.tty, None, "Windows 的控制台不是设备文件");
        assert_eq!(d.cgroup, None, "Windows 没有 cgroup");
        assert_eq!(d.fds, None, "句柄表要全局枚举，代价与收益不匹配");
        assert_eq!(d.unit, None, "测试进程不是服务");

        eprintln!(
            "本进程详情：exe={exe}\n  cwd={cwd}\n  argv={:?}\n  \
             uid={} user={:?} gid={:?}\n  环境变量 {} 项，累计读 {:?} 字节",
            d.cmdline_args,
            d.summary.uid,
            d.summary.user,
            d.gid,
            env.len(),
            d.io_read_bytes,
        );
    }

    #[test]
    fn 不存在的进程() {
        let provider = ProcProvider::new();
        // Windows 的 pid 是 4 的倍数，且远小于这个值
        let ghost = 2_000_000_000;
        assert_eq!(
            provider.detail_blocking(ghost).unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            provider.signal(ghost, SignalName::Term).unwrap_err().code,
            ErrorCode::NotFound
        );
        assert_eq!(
            provider.renice(ghost, 5).unwrap_err().code,
            ErrorCode::NotFound
        );
    }

    #[test]
    fn 拒绝操作内核自身的两个_pid() {
        let provider = ProcProvider::new();
        // 外壳只挡 pid 0 与 pid 1，pid 4 必须由后端自己挡住
        assert_eq!(
            provider.signal(PID_SYSTEM, SignalName::Kill).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            provider.renice(PID_SYSTEM, 10).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        // 直接调后端同样要被拒（这两个函数是 pub 的）
        for pid in [PID_IDLE, PID_SYSTEM] {
            assert_eq!(
                send_signal(pid, pid as i32, SignalName::Term)
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidRequest,
                "pid {pid} 必须被拒"
            );
            assert_eq!(
                set_nice(pid, pid as i32, 0).unwrap_err().code,
                ErrorCode::InvalidRequest,
                "pid {pid} 必须被拒"
            );
        }
        // System 仍然活着
        assert!(
            ntdll::system_processes()
                .unwrap()
                .iter()
                .any(|p| p.pid == PID_SYSTEM)
        );
    }

    #[test]
    fn hup_如实报不可用而不是拿终止凑数() {
        let provider = ProcProvider::new();
        let me = std::process::id();
        let err = provider.signal(me, SignalName::Hup).unwrap_err();
        assert_eq!(err.code, ErrorCode::CapabilityUnavailable);
        assert_eq!(err.capability.as_deref(), Some("proc"));
        assert!(err.detail.is_some(), "必须说明去哪儿找等价物");
        // 关键：本进程还活着
        assert!(
            ntdll::system_processes()
                .unwrap()
                .iter()
                .any(|p| p.pid == me),
            "hup 绝不能真的把进程终止掉"
        );
    }

    #[test]
    fn 服务_pid_映射可用() {
        let map = ServicePids::new();
        let table = map.snapshot();
        assert!(
            !table.is_empty(),
            "任何一台 Windows 都有正在运行的服务，一个都枚举不到说明 SCM 调用写错了"
        );
        let unique = table.values().filter(|v| v.is_some()).count();
        for unit in table.values().flatten() {
            assert!(unit.ends_with(".service"), "unit 命名不对：{unit}");
            assert!(unit.len() > ".service".len());
        }
        // 缓存命中时拿到的是同一份快照
        assert!(Arc::ptr_eq(&table, &map.snapshot()));
        eprintln!(
            "服务宿主进程：{unique} 个可唯一归属，{} 个被多个服务共享（报 None）",
            table.len() - unique
        );
    }

    /// 服务的宿主进程在详情里应当带上 unit，这是 Windows 上 `unit` 的唯一来源。
    #[test]
    fn 服务进程的详情带_unit() {
        let map = ServicePids::new();
        let table = map.snapshot();
        let Some((pid, unit)) = table
            .iter()
            .find_map(|(pid, unit)| unit.as_ref().map(|u| (*pid, u.clone())))
        else {
            eprintln!("本机没有可唯一归属的服务宿主进程，跳过");
            return;
        };
        let provider = ProcProvider::new();
        match provider.detail_blocking(pid) {
            Ok(d) => {
                assert_eq!(d.unit.as_deref(), Some(unit.as_str()));
                eprintln!("服务进程 pid={pid} unit={unit} name={}", d.summary.name);
            }
            // 服务可能在两次调用之间停掉
            Err(e) => assert_eq!(e.code, ErrorCode::NotFound, "意外的错误：{e:?}"),
        }
    }

    #[test]
    fn 物理内存总量读得到() {
        let total = total_physical_memory();
        assert!(total > 512 * 1024 * 1024, "物理内存只有 {total} 字节？");
        assert!(total < 1 << 44);
        eprintln!("物理内存 {:.1} GiB", total as f64 / 1024.0 / 1024.0 / 1024.0);
    }

    #[test]
    fn 探测可用() {
        assert_eq!(probe(), Probe::Available);
    }

    /// 打不开的进程必须老实报「身份未知」，而不是随手给一个 uid。
    #[test]
    fn 打不开的进程身份为空() {
        let facts = facts_of_process(PID_SYSTEM);
        if crate::platform::windows::token::is_elevated() {
            eprintln!("当前进程已提升，System 可能打得开：{facts:?}");
            return;
        }
        assert_eq!(
            facts,
            ProcFacts::default(),
            "未提升时打不开 System，应当整个退化成默认值"
        );
    }
}
