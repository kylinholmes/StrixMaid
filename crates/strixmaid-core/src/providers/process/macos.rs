//! macOS 进程枚举后端：`libproc`（`proc_listpids` / `proc_pidinfo` / `proc_pidpath`）
//! 加 `sysctl KERN_PROCARGS2`。
//!
//! # 一次调用拿齐大部分字段
//!
//! `proc_pidinfo(pid, PROC_PIDTASKALLINFO, …)` 返回 [`libc::proc_taskallinfo`]，
//! 里面同时有 BSD 侧（pid / ppid / uid / 名字 / nice / 状态 / 启动时刻）与 task 侧
//! （RSS / 虚拟大小 / 线程数 / 累计 CPU 时间）。列表遍历因此是「每进程一次系统调用」，
//! 与 Linux 版每进程读两个 `/proc` 文件的代价相当。
//!
//! # CPU%
//!
//! `pti_total_user` / `pti_total_system` 的单位是**纳秒**，不是 Linux 的 jiffies。
//! 差分公式 `Δticks / hz / Δ墙钟 × 100` 本身通用，把纳秒当 tick、`hz` 取
//! 10⁹ 即可，[`super::cpu`] 不需要任何改动。
//!
//! # 拿不到的字段
//!
//! 这些在 macOS 上要么需要额外一大串 `proc_pidinfo` 调用、要么根本没有对应概念，
//! 一律填 `None`——DTO 的 `Option` 就是为此存在的，不编数据：
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `cgroup` / `unit` | macOS 没有 cgroup；launchd 的归属关系不在进程属性里 |
//! | `fds` | 需要 `PROC_PIDLISTFDS` 再对每个 fd 单独取路径，代价与联调收益不匹配 |
//! | `tty` | `e_tdev` 是设备号，映射回名字要扫 `/dev` |
//!
//! `cwd` 曾在此列（`libc` 未声明 `proc_vnodepathinfo`），12 号方案 C 期需要它做
//! 终端 cwd 的兜底轮询，改为自带结构体声明取之（见 [`cwd_path`]）。
//!
//! # 磁盘 IO
//!
//! `proc_pid_rusage(pid, RUSAGE_INFO_V2, …)` 的 `ri_diskio_bytesread/written`
//! 是累计磁盘字节数（`libc` 未声明，本文件手写 FFI）。**只有同 uid 或 root 能读**
//! 别人的进程，读不到 → 速率为 `None`。差分见 [`super::io`]。
//!
//! `cmdline` 与 `environ` 走 `sysctl KERN_PROCARGS2`：**只有同 uid 或 root 能读**，
//! 别人的进程会退化成 `None`（列表里则退回进程名）。这与 Linux 上读不到
//! `/proc/<pid>/environ` 的表现一致。

use std::collections::{BTreeMap, HashSet};

use strixmaid_types::process::{ProcessDetail, ProcessState, ProcessSummary};
use strixmaid_types::{ApiError, ApiResult};

use super::super::Probe;
use super::cpu::CpuSamples;
use super::io::IoSamples;
use super::{Context, users::UserTable};
use crate::platform::macos::{c_array_to_string, sysctl_scalar};

/// `libproc.h` 的 `PROC_ALL_PIDS`：列出全部 pid，不按类型过滤。
///
/// `libc` 没有为 Apple 目标导出这个常量，值取自 SDK 头文件 `sys/proc_info.h`。
const PROC_ALL_PIDS: u32 = 1;

/// `pti_total_*` 是纳秒，把它当 tick 时对应的「每秒 tick 数」。
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// libproc 总是可用——它是 macOS 的系统库，不存在「探测不到」的情况。
/// 真读不出任何进程才算不可用（例如极端受限的沙箱）。
pub fn probe() -> Probe {
    match list_pids() {
        Some(pids) if !pids.is_empty() => Probe::Available,
        _ => Probe::unavailable("proc_listpids 未返回任何进程"),
    }
}

