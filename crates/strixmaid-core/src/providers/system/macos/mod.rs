//! macOS 主机信息采集：`sysctl` + `SystemVersion.plist` + `getfsstat`。
//!
//! # 不调外部命令
//!
//! 主机名、机器 UUID、系统版本、CPU 型号、开机时刻全部来自 `sysctl`，
//! 系统版本名从 `/System/Library/CoreServices/SystemVersion.plist` 直读，
//! 挂载表来自 `getfsstat(2)`——**读路径上一个子进程都不起**，
//! 与 Linux 侧「直读 `/proc`、不调 hostnamectl」的原则一致
//! （`design.md` §1）。写操作（改主机名 / 时区 / 电源）绕不开系统工具，见 [`actions`]。
//!
//! # 拿不到的字段
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `disks` | 枚举物理盘要走 IOKit（`IOServiceMatching("IOMedia")`），是一整套 CoreFoundation 交互 |
//! | `hardware.serial` / `bios_version` | 序列号同样在 IOKit 的 `IOPlatformSerialNumber` 里；Mac 没有「BIOS 版本」这个概念 |
//! | `cpu.numa_nodes` | macOS 不暴露 NUMA 拓扑（Apple Silicon 上也不存在多 NUMA 节点） |
//! | `cpu.quota_cores` | 没有 cgroup 配额 |
//! | `ntp_*` | 判断是否启用要 `systemsetup -getusingnetworktime`，那需要 root |
//!
//! 全部填 `None` / 空列表，并在健康报告的 `skipped` 里如实标出，不编数据。

pub mod actions;
pub mod health;
pub mod os_version;
pub mod storage;
pub mod time;

use strixmaid_types::system::{
    CpuInfo, HardwareInfo, HealthReport, MemoryInfo, SystemInfo, TimeInfo,
};

use super::super::Probe;
use super::health::{HealthInputs, build_report};
use crate::platform::macos::{sysctl_scalar, sysctl_str};

pub use actions::{power, set_hostname, set_timezone};

/// `sysctl` 可用即可用。`hw.memsize` 在任何 macOS 上都存在，取不到说明
/// 进程被沙箱限制到连 sysctl 都调不了，此时后面的采集全都没意义。
pub fn probe() -> Probe {
    match sysctl_scalar::<u64>("hw.memsize") {
        Some(_) => Probe::Available,
        None => Probe::unavailable("sysctl hw.memsize 不可读"),
    }
}

/// 同步采集完整的 [`SystemInfo`]。任何一项读不到都退化成 `None` / 兜底值。
pub fn collect_system_info() -> SystemInfo {
    let ts = time::unix_now();
    let boot_ts = boot_time().unwrap_or(0);
    let version = os_version::read();

    SystemInfo {
        ts,
        hostname: sysctl_str("kern.hostname").unwrap_or_else(|| "localhost".to_owned()),
        // ComputerName（「XX 的 MacBook Air」那个）存在 SystemConfiguration 的动态存储里，
        // 只能经 SCDynamicStore 或 `scutil` 拿到；读路径不起子进程，故留空。
        pretty_hostname: None,
        // 硬件 UUID。语义上对应 Linux 的 /etc/machine-id：一台机器一个稳定标识。
        machine_id: sysctl_str("kern.uuid"),
        os: version.to_os_info(),
        // Darwin 内核版本，如 25.5.0。与 Linux 侧 /proc/sys/kernel/osrelease 同义。
        kernel: sysctl_str("kern.osrelease").unwrap_or_else(|| "unknown".to_owned()),
        arch: sysctl_str("hw.machine").unwrap_or_else(|| std::env::consts::ARCH.to_owned()),
        virtualization: detect_virtualization(),
        hardware: read_hardware(),
        cpu: read_cpu_info(),
        memory: read_memory_info(),
        // 见模块文档「拿不到的字段」
        disks: Vec::new(),
        filesystems: storage::read_filesystems(),
        uptime_secs: (ts - boot_ts).max(0) as u64,
        boot_ts,
        // GPU / 网卡拓扑走 IOKit / getifaddrs，P0 不做（macOS 只是开发平台，
        // 面板的资源组在 Linux 上验证即可）。如实留空，不是缺陷。
        gpus: Vec::new(),
        networks: Vec::new(),
    }
}

