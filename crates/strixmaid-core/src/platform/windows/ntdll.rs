//! `NtQuerySystemInformation` / `NtQueryInformationProcess` 的最小封装。
//!
//! # 为什么非它不可
//!
//! 这两个函数不在 `windows-sys` 的公开表面里（微软把它们标成「可能变更」的
//! 半公开 API），所以这里手写 `extern` 声明——与 helper 的自写 PAM FFI、
//! [`crate::platform::iokit`] 同一做法。
//!
//! 代价是「可能变更」，收益是**一次调用取全整张进程表**。替代方案的代价：
//!
//! | 方案 | 一次进程列表的系统调用数（约 300 个进程） | 拿不到的东西 |
//! |---|---|---|
//! | `SystemProcessInformation` | **1** | — |
//! | `CreateToolhelp32Snapshot` + 逐进程 `OpenProcess` × 3~4 个 API | **1000+** | 受保护进程连内存都读不到 |
//! | PDH 性能计数器 | 2（要两轮） | pid/ppid 关系、精确的 IO 计数 |
//!
//! 这正是 Linux 侧「读 `/proc` 一圈」的等价物：pid、ppid、名字、线程数、
//! CPU 时间、内存、IO 计数**一次拿齐**，且不需要对任何进程开句柄。
//!
//! 结构体布局逐字段抄自 SDK 的 `winternl.h` 与公开文档。**顺序与填充不能改**：
//! 抄错一个字段不会编译失败，只会让后面所有字段读出垃圾。
//! [`tests::进程表自洽`] 用「本进程必须出现在表里且各字段合理」把这一点钉住。
//!
//! # 版本兼容
//!
//! 结构体自 Windows 7 起稳定（Vista 与 Win7 各加过一次字段，都在尾部之前，
//! 本文件按 Win7+ 的布局写）。本项目的最低目标是 Windows 10——
//! ConPTY（终端）本来就要求 1809+，见 `worker/terminal/windows.rs`。

use std::io;

use windows_sys::Win32::Foundation::HANDLE;

use super::{filetime_to_unix, wide};

// ===========================================================================
// FFI
// ===========================================================================

/// `SYSTEM_INFORMATION_CLASS::SystemBasicInformation`
const SYSTEM_BASIC_INFORMATION: u32 = 0;
/// `SystemProcessInformation`
const SYSTEM_PROCESS_INFORMATION: u32 = 5;
/// `SystemProcessorPerformanceInformation`
const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;
/// `SystemTimeOfDayInformation`
const SYSTEM_TIMEOFDAY_INFORMATION: u32 = 3;
/// `SystemPagefileInformation`
const SYSTEM_PAGEFILE_INFORMATION: u32 = 18;

/// `ProcessCommandLineInformation`（Win8.1+）。
///
/// 与「读 PEB」相比它只要 `PROCESS_QUERY_LIMITED_INFORMATION`，**不要
/// `PROCESS_VM_READ`**——后者对受保护进程一律被拒，而前者能拿到绝大多数进程
/// 的命令行。这是 Windows 上最接近 `/proc/<pid>/cmdline` 的东西。
pub const PROCESS_COMMAND_LINE_INFORMATION: u32 = 60;
/// `ProcessBasicInformation`，用于取 PEB 地址（读 cwd / 环境变量）。
pub const PROCESS_BASIC_INFORMATION: u32 = 0;
/// `ProcessWow64Information`：非 0 表示目标是 WOW64 下的 32 位进程。
pub const PROCESS_WOW64_INFORMATION: u32 = 26;

/// `STATUS_SUCCESS`
const STATUS_SUCCESS: i32 = 0;
/// `STATUS_INFO_LENGTH_MISMATCH`
const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut core::ffi::c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;

    fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut core::ffi::c_void,
        length: u32,
        return_length: *mut u32,
    ) -> i32;
}

/// `UNICODE_STRING`。`Buffer` 指向调用方缓冲内部，随缓冲一起失效。
#[repr(C)]
#[derive(Clone, Copy)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