/// macOS 后端。构造时读一次运行期恒定的常量。
pub struct Backend {
    /// `hw.memsize`。进程遍历期间不会变，构造时读一次即可。
    mem_total: u64,
    /// `kern.argmax`：一个进程的参数 + 环境的字节上限，通常 1 MiB。
    ///
    /// 缓存它是为了让 [`Self::list`] 只分配**一次**缓冲区并在几百个进程之间复用。
    /// 每进程各分配一次 1 MiB 会让一轮列表多出几百兆的分配量，
    /// 这是 macOS 后端与 Linux 后端（每进程读一个几百字节的 `/proc` 文件）
    /// 代价差距最大的地方，必须在这里抹平。
    argmax: usize,
}

impl Default for Backend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            mem_total: sysctl_scalar::<u64>("hw.memsize").unwrap_or(0),
            argmax: argmax(),
        }
    }

    /// 物理内存总量。macOS 上这个值在运行期恒定，直接返回缓存。
    pub fn mem_total(&self) -> u64 {
        self.mem_total
    }

    /// 遍历全部 pid，顺带更新 CPU / IO 快照并清理已消失的 pid。
    pub fn list(
        &self,
        cpu: &mut CpuSamples,
        io: &mut IoSamples,
        ctx: &Context,
    ) -> Vec<ProcessSummary> {
        let Some(pids) = list_pids() else {
            return Vec::new();
        };
        let mut all = Vec::with_capacity(pids.len());
        let mut seen: HashSet<u32> = HashSet::with_capacity(pids.len());
        // 整轮共用一块缓冲，见 `Backend::argmax` 的说明。
        let mut buf = vec![0u8; self.argmax];
        for pid in pids {
            // 遍历期间进程随时可能退出，取不到信息就跳过——不是错误。
            let Some(info) = task_all_info(pid) else {
                continue;
            };
            let cmdline = ProcArgs::read_into(pid as i32, &mut buf)
                .map(|a| a.argv.join(" "))
                .filter(|s| !s.is_empty());
            let summary = summarize(pid, &info, cmdline, cpu, io, ctx);
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
        let info =
            task_all_info(pid).ok_or_else(|| ApiError::not_found(format!("进程 {pid} 不存在")))?;
        let mut buf = vec![0u8; self.argmax];
        let args = ProcArgs::read_into(raw_pid, &mut buf);
        let cmdline = args
            .as_ref()
            .map(|a| a.argv.join(" "))
            .filter(|s| !s.is_empty());
        let summary = summarize(pid, &info, cmdline, cpu, io, ctx);
        let disk = disk_io(pid);

        Ok(ProcessDetail {
            summary,
            cmdline_args: args.as_ref().map(|a| a.argv.clone()).unwrap_or_default(),
            exe: exe_path(raw_pid).or_else(|| args.as_ref().map(|a| a.exec_path.clone())),
            cwd: cwd_path(raw_pid),
            euid: Some(info.pbsd.pbi_uid),
            gid: Some(info.pbsd.pbi_gid),
            tty: None,
            cgroup: None,
            unit: None,
            environ: args.map(|a| a.environ),
            fds: None,
            io_read_bytes: disk.map(|(r, _)| r),
            io_write_bytes: disk.map(|(_, w)| w),
        })
    }
}