/// 同步生成健康报告。
pub fn collect_health() -> HealthReport {
    let inputs = HealthInputs {
        ts: time::unix_now(),
        filesystems: storage::read_filesystems(),
        load1: health::read_load1(),
        logical_cores: sysctl_scalar::<i32>("hw.logicalcpu").unwrap_or(1).max(1) as u32,
        // macOS 没有「需要重启」的标记文件；softwareupdate 的待装更新要起子进程去问，
        // 且那是「有更新可装」而非「已装完待重启」，语义对不上，故不检测。
        reboot: None,
        skipped: vec!["reboot".into(), "launchd".into(), "smart".into()],
    };
    build_report(&inputs)
}

/// 读一次时间信息。
pub fn collect_time_info() -> TimeInfo {
    time::read_time_info()
}

/// 开机时刻（unix 秒）。`kern.boottime` 是一个 `struct timeval`。
fn boot_time() -> Option<i64> {
    let tv = sysctl_scalar::<libc::timeval>("kern.boottime")?;
    Some(tv.tv_sec)
}

/// 虚拟化识别。
///
/// `kern.hv_vmm_present` 为 1 表示**本机跑在某个 hypervisor 之下**（Apple 从 macOS 11
/// 起提供这个 sysctl）。它只说「在虚拟机里」，不说是哪一家，因此再用 `hw.model`
/// 认一下常见的几个：VMware / Parallels / VirtualBox 会把自己写进机型串，
/// Apple 自家的 Virtualization.framework 客户机则是 `VirtualMac2,1` 这种。
fn detect_virtualization() -> Option<String> {
    let model = sysctl_str("hw.model").unwrap_or_default();
    let lower = model.to_ascii_lowercase();
    for (needle, name) in [
        ("vmware", "vmware"),
        ("parallels", "parallels"),
        ("virtualbox", "oracle"),
        ("virtualmac", "apple-virtualization"),
        ("vmapple", "apple-virtualization"),
    ] {
        if lower.contains(needle) {
            return Some(name.to_owned());
        }
    }
    // 认不出具体产品，但内核确认在 hypervisor 之下
    (sysctl_scalar::<i32>("kern.hv_vmm_present") == Some(1)).then(|| "vmm".to_owned())
}

/// 机型信息。Mac 没有 DMI，能给的只有厂商与机型标识。
fn read_hardware() -> Option<HardwareInfo> {
    let product = sysctl_str("hw.model");
    product.as_ref()?;
    Some(HardwareInfo {
        vendor: Some("Apple Inc.".to_owned()),
        product,
        // 见模块文档「拿不到的字段」
        bios_version: None,
        serial: None,
    })
}

/// CPU 信息。
fn read_cpu_info() -> CpuInfo {
    // Apple Silicon 上 machdep.cpu.brand_string 是 "Apple M3 Pro"；
    // Intel Mac 上是完整的 Intel 型号串。
    let model = sysctl_str("machdep.cpu.brand_string")
        .or_else(|| sysctl_str("hw.model"))
        .unwrap_or_else(|| "unknown".to_owned());
    // machdep.cpu.vendor 只在 Intel Mac 上存在
    let vendor = sysctl_str("machdep.cpu.vendor")
        .or_else(|| model.starts_with("Apple ").then(|| "Apple".to_owned()));
    // hw.cpufrequency 在 Apple Silicon 上恒为 0（大小核频率不同，没有单一值）
    let mhz = sysctl_scalar::<u64>("hw.cpufrequency")
        .filter(|hz| *hz > 0)
        .map(|hz| hz as f64 / 1e6);

    CpuInfo {
        model,
        vendor,
        logical_cores: sysctl_scalar::<i32>("hw.logicalcpu").unwrap_or(1).max(1) as u32,
        physical_cores: sysctl_scalar::<i32>("hw.physicalcpu")
            .filter(|n| *n > 0)
            .map(|n| n as u32),
        // 见模块文档「拿不到的字段」
        numa_nodes: None,
        mhz,
        quota_cores: None,
        // Apple Silicon 的封装拓扑要走 IOKit（大小核簇），P0 退化为单封装含全部核。
        packages: vec![strixmaid_types::system::CpuPackage {
            id: 0,
            logical_cores: (0..sysctl_scalar::<i32>("hw.logicalcpu").unwrap_or(1).max(1) as u32)
                .collect(),
        }],
    }
}

