//! 磁盘：`IOCTL_DISK_PERFORMANCE` 差分 → 每块**物理盘**的吞吐 / IOPS / util% / await。
//!
//! 取数在 [`crate::platform::windows::volume::disk_counters`]，那正是
//! `typeperf \PhysicalDisk(*)\…` 与任务管理器「磁盘」页的同一个数据源
//! （分区管理器在每块盘上维护 `DISK_PERFORMANCE`，Vista 起默认启用）。
//! 与 Linux 版（`/proc/diskstats`）产出完全相同的五个指标：
//!
//! | 指标 | 算法 |
//! |---|---|
//! | `disk.read_bytes` / `disk.write_bytes` | `BytesRead` / `BytesWritten` 差分 |
//! | `disk.iops` | `(ReadCount + WriteCount)` 差分（读写合计，roadmap/08 §4.2） |
//! | `disk.util` | `100 × (1 − Δidle / Δ墙钟)` |
//! | `disk.await` | `Δ(ReadTime + WriteTime) / Δ操作数`，毫秒 |
//!
//! # `util` 为什么用 idle 而不是 busy
//!
//! `DISK_PERFORMANCE` 同时给 `ReadTime + WriteTime`（忙）与 `IdleTime`（闲），
//! 两者**不互补**：前者是每个 IO 的处理时长累计，NVMe 的深队列下 N 个请求并发
//! 时一个墙钟秒能累出 N 秒「忙」；后者是「队列空着」的真实墙钟时间。
//! `iostat` 的 `%util` 要的是「设备有活干的时间占比」，所以用
//! `1 − Δidle/Δ墙钟` 算，得到的值天然落在 `0..=100`，不需要靠封顶去掩盖溢出
//! （仍然封顶一次，防的是计数器与单调时钟之间的采样错位）。
//! 忙时间留给 `await` 用——那里要的正是「处理时长累计 ÷ 请求数」。
//!
//! # `await` 无 IO 的轮次不产出
//!
//! `Δ操作数 == 0` 时平均等待时间没有定义。产出 0 会把「这一秒没有 IO」画成
//! 「这一秒的 IO 零延迟」，那是两回事（roadmap/08 §6.2：await 不可合计，
//! 也不可编造）。宁缺毋滥，与 Linux / macOS 版一致。
//!
//! # 标签是物理盘而不是卷
//!
//! `dev` 取 `PhysicalDriveN`，与 Linux 的 `sda` / `nvme0n1` 同级。卷（`C:`）的
//! 空间用量在 [`super::fs`]，两者是两件事：一块盘可以挂多个卷，也可以一个都不挂。
//!
//! 每轮重新枚举物理盘（而不是构造时定死）：USB 盘、热插拔的 NVMe 都会改变盘号
//! 集合，一次枚举只是几次 `CreateFileW`，成本可忽略。首轮只建基线不产出。
//!
//! # 枚举有一条退路
//!
//! [`physical_disks`] 是盘的权威枚举（它顺带给出型号、容量、是否机械盘，
//! `providers::system` 那边要用），但它把 `IOCTL_DISK_GET_LENGTH_INFO` 当作
//! 「这个盘号存在」的判据，而那个 IOCTL 声明的是 `FILE_READ_ACCESS`——
//! 非管理员进程拿不到，于是整张表退化成空（本机实测：`PhysicalDrive0` 能打开、
//! 型号读得出、`IOCTL_DISK_PERFORMANCE` 也走得通，唯独长度查询回 `ERROR_ACCESS_DENIED`，
//! `physical_disks()` 返回 0 块）。
//!
//! 指标采集只需要盘号，而 `IOCTL_DISK_PERFORMANCE` 是 `FILE_ANY_ACCESS` 的，
//! 不需要提权。所以这里加一条退路：权威枚举为空时，直接按盘号逐个试
//! [`disk_counters`]——**能采到计数的盘就是能采的盘**，判据与采集本身同一件事，
//! 和 `linux/gpu.rs` 用「`gpu_busy_percent` 读得到」当作「这块卡可采集」是同一手法。
//! 权威枚举一旦修好，这条退路自然不会被走到。

use std::collections::HashMap;
use std::time::Instant;

use super::{CollectError, Collector, Sample, elapsed_secs, rate, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::windows::volume::{DiskCounters, disk_counters, physical_disks};

/// 100 纳秒为单位的计数换算：1 秒 = 10^7 个 100ns。
const TICKS_PER_SEC: f64 = 1e7;
/// 100 纳秒 → 毫秒。
const TICKS_PER_MS: f64 = 1e4;
/// 退路枚举：连续多少个盘号采不到计数就认为到头了（与 `physical_disks` 同值）。
const PROBE_GAP: u32 = 4;
/// 退路枚举的盘号上界，防御性兜底（与 `physical_disks` 同值）。
const MAX_DISK: u32 = 64;

/// 一块盘两轮之间的速率。全部字段已差分。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskRates {
    pub read_bytes: f64,
    pub write_bytes: f64,
    /// 读 + 写，每秒。
    pub iops: f64,
    /// `0..=100`。
    pub util: f64,
    /// 每次 IO 的平均毫秒；本轮没有 IO 时为 `None`。
    pub await_ms: Option<f64>,
}