impl UnicodeString {
    /// 转成 `String`。`length` 是**字节**数，不是字符数。
    ///
    /// # Safety
    ///
    /// `buffer` 必须仍然有效（即承载它的缓冲还活着）。
    unsafe fn to_string_lossy(self) -> String {
        if self.buffer.is_null() || self.length == 0 {
            return String::new();
        }
        // SAFETY: 调用方保证 buffer 有效；length 是内核给出的字节数。
        let slice =
            unsafe { std::slice::from_raw_parts(self.buffer, self.length as usize / 2) };
        wide::from_wide(slice)
    }
}

/// `SYSTEM_BASIC_INFORMATION`（只用到页大小与处理器数）。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemBasicInformation {
    reserved: u32,
    timer_resolution: u32,
    page_size: u32,
    number_of_physical_pages: u32,
    lowest_physical_page_number: u32,
    highest_physical_page_number: u32,
    allocation_granularity: u32,
    minimum_user_mode_address: usize,
    maximum_user_mode_address: usize,
    active_processors_affinity_mask: usize,
    number_of_processors: i8,
}

/// `SYSTEM_TIMEOFDAY_INFORMATION`。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemTimeOfDayInformation {
    boot_time: i64,
    current_time: i64,
    time_zone_bias: i64,
    time_zone_id: u32,
    reserved: u32,
    boot_time_bias: u64,
    sleep_time_bias: u64,
}

/// `SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION`，每个逻辑处理器一项。
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SystemProcessorPerformanceInformation {
    idle_time: i64,
    /// **含** `idle_time`（这是 Windows 的定义，不是笔误）。
    kernel_time: i64,
    user_time: i64,
    /// 含在 `kernel_time` 里。
    dpc_time: i64,
    /// 含在 `kernel_time` 里。
    interrupt_time: i64,
    interrupt_count: u32,
}

/// `SYSTEM_PAGEFILE_INFORMATION`（变长链表，单位是页）。
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemPagefileInformation {
    next_entry_offset: u32,
    total_size: u32,
    total_in_use: u32,
    peak_usage: u32,
    page_file_name: UnicodeString,
}

/// `SYSTEM_THREAD_INFORMATION`。
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemThreadInformation {
    kernel_time: i64,
    user_time: i64,
    create_time: i64,
    wait_time: u32,
    start_address: *mut core::ffi::c_void,
    unique_process: HANDLE,
    unique_thread: HANDLE,
    priority: i32,
    base_priority: i32,
    context_switches: u32,
    thread_state: u32,
    wait_reason: u32,
}

/// `SYSTEM_PROCESS_INFORMATION`（Win7+ 布局）。**字段顺序不可改**，见模块文档。
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemProcessInformation {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: HANDLE,
    inherited_from_unique_process_id: HANDLE,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
    private_page_count: usize,
    read_operation_count: i64,
    write_operation_count: i64,
    other_operation_count: i64,
    read_transfer_count: i64,
    write_transfer_count: i64,
    other_transfer_count: i64,
}

// ===========================================================================
// 线程状态
// ===========================================================================

/// `KTHREAD_STATE`：只用到判断「可运行」的那几个。
mod thread_state {
    pub const READY: u32 = 1;
    pub const RUNNING: u32 = 2;
    pub const STANDBY: u32 = 3;
    pub const TERMINATED: u32 = 4;
    pub const WAITING: u32 = 5;
}

/// 一个进程里各类线程的计数，用于推导进程状态与全局运行队列长度。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadStates {
    /// `Running`：正占着某个逻辑处理器。
    pub running: u32,
    /// `Ready` + `Standby`：可运行、在排队。
    pub ready: u32,
    /// `Waiting`。
    pub waiting: u32,
    /// `Terminated`。
    pub terminated: u32,
    /// 全部线程数（含上面没分类的 Initialized / Transition 等）。
    pub total: u32,
}

impl ThreadStates {
    /// 可运行的线程数 = 正在跑 + 在就绪队列里。
    ///
    /// 这正是 Linux `/proc/stat` 的 `procs_running` 与
    /// `/proc/loadavg` 第四个字段分子的口径。
    pub const fn runnable(&self) -> u32 {
        self.running + self.ready
    }
}

