//! 系统信息、健康聚合、时间与电源操作（`docs/design.md` §9.1「系统」组）。
//!
//! 数据全部来自 `/proc`、`/sys`、`/etc/os-release`、DMI，不依赖 udisks2 / NetworkManager
//! （`docs/design.md` §1 原则 1）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================== /api/v1/system/info ==============================

/// `GET /api/v1/system/info` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SystemInfo {
    /// 采样时刻。
    pub ts: i64,
    /// 静态主机名（`/etc/hostname` / `uname -n`）。
    #[schema(example = "web-01")]
    pub hostname: String,
    /// 可读的「漂亮主机名」（`/etc/machine-info` 的 `PRETTY_HOSTNAME`）。多数机器没有，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "生产 Web 节点 1")]
    pub pretty_hostname: Option<String>,
    /// 机器唯一 id（`/etc/machine-id`）。读不到时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// 发行版信息（`/etc/os-release`）。
    pub os: OsInfo,
    /// 内核版本（`uname -r`）。
    #[schema(example = "6.8.0-71-generic")]
    pub kernel: String,
    /// CPU 架构（`uname -m`）。
    #[schema(example = "x86_64")]
    pub arch: String,
    /// 虚拟化类型：`"kvm"` / `"vmware"` / `"docker"` / `"lxc"` / `"wsl"` 等，
    /// 与 `systemd-detect-virt` 的取值一致。**物理机为 `None`**（而不是字符串 `"none"`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "kvm")]
    pub virtualization: Option<String>,
    /// 硬件信息（DMI）。容器内通常读不到，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware: Option<HardwareInfo>,
    /// CPU 信息。
    pub cpu: CpuInfo,
    /// 内存与 swap 容量。
    pub memory: MemoryInfo,
    /// **块设备**列表（`/sys/block`）。不含分区，不含虚拟设备（loop / ram）。
    #[serde(default)]
    pub disks: Vec<DiskInfo>,
    /// **已挂载文件系统**的容量占用（`/proc/self/mountinfo` + `statvfs`）。
    /// 与 `disks` 是两件事：一块盘可以挂多个文件系统，也可以一个都不挂。
    #[serde(default)]
    pub filesystems: Vec<FilesystemInfo>,
    /// 开机时长，秒。
    #[schema(example = 864_000_u64)]
    pub uptime_secs: u64,
    /// 开机时刻（`ts - uptime_secs`）。
    pub boot_ts: i64,
}

/// 发行版信息，来自 `/etc/os-release`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OsInfo {
    /// `ID`，如 `"ubuntu"` / `"debian"` / `"fedora"`。用于安装期选 PAM 模板等分支判断。
    #[schema(example = "ubuntu")]
    pub id: String,
    /// `NAME`。
    #[schema(example = "Ubuntu")]
    pub name: String,
    /// `VERSION_ID`。滚动发行版（Arch 等）没有此字段，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "24.04")]
    pub version: Option<String>,
    /// `PRETTY_NAME`，直接用于展示。
    #[schema(example = "Ubuntu 24.04.2 LTS")]
    pub pretty_name: String,
}

/// DMI 硬件信息（`/sys/class/dmi/id/`）。字段读不到时为 `None`——容器与多数 ARM 板子都读不到。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct HardwareInfo {
    /// `sys_vendor`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Dell Inc.")]
    pub vendor: Option<String>,
    /// `product_name`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "PowerEdge R650")]
    pub product: Option<String>,
    /// BIOS / 固件版本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bios_version: Option<String>,
    /// 机器序列号。**读取需要 root**，非特权进程通常拿不到，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