/// 两轮计数差分。任一计数器回退（盘被换掉、计数器重置）时返回 `None`。
pub fn rates_between(prev: &DiskCounters, cur: &DiskCounters, secs: f64) -> Option<DiskRates> {
    let read_bytes = rate(prev.read_bytes, cur.read_bytes, secs)?;
    let write_bytes = rate(prev.write_bytes, cur.write_bytes, secs)?;
    let read_ops = rate(prev.read_ops, cur.read_ops, secs)?;
    let write_ops = rate(prev.write_ops, cur.write_ops, secs)?;
    // idle / busy 也要过一遍 rate()，图的是它对「计数器回退」的判定。
    let idle_per_sec = rate(prev.idle_100ns, cur.idle_100ns, secs)?;
    let busy_delta = cur.busy_100ns.checked_sub(prev.busy_100ns)?;

    // idle 是墙钟时间，Δidle/Δ墙钟 即空闲占比；采样错位可能让它略超 1，夹一下。
    let util = (100.0 * (1.0 - idle_per_sec / TICKS_PER_SEC)).clamp(0.0, 100.0);

    let ops_delta = (cur.read_ops + cur.write_ops).saturating_sub(prev.read_ops + prev.write_ops);
    let await_ms = (ops_delta > 0).then(|| busy_delta as f64 / TICKS_PER_MS / ops_delta as f64);

    Some(DiskRates {
        read_bytes,
        write_bytes,
        iops: read_ops + write_ops,
        util,
        await_ms,
    })
}

/// 读一轮全部可采盘的计数，键为 `PhysicalDriveN`。
///
/// 先问权威枚举；它给不出盘（非管理员，见模块文档）时退化成按盘号逐个试计数器。
pub fn snapshot() -> Vec<(String, DiskCounters)> {
    let listed = physical_disks();
    if !listed.is_empty() {
        return listed
            .into_iter()
            // 性能计数取不到的盘（虚拟盘、被驱动挡住的盘）少一组曲线，不是错误。
            .filter_map(|d| disk_counters(d.number).map(|c| (d.name, c)))
            .collect();
    }

    let mut out = Vec::new();
    let mut miss = 0;
    for n in 0..MAX_DISK {
        match disk_counters(n) {
            Some(c) => {
                out.push((format!("PhysicalDrive{n}"), c));
                miss = 0;
            }
            None => {
                miss += 1;
                // 盘号不保证连续（拔掉中间一块会留空洞），所以要连着几个都没有才停。
                if miss >= PROBE_GAP && !out.is_empty() {
                    break;
                }
            }
        }
    }
    out
}

/// 磁盘采集器。持有每块盘上一轮的计数。
#[derive(Debug, Default)]
pub struct DiskCollector {
    prev: HashMap<String, (Instant, DiskCounters)>,
}

impl DiskCollector {
    pub fn new() -> Self {
        DiskCollector {
            prev: HashMap::new(),
        }
    }

    /// 喂入一轮计数，产出与上一轮的速率样本；某块盘第一次出现时只建基线。
    pub fn ingest(&mut self, now: Instant, snapshot: Vec<(String, DiskCounters)>) -> Vec<Sample> {
        let mut out = Vec::with_capacity(snapshot.len() * 5);
        for (dev_raw, cur) in snapshot {
            let dev = sanitize_label(&dev_raw);
            let prev = self.prev.insert(dev.clone(), (now, cur));
            let Some((prev_at, p)) = prev else {
                continue; // 首轮：只建基线
            };
            let Some(r) = rates_between(&p, &cur, elapsed_secs(prev_at, now)) else {
                continue; // 计数器回退（盘被换掉）：这一轮作废，基线已更新
            };
            let mk = |m, v| Sample::labeled(m, label::DEV, dev.clone(), v);
            out.push(mk(cat::DISK_READ_BYTES, r.read_bytes));
            out.push(mk(cat::DISK_WRITE_BYTES, r.write_bytes));
            out.push(mk(cat::DISK_IOPS, r.iops));
            out.push(mk(cat::DISK_UTIL, r.util));
            if let Some(ms) = r.await_ms {
                out.push(mk(cat::DISK_AWAIT, ms));
            }
        }
        out
    }
}

