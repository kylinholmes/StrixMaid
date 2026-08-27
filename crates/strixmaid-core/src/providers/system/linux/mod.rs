//! Linux 主机信息采集：直读 `/proc`、`/sys`、`/etc`。
//!
//! 全部数据都不经过 hostnamectl / timedatectl / udisks2 / systemd-detect-virt
//! （`docs/design.md` §1），唯一的外部命令是电源操作用的 `systemctl`。
//!
//! | 子模块 | 内容 |
//! |---|---|
//! | [`os_release`] | `/etc/os-release`、`/etc/machine-info` |
//! | [`virt`] | 虚拟化 / 容器识别 |
//! | [`hardware`] | DMI / 设备树 |
//! | [`cpu`] | `/proc/cpuinfo`、NUMA、cgroup 配额 |
//! | [`storage`] | `/sys/block`、`/proc/self/mounts` + `statvfs` |
//! | [`time`] | 时区、`adjtimex`、NTP 服务 |
//! | [`actions`] | 三个写操作 |
//!
//! 八个子模块的内容与平台化改造之前逐字节相同——它们原本直接位于
//! `providers/system/` 下，为给 macOS 让出并列位置才整体移入本目录，
//! 彼此之间的 `super::util` 之类的引用因此仍然成立。
//!
//! `collect_system_info` / `collect_health` / `probe` 三个函数从
//! `providers/system/mod.rs` 挪了进来：它们是「怎么在 Linux 上取数」，
//! 而留在上一层的 [`HostProvider`](super::HostProvider) 是「对外的接口形状」。

pub mod actions;
pub mod cpu;
pub mod hardware;
pub mod health;
pub mod os_release;
pub mod storage;
pub mod time;
pub(crate) mod util;
pub mod virt;

use std::fs;

use strixmaid_types::system::{MemoryInfo, OsInfo, SystemInfo, TimeInfo};

use super::super::Probe;
use super::health::{HealthInputs, build_report};
use strixmaid_types::system::HealthReport;
use util::{meminfo_value, read_trimmed, uname_machine, unix_now};

pub use actions::{power, set_hostname, set_timezone};
pub use time::read_time_info as collect_time;

/// `/proc` 可读即可用；DMI、`/sys/block` 等缺失只影响个别字段，不算不可用。
pub fn probe() -> Probe {
    match fs::read_to_string("/proc/sys/kernel/osrelease") {
        Ok(_) => Probe::Available,
        Err(e) => Probe::unavailable(format!("无法读取 /proc/sys/kernel/osrelease：{e}")),
    }
}

/// 同步采集完整的 [`SystemInfo`]。任何一项读不到都退化成 `None` / 兜底值。
pub fn collect_system_info() -> SystemInfo {
    let ts = unix_now();
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let uptime_secs = read_uptime_secs().unwrap_or(0);

    SystemInfo {
        ts,
        hostname: read_trimmed("/proc/sys/kernel/hostname")
            .or_else(|| read_trimmed("/etc/hostname"))
            .unwrap_or_else(|| "localhost".to_owned()),
        pretty_hostname: os_release::read_pretty_hostname(),
        machine_id: read_trimmed("/etc/machine-id")
            .or_else(|| read_trimmed("/var/lib/dbus/machine-id")),
        os: os_release::read_os_release().unwrap_or_else(fallback_os),
        kernel: read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".to_owned()),
        arch: uname_machine().unwrap_or_else(|| std::env::consts::ARCH.to_owned()),
        virtualization: virt::detect_virtualization(),
        hardware: hardware::read_hardware(),
        cpu: cpu::read_cpu_info(),
        memory: MemoryInfo {
            total_bytes: meminfo_value(&meminfo, "MemTotal").unwrap_or(0),
            // 老内核（< 3.14）没有 MemAvailable，退回 MemFree
            available_bytes: meminfo_value(&meminfo, "MemAvailable")
                .or_else(|| meminfo_value(&meminfo, "MemFree"))
                .unwrap_or(0),
            swap_total_bytes: meminfo_value(&meminfo, "SwapTotal").unwrap_or(0),
            swap_free_bytes: meminfo_value(&meminfo, "SwapFree").unwrap_or(0),
        },
        disks: storage::read_disks(),
        filesystems: storage::read_filesystems(),
        uptime_secs,
        boot_ts: read_btime().unwrap_or(ts - uptime_secs as i64),
    }
}

/// 同步生成健康报告。
pub fn collect_health() -> HealthReport {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let inputs = HealthInputs {
        ts: unix_now(),
        filesystems: storage::read_filesystems(),
        load1: health::read_load1(),
        logical_cores: cpu::count_stat_cpus(&stat).max(1),
        reboot: health::detect_reboot_required(),
        // 未检查：failed units 由 service provider 提供（能力名 systemd），
        // SMART 需要 root + 直读设备，P0 不做。
        skipped: vec!["systemd".into(), "smart".into()],
    };
    build_report(&inputs)
}

/// 读一次时间信息。
pub fn collect_time_info() -> TimeInfo {
    time::read_time_info()
}

/// 极简容器镜像里可能没有 os-release；[`SystemInfo::os`] 是必填的，只能给个诚实的兜底。
fn fallback_os() -> OsInfo {
    OsInfo {
        id: "linux".to_owned(),
        name: "Linux".to_owned(),
        version: None,
        pretty_name: "Linux（无 os-release）".to_owned(),
    }
}

/// `/proc/uptime` 第一个字段。
fn read_uptime_secs() -> Option<u64> {
    let raw = read_trimmed("/proc/uptime")?;
    let first: f64 = raw.split_whitespace().next()?.parse().ok()?;
    Some(first as u64)
}

/// `/proc/stat` 的 `btime`（开机时刻，unix 秒）。
fn read_btime() -> Option<i64> {
    let raw = fs::read_to_string("/proc/stat").ok()?;
    raw.lines()
        .find_map(|l| l.strip_prefix("btime "))
        .and_then(|v| v.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 本机完整采集() {
        let info = collect_system_info();
        assert!(!info.hostname.is_empty());
        assert!(!info.kernel.is_empty() && info.kernel != "unknown");
        assert!(!info.arch.is_empty());
        assert!(!info.os.id.is_empty());
        assert!(info.cpu.logical_cores >= 1);
        assert!(info.memory.total_bytes > 0);
        assert!(info.memory.available_bytes <= info.memory.total_bytes);
        assert!(info.uptime_secs > 0);
        assert!(info.boot_ts > 0 && info.boot_ts <= info.ts);
        assert!(info.filesystems.iter().any(|f| f.mount_point == "/"));
        let json = serde_json::to_string_pretty(&info).unwrap();
        assert!(json.contains("\"hostname\""));
        eprintln!("本机 SystemInfo:\n{json}");
    }

    #[test]
    fn 本机健康报告() {
        let r = collect_health();
        assert!(r.ts > 0);
        assert_eq!(r.skipped, vec!["systemd", "smart"]);
        for item in &r.items {
            assert!(!item.id.is_empty());
            assert!(!item.title.is_empty());
        }
        eprintln!(
            "本机 HealthReport:\n{}",
            serde_json::to_string_pretty(&r).unwrap()
        );
    }
}