/// CPU 信息（`/proc/cpuinfo` + `/sys/devices/system/cpu`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct CpuInfo {
    /// 型号名。异构机器（big.LITTLE）取第一个核的。
    #[schema(example = "AMD EPYC 7543 32-Core Processor")]
    pub model: String,
    /// 厂商标识（`vendor_id`）。ARM 上常缺失，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "AuthenticAMD")]
    pub vendor: Option<String>,
    /// **逻辑**核数（含超线程），即 `/proc/stat` 里 `cpuN` 的条数。
    #[schema(example = 16)]
    pub logical_cores: u32,
    /// **物理**核数。无法可靠推断时为 `None`（不要用 `logical_cores` 顶替）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 8)]
    pub physical_cores: Option<u32>,
    /// NUMA 节点数。读不到为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numa_nodes: Option<u32>,
    /// 当前主频，MHz。会随调频变化，仅供展示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 2794.7)]
    pub mhz: Option<f64>,
    /// cgroup 配额换算出的可用 CPU 数（容器里 `cpu.max` / `cpu.cfs_quota_us`）。
    /// 无配额时为 `None`。有值且小于 `logical_cores` 时前端应按它算百分比。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 2.0)]
    pub quota_cores: Option<f64>,
}

/// 内存与 swap 容量（`/proc/meminfo`）。这里只放**容量**，实时使用率走指标接口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct MemoryInfo {
    /// `MemTotal`，字节。
    #[schema(example = 67_108_864_000_u64)]
    pub total_bytes: u64,
    /// `MemAvailable`，字节。这是「还能给新进程用多少」，**不是** `free`——展示占用率请用它。
    #[schema(example = 41_943_040_000_u64)]
    pub available_bytes: u64,
    /// `SwapTotal`，字节。无 swap 时为 0。
    #[schema(example = 0_u64)]
    pub swap_total_bytes: u64,
    /// `SwapFree`，字节。
    #[schema(example = 0_u64)]
    pub swap_free_bytes: u64,
}

/// 块设备（`/sys/block/<name>`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiskInfo {
    /// 内核设备名，不带 `/dev/` 前缀。
    #[schema(example = "nvme0n1")]
    pub name: String,
    /// 设备型号（`device/model`）。虚拟设备读不到，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "SAMSUNG MZQL21T9HCJR-00A07")]
    pub model: Option<String>,
    /// 容量，字节（`size` × 512）。
    #[schema(example = 1_920_383_410_176_u64)]
    pub size_bytes: u64,
    /// 是否机械盘（`queue/rotational == 1`）。
    pub rotational: bool,
    /// 是否可移动介质（`removable == 1`）。
    pub removable: bool,
    /// 是否只读。
    pub read_only: bool,
    /// SMART 总体健康结论。**P0 通常为 `None`**——判定 SMART 需要 root + `smartctl`，
    /// 而本项目默认「系统里什么都没有」。有值时同时会出现在 [`HealthReport`] 里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_healthy: Option<bool>,
}

/// 已挂载文件系统的容量占用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FilesystemInfo {
    /// 挂载点。
    #[schema(example = "/")]
    pub mount_point: String,
    /// 源设备或伪文件系统名。
    #[schema(example = "/dev/nvme0n1p2")]
    pub device: String,
    /// 文件系统类型。
    #[schema(example = "ext4")]
    pub fs_type: String,
    /// 总容量，字节。
    pub total_bytes: u64,
    /// 已用容量，字节。等于 `total - free`，**含 root 保留块**（所以 `used + available < total`）。
    pub used_bytes: u64,
    /// 非特权用户可用容量，字节（`statvfs.f_bavail`）。展示「还剩多少」用它。
    pub available_bytes: u64,
    /// inode 总数。不支持 inode 概念的文件系统（btrfs / tmpfs 部分实现）为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inodes_total: Option<u64>,
    /// 已用 inode 数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inodes_used: Option<u64>,
    /// 是否以只读挂载。
    pub read_only: bool,
}

// ============================= /api/v1/system/health =============================

/// 健康条目的严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthSeverity {
    /// 正常，仅用于 [`HealthReport::status`]；条目列表里一般不出现 `ok` 项。
    Ok,
    /// 提示性信息，不需要立即处理（如「有可用更新」）。
    Info,
    /// 需要关注（磁盘 85%、有 unit 处于 failed）。
    Warning,
    /// 需要立即处理（磁盘 95%、SMART 报废、根文件系统只读）。
    Critical,
}

