//! 指标常量表：名字 / 单位 / 说明 / 标签键 / 所属面板，全项目唯一定义处。
//!
//! 采集器只引用这里的 `pub const` 名字常量，不写字面量；`GET /api/v1/metrics/series`
//! 用 [`MetricDef::series_meta`] 把表项导出成 [`SeriesMeta`]。
//!
//! # 34 项的口径（roadmap/08 §4）
//!
//! 本表在 2026-08-28 由 58 项裁到 34 项，裁剪按三条规则执行（§4.1）：
//! 派生量不存（`cpu.idle` = 100 − 其余）、同向计数器合成一条异常信号
//! （`net.errors`）、慢变量进健康检查而非时序库（inode → `disk.inodes`）。
//! 四个**合并项**的加法在采集器里做：`cpu.irq` = 硬 + 软中断、`mem.cached` =
//! Cached + Buffers、`disk.iops` = 读 + 写、`net.errors` = 收发错误 + 收发丢包。
//! 被裁名字的老库清理见 `migrations/0002_metrics_trim.sql`，**名单逐字列出，
//! 不按前缀匹配**。
//!
//! # 单位取值
//!
//! [`SeriesMeta::unit`] 的文档给出了建议词表（`bytes` / `bytes/s` / `percent` / `count` /
//! `seconds` / `iops`）。本表在此基础上补了三个：
//!
//! - `count/s`：异常包速率（每秒个数）；
//! - `ms`：磁盘 `await`（每次 IO 平均等待毫秒）。用 `seconds` 会得到 `0.0012` 这种数字，
//!   与 `iostat` 的习惯相悖；
//! - `celsius`：GPU 温度。
//!
//! 负载均值（`load.1m`）无单位，`unit` 为 `None`。

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
    /// 摄氏度。
    pub const CELSIUS: &str = "celsius";
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
    /// DRM 卡名，如 `card0`。
    pub const GPU: &str = "gpu";
}

// ============================ 指标名 ============================

// --- CPU（总） ---
pub const CPU_USAGE: &str = "cpu.usage";
pub const CPU_SYSTEM: &str = "cpu.system";
pub const CPU_IOWAIT: &str = "cpu.iowait";
/// **含软中断**：`irq + softirq` 之和（roadmap/08 §4.2 的语义变更）。
pub const CPU_IRQ: &str = "cpu.irq";
pub const CPU_STEAL: &str = "cpu.steal";
// --- CPU（每核，标签 core）。唯一保留的每核指标 ---
pub const CPU_CORE_USAGE: &str = "cpu.core.usage";
// --- GPU（标签 gpu） ---
pub const GPU_USAGE: &str = "gpu.usage";
pub const GPU_MEM_USED: &str = "gpu.mem_used";
pub const GPU_MEM_TOTAL: &str = "gpu.mem_total";
pub const GPU_TEMP: &str = "gpu.temp";
// --- 内存 ---
pub const MEM_TOTAL: &str = "mem.total";
pub const MEM_USED: &str = "mem.used";
pub const MEM_AVAILABLE: &str = "mem.available";
/// **含块设备缓冲**：`Cached + Buffers` 之和（roadmap/08 §4.2 的语义变更）。
pub const MEM_CACHED: &str = "mem.cached";
pub const MEM_SWAP_TOTAL: &str = "mem.swap_total";
pub const MEM_SWAP_USED: &str = "mem.swap_used";
// --- 负载与进程数 ---
pub const LOAD_1M: &str = "load.1m";
pub const PROCS_RUNNING: &str = "procs.running";
pub const PROCS_TOTAL: &str = "procs.total";
// --- PSI（cpu 无 full：内核在整机层面对 cpu 的 full 没有定义） ---
pub const PSI_CPU_SOME: &str = "psi.cpu.some";
pub const PSI_MEMORY_SOME: &str = "psi.memory.some";
pub const PSI_MEMORY_FULL: &str = "psi.memory.full";
pub const PSI_IO_SOME: &str = "psi.io.some";
pub const PSI_IO_FULL: &str = "psi.io.full";
// --- 磁盘（标签 dev） ---
pub const DISK_READ_BYTES: &str = "disk.read_bytes";
pub const DISK_WRITE_BYTES: &str = "disk.write_bytes";
/// 读写合计（roadmap/08 §4.2：方向已由两条 bytes 给出，IOPS 只回答大 IO 还是小 IO）。
pub const DISK_IOPS: &str = "disk.iops";
pub const DISK_UTIL: &str = "disk.util";
pub const DISK_AWAIT: &str = "disk.await";
// --- 文件系统（标签 mount；usage 由前端做除法，inode 在健康检查 `disk.inodes` 里） ---
pub const FS_USED: &str = "fs.used";
pub const FS_TOTAL: &str = "fs.total";
// --- 网络（标签 iface） ---
pub const NET_RX_BYTES: &str = "net.rx_bytes";
pub const NET_TX_BYTES: &str = "net.tx_bytes";
/// 收发错误 + 收发丢包之和。非零即异常；要分方向 `ip -s link` 一条命令。
pub const NET_ERRORS: &str = "net.errors";