/// 把一次 `proc_taskallinfo` 转成 [`ProcessSummary`]，同时更新 CPU 快照。
///
/// `cmdline` 由调用方给出：列表与详情共用一块 `KERN_PROCARGS2` 缓冲，
/// 不在这里重复读取。
fn summarize(
    pid: u32,
    info: &libc::proc_taskallinfo,
    cmdline: Option<String>,
    cpu: &mut CpuSamples,
    io: &mut IoSamples,
    ctx: &Context,
) -> ProcessSummary {
    let bsd = &info.pbsd;
    let task = &info.ptinfo;

    // 累计 CPU 纳秒。starttime 用启动时刻（秒）做 pid 复用的判别键，
    // 作用与 Linux 版的 stat.starttime 相同。
    let nanos = task.pti_total_user.saturating_add(task.pti_total_system);
    let cpu_percent = cpu
        .observe(pid, bsd.pbi_start_tvsec, nanos, ctx.now, NANOS_PER_SEC)
        .unwrap_or(0.0);

    // pbi_name 有 32 字节，比 16 字节的 pbi_comm 少截断；为空时退回 comm。
    let name = {
        let long = c_array_to_string(&bsd.pbi_name);
        if long.is_empty() {
            c_array_to_string(&bsd.pbi_comm)
        } else {
            long
        }
    };
    let rss_bytes = task.pti_resident_size;
    let uid = bsd.pbi_uid;

    // 磁盘累计字节 → 差分成速率。读不到（别人的进程）→ None，
    // 读得到但首轮没基线 → Some(0.0)，与 types 文档一致。
    let (io_read_rate, io_write_rate) = match disk_io(pid) {
        Some((r, w)) => {
            let (rr, wr) = io
                .observe(pid, bsd.pbi_start_tvsec, r, w, ctx.now)
                .unwrap_or((0.0, 0.0));
            (Some(rr), Some(wr))
        }
        None => (None, None),
    };

    ProcessSummary {
        pid,
        ppid: bsd.pbi_ppid,
        cmdline,
        name,
        uid,
        user: name_of_uid(&ctx.users, uid),
        state: map_state(bsd.pbi_status),
        cpu_percent,
        rss_bytes,
        vms_bytes: task.pti_virtual_size,
        mem_percent: ctx.mem_percent(rss_bytes),
        threads: u32::try_from(task.pti_threadnum).unwrap_or(0),
        start_ts: bsd.pbi_start_tvsec as i64,
        nice: bsd.pbi_nice,
        io_read_rate,
        io_write_rate,
    }
}

/// `rusage_info_v2`（`sys/resource.h`）。`libc` 没有导出，按头文件手写布局；
/// 只用到最后两个字段，但前面的必须逐一列出才能对上偏移。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RUsageInfoV2 {
    ri_uuid: [u8; 16],
    ri_user_time: u64,
    ri_system_time: u64,
    ri_pkg_idle_wkups: u64,
    ri_interrupt_wkups: u64,
    ri_pageins: u64,
    ri_wired_size: u64,
    ri_resident_size: u64,
    ri_phys_footprint: u64,
    ri_proc_start_abstime: u64,
    ri_proc_exit_abstime: u64,
    ri_child_user_time: u64,
    ri_child_system_time: u64,
    ri_child_pkg_idle_wkups: u64,
    ri_child_interrupt_wkups: u64,
    ri_child_pageins: u64,
    ri_child_elapsed_abstime: u64,
    ri_diskio_bytesread: u64,
    ri_diskio_byteswritten: u64,
}

/// `sys/resource.h` 的 `RUSAGE_INFO_V2`。
const RUSAGE_INFO_V2: libc::c_int = 2;

unsafe extern "C" {
    /// `libproc.h`：`int proc_pid_rusage(int pid, int flavor, rusage_info_t *buffer)`。
    /// `rusage_info_t` 是 `void *`，按 flavor 决定实际结构，这里恒用 V2。
    fn proc_pid_rusage(pid: libc::c_int, flavor: libc::c_int, buffer: *mut RUsageInfoV2)
    -> libc::c_int;
}