// ===========================================================================
// 进程表
// ===========================================================================

/// 一个进程的快照。字段名与 Linux 侧的取数含义对齐，便于两边的 provider 共用口径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub ppid: u32,
    /// 映像名（`notepad.exe`）。系统空闲进程的名字为空，见 [`system_processes`]。
    pub name: String,
    /// 会话 id（0 = 服务所在的会话）。
    pub session_id: u32,
    /// 线程数。
    pub threads: u32,
    /// 各类线程的计数。
    pub thread_states: ThreadStates,
    /// 创建时刻，unix 秒。
    pub start_ts: i64,
    /// 用户态 + 内核态累计时间，**100 纳秒**为单位。
    pub cpu_100ns: u64,
    /// 工作集（≈ RSS），字节。
    pub working_set: u64,
    /// 虚拟大小（≈ VSZ），字节。
    pub virtual_size: u64,
    /// 私有提交量，字节。Windows 上「这个进程真正占了多少」看它比工作集准。
    pub private_bytes: u64,
    /// 基础优先级（0–31），用于映射 nice。
    pub base_priority: i32,
    /// 句柄数。
    pub handles: u32,
    /// 累计读字节数（含文件、管道、设备）。
    pub read_bytes: u64,
    /// 累计写字节数。
    pub write_bytes: u64,
}

/// 一次取全整张进程表。
///
/// # 空名字的那个进程
///
/// pid 0 是「System Idle Process」，内核不给它名字。调用方按需过滤——
/// 本函数不替调用方做取舍，只如实返回内核给的东西。
pub fn system_processes() -> io::Result<Vec<ProcessEntry>> {
    let buf = query_system(SYSTEM_PROCESS_INFORMATION)?;
    let mut out = Vec::with_capacity(512);
    let mut offset = 0usize;

    loop {
        if offset + std::mem::size_of::<SystemProcessInformation>() > buf.len() {
            break;
        }
        // SAFETY: 上面确认了剩余字节足以容纳一个 SYSTEM_PROCESS_INFORMATION。
        // read_unaligned 是必须的：buf 是 Vec<u8>，没有结构体对齐保证。
        let info = unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr().add(offset).cast::<SystemProcessInformation>(),
            )
        };

        // 线程数组紧跟在结构体后面。
        let threads_at = offset + std::mem::size_of::<SystemProcessInformation>();
        let mut states = ThreadStates {
            total: info.number_of_threads,
            ..ThreadStates::default()
        };
        let each = std::mem::size_of::<SystemThreadInformation>();
        for i in 0..info.number_of_threads as usize {
            let at = threads_at + i * each;
            if at + each > buf.len() {
                break;
            }
            // SAFETY: 刚确认过剩余字节够一个 SYSTEM_THREAD_INFORMATION。
            let t = unsafe {
                std::ptr::read_unaligned(
                    buf.as_ptr().add(at).cast::<SystemThreadInformation>(),
                )
            };
            match t.thread_state {
                thread_state::RUNNING => states.running += 1,
                thread_state::READY | thread_state::STANDBY => states.ready += 1,
                thread_state::WAITING => states.waiting += 1,
                thread_state::TERMINATED => states.terminated += 1,
                _ => {}
            }
        }

        out.push(ProcessEntry {
            pid: info.unique_process_id as usize as u32,
            ppid: info.inherited_from_unique_process_id as usize as u32,
            // SAFETY: image_name.buffer 指向 buf 内部，而 buf 在本函数内一直有效。
            name: unsafe { info.image_name.to_string_lossy() },
            session_id: info.session_id,
            threads: info.number_of_threads,
            thread_states: states,
            start_ts: filetime_to_unix(info.create_time as u64),
            cpu_100ns: (info.user_time as u64).saturating_add(info.kernel_time as u64),
            working_set: info.working_set_size as u64,
            virtual_size: info.virtual_size as u64,
            private_bytes: info.private_page_count as u64,
            base_priority: info.base_priority,
            handles: info.handle_count,
            read_bytes: info.read_transfer_count as u64,
            write_bytes: info.write_transfer_count as u64,
        });

        if info.next_entry_offset == 0 {
            break;
        }
        offset += info.next_entry_offset as usize;
    }
    Ok(out)
}

