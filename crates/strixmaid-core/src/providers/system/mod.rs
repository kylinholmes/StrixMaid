//! system provider（id `"host"`）：主机信息 / 健康聚合 / 时间 / 主机名·时区·电源操作。
//!
//! 全部数据直读 `/proc`、`/sys`、`/etc`（`docs/design.md` §1），不依赖 hostnamectl /
//! timedatectl / udisks2 / systemd-detect-virt。唯一的外部命令是电源操作用的 `systemctl`。
//!
//! # 结构
//!
//! | 子模块 | 内容 |
//! |---|---|
//! | [`os_release`] | `/etc/os-release`、`/etc/machine-info` |
//! | [`virt`] | 虚拟化 / 容器识别 |
//! | [`hardware`] | DMI / 设备树 |
//! | [`cpu`] | `/proc/cpuinfo`、NUMA、cgroup 配额 |
//! | [`storage`] | `/sys/block`、`/proc/self/mounts` + `statvfs` |
//! | [`health`] | 需重启、容量 / inode 阈值、负载 |
//! | [`time`] | 时区、`adjtimex`、NTP 服务 |
//! | [`actions`] | 三个写操作 |
//!
//! 采集函数（`collect_*`）全是同步、永不失败的纯 I/O；[`HostProvider`] 的 async 方法把它们
//! 丢进 `spawn_blocking`——`statvfs` 碰上挂死的网络挂载会阻塞，不能占用运行时线程。

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

use async_trait::async_trait;
use strixmaid_types::system::{
    HealthReport, MemoryInfo, OsInfo, PowerAction, SetHostnameReq, SystemInfo, TimeInfo,
};
use strixmaid_types::{ApiError, ApiResult};

use super::{Probe, Provider};
use util::{meminfo_value, read_trimmed, uname_machine, unix_now};

pub use health::RebootReason;

/// 主机信息 provider。无状态，可随意 `Clone` / `Copy`。
#[derive(Debug, Clone, Copy, Default)]
pub struct HostProvider;

impl HostProvider {
    /// 创建 provider。
    pub fn new() -> Self {
        Self
    }

    /// `GET /system/info`。
    pub async fn system_info(&self) -> ApiResult<SystemInfo> {
        blocking(collect_system_info).await
    }

    /// `GET /system/health`。
    pub async fn health(&self) -> ApiResult<HealthReport> {
        blocking(collect_health).await
    }

    /// `GET /system/time`。
    pub async fn time(&self) -> ApiResult<TimeInfo> {
        blocking(time::read_time_info).await
    }

    /// `PUT /system/hostname`。非 root 返回 `PermissionDenied`。
    pub async fn set_hostname(&self, req: SetHostnameReq) -> ApiResult<()> {
        blocking(move || actions::set_hostname(&req)).await?
    }

    /// `PUT /system/timezone`。非 root 返回 `PermissionDenied`。
    pub async fn set_timezone(&self, timezone: String) -> ApiResult<()> {
        blocking(move || actions::set_timezone(&timezone)).await?
    }

    /// `POST /system/power`。
    pub async fn power(&self, action: PowerAction) -> ApiResult<()> {
        actions::power(action).await
    }
}

#[async_trait]
impl Provider for HostProvider {
    fn id(&self) -> &'static str {
        "host"
    }

    /// `/proc` 可读即可用；DMI、`/sys/block` 等缺失只影响个别字段，不算不可用。
    async fn probe(&self) -> Probe {
        match fs::read_to_string("/proc/sys/kernel/osrelease") {
            Ok(_) => Probe::Available,
            Err(e) => Probe::unavailable(format!("无法读取 /proc/sys/kernel/osrelease：{e}")),
        }
    }
}

/// 在阻塞线程池里跑一段同步采集。
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal("采集任务异常终止").with_detail(e.to_string()))
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
    let inputs = health::HealthInputs {
        ts: unix_now(),
        filesystems: storage::read_filesystems(),
        load1: health::read_load1(),
        logical_cores: cpu::count_stat_cpus(&stat).max(1),
        reboot: health::detect_reboot_required(),
    };
    health::build_report(&inputs)
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
        // 串行化必须成功（ToSchema/Serialize 都在 types 里保证，这里只是冒烟）
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
        eprintln!("本机 HealthReport:\n{}", serde_json::to_string_pretty(&r).unwrap());
    }

    #[tokio::test]
    async fn provider_探测与_async_接口() {
        let p = HostProvider::new();
        assert_eq!(p.id(), "host");
        assert_eq!(p.probe().await, Probe::Available);
        let info = p.system_info().await.unwrap();
        assert!(!info.hostname.is_empty());
        let _ = p.health().await.unwrap();
        let t = p.time().await.unwrap();
        assert!(!t.timezone.is_empty());
        eprintln!("本机 TimeInfo: {}", serde_json::to_string(&t).unwrap());
    }
}