/// 进程累计磁盘（读, 写）字节数。别人的进程无权限 / 进程已退出 → `None`。
fn disk_io(pid: u32) -> Option<(u64, u64)> {
    let mut info = std::mem::MaybeUninit::<RUsageInfoV2>::zeroed();
    // SAFETY: buffer 指向一整个 RUsageInfoV2，布局照 SDK 头文件逐字段抄写。
    let rc = unsafe { proc_pid_rusage(pid as libc::c_int, RUSAGE_INFO_V2, info.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: rc == 0 表示内核已写满结构体。
    let info = unsafe { info.assume_init() };
    Some((info.ri_diskio_bytesread, info.ri_diskio_byteswritten))
}

/// uid → 用户名。
///
/// 先查 `/etc/passwd`（[`UserTable`] 的快照），查不到再落到 `getpwuid_r`。
/// macOS 的 `/etc/passwd` 只有系统账户，真实用户住在 Open Directory 里，
/// 只靠文件会让所有 uid ≥ 500 的进程都显示不出用户名。`getpwuid_r` 走 NSS/DS，
/// 在动态链接的 macOS 上是可用的（Linux 侧因为静态 musl 才刻意避开它，见 [`super::users`]）。
fn name_of_uid(table: &UserTable, uid: u32) -> Option<String> {
    if let Some(name) = table.name_of(uid) {
        return Some(name.to_owned());
    }
    getpwuid_name(uid)
}

/// `getpwuid_r(3)`。缓冲区不够或用户不存在时返回 `None`。
fn getpwuid_name(uid: u32) -> Option<String> {
    let mut pwd = std::mem::MaybeUninit::<libc::passwd>::zeroed();
    let mut buf = vec![0 as libc::c_char; 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: pwd 与 buf 都是本栈帧上合法的可写内存，buflen 如实描述 buf 大小；
    // 成功时 result 指向 pwd，其中的字符串指针指向 buf 内部。
    let rc = unsafe {
        libc::getpwuid_r(
            uid,
            pwd.as_mut_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut result,
        )
    };
    if rc != 0 || result.is_null() {
        return None;
    }
    // SAFETY: result 非空即表示 pwd 已初始化，pw_name 指向 buf 内以 NUL 结尾的字符串。
    let name = unsafe { std::ffi::CStr::from_ptr((*result).pw_name) };
    Some(name.to_string_lossy().into_owned())
}

/// `p_stat` → [`ProcessState`]。
///
/// macOS 只有五种状态，没有 Linux 的 `D`（不可中断睡眠）、`t`（被 trace 暂停）与 `I`。
pub fn map_state(status: u32) -> ProcessState {
    match status {
        libc::SIDL => ProcessState::Idle,
        libc::SRUN => ProcessState::Running,
        libc::SSLEEP => ProcessState::Sleeping,
        libc::SSTOP => ProcessState::Stopped,
        libc::SZOMB => ProcessState::Zombie,
        _ => ProcessState::Unknown,
    }
}

/// 全部 pid。
fn list_pids() -> Option<Vec<u32>> {
    // SAFETY: buffer 为空、buffersize 为 0 时只返回所需字节数，不写内存。
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return None;
    }

    // 两次调用之间可能有新进程，多留一些余量。
    let cap = bytes as usize / std::mem::size_of::<libc::pid_t>() + 64;
    let mut buf = vec![0 as libc::pid_t; cap];
    let size = (cap * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: buf 有 size 字节可写，size 如实描述其大小。
    let got = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            size,
        )
    };
    if got <= 0 {
        return None;
    }
    let n = got as usize / std::mem::size_of::<libc::pid_t>();
    // pid 0（kernel_task 的占位）不是可操作的进程，滤掉。
    Some(
        buf[..n.min(cap)]
            .iter()
            .filter(|p| **p > 0)
            .map(|p| *p as u32)
            .collect(),
    )
}

/// `proc_pidinfo(pid, PROC_PIDTASKALLINFO, …)`。进程不存在或无权限时返回 `None`。
fn task_all_info(pid: u32) -> Option<libc::proc_taskallinfo> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskallinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskallinfo>() as libc::c_int;
    // SAFETY: buffer 指向一整个 proc_taskallinfo，buffersize 如实描述其大小。
    let got = unsafe {
        libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKALLINFO,
            0,
            info.as_mut_ptr().cast::<libc::c_void>(),
            size,
        )
    };
    // 返回值小于结构体大小说明内核没填满，此时里面的内容不可信。
    if got != size {
        return None;
    }
    // SAFETY: 内核已写满整个结构体。
    Some(unsafe { info.assume_init() })
}

// ---------------------------------------------------------------- cwd

/// `proc_pidinfo` 的 `PROC_PIDVNODEPATHINFO` flavor（XNU `sys/proc_info.h`）。
const PROC_PIDVNODEPATHINFO: libc::c_int = 9;