/// 全系统可运行线程数（≈ Linux 的 `procs_running`）与线程总数。
pub fn runnable_and_total_threads(procs: &[ProcessEntry]) -> (u32, u32) {
    let mut runnable = 0;
    let mut total = 0;
    for p in procs {
        runnable += p.thread_states.runnable();
        total += p.threads;
    }
    (runnable, total)
}

// ===========================================================================
// 其余系统信息
// ===========================================================================

/// 每个逻辑处理器的 CPU 时间累计值（100 纳秒）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    /// 空闲。
    pub idle: u64,
    /// 内核态，**已扣掉 idle**（原始值含 idle，这里减过了）。
    pub system: u64,
    /// 用户态。
    pub user: u64,
    /// DPC（延迟过程调用）+ 硬中断。含在原始 kernel 里，这里单列。
    pub interrupt: u64,
}

impl CpuTimes {
    /// 四态之和，即这颗核在采样点之前的总时间。
    pub const fn total(&self) -> u64 {
        self.idle + self.system + self.user
    }
}

/// 逐核 CPU 时间。
///
/// # 处理器组的限制
///
/// `SystemProcessorPerformanceInformation` 只覆盖**调用线程所在的处理器组**，
/// 即最多 64 颗逻辑处理器。超过 64 核的机器上会少报后面的组。
/// 这是该 API 的固有限制，不是本实现的疏漏；真要覆盖全部组得逐组切换线程亲和性，
/// 代价与收益不匹配（本项目的面板本来就只画每核曲线，128 核的第 97 核没人看）。
/// 少报的核**不产出样本**，而不是报 0——缺席是缺席，不是「这颗核 100% 空闲」。
pub fn processor_times() -> io::Result<Vec<CpuTimes>> {
    let buf = query_system(SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION)?;
    let each = std::mem::size_of::<SystemProcessorPerformanceInformation>();
    let count = buf.len() / each;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        // SAFETY: i < buf.len()/each，因此这一项完整落在缓冲内。
        let raw = unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr()
                    .add(i * each)
                    .cast::<SystemProcessorPerformanceInformation>(),
            )
        };
        let idle = raw.idle_time.max(0) as u64;
        let kernel = raw.kernel_time.max(0) as u64;
        out.push(CpuTimes {
            idle,
            // Windows 的 KernelTime 含 IdleTime，这里扣掉才是「真的在跑内核代码」。
            system: kernel.saturating_sub(idle),
            user: raw.user_time.max(0) as u64,
            interrupt: (raw.dpc_time.max(0) as u64)
                .saturating_add(raw.interrupt_time.max(0) as u64),
        });
    }
    Ok(out)
}

/// 开机时刻（unix 秒）。
pub fn boot_time_unix() -> io::Result<i64> {
    let buf = query_system(SYSTEM_TIMEOFDAY_INFORMATION)?;
    if buf.len() < std::mem::size_of::<SystemTimeOfDayInformation>() {
        return Err(io::Error::other("SystemTimeOfDayInformation 返回的数据过短"));
    }
    // SAFETY: 刚确认过长度。
    let info = unsafe {
        std::ptr::read_unaligned(buf.as_ptr().cast::<SystemTimeOfDayInformation>())
    };
    Ok(filetime_to_unix(info.boot_time as u64))
}

/// 内存页大小（字节）。
pub fn page_size() -> u64 {
    basic_information().map_or(4096, |b| u64::from(b.page_size).max(1))
}

fn basic_information() -> io::Result<SystemBasicInformation> {
    let buf = query_system(SYSTEM_BASIC_INFORMATION)?;
    if buf.len() < std::mem::size_of::<SystemBasicInformation>() {
        return Err(io::Error::other("SystemBasicInformation 返回的数据过短"));
    }
    // SAFETY: 刚确认过长度。
    Ok(unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<SystemBasicInformation>()) })
}