// ============================ 表 ============================

/// 指标在前端归入哪个性能面板（roadmap/08 §9）。
///
/// 前端按它分组，**不再硬编码指标名前缀**。PSI 三项跟随压力带的归属
/// （roadmap/08 §6.7）：`psi.cpu` 归 CPU、`psi.memory` 归内存、`psi.io` 归磁盘；
/// 负载与进程数归 CPU。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Cpu,
    Gpu,
    Memory,
    Disk,
    Filesystem,
    Network,
}

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
    /// 所属面板。
    pub panel: Panel,
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
    panel: Panel,
) -> MetricDef {
    MetricDef {
        name,
        unit,
        desc,
        labels,
        panel,
    }
}

const PCT: Option<&str> = Some(unit::PERCENT);
const BYTES: Option<&str> = Some(unit::BYTES);
const BPS: Option<&str> = Some(unit::BYTES_PER_SEC);
const CNT: Option<&str> = Some(unit::COUNT);
const CPS: Option<&str> = Some(unit::COUNT_PER_SEC);
const IOPS: Option<&str> = Some(unit::IOPS);
const MS: Option<&str> = Some(unit::MS);
const DEG: Option<&str> = Some(unit::CELSIUS);
const NONE: Option<&str> = None;

const CORE: &[&str] = &[label::CORE];
const DEV: &[&str] = &[label::DEV];
const MOUNT: &[&str] = &[label::MOUNT];
const IFACE: &[&str] = &[label::IFACE];
const GPU: &[&str] = &[label::GPU];