/// 下面三个结构体是 XNU `sys/proc_info.h` 的逐字段复刻——`libc` crate 没有声明
/// 它们。**布局是内核 ABI**（`proc_pidinfo` 按 `sizeof` 校验调用方缓冲区，错一个
/// 字段整个调用返回 0），字段名保持与头文件一致以便对照。
#[repr(C)]
struct VinfoStat {
    vst_dev: u32,
    vst_mode: u16,
    vst_nlink: u16,
    vst_ino: u64,
    vst_uid: libc::uid_t,
    vst_gid: libc::gid_t,
    vst_atime: i64,
    vst_atimensec: i64,
    vst_mtime: i64,
    vst_mtimensec: i64,
    vst_ctime: i64,
    vst_ctimensec: i64,
    vst_birthtime: i64,
    vst_birthtimensec: i64,
    vst_size: libc::off_t,
    vst_blocks: i64,
    vst_blksize: i32,
    vst_flags: u32,
    vst_gen: u32,
    vst_rdev: u32,
    vst_qspare: [i64; 2],
}

/// 见 [`VinfoStat`]。
#[repr(C)]
struct VnodeInfoPath {
    vip_stat: VinfoStat,
    vip_type: libc::c_int,
    vip_pad: libc::c_int,
    vip_fsid: [i32; 2],
    vip_path: [u8; libc::PATH_MAX as usize],
}

/// 见 [`VinfoStat`]。`pvi_cdir` 是当前目录，`pvi_rdir` 是 chroot 根（通常为空）。
#[repr(C)]
struct ProcVnodePathInfo {
    pvi_cdir: VnodeInfoPath,
    pvi_rdir: VnodeInfoPath,
}

/// 进程的当前工作目录（`PROC_PIDVNODEPATHINFO`）。
///
/// 拿不到（进程不存在、无权限——只对同 uid 或特权进程可见）返回 `None`。
/// 12 号方案 §4.4 的兜底轮询靠它：终端标签在没有 OSC 7 时按 `TerminalInfo.pid`
/// 轮询这个字段跟随 `cd`。
fn cwd_path(pid: i32) -> Option<String> {
    // SAFETY: 全是整数与字节数组，全零是合法初值。
    let mut info: ProcVnodePathInfo = unsafe { std::mem::zeroed() };
    let want = std::mem::size_of::<ProcVnodePathInfo>() as libc::c_int;
    // SAFETY: 缓冲区大小如实给出；内核只在返回值 == want 时保证填满。
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast::<libc::c_void>(),
            want,
        )
    };
    if got != want {
        return None;
    }
    let path = &info.pvi_cdir.vip_path;
    let len = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    if len == 0 {
        return None;
    }
    String::from_utf8(path[..len].to_vec()).ok()
}

/// 进程的可执行文件绝对路径。
fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buf 有 buffersize 字节可写。
    let n = unsafe {
        libc::proc_pidpath(
            pid,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    String::from_utf8(buf).ok().filter(|s| !s.is_empty())
}

/// `KERN_PROCARGS2` 解析出来的参数与环境。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcArgs {
    /// `argv[0]` 之前那个完整的可执行路径（内核单独放在最前面）。
    pub exec_path: String,
    pub argv: Vec<String>,
    pub environ: BTreeMap<String, String>,
}

/// `kern.argmax`：一个进程的参数 + 环境的字节上限。读不到时按 256 KiB 兜底。
fn argmax() -> usize {
    sysctl_scalar::<libc::c_int>("kern.argmax")
        .unwrap_or(256 * 1024)
        .max(4096) as usize
}

