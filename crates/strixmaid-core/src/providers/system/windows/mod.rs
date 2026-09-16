//! Windows 主机信息采集：注册表 + Win32 API，读路径上不起子进程。
//!
//! # 数据源一览
//!
//! | 字段 | 来源 |
//! |---|---|
//! | 系统版本 / 版本号 | `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion`（[`os_version`]） |
//! | 主机名 | `GetComputerNameExW`（[`actions::computer_name`]） |
//! | `machine_id` | `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid` |
//! | CPU 型号 / 拓扑 | 注册表 `CentralProcessor\0` + `GetLogicalProcessorInformationEx`（[`cpu`]） |
//! | 内存 | `GlobalMemoryStatusEx` + `NtQuerySystemInformation`（页文件） |
//! | 磁盘 / 文件系统 | 卷与物理盘枚举（[`storage`]，底层是 [`crate::platform::windows::volume`]） |
//! | GPU | `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-…}`（[`gpu`]） |
//! | 网卡 | `GetAdaptersAddresses` + `GetIfTable2`（[`net`]） |
//! | 机型 / BIOS | `HKLM\HARDWARE\DESCRIPTION\System\BIOS`（[`hardware`]） |
//! | 开机时刻 | `NtQuerySystemInformation(SystemTimeOfDayInformation)` |
//!
//! 写操作（改主机名 / 时区 / 电源）在 [`actions`]，那些是真的要动系统状态的，
//! 需要相应特权。
//!
//! # 拿不到的字段
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `cpu.quota_cores` | Windows 没有 cgroup 式的 CPU 配额。作业对象（Job Object）能限速，但那是**按进程**的，不是整机口径 |
//! | `cpu.numa_nodes` | 能读到，见 [`cpu::Topology`]；这里如实填 |
//! | `ntp_*`（`TimeInfo`） | 见 [`time`]：同步状态在注册表里，同步**服务器**要问 W32Time 服务 |
//!
//! 健康报告里跳过的项见 [`collect_health`]。一律 `None` / 空列表，不编数据。

pub mod actions;
pub mod cpu;
pub mod gpu;
pub mod hardware;
pub mod health;
pub mod net;
pub mod os_version;
pub mod storage;
pub mod time;

use strixmaid_types::system::{HealthReport, MemoryInfo, SystemInfo, TimeInfo};

use super::super::Probe;
use super::health::{HealthInputs, build_report};
use crate::platform::windows::{HKLM, ntdll, reg_string};

pub use actions::{power, set_hostname, set_timezone};

/// 机器唯一标识所在的注册表位置。
///
/// 这是 Windows 上与 `/etc/machine-id` 语义最接近的东西：安装时生成、
/// 此后随系统一生不变（重装会换），且不需要任何特权就能读。
const MACHINE_GUID_KEY: &str = r"SOFTWARE\Microsoft\Cryptography";
/// 见 [`MACHINE_GUID_KEY`]。
const MACHINE_GUID_VALUE: &str = "MachineGuid";

/// 能读到系统版本即可用。
///
/// 选 `CurrentVersion` 而不是别的：它是**任何** Windows NT 上都存在、
/// 任何用户都可读的一处注册表项。读不到只可能是进程被沙箱或组策略卡到
/// 连 HKLM 都打不开，那时后面每一项采集都会同样失败。
pub fn probe() -> Probe {
    match os_version::read().build {
        Some(_) => Probe::Available,
        None => Probe::unavailable("读不到 HKLM\\SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"),
    }
}

/// 同步采集完整的 [`SystemInfo`]。任何一项读不到都退化成 `None` / 兜底值。
pub fn collect_system_info() -> SystemInfo {
    let ts = time::unix_now();
    let boot_ts = ntdll::boot_time_unix().unwrap_or(0);
    let version = os_version::read();

    SystemInfo {
        ts,
        hostname: actions::computer_name().unwrap_or_else(|| "localhost".to_owned()),
        // 「这台电脑」的显示名。Windows 上它与 NetBIOS 名是同一个值的两种形式，
        // 没有 Linux 那种「pretty hostname 与 hostname 分开」的概念，故不重复报。
        pretty_hostname: None,
        machine_id: reg_string(HKLM, MACHINE_GUID_KEY, MACHINE_GUID_VALUE),
        os: version.to_os_info(),
        // Windows 的内核与系统同版本发布，见 `OsVersion::kernel`。
        kernel: version.kernel(),
        arch: std::env::consts::ARCH.to_owned(),
        virtualization: hardware::detect_virtualization(),
        hardware: hardware::read_hardware(),
        cpu: cpu::read_cpu_info(),
        memory: read_memory_info(),
        disks: storage::read_disks(),
        filesystems: storage::read_filesystems(),
        uptime_secs: (ts - boot_ts).max(0) as u64,
        boot_ts,
        gpus: gpu::read_gpus(),
        networks: net::read_networks(),
    }
}

