//! 指标常量表：名字 / 单位 / 说明 / 标签键，全项目唯一定义处。
//!
//! 采集器只引用这里的 `pub const` 名字常量，不写字面量；`GET /api/v1/metrics/series`
//! 用 [`MetricDef::series_meta`] 把表项导出成 [`SeriesMeta`]。
//!
//! # 单位取值
//!
//! [`SeriesMeta::unit`] 的文档给出了建议词表（`bytes` / `bytes/s` / `percent` / `count` /
//! `seconds` / `iops`）。本表在此基础上补了两个：
//!
//! - `count/s`：网络包 / 错误 / 丢包速率（每秒个数）；
//! - `ms`：磁盘 `await`（每次 IO 平均等待毫秒）。用 `seconds` 会得到 `0.0012` 这种数字，
//!   与 `iostat` 的习惯相悖。
//!
//! 负载均值（`load.*`）无单位，`unit` 为 `None`。

use strixmaid_types::metrics::SeriesMeta;

// ============================ 单位 ============================

/// 单位字符串常量，与 [`SeriesMeta::unit`] 的取值一致。
pub mod unit {
    /// 字节数（瞬时量）。
    pub const BYTES: &str = "bytes";
    /// 字节速率，已由采集器差分。
    pub const BYTES_PER_SEC: &str = "bytes/s";
    /// 百分比，`0.0..=100.0`。
    pub const PERCENT: &str = "percent";
    /// 个数（瞬时量）。
    pub const COUNT: &str = "count";
    /// 每秒个数，已由采集器差分。
    pub const COUNT_PER_SEC: &str = "count/s";
    /// 每秒 IO 次数。
    pub const IOPS: &str = "iops";
    /// 毫秒。
    pub const MS: &str = "ms";
}

// ============================ 标签键 ============================

/// 标签键常量。值由采集器生成，不含 `,` / `=`（见 `store::canonical_labels` 的约定）。
pub mod label {
    /// CPU 核编号，`/proc/stat` 里 `cpuN` 的 N。
    pub const CORE: &str = "core";
    /// 块设备名（整盘），如 `sda` / `nvme0n1` / `dm-0`。
    pub const DEV: &str = "dev";
    /// 挂载点路径，如 `/` / `/boot/efi`。
    pub const MOUNT: &str = "mount";
    /// 网络接口名。
    pub const IFACE: &str = "iface";
}

// ============================ 指标名 ============================