/// 全部 P0 指标（roadmap/08 §4.2，34 项）。顺序即 API 里的展示顺序。
pub const CATALOG: &[MetricDef] = &[
    // CPU 总
    def(
        CPU_USAGE,
        PCT,
        "CPU 总占用（100 − idle − iowait），主曲线",
        &[],
        Panel::Cpu,
    ),
    def(
        CPU_SYSTEM,
        PCT,
        "内核态时间占比；面板上作主曲线下方的深色填充",
        &[],
        Panel::Cpu,
    ),
    def(CPU_IOWAIT, PCT, "等待 IO 的空闲时间占比", &[], Panel::Cpu),
    def(
        CPU_IRQ,
        PCT,
        "中断时间占比（硬中断与软中断之和）",
        &[],
        Panel::Cpu,
    ),
    def(
        CPU_STEAL,
        PCT,
        "被宿主机偷走的时间占比（虚拟机；非虚拟化环境整条隐藏）",
        &[],
        Panel::Cpu,
    ),
    // CPU 每核
    def(
        CPU_CORE_USAGE,
        PCT,
        "单核占用（100 − idle − iowait）",
        CORE,
        Panel::Cpu,
    ),
    // GPU
    def(GPU_USAGE, PCT, "GPU 利用率", GPU, Panel::Gpu),
    def(GPU_MEM_USED, BYTES, "已用显存", GPU, Panel::Gpu),
    def(GPU_MEM_TOTAL, BYTES, "显存总量", GPU, Panel::Gpu),
    def(GPU_TEMP, DEG, "GPU 温度", GPU, Panel::Gpu),
    // 内存
    def(
        MEM_TOTAL,
        BYTES,
        "物理内存总量（MemTotal）",
        &[],
        Panel::Memory,
    ),
    def(
        MEM_USED,
        BYTES,
        "已用内存（MemTotal − MemAvailable）",
        &[],
        Panel::Memory,
    ),
    def(
        MEM_AVAILABLE,
        BYTES,
        "可供新进程使用的内存估计值（MemAvailable）",
        &[],
        Panel::Memory,
    ),
    def(
        MEM_CACHED,
        BYTES,
        "页缓存与块设备缓冲之和（Cached + Buffers）",
        &[],
        Panel::Memory,
    ),
    def(
        MEM_SWAP_TOTAL,
        BYTES,
        "交换空间总量（SwapTotal）",
        &[],
        Panel::Memory,
    ),
    def(
        MEM_SWAP_USED,
        BYTES,
        "已用交换空间（SwapTotal − SwapFree）",
        &[],
        Panel::Memory,
    ),
    // 负载与进程数
    def(
        LOAD_1M,
        NONE,
        "1 分钟负载均值（只作数字展示，不画曲线）",
        &[],
        Panel::Cpu,
    ),
    def(
        PROCS_RUNNING,
        CNT,
        "可运行（运行队列）的调度实体数",
        &[],
        Panel::Cpu,
    ),
    def(
        PROCS_TOTAL,
        CNT,
        "系统内调度实体（线程）总数",
        &[],
        Panel::Cpu,
    ),
    // PSI
    def(
        PSI_CPU_SOME,
        PCT,
        "CPU 压力：至少一个任务在等 CPU 的时间占比（avg10）",
        &[],
        Panel::Cpu,
    ),
    def(
        PSI_MEMORY_SOME,
        PCT,
        "内存压力：至少一个任务因内存停滞的时间占比（avg10）",
        &[],
        Panel::Memory,
    ),
    def(
        PSI_MEMORY_FULL,
        PCT,
        "内存压力：全部任务因内存停滞的时间占比（avg10）",
        &[],
        Panel::Memory,
    ),
    def(
        PSI_IO_SOME,
        PCT,
        "IO 压力：至少一个任务因 IO 停滞的时间占比（avg10）",
        &[],
        Panel::Disk,
    ),
    def(
        PSI_IO_FULL,
        PCT,
        "IO 压力：全部任务因 IO 停滞的时间占比（avg10）",
        &[],
        Panel::Disk,
    ),
    // 磁盘
    def(DISK_READ_BYTES, BPS, "读吞吐", DEV, Panel::Disk),
    def(DISK_WRITE_BYTES, BPS, "写吞吐", DEV, Panel::Disk),
    def(
        DISK_IOPS,
        IOPS,
        "每秒完成的 IO 请求数（读与写之和）",
        DEV,
        Panel::Disk,
    ),
    def(
        DISK_UTIL,
        PCT,
        "设备忙碌时间占比（iostat 的 %util，「活动时间」）",
        DEV,
        Panel::Disk,
    ),
    def(
        DISK_AWAIT,
        MS,
        "每次 IO 的平均等待毫秒（含排队与服务时间，「平均响应时间」）",
        DEV,
        Panel::Disk,
    ),
    // 文件系统
    def(
        FS_USED,
        BYTES,
        "已用空间（total − 非特权可用）",
        MOUNT,
        Panel::Filesystem,
    ),
    def(FS_TOTAL, BYTES, "文件系统总容量", MOUNT, Panel::Filesystem),
    // 网络
    def(NET_RX_BYTES, BPS, "接收吞吐", IFACE, Panel::Network),
    def(NET_TX_BYTES, BPS, "发送吞吐", IFACE, Panel::Network),
    def(
        NET_ERRORS,
        CPS,
        "每秒异常包数（收发错误与收发丢包之和），非零即异常",
        IFACE,
        Panel::Network,
    ),
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

    /// roadmap/08 §4.2 的 34 项快照，防止误删误增。改动 CATALOG 必须同步改这里，
    /// 且新增名不得与 `migrations/0002_metrics_trim.sql` 的删除名单重合。
    #[test]
    fn 常量表与_roadmap_08_的_34_项快照一致() {
        let expected: HashSet<&str> = [
            "cpu.usage",
            "cpu.system",
            "cpu.iowait",
            "cpu.irq",
            "cpu.steal",
            "cpu.core.usage",
            "gpu.usage",
            "gpu.mem_used",
            "gpu.mem_total",
            "gpu.temp",
            "mem.total",
            "mem.used",
            "mem.available",
            "mem.cached",
            "mem.swap_total",
            "mem.swap_used",
            "load.1m",
            "procs.running",
            "procs.total",
            "psi.cpu.some",
            "psi.memory.some",
            "psi.memory.full",
            "psi.io.some",
            "psi.io.full",
            "disk.read_bytes",
            "disk.write_bytes",
            "disk.iops",
            "disk.util",
            "disk.await",
            "fs.used",
            "fs.total",
            "net.rx_bytes",
            "net.tx_bytes",
            "net.errors",
        ]
        .into();
        let actual: HashSet<&str> = CATALOG.iter().map(|d| d.name).collect();
        assert_eq!(actual, expected);
        assert_eq!(CATALOG.len(), 34, "34 项之外的增删必须走 roadmap/08 的评审");
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
        assert_eq!(unit_of(GPU_TEMP), Some(unit::CELSIUS));
        assert_eq!(unit_of(NET_ERRORS), Some(unit::COUNT_PER_SEC));
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

    #[test]
    fn 面板归属跟随压力带的语义() {
        // roadmap/08 §6.7：psi.cpu → CPU、psi.memory → 内存、psi.io → 磁盘。
        assert_eq!(find(PSI_CPU_SOME).unwrap().panel, Panel::Cpu);
        assert_eq!(find(PSI_MEMORY_FULL).unwrap().panel, Panel::Memory);
        assert_eq!(find(PSI_IO_SOME).unwrap().panel, Panel::Disk);
        assert_eq!(find(LOAD_1M).unwrap().panel, Panel::Cpu);
        assert_eq!(find(GPU_TEMP).unwrap().panel, Panel::Gpu);
    }
}