/// 全部页面文件的 `(总量, 已用)`，字节。
///
/// # 这才是 Windows 的「swap」
///
/// `GlobalMemoryStatusEx` 的 `ullTotalPageFile` 是**提交上限**（物理内存 + 页面文件），
/// 不是页面文件大小；拿它当 `mem.swap_total` 会让一台 32G 内存、4G 页面文件的机器
/// 报出 36G 的「交换空间」。`SystemPagefileInformation` 给的才是页面文件本身。
///
/// 没有配置页面文件时返回 `(0, 0)`——与 Linux 上没有 swap 分区一样，不是错误。
pub fn pagefile_totals() -> io::Result<(u64, u64)> {
    let buf = query_system(SYSTEM_PAGEFILE_INFORMATION)?;
    let page = page_size();
    let mut total = 0u64;
    let mut used = 0u64;
    let mut offset = 0usize;
    loop {
        if offset + std::mem::size_of::<SystemPagefileInformation>() > buf.len() {
            break;
        }
        // SAFETY: 刚确认过剩余字节足够一项。
        let info = unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr().add(offset).cast::<SystemPagefileInformation>(),
            )
        };
        total = total.saturating_add(u64::from(info.total_size) * page);
        used = used.saturating_add(u64::from(info.total_in_use) * page);
        if info.next_entry_offset == 0 {
            break;
        }
        offset += info.next_entry_offset as usize;
    }
    Ok((total, used))
}

// ===========================================================================
// 进程信息
// ===========================================================================

/// 一个进程的完整命令行。
///
/// # Safety
///
/// `process` 必须是带 `PROCESS_QUERY_LIMITED_INFORMATION` 的有效进程句柄。
pub unsafe fn process_command_line(process: HANDLE) -> Option<String> {
    let mut len: u32 = 0;
    // SAFETY: 缓冲为空时只回填所需长度。
    unsafe {
        NtQueryInformationProcess(
            process,
            PROCESS_COMMAND_LINE_INFORMATION,
            std::ptr::null_mut(),
            0,
            &raw mut len,
        );
    }
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    // SAFETY: buf 有 len 字节可写。
    let status = unsafe {
        NtQueryInformationProcess(
            process,
            PROCESS_COMMAND_LINE_INFORMATION,
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            len,
            &raw mut len,
        )
    };
    if status != STATUS_SUCCESS || buf.len() < std::mem::size_of::<UnicodeString>() {
        return None;
    }
    // SAFETY: 返回布局是一个 UNICODE_STRING，其 Buffer 指向同一缓冲内部。
    let us = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<UnicodeString>()) };
    // SAFETY: buf 在本作用域内有效，us.buffer 指向它内部。
    let s = unsafe { us.to_string_lossy() };
    (!s.is_empty()).then_some(s)
}

/// 原始的 `NtQueryInformationProcess`，供需要别的信息类的调用方使用。
///
/// # Safety
///
/// `process` 必须有效且带足够的访问权；`buffer` 必须有 `length` 字节可写。
pub unsafe fn query_process(
    process: HANDLE,
    class: u32,
    buffer: *mut core::ffi::c_void,
    length: u32,
) -> io::Result<u32> {
    let mut out_len: u32 = 0;
    // SAFETY: 调用方保证参数有效。
    let status =
        unsafe { NtQueryInformationProcess(process, class, buffer, length, &raw mut out_len) };
    if status != STATUS_SUCCESS {
        return Err(io::Error::other(format!(
            "NtQueryInformationProcess(class={class}) 失败: {status:#x}"
        )));
    }
    Ok(out_len)
}

// ===========================================================================
// 内部
// ===========================================================================