/// 同步生成健康报告。
pub fn collect_health() -> HealthReport {
    let topology = cpu::query_logical_processor_info()
        .map(|buf| cpu::parse_logical_processor_info(&buf))
        .unwrap_or_default();
    let inputs = HealthInputs {
        ts: time::unix_now(),
        filesystems: storage::read_filesystems(),
        load1: health::read_load1(),
        logical_cores: (topology.logical.len() as u32).max(1),
        reboot: health::detect_reboot_required(),
        // `systemd` / `launchd`：Windows 的对应物是 SCM，失败服务归 service
        // provider 报，这里重复检查只会让两处口径打架。
        // `smart`：读 SMART 要 `IOCTL_STORAGE_PREDICT_FAILURE`，需要管理员，
        // 而健康报告在非特权下也必须能出，故整项不做。
        skipped: vec!["scm".into(), "smart".into()],
    };
    build_report(&inputs)
}

/// 读一次时间信息。
pub fn collect_time_info() -> TimeInfo {
    time::read_time_info()
}

/// 内存总量 / 可用量 / 页文件。
///
/// # `available_bytes` 取 `ullAvailPhys`
///
/// API 契约里这一项是 Linux 的 `MemAvailable`：「还能给新进程用多少」。
/// Windows 的 `ullAvailPhys` 是「空闲 + 待命（standby）」，语义正好对上
/// ——待命页装的是可丢弃的缓存，要用随时能回收，与 `MemAvailable` 把
/// 可回收的 page cache 算进去是同一个口径。
///
/// # swap 为什么不用 `ullTotalPageFile`
///
/// 那**不是**页文件大小，而是「提交限额」（物理内存 + 页文件）。直接拿它当
/// swap 会让一台 64 GiB 内存、无页文件的机器显示 64 GiB swap。真正的页文件
/// 大小要问 `NtQuerySystemInformation`，见 [`ntdll::pagefile_totals`]。
fn read_memory_info() -> MemoryInfo {
    use windows_sys::Win32::System::SystemInformation::{
        GlobalMemoryStatusEx, MEMORYSTATUSEX,
    };

    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: dwLength 已按文档填成结构体自身大小，其余字段是输出参数。
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    let (total, available) = if ok == 0 {
        (0, 0)
    } else {
        (status.ullTotalPhys, status.ullAvailPhys)
    };

    // 页文件读不到时报 0 而不是猜：0 与「确实没配页文件」是同一个显示，
    // 而那正是现代 SSD 机器上常见的真实配置。
    let (page_total, page_used) = ntdll::pagefile_totals().unwrap_or((0, 0));

    MemoryInfo {
        total_bytes: total,
        available_bytes: available,
        swap_total_bytes: page_total,
        swap_free_bytes: page_total.saturating_sub(page_used),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机冒烟：这些字段在任何 Windows 上都该有值。
    #[test]
    fn 采集出的主机信息有基本字段() {
        let info = collect_system_info();
        assert!(!info.hostname.is_empty(), "主机名不该为空");
        assert!(info.memory.total_bytes > 0, "物理内存总量不该为 0");
        assert_eq!(info.os.id, "windows");
        assert!(info.cpu.logical_cores >= 1);
        // 开机时刻必须早于现在；等于 0 说明 NtQuerySystemInformation 失败了。
        assert!(info.boot_ts > 0 && info.boot_ts <= info.ts);
    }

    /// `machine_id` 是 `HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid`，
    /// 任何 Windows 安装都有；形状是 GUID。
    #[test]
    fn 机器标识是一个_guid() {
        let Some(id) = reg_string(HKLM, MACHINE_GUID_KEY, MACHINE_GUID_VALUE) else {
            panic!("读不到 MachineGuid");
        };
        assert_eq!(id.len(), 36, "GUID 应为 36 字符：{id}");
        assert_eq!(id.matches('-').count(), 4, "GUID 应有 4 个连字符：{id}");
    }

    #[test]
    fn 内存可用量不超过总量() {
        let m = read_memory_info();
        assert!(m.total_bytes > 0);
        assert!(m.available_bytes <= m.total_bytes);
        assert!(m.swap_free_bytes <= m.swap_total_bytes);
    }

    #[test]
    fn 探测为可用() {
        assert_eq!(probe(), Probe::Available);
    }

    #[test]
    fn 健康报告能生成() {
        let r = collect_health();
        assert!(r.skipped.contains(&"smart".to_owned()));
    }
}