/// 内存信息。`available` 的估算口径与指标采集器一致，见
/// [`crate::metrics::collect::macos::mem`]。
fn read_memory_info() -> MemoryInfo {
    use crate::metrics::collect::macos::mem::{read_swap, read_vm_pages};
    use crate::platform::macos::page_size;

    let total = sysctl_scalar::<u64>("hw.memsize").unwrap_or(0);
    let swap = read_swap();
    let available = read_vm_pages()
        .map(|p| p.to_stat(total, page_size(), swap).available)
        // 读不到页计数时给 0 而不是 total：宁可显示「没有可用内存」这种
        // 一眼看出不对的值，也不要显示「内存全空」这种看起来正常的假象。
        .unwrap_or(0);

    MemoryInfo {
        total_bytes: total,
        available_bytes: available,
        swap_total_bytes: swap.total,
        swap_free_bytes: swap.free,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 本机完整采集() {
        let info = collect_system_info();
        assert!(!info.hostname.is_empty());
        assert!(!info.kernel.is_empty() && info.kernel != "unknown");
        assert!(
            info.arch == "arm64" || info.arch == "x86_64",
            "实际 {}",
            info.arch
        );
        assert_eq!(info.os.id, "macos");
        assert!(!info.os.version.as_deref().unwrap_or_default().is_empty());
        assert!(info.cpu.logical_cores >= 1);
        assert!(info.cpu.physical_cores.is_some_and(|n| n >= 1));
        assert!(info.memory.total_bytes > 0);
        assert!(info.memory.available_bytes <= info.memory.total_bytes);
        assert!(info.uptime_secs > 0);
        assert!(info.boot_ts > 0 && info.boot_ts <= info.ts);
        assert!(info.filesystems.iter().any(|f| f.mount_point == "/"));
        assert!(info.machine_id.is_some(), "kern.uuid 应可读");
        // 见模块文档：这些在 macOS 上不采
        assert!(info.disks.is_empty());
        assert_eq!(info.cpu.numa_nodes, None);

        let json = serde_json::to_string_pretty(&info).unwrap();
        assert!(json.contains("\"hostname\""));
        eprintln!("本机 SystemInfo:\n{json}");
    }

    #[test]
    fn 本机健康报告() {
        let r = collect_health();
        assert!(r.ts > 0);
        assert_eq!(r.skipped, vec!["reboot", "launchd", "smart"]);
        assert!(
            !r.skipped.contains(&"systemd".to_string()),
            "macOS 上报 systemd 未检查只会误导前端"
        );
        for item in &r.items {
            assert!(!item.id.is_empty());
            assert!(!item.title.is_empty());
        }
        eprintln!(
            "本机 HealthReport:\n{}",
            serde_json::to_string_pretty(&r).unwrap()
        );
    }

    #[test]
    fn 虚拟化识别不误报() {
        // 本机可能是也可能不是虚拟机，只断言「有值时是已知取值」。
        if let Some(v) = detect_virtualization() {
            assert!(
                [
                    "vmware",
                    "parallels",
                    "oracle",
                    "apple-virtualization",
                    "vmm"
                ]
                .contains(&v.as_str()),
                "未知的虚拟化标识：{v}"
            );
        }
    }
}