// --- CPU（总） ---
pub const CPU_USAGE: &str = "cpu.usage";
pub const CPU_USER: &str = "cpu.user";
pub const CPU_NICE: &str = "cpu.nice";
pub const CPU_SYSTEM: &str = "cpu.system";
pub const CPU_IDLE: &str = "cpu.idle";
pub const CPU_IOWAIT: &str = "cpu.iowait";
pub const CPU_IRQ: &str = "cpu.irq";
pub const CPU_SOFTIRQ: &str = "cpu.softirq";
pub const CPU_STEAL: &str = "cpu.steal";
// --- CPU（每核，标签 core） ---
pub const CPU_CORE_USAGE: &str = "cpu.core.usage";
pub const CPU_CORE_USER: &str = "cpu.core.user";
pub const CPU_CORE_NICE: &str = "cpu.core.nice";
pub const CPU_CORE_SYSTEM: &str = "cpu.core.system";
pub const CPU_CORE_IDLE: &str = "cpu.core.idle";
pub const CPU_CORE_IOWAIT: &str = "cpu.core.iowait";
pub const CPU_CORE_IRQ: &str = "cpu.core.irq";
pub const CPU_CORE_SOFTIRQ: &str = "cpu.core.softirq";
pub const CPU_CORE_STEAL: &str = "cpu.core.steal";
// --- 内存 ---
pub const MEM_TOTAL: &str = "mem.total";
pub const MEM_AVAILABLE: &str = "mem.available";
pub const MEM_USED: &str = "mem.used";
pub const MEM_FREE: &str = "mem.free";
pub const MEM_BUFFERS: &str = "mem.buffers";
pub const MEM_CACHED: &str = "mem.cached";
pub const MEM_DIRTY: &str = "mem.dirty";
pub const MEM_SWAP_TOTAL: &str = "mem.swap_total";
pub const MEM_SWAP_FREE: &str = "mem.swap_free";
pub const MEM_SWAP_USED: &str = "mem.swap_used";
// --- 负载与进程数 ---
pub const LOAD_1M: &str = "load.1m";
pub const LOAD_5M: &str = "load.5m";
pub const LOAD_15M: &str = "load.15m";
pub const PROCS_RUNNING: &str = "procs.running";
pub const PROCS_TOTAL: &str = "procs.total";
// --- PSI ---
pub const PSI_CPU_SOME: &str = "psi.cpu.some";
pub const PSI_CPU_FULL: &str = "psi.cpu.full";
pub const PSI_MEMORY_SOME: &str = "psi.memory.some";
pub const PSI_MEMORY_FULL: &str = "psi.memory.full";
pub const PSI_IO_SOME: &str = "psi.io.some";
pub const PSI_IO_FULL: &str = "psi.io.full";
// --- 磁盘（标签 dev） ---
pub const DISK_READ_BYTES: &str = "disk.read_bytes";
pub const DISK_WRITE_BYTES: &str = "disk.write_bytes";
pub const DISK_READ_IOPS: &str = "disk.read_iops";
pub const DISK_WRITE_IOPS: &str = "disk.write_iops";
pub const DISK_UTIL: &str = "disk.util";
pub const DISK_AWAIT: &str = "disk.await";
// --- 文件系统（标签 mount） ---
pub const FS_USED: &str = "fs.used";
pub const FS_TOTAL: &str = "fs.total";
pub const FS_USAGE: &str = "fs.usage";
pub const FS_INODES_USED: &str = "fs.inodes_used";
pub const FS_INODES_TOTAL: &str = "fs.inodes_total";
// --- 网络（标签 iface） ---
pub const NET_RX_BYTES: &str = "net.rx_bytes";
pub const NET_TX_BYTES: &str = "net.tx_bytes";
pub const NET_RX_PACKETS: &str = "net.rx_packets";
pub const NET_TX_PACKETS: &str = "net.tx_packets";
pub const NET_RX_ERRORS: &str = "net.rx_errors";
pub const NET_TX_ERRORS: &str = "net.tx_errors";
pub const NET_RX_DROPS: &str = "net.rx_drops";
pub const NET_TX_DROPS: &str = "net.tx_drops";

// ============================ 表 ============================

/// 一个指标的静态定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricDef {
    /// 指标名，点分层级，与 `series.metric` 一致。
    pub name: &'static str,
    /// 单位，见模块文档。`None` 表示无量纲。
    pub unit: Option<&'static str>,
    /// 一句话说明，面向前端 tooltip 与文档。
    pub desc: &'static str,
    /// 该指标携带的标签键（按 [`label`] 常量）。空切片表示无标签、全局唯一一条。
    pub labels: &'static [&'static str],
}

impl MetricDef {
    /// 导出成 API 的 [`SeriesMeta`]。`labels` 须已是规范形式（`store::canonical_labels`）。
    pub fn series_meta(&self, id: i64, node: &str, labels: &str) -> SeriesMeta {
        SeriesMeta {
            id,
            node: node.to_owned(),
            metric: self.name.to_owned(),
            labels: labels.to_owned(),
            unit: self.unit.map(str::to_owned),
        }
    }
}

const fn def(
    name: &'static str,
    unit: Option<&'static str>,
    desc: &'static str,
    labels: &'static [&'static str],
) -> MetricDef {
    MetricDef {
        name,
        unit,
        desc,
        labels,
    }
}

const PCT: Option<&str> = Some(unit::PERCENT);
const BYTES: Option<&str> = Some(unit::BYTES);
const BPS: Option<&str> = Some(unit::BYTES_PER_SEC);
const CNT: Option<&str> = Some(unit::COUNT);
const CPS: Option<&str> = Some(unit::COUNT_PER_SEC);
const IOPS: Option<&str> = Some(unit::IOPS);
const MS: Option<&str> = Some(unit::MS);
const NONE: Option<&str> = None;