/// `GET /api/v1/system/health` 的响应体：结构化健康条目列表。
///
/// 不返回自由文本——每条都带稳定的 [`HealthItem::id`]，前端据此做本地化、去重与跳转。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HealthReport {
    /// 采样时刻。
    pub ts: i64,
    /// 总体结论 = 所有条目里最高的严重级别；无条目时为 [`HealthSeverity::Ok`]。
    pub status: HealthSeverity,
    /// 条目列表，建议按 `severity` 降序排列。**空数组表示一切正常**，不是「没检查」。
    #[serde(default)]
    pub items: Vec<HealthItem>,
    /// 因能力缺失而**未能检查**的项，取值为能力名（如 `"systemd"`、`"journal"`）。
    /// 前端应说明「未检查」而非「正常」。
    #[serde(default)]
    pub skipped: Vec<String>,
}

/// 一条健康检查结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HealthItem {
    /// 稳定的机器可读 id，同一类问题共用同一个 id：
    /// `"unit.failed"` / `"disk.usage"` / `"disk.inodes"` / `"disk.smart"` /
    /// `"reboot.required"` / `"fs.read_only"`。前端按它决定图标与跳转目标。
    #[schema(example = "unit.failed")]
    pub id: String,
    /// 严重级别。
    pub severity: HealthSeverity,
    /// 一句话标题，可直接展示。
    #[schema(example = "3 个 unit 处于 failed 状态")]
    pub title: String,
    /// 展开后的详细说明；没有更多可说时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "nginx.service, redis.service, backup.timer")]
    pub detail: Option<String>,
    /// 具体对象，供前端做深链：unit 名 / 挂载点 / 块设备名。
    /// 同一个 `id` 可以出现多条，靠 `target` 区分（比如两个挂载点都超阈值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/var")]
    pub target: Option<String>,
}

// ============================== /api/v1/system/time ==============================

/// `GET /api/v1/system/time` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TimeInfo {
    /// 服务端当前时刻。前端可与本地时钟比对，提示「服务器时间偏差 N 秒」。
    pub ts: i64,
    /// IANA 时区名。读不到时为 `"UTC"`。
    #[schema(example = "Asia/Shanghai")]
    pub timezone: String,
    /// 当前时区相对 UTC 的偏移，**秒**（东为正）。含夏令时。
    #[schema(example = 28800)]
    pub utc_offset_secs: i32,
    /// 是否启用了网络时间同步（`systemd-timesyncd` / `chronyd` / `ntpd` 已 enable）。
    /// 探测不到任何 NTP 实现时为 `None`（区别于「明确没开」的 `Some(false)`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp_enabled: Option<bool>,
    /// 时钟是否**已经**同步上。同 `ntp_enabled`，探测不到为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ntp_synchronized: Option<bool>,
    /// 提供 NTP 的服务名，用于前端展示与跳转到对应 unit。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "systemd-timesyncd.service")]
    pub ntp_service: Option<String>,
    /// RTC 是否按本地时间而非 UTC 存储（双系统机器常见）。读不到为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtc_local: Option<bool>,
}

/// `PUT /api/v1/system/hostname` 的请求体（⭕ 可选项）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SetHostnameReq {
    /// 新的静态主机名。必须是合法 hostname（仅 `[a-zA-Z0-9-.]`，不超过 64 字节）。
    #[schema(example = "web-02")]
    pub hostname: String,
    /// 新的「漂亮主机名」，允许任意 UTF-8。传 `None` 表示不改动。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pretty_hostname: Option<String>,
}

/// `PUT /api/v1/system/timezone` 的请求体（⭕ 可选项）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SetTimezoneReq {
    /// IANA 时区名，必须存在于 `/usr/share/zoneinfo`。
    #[schema(example = "Asia/Shanghai")]
    pub timezone: String,
}

// ============================= /api/v1/system/power ==============================

/// 电源操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    /// 重启。
    Reboot,
    /// 关机。**远程管理场景下关机等于失联**，前端必须二次确认。
    Poweroff,
}

/// `POST /api/v1/system/power` 的请求体。
///
/// 该操作必然要求管理访问；未提权时返回 [`crate::ErrorCode::ElevationRequired`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PowerReq {
    /// 要执行的操作。
    pub action: PowerAction,
}