impl Collector for DiskCollector {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError> {
        // 一块盘都采不到不是错误（全是被驱动挡住的虚拟盘时就会这样），返回空表。
        Ok(self.ingest(now, snapshot()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn counters(rb: u64, wb: u64, ro: u64, wo: u64, busy: u64, idle: u64) -> DiskCounters {
        DiskCounters {
            read_bytes: rb,
            write_bytes: wb,
            read_ops: ro,
            write_ops: wo,
            busy_100ns: busy,
            idle_100ns: idle,
        }
    }

    #[test]
    fn 速率与_util_口径() {
        // 2 秒里：读 2000 字节 / 20 次，写 4000 字节 / 20 次，
        // 空闲 1.5 秒（1.5e7 个 100ns），忙 0.8 秒（8e6 个 100ns）
        let a = counters(0, 0, 0, 0, 0, 0);
        let b = counters(2000, 4000, 20, 20, 8_000_000, 15_000_000);
        let r = rates_between(&a, &b, 2.0).unwrap();
        let close = |x: f64, y: f64| (x - y).abs() < 1e-9;
        assert!(close(r.read_bytes, 1000.0));
        assert!(close(r.write_bytes, 2000.0));
        assert!(close(r.iops, 20.0), "40 次 / 2 秒");
        // 空闲 1.5s / 墙钟 2s = 75% 闲 → 25% 忙
        assert!(close(r.util, 25.0), "util = {}", r.util);
        // 0.8 秒 = 800ms，40 次 → 20ms
        assert!(close(r.await_ms.unwrap(), 20.0));
    }

    #[test]
    fn 并发深队列下_util_不会溢出() {
        // 忙时间累计 8 秒（8 个请求并发），但墙钟只过了 2 秒、且一直没空闲
        let a = counters(0, 0, 0, 0, 0, 0);
        let b = counters(0, 0, 100, 0, 80_000_000, 0);
        let r = rates_between(&a, &b, 2.0).unwrap();
        assert_eq!(r.util, 100.0, "用 idle 算就不会超过 100");
    }

    #[test]
    fn 空闲时间略超墙钟也夹在零到一百之间() {
        // 采样错位：Δidle 比 Δ墙钟还大
        let a = counters(0, 0, 0, 0, 0, 0);
        let b = counters(0, 0, 0, 0, 0, 21_000_000);
        let r = rates_between(&a, &b, 2.0).unwrap();
        assert_eq!(r.util, 0.0, "不该出现负的 util");
    }

    #[test]
    fn 无_io_的轮次不产出_await() {
        let a = counters(10, 10, 5, 5, 100, 0);
        let b = counters(10, 10, 5, 5, 100, 20_000_000);
        let r = rates_between(&a, &b, 2.0).unwrap();
        assert_eq!(r.iops, 0.0);
        assert!(r.await_ms.is_none(), "没有 IO 时 await 无定义");
    }

    #[test]
    fn 计数器回退返回_none() {
        let a = counters(5000, 0, 10, 0, 100, 100);
        let b = counters(1000, 0, 10, 0, 100, 200);
        assert!(rates_between(&a, &b, 2.0).is_none());
        // 时间差为 0 也不产出
        assert!(rates_between(&a, &a, 0.0).is_none());
    }

    #[test]
    fn 首轮只建基线第二轮才有样本() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mut c = DiskCollector::new();
        let a = vec![("PhysicalDrive0".to_string(), counters(0, 0, 0, 0, 0, 0))];
        assert!(c.ingest(t0, a).is_empty(), "首轮只建基线");
        let b = vec![(
            "PhysicalDrive0".to_string(),
            counters(2000, 0, 20, 0, 8_000_000, 15_000_000),
        )];
        let out = c.ingest(t1, b);
        // read / write / iops / util / await
        assert_eq!(out.len(), 5);
        assert!(
            out.iter()
                .all(|s| s.labels == vec![(label::DEV, "PhysicalDrive0".to_string())])
        );
    }

    /// 一台 Windows 至少有一块能采到性能计数的物理盘（系统就装在上面）。
    /// 这条同时把「权威枚举为空时退路生效」钉住。
    #[test]
    fn 本机能枚举出可采的盘() {
        let started = Instant::now();
        let snap = snapshot();
        let elapsed = started.elapsed();
        assert!(!snap.is_empty(), "系统盘所在的物理盘必须能采到计数");
        for (name, c) in &snap {
            assert!(name.starts_with("PhysicalDrive"), "{name}");
            assert!(
                c.idle_100ns > 0 || c.busy_100ns > 0,
                "{name} 的计数器全是 0，大概率没工作"
            );
        }
        eprintln!(
            "本机可采物理盘 {} 块（枚举耗时 {:?}，权威枚举 {} 块）：{}",
            snap.len(),
            elapsed,
            physical_disks().len(),
            snap.iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    #[test]
    fn 本机两轮采集() {
        let mut c = DiskCollector::new();
        let t0 = Instant::now();
        let first = c.collect(t0).expect("磁盘采集不应失败");
        assert!(first.is_empty(), "首轮只建基线");
        std::thread::sleep(Duration::from_millis(250));
        let out = c.collect(Instant::now()).expect("磁盘采集不应失败");
        assert!(!out.is_empty(), "第二轮必须有样本");
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
            assert_eq!(s.labels.len(), 1);
            assert_eq!(s.labels[0].0, label::DEV);
            assert!(s.labels[0].1.starts_with("PhysicalDrive"), "{:?}", s.labels);
            if s.metric == cat::DISK_UTIL {
                assert!(s.value <= 100.0, "util = {}", s.value);
            }
        }
        eprintln!(
            "本机磁盘：{}",
            out.iter()
                .map(|s| format!("{}{{{}}}={:.2}", s.metric, s.labels[0].1, s.value))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}