const CORE: &[&str] = &[label::CORE];
const DEV: &[&str] = &[label::DEV];
const MOUNT: &[&str] = &[label::MOUNT];
const IFACE: &[&str] = &[label::IFACE];

/// 全部 P0 指标（design.md §7.1）。顺序即 API 里的展示顺序。
pub const CATALOG: &[MetricDef] = &[
    // CPU 总
    def(CPU_USAGE, PCT, "CPU 总占用（100 − idle − iowait）", &[]),
    def(CPU_USER, PCT, "用户态时间占比（不含 guest）", &[]),
    def(
        CPU_NICE,
        PCT,
        "低优先级用户态时间占比（不含 guest_nice）",
        &[],
    ),
    def(CPU_SYSTEM, PCT, "内核态时间占比", &[]),
    def(CPU_IDLE, PCT, "空闲时间占比", &[]),
    def(CPU_IOWAIT, PCT, "等待 IO 的空闲时间占比", &[]),
    def(CPU_IRQ, PCT, "硬中断时间占比", &[]),
    def(CPU_SOFTIRQ, PCT, "软中断时间占比", &[]),
    def(CPU_STEAL, PCT, "被宿主机偷走的时间占比（虚拟机）", &[]),
    // CPU 每核
    def(CPU_CORE_USAGE, PCT, "单核占用（100 − idle − iowait）", CORE),
    def(CPU_CORE_USER, PCT, "单核用户态时间占比", CORE),
    def(CPU_CORE_NICE, PCT, "单核低优先级用户态时间占比", CORE),
    def(CPU_CORE_SYSTEM, PCT, "单核内核态时间占比", CORE),
    def(CPU_CORE_IDLE, PCT, "单核空闲时间占比", CORE),
    def(CPU_CORE_IOWAIT, PCT, "单核等待 IO 时间占比", CORE),
    def(CPU_CORE_IRQ, PCT, "单核硬中断时间占比", CORE),
    def(CPU_CORE_SOFTIRQ, PCT, "单核软中断时间占比", CORE),
    def(CPU_CORE_STEAL, PCT, "单核 steal 时间占比", CORE),
    // 内存
    def(MEM_TOTAL, BYTES, "物理内存总量（MemTotal）", &[]),
    def(
        MEM_AVAILABLE,
        BYTES,
        "可供新进程使用的内存估计值（MemAvailable）",
        &[],
    ),
    def(MEM_USED, BYTES, "已用内存（MemTotal − MemAvailable）", &[]),
    def(MEM_FREE, BYTES, "完全空闲内存（MemFree）", &[]),
    def(MEM_BUFFERS, BYTES, "块设备缓冲（Buffers）", &[]),
    def(MEM_CACHED, BYTES, "页缓存（Cached）", &[]),
    def(MEM_DIRTY, BYTES, "等待写回的脏页（Dirty）", &[]),
    def(MEM_SWAP_TOTAL, BYTES, "交换空间总量（SwapTotal）", &[]),
    def(MEM_SWAP_FREE, BYTES, "空闲交换空间（SwapFree）", &[]),
    def(
        MEM_SWAP_USED,
        BYTES,
        "已用交换空间（SwapTotal − SwapFree）",
        &[],
    ),
    // 负载
    def(LOAD_1M, NONE, "1 分钟负载均值", &[]),
    def(LOAD_5M, NONE, "5 分钟负载均值", &[]),
    def(LOAD_15M, NONE, "15 分钟负载均值", &[]),
    def(PROCS_RUNNING, CNT, "可运行（运行队列）的调度实体数", &[]),
    def(PROCS_TOTAL, CNT, "系统内调度实体（线程）总数", &[]),
    // PSI
    def(
        PSI_CPU_SOME,
        PCT,
        "CPU 压力：至少一个任务在等 CPU 的时间占比（avg10）",
        &[],
    ),
    def(
        PSI_CPU_FULL,
        PCT,
        "CPU 压力：全部任务都在等 CPU 的时间占比（avg10，5.13+ 内核）",
        &[],
    ),
    def(
        PSI_MEMORY_SOME,
        PCT,
        "内存压力：至少一个任务因内存停滞的时间占比（avg10）",
        &[],
    ),
    def(
        PSI_MEMORY_FULL,
        PCT,
        "内存压力：全部任务因内存停滞的时间占比（avg10）",
        &[],
    ),
    def(
        PSI_IO_SOME,
        PCT,
        "IO 压力：至少一个任务因 IO 停滞的时间占比（avg10）",
        &[],
    ),
    def(
        PSI_IO_FULL,
        PCT,
        "IO 压力：全部任务因 IO 停滞的时间占比（avg10）",
        &[],
    ),
    // 磁盘
    def(DISK_READ_BYTES, BPS, "读吞吐", DEV),
    def(DISK_WRITE_BYTES, BPS, "写吞吐", DEV),
    def(DISK_READ_IOPS, IOPS, "每秒完成的读请求数", DEV),
    def(DISK_WRITE_IOPS, IOPS, "每秒完成的写请求数", DEV),
    def(DISK_UTIL, PCT, "设备忙碌时间占比（iostat 的 %util）", DEV),
    def(
        DISK_AWAIT,
        MS,
        "每次 IO 的平均等待毫秒（含排队与服务时间）",
        DEV,
    ),
    // 文件系统
    def(FS_USED, BYTES, "已用空间（total − 非特权可用）", MOUNT),
    def(FS_TOTAL, BYTES, "文件系统总容量", MOUNT),
    def(
        FS_USAGE,
        PCT,
        "空间使用率（used / (used + 非特权可用)）",
        MOUNT,
    ),
    def(FS_INODES_USED, CNT, "已用 inode 数", MOUNT),
    def(FS_INODES_TOTAL, CNT, "inode 总数", MOUNT),
    // 网络
    def(NET_RX_BYTES, BPS, "接收吞吐", IFACE),
    def(NET_TX_BYTES, BPS, "发送吞吐", IFACE),
    def(NET_RX_PACKETS, CPS, "每秒接收包数", IFACE),
    def(NET_TX_PACKETS, CPS, "每秒发送包数", IFACE),
    def(NET_RX_ERRORS, CPS, "每秒接收错误数", IFACE),
    def(NET_TX_ERRORS, CPS, "每秒发送错误数", IFACE),
    def(NET_RX_DROPS, CPS, "每秒接收丢包数", IFACE),
    def(NET_TX_DROPS, CPS, "每秒发送丢包数", IFACE),
];