impl ProcArgs {
    /// 读并解析一个进程的 `KERN_PROCARGS2`，复用调用方提供的缓冲。
    ///
    /// 非同 uid 且非 root 时内核直接拒绝，返回 `None`。
    ///
    /// `buf` 的长度必须至少是 [`argmax`]——`KERN_PROCARGS2` 不支持
    /// 「先用 `oldp = NULL` 问实际长度」那套：空指针时它返回的是 `kern.argmax`
    /// 这个**上限**而非实际长度，所以只能一次给足再按返回值截断。
    pub fn read_into(pid: i32, buf: &mut [u8]) -> Option<ProcArgs> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut len = buf.len();
        // SAFETY: mib 指向 3 个 c_int；oldp 指向 len 字节的可写内存，
        // 内核把实际写入的字节数回填进 len。
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buf.as_mut_ptr().cast::<libc::c_void>(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len > buf.len() {
            return None;
        }
        Self::parse(&buf[..len])
    }

    /// 自带缓冲的便利版本，只在一次性调用（测试、单个进程）时用。
    /// 批量遍历务必用 [`Self::read_into`] 复用缓冲。
    pub fn read(pid: i32) -> Option<ProcArgs> {
        let mut buf = vec![0u8; argmax()];
        Self::read_into(pid, &mut buf)
    }

    /// 解析 `KERN_PROCARGS2` 的字节布局，与 FFI 无关，可单测。
    ///
    /// 布局：`i32 argc` + 可执行路径（NUL 结尾）+ 若干 NUL 填充 +
    /// `argc` 个以 NUL 分隔的参数 + 环境变量（以 NUL 分隔，到缓冲区末尾或空串为止）。
    pub fn parse(raw: &[u8]) -> Option<ProcArgs> {
        const ARGC_LEN: usize = std::mem::size_of::<i32>();
        if raw.len() < ARGC_LEN {
            return None;
        }
        let argc = i32::from_ne_bytes(raw[..ARGC_LEN].try_into().ok()?);
        if argc < 0 {
            return None;
        }

        let mut rest = &raw[ARGC_LEN..];
        // 可执行路径
        let end = rest.iter().position(|b| *b == 0)?;
        let exec_path = String::from_utf8_lossy(&rest[..end]).into_owned();
        rest = &rest[end..];
        // 内核在路径后面补若干 NUL 做对齐，全部跳过
        while rest.first() == Some(&0) {
            rest = &rest[1..];
        }

        let mut argv = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            let Some(end) = rest.iter().position(|b| *b == 0) else {
                break;
            };
            argv.push(String::from_utf8_lossy(&rest[..end]).into_owned());
            rest = &rest[end + 1..];
        }

        // 余下是环境变量，形如 KEY=VALUE，以 NUL 分隔。
        // 不含 `=` 的条目跳过（尾部可能有对齐填充产生的空串）。
        let mut environ = BTreeMap::new();
        for item in rest.split(|b| *b == 0) {
            if item.is_empty() {
                continue;
            }
            let s = String::from_utf8_lossy(item);
            if let Some((k, v)) = s.split_once('=') {
                environ.insert(k.to_owned(), v.to_owned());
            }
        }