/// 取一项系统信息。
///
/// # 「缓冲不足就加倍」在这里是错的
///
/// 这个接口下面有两类信息类，它们对缓冲长度的要求正好相反：
///
/// | 类 | 例子 | 对长度的要求 |
/// |---|---|---|
/// | 定长 | `SystemTimeOfDayInformation`（48 字节） | **必须精确相等**。给多了同样返回 `STATUS_INFO_LENGTH_MISMATCH` |
/// | 定长数组 | `SystemProcessorPerformanceInformation` | 必须是单项大小的**整数倍**（每逻辑处理器一项） |
/// | 变长 | `SystemProcessInformation` | 至少要够，多给无妨 |
///
/// 前两类会拒绝过大的缓冲，所以「不够就翻倍」这种只会变大的策略永远撞不上
/// 它们要的那个值——初始的 64 KiB 既不等于 48，也不是 48 的整数倍。
///
/// 因此：**第一次失败按内核报的 `needed` 精确重试**，这一下就把两类定长的
/// 情形解决了。只有在「按它说的给了还是不够」时才判定是变长类在增长，
/// 这时才加 25% 余量。
///
/// 变长类的长度在两次调用之间会变（进程随时增减），所以仍然要循环；
/// 连续 8 次还不够说明系统正在剧烈变化，返回错误让调用方跳过这一轮，
/// 而不是无限打转。
fn query_system(class: u32) -> io::Result<Vec<u8>> {
    // 起手 64 KiB：变长类里最大的进程表通常一次就够，省掉一轮往返。
    // 定长类会在第一轮被拒，然后走下面的精确重试。
    let mut cap: u32 = 64 * 1024;
    let mut last_needed: u32 = 0;

    for _ in 0..8 {
        let mut buf = vec![0u8; cap as usize];
        let mut needed: u32 = 0;
        // SAFETY: buf 有 cap 字节可写，cap 如实描述其容量。
        let status = unsafe {
            NtQuerySystemInformation(
                class,
                buf.as_mut_ptr().cast::<core::ffi::c_void>(),
                cap,
                &raw mut needed,
            )
        };
        if status == STATUS_SUCCESS {
            buf.truncate(if needed == 0 { cap } else { needed } as usize);
            return Ok(buf);
        }
        if status != STATUS_INFO_LENGTH_MISMATCH {
            return Err(io::Error::other(format!(
                "NtQuerySystemInformation(class={class}) 失败: {status:#x}"
            )));
        }

        cap = if needed == 0 {
            // 内核没说要多少，只能翻倍试。
            cap.saturating_mul(2)
        } else if needed == last_needed {
            // 上一轮已经精确按它说的给过了，仍然不够 → 是变长类在增长，加余量。
            needed
                .saturating_add(needed / 4)
                .max(cap.saturating_add(4096))
        } else {
            // 精确按内核报的长度重试。定长类**必须**走这一步。
            needed
        };
        last_needed = needed;
    }
    Err(io::Error::other(format!(
        "NtQuerySystemInformation(class={class}) 连续 8 次缓冲不足"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 结构体布局抄错一个字段不会编译失败，只会让后面全是垃圾。
    /// 这条用「本进程必须在表里，且它的每个字段都合理」把布局钉住。
    #[test]
    fn 进程表自洽() {
        let procs = system_processes().expect("NtQuerySystemInformation 应当可用");
        assert!(procs.len() > 10, "只枚举到 {} 个进程", procs.len());

        let me = std::process::id();
        let mine = procs
            .iter()
            .find(|p| p.pid == me)
            .expect("本进程必须出现在进程表里");

        assert!(
            mine.name.to_lowercase().contains("strixmaid")
                || mine.name.to_lowercase().ends_with(".exe"),
            "本进程的映像名不对：{:?}",
            mine.name
        );
        assert!(mine.threads >= 1, "线程数至少是 1");
        assert_eq!(mine.threads, mine.thread_states.total);
        assert!(mine.working_set > 0, "工作集应当大于 0");
        assert!(
            mine.virtual_size >= mine.working_set,
            "虚拟大小不该小于工作集：vsz={} ws={}",
            mine.virtual_size,
            mine.working_set
        );
        assert!(mine.private_bytes > 0);
        assert!(
            mine.start_ts > 1_577_836_800,
            "启动时刻明显不对：{}",
            mine.start_ts
        );
        assert!(
            mine.start_ts <= super::super::unix_now() + 1,
            "启动时刻在未来"
        );
        assert!(mine.handles > 0);
        // 基础优先级：普通进程是 8（NORMAL_PRIORITY_CLASS）
        assert!(
            (0..=31).contains(&mine.base_priority),
            "基础优先级越界：{}",
            mine.base_priority
        );
        // 父进程必须也在表里（除非父已退出）
        assert!(mine.ppid > 0);

        eprintln!(
            "本进程：pid={} ppid={} name={} threads={} ws={} priv={} cpu={}ms",
            mine.pid,
            mine.ppid,
            mine.name,
            mine.threads,
            mine.working_set,
            mine.private_bytes,
            mine.cpu_100ns / 10_000
        );
    }

    /// pid 0（System Idle Process）没有名字——如实返回空串，由调用方决定要不要滤掉。
    #[test]
    fn 系统空闲进程的名字为空() {
        let procs = system_processes().unwrap();
        if let Some(idle) = procs.iter().find(|p| p.pid == 0) {
            assert!(idle.name.is_empty(), "pid 0 不该有名字：{:?}", idle.name);
        }
    }

    #[test]
    fn 可运行线程统计() {
        let procs = system_processes().unwrap();
        let (runnable, total) = runnable_and_total_threads(&procs);
        assert!(total > 50, "线程总数只有 {total}，明显不对");
        assert!(runnable <= total);
        // 本测试线程自己就在跑，所以至少有一个
        assert!(runnable >= 1, "至少本线程在运行");
        eprintln!("可运行线程 {runnable} / 总线程 {total}");
    }

    #[test]
    fn 逐核_cpu_时间单调且自洽() {
        let first = processor_times().expect("逐核时间应当可用");
        assert!(!first.is_empty(), "至少一颗核");
        for t in &first {
            assert!(t.total() > 0, "总时间为 0，字段大概率错位");
            // interrupt 含在原始 kernel 里，扣掉 idle 之后的 system 仍应 >= interrupt
            // （极少数刚启动的核上可能相等）
            assert!(
                t.system + 1 >= t.interrupt,
                "中断时间超过了内核时间：system={} interrupt={}",
                t.system,
                t.interrupt
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(120));
        let second = processor_times().unwrap();
        assert_eq!(first.len(), second.len(), "核数不该在 120ms 内变化");
        for (a, b) in first.iter().zip(second.iter()) {
            assert!(b.total() >= a.total(), "累计时间倒退了");
        }
        let delta: u64 = second
            .iter()
            .zip(first.iter())
            .map(|(b, a)| b.total() - a.total())
            .sum();
        assert!(delta > 0, "两次采样之间总时间没有增长");
    }

    #[test]
    fn 开机时刻合理() {
        let boot = boot_time_unix().expect("开机时刻应当可读");
        let now = super::super::unix_now();
        assert!(boot > 1_577_836_800, "开机时刻明显不对：{boot}");
        assert!(boot <= now, "开机时刻在未来：boot={boot} now={now}");
        eprintln!("开机于 {boot}，已运行 {} 秒", now - boot);
    }

    #[test]
    fn 页大小是_2_的幂() {
        let p = page_size();
        assert!(p.is_power_of_two(), "页大小 {p} 不是 2 的幂");
        assert!((4096..=2 * 1024 * 1024).contains(&p));
    }

    /// 页面文件的口径必须是「页面文件本身」，不能是提交上限。
    #[test]
    fn 页面文件总量不等于提交上限() {
        let (total, used) = pagefile_totals().expect("页面文件信息应当可读");
        assert!(used <= total, "已用 {used} 超过总量 {total}");
        // 没配页面文件是合法的（total == 0）；配了的话不该大得离谱
        if total > 0 {
            assert!(total < 1 << 44, "页面文件 {total} 字节，明显不对");
        }
        eprintln!("页面文件：{used} / {total} 字节");
    }

    #[test]
    fn 本进程的命令行可读() {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: 伪句柄永远有效，且带全部访问权。
        let cmd = unsafe { process_command_line(GetCurrentProcess()) };
        let cmd = cmd.expect("本进程的命令行必须读得到");
        assert!(!cmd.is_empty());
        eprintln!("本进程命令行：{cmd}");
    }
}