/// 按名字查表。
pub fn find(name: &str) -> Option<&'static MetricDef> {
    CATALOG.iter().find(|d| d.name == name)
}

/// 指标的单位；未登记的指标返回 `None`。
pub fn unit_of(name: &str) -> Option<&'static str> {
    find(name).and_then(|d| d.unit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn 指标名唯一且格式合法() {
        let mut seen = HashSet::new();
        for d in CATALOG {
            assert!(seen.insert(d.name), "重复的指标名 {}", d.name);
            assert!(
                d.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'),
                "指标名只能含小写字母/数字/点/下划线：{}",
                d.name
            );
            assert!(!d.desc.is_empty(), "{} 缺说明", d.name);
        }
    }

    #[test]
    fn 速率类指标的单位带每秒() {
        for d in CATALOG {
            let is_rate = d.name.ends_with("_bytes")
                && (d.name.starts_with("disk.") || d.name.starts_with("net."));
            if is_rate {
                assert_eq!(d.unit, Some(unit::BYTES_PER_SEC), "{}", d.name);
            }
        }
        assert_eq!(unit_of(CPU_USAGE), Some(unit::PERCENT));
        assert_eq!(unit_of(LOAD_1M), None);
        assert_eq!(unit_of("no.such"), None);
    }

    #[test]
    fn 导出_series_meta() {
        let m = find(DISK_READ_BYTES)
            .unwrap()
            .series_meta(7, "local", "dev=sda");
        assert_eq!(m.id, 7);
        assert_eq!(m.node, "local");
        assert_eq!(m.metric, "disk.read_bytes");
        assert_eq!(m.labels, "dev=sda");
        assert_eq!(m.unit.as_deref(), Some("bytes/s"));
    }
}