        Some(ProcArgs {
            exec_path,
            argv,
            environ,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::process::ProcProvider;

    /// 结构体布局对不对，唯一可信的裁判是内核自己：对本进程取 cwd，
    /// 必须与 `std::env::current_dir` 一致。布局错一个字段，`proc_pidinfo`
    /// 返回 0（测试转红）或路径字段错位（比对失败）。
    #[test]
    fn cwd_与_current_dir_一致() {
        let real = std::env::current_dir().unwrap();
        let got = cwd_path(std::process::id() as i32).expect("本进程的 cwd 必然可取");
        assert_eq!(std::path::PathBuf::from(got), real);
    }

    #[test]
    fn 不存在的进程取不到_cwd() {
        // pid 0 是内核，普通进程无权限；负 pid 直接非法。
        assert_eq!(cwd_path(-1), None);
    }

    #[test]
    fn 状态映射() {
        assert_eq!(map_state(libc::SRUN), ProcessState::Running);
        assert_eq!(map_state(libc::SSLEEP), ProcessState::Sleeping);
        assert_eq!(map_state(libc::SZOMB), ProcessState::Zombie);
        assert_eq!(map_state(libc::SSTOP), ProcessState::Stopped);
        assert_eq!(map_state(99), ProcessState::Unknown);
    }

    #[test]
    fn 解析_procargs2_布局() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&2i32.to_ne_bytes());
        raw.extend_from_slice(b"/usr/bin/tool\0");
        raw.extend_from_slice(&[0, 0, 0]); // 对齐填充
        raw.extend_from_slice(b"tool\0");
        raw.extend_from_slice(b"--flag\0");
        raw.extend_from_slice(b"PATH=/bin\0");
        raw.extend_from_slice(b"HOME=/Users/x\0");
        raw.extend_from_slice(b"NOTANENV\0"); // 无 `=`，应被跳过

        let a = ProcArgs::parse(&raw).unwrap();
        assert_eq!(a.exec_path, "/usr/bin/tool");
        assert_eq!(a.argv, vec!["tool", "--flag"]);
        assert_eq!(a.environ.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(a.environ.get("HOME").map(String::as_str), Some("/Users/x"));
        assert!(!a.environ.contains_key("NOTANENV"));
        assert_eq!(a.environ.len(), 2);
    }

    #[test]
    fn 解析_procargs2_异常输入() {
        assert!(ProcArgs::parse(&[]).is_none(), "空缓冲");
        assert!(ProcArgs::parse(&[1, 2]).is_none(), "不足以放下 argc");
        assert!(ProcArgs::parse(&(-1i32).to_ne_bytes()).is_none(), "负 argc");
        // argc 声称有 5 个但实际只给了 1 个：解析到没有 NUL 就停，不 panic
        let mut raw = Vec::new();
        raw.extend_from_slice(&5i32.to_ne_bytes());
        raw.extend_from_slice(b"/bin/x\0\0");
        raw.extend_from_slice(b"x\0");
        let a = ProcArgs::parse(&raw).unwrap();
        assert_eq!(a.argv, vec!["x"]);
    }

    #[test]
    fn 本机能读到自己的参数与环境() {
        let me = std::process::id() as i32;
        let a = ProcArgs::read(me).expect("自己的进程一定读得到");
        assert!(!a.exec_path.is_empty());
        assert!(!a.argv.is_empty());
        // 测试进程一定有 PATH
        assert!(
            a.environ.contains_key("PATH"),
            "环境变量：{:?}",
            a.environ.keys()
        );
    }

    #[test]
    fn 本机能读到自己的_exe_与_taskinfo() {
        let me = std::process::id();
        let path = exe_path(me as i32).expect("proc_pidpath");
        assert!(path.starts_with('/'), "应为绝对路径：{path}");
        let info = task_all_info(me).expect("PROC_PIDTASKALLINFO");
        assert_eq!(info.pbsd.pbi_pid, me);
        assert!(info.ptinfo.pti_resident_size > 0);
        assert!(info.ptinfo.pti_threadnum >= 1);
    }

    #[test]
    fn 用户名解析走_open_directory() {
        // SAFETY: getuid 无副作用。
        let uid = unsafe { libc::getuid() };
        assert!(
            getpwuid_name(uid).is_some(),
            "当前 uid {uid} 必须能解析出用户名"
        );
        // 一个几乎不可能存在的 uid
        assert_eq!(getpwuid_name(4_000_000_000), None);
    }

    #[test]
    fn macos_特有字段为_none_而非编造() {
        let provider = ProcProvider::new();
        let d = provider.detail_blocking(std::process::id()).unwrap();
        assert_eq!(d.cgroup, None, "macOS 没有 cgroup");
        assert_eq!(d.unit, None);
        assert_eq!(d.fds, None);
        // 自己的进程 rusage 可读:累计磁盘字节与速率都该在
        assert!(d.io_read_bytes.is_some(), "proc_pid_rusage 对自己必须可读");
        assert!(d.summary.io_read_rate.is_some());
        // 自己的进程，这些必须有
        assert!(d.environ.as_ref().is_some_and(|e| !e.is_empty()));
        assert!(d.summary.user.is_some(), "自己的用户名必须解析得出");
    }
}
