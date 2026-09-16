//! CPU：`SystemProcessorPerformanceInformation` → 总 + 每核的百分比（两轮差分）。
//!
//! 取数在 [`crate::platform::windows::ntdll::processor_times`]，它已经把
//! 「`KernelTime` 含 `IdleTime`」这个 Windows 特有的坑扣平了（见那边的文档）。
//! 本文件只决定口径。
//!
//! # 与 Linux 的覆盖差异
//!
//! `/proc/stat` 给 8 态，内核的逐核记账只有 **idle / kernel / user / dpc /
//! interrupt** 五项。叠加 roadmap/08 §4.2 的裁剪后，本采集器产出
//! `cpu.usage` / `cpu.system` / `cpu.irq` 与每核 `cpu.core.usage`，另外两项如实缺席：
//!
//! - **没有 `cpu.iowait`**。Windows 不把「等 IO 的那段空闲时间」与普通空闲分开
//!   统计：线程在等 IO 时被挂起，那颗核上跑的是空闲线程，内核只记 `IdleTime`。
//!   有人会拿 `\LogicalDisk(_Total)\% Disk Time` 或 `Avg. Disk Queue Length` 冒充，
//!   那是磁盘忙碌度而不是 CPU 时间构成，两者不是一回事——宁可没有。
//! - **没有 `cpu.steal`**。虚拟化下「被宿主机偷走的时间」需要 guest 侧内核与
//!   hypervisor 协作记账（Linux 的 `steal` 来自 paravirt 时钟）。Windows guest
//!   没有对应概念，Hyper-V 的宿主机侧计数器（`Hyper-V Hypervisor Logical
//!   Processor`）只有宿主机自己能读，guest 里读不到。
//!
//! 连带影响 usage 的定义：Linux 是 `100 − idle − iowait`，这里是 `100 − idle`。
//! 两者在各自平台上都表示「非空闲时间占比」，语义一致（与 macOS 侧同理）。
//!
//! # `cpu.system` 里扣掉了中断
//!
//! Windows 的 `InterruptTime` 与 `DpcTime` **包含在** `KernelTime` 里；而
//! `/proc/stat` 的 `system` / `irq` / `softirq` 是三列互斥的时间。常量表里
//! `cpu.system` 与 `cpu.irq` 是给前端叠成两条带子用的，重叠会让面板重复计数。
//! 所以这里产出的 `cpu.system` = `kernel − idle − (dpc + interrupt)`，
//! 与 Linux 的三列互斥对齐。`cpu.irq` = `dpc + interrupt`，正对常量表里
//! 「硬中断与软中断之和」的语义——DPC 就是 Windows 的软中断。
//!
//! # 没有「总」这一行
//!
//! 内核只按核返回，没有 `/proc/stat` 的 `cpu` 汇总行。总量由本模块把各核的
//! **原始 tick 逐态相加**得到，再走同一套差分——不是「各核百分比取平均」。
//! 两者在核数不变时等价，但前者在热插拔（VM 里加减 vCPU）时不会算错。
//!
//! 超过 64 逻辑处理器的机器只覆盖调用线程所在的处理器组，这是
//! `SystemProcessorPerformanceInformation` 的固有限制，见 `ntdll` 的模块文档。

use std::time::Instant;

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::windows::ntdll::{self, CpuTimes};

/// 逐态相加，用于把各核合成总量。
fn add_ticks(acc: &mut CpuTimes, other: &CpuTimes) {
    acc.idle = acc.idle.saturating_add(other.idle);
    acc.system = acc.system.saturating_add(other.system);
    acc.user = acc.user.saturating_add(other.user);
    acc.interrupt = acc.interrupt.saturating_add(other.interrupt);
}

/// 一次采样：总量与各核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuStat {
    /// 各核逐态求和。
    pub total: CpuTimes,
    /// `(核编号, ticks)`，编号即 `processor_times()` 返回的下标。
    pub cores: Vec<(u32, CpuTimes)>,
}

impl CpuStat {
    /// 由各核 ticks 合成，总量为逐态求和。
    pub fn from_cores(cores: Vec<(u32, CpuTimes)>) -> CpuStat {
        let mut total = CpuTimes::default();
        for (_, t) in &cores {
            add_ticks(&mut total, t);
        }
        CpuStat { total, cores }
    }
}

/// 两轮之间各状态的百分比。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuPercent {
    /// `100 − idle`。
    pub usage: f64,
    /// 内核态，**已扣掉中断**（见模块文档）。
    pub system: f64,
    /// DPC + 硬中断。
    pub irq: f64,
    /// 用户态。
    pub user: f64,
    /// 空闲。
    pub idle: f64,
}

/// 差分求百分比。总 tick 无增长（或倒退）时返回 `None`。
pub fn percent_between(prev: &CpuTimes, cur: &CpuTimes) -> Option<CpuPercent> {
    let total = cur.total().checked_sub(prev.total())?;
    if total == 0 {
        return None;
    }
    let pct =
        |c: u64, p: u64| (c.saturating_sub(p) as f64 * 100.0 / total as f64).clamp(0.0, 100.0);
    let idle = pct(cur.idle, prev.idle);
    let irq = pct(cur.interrupt, prev.interrupt);
    // interrupt ⊂ kernel，扣掉之后才与 Linux 的 system / irq / softirq 三列互斥对齐。
    // 两个计数器不是同一时刻读的，极端情况下相减可能出负，夹到 0。
    let system = (pct(cur.system, prev.system) - irq).max(0.0);
    Some(CpuPercent {
        usage: (100.0 - idle).clamp(0.0, 100.0),
        system,
        irq,
        user: pct(cur.user, prev.user),
        idle,
    })
}

/// CPU 采集器。持有上一轮 tick 用于差分。
pub struct CpuCollector {
    prev: Option<CpuStat>,
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuCollector {
    pub fn new() -> Self {
        CpuCollector { prev: None }
    }

    /// 喂入一次采样，产出与上一轮的差分样本；第一轮返回空。
    pub fn ingest(&mut self, stat: CpuStat) -> Vec<Sample> {
        let mut out = Vec::new();
        if let Some(prev) = &self.prev {
            if let Some(p) = percent_between(&prev.total, &stat.total) {
                out.extend([
                    Sample::new(cat::CPU_USAGE, p.usage),
                    Sample::new(cat::CPU_SYSTEM, p.system),
                    Sample::new(cat::CPU_IRQ, p.irq),
                ]);
            }
            for (pos, (idx, cur)) in stat.cores.iter().enumerate() {
                // 核序两轮通常一致，先按位置试，核数变化后再退化成查找。
                let prev_times = match prev.cores.get(pos) {
                    Some((i, t)) if i == idx => Some(t),
                    _ => prev.cores.iter().find(|(i, _)| i == idx).map(|(_, t)| t),
                };
                let Some(prev_times) = prev_times else {
                    continue;
                };
                let Some(p) = percent_between(prev_times, cur) else {
                    continue;
                };
                out.push(Sample::labeled(
                    cat::CPU_CORE_USAGE,
                    label::CORE,
                    idx.to_string(),
                    p.usage,
                ));
            }
        }
        self.prev = Some(stat);
        out
    }
}

impl Collector for CpuCollector {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let cores = ntdll::processor_times().map_err(|e| {
            CollectError::new(
                self.name(),
                format!("SystemProcessorPerformanceInformation 读取失败: {e}"),
            )
        })?;
        if cores.is_empty() {
            return Err(CollectError::new(self.name(), "内核未返回任何逻辑处理器"));
        }
        let cores = cores
            .into_iter()
            .enumerate()
            .map(|(i, t)| (i as u32, t))
            .collect();
        Ok(self.ingest(CpuStat::from_cores(cores)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ticks(idle: u64, system: u64, user: u64, interrupt: u64) -> CpuTimes {
        CpuTimes {
            idle,
            system,
            user,
            interrupt,
        }
    }

    #[test]
    fn 总量是各核逐态求和() {
        let s = CpuStat::from_cores(vec![(0, ticks(70, 20, 10, 5)), (1, ticks(60, 10, 30, 3))]);
        assert_eq!(s.total, ticks(130, 30, 40, 8));
        // total() 不含 interrupt（它已经算在 system 里）
        assert_eq!(s.total.total(), 200);
    }

    #[test]
    fn 差分百分比三态互斥() {
        // 差分：idle 700、system 200、user 100 → 总 1000；其中 interrupt 50
        let a = ticks(100, 100, 100, 10);
        let b = ticks(800, 300, 200, 60);
        let p = percent_between(&a, &b).unwrap();
        let close = |x: f64, y: f64| (x - y).abs() < 1e-9;
        assert!(close(p.idle, 70.0));
        assert!(close(p.usage, 30.0), "usage = 100 − idle");
        assert!(close(p.irq, 5.0), "irq = Δinterrupt / Δtotal");
        assert!(close(p.system, 15.0), "system 要扣掉 irq：20 − 5");
        assert!(close(p.user, 10.0));
        // 四态之和恒为 100（与 /proc/stat 的恒等式一致）
        let sum = p.idle + p.system + p.irq + p.user;
        assert!(close(sum, 100.0), "四态之和应为 100，实际 {sum}");
    }

    #[test]
    fn 无变化与计数器倒退都返回_none() {
        let a = ticks(100, 100, 100, 10);
        let b = ticks(800, 300, 200, 60);
        assert!(percent_between(&a, &a).is_none(), "无变化");
        assert!(percent_between(&b, &a).is_none(), "计数器倒退");
    }

    #[test]
    fn 中断超过内核时间时_system_夹到零而不是负数() {
        // 采样错位造成的病态输入：Δinterrupt 比 Δsystem 还大
        let a = ticks(0, 0, 0, 0);
        let b = ticks(900, 50, 50, 80);
        let p = percent_between(&a, &b).unwrap();
        assert_eq!(p.system, 0.0, "不该出现负的百分比");
        assert!(p.irq > 0.0);
    }

    #[test]
    fn 采集器两轮差分与每核标签() {
        let a = CpuStat::from_cores(vec![(0, ticks(100, 100, 100, 10))]);
        let b = CpuStat::from_cores(vec![(0, ticks(800, 300, 200, 60))]);
        let mut c = CpuCollector::new();
        assert!(c.ingest(a).is_empty(), "第一轮无基线");
        // 总 3（usage / system / irq）+ 每核 usage × 1
        let out = c.ingest(b);
        assert_eq!(out.len(), 3 + 1);
        let core0 = out
            .iter()
            .find(|s| {
                s.metric == cat::CPU_CORE_USAGE && s.labels == vec![(label::CORE, "0".to_string())]
            })
            .expect("cpu0 的 usage");
        assert!((core0.value - 30.0).abs() < 1e-9);
    }

    #[test]
    fn 核数变化时按编号匹配() {
        let mut c = CpuCollector::new();
        c.ingest(CpuStat::from_cores(vec![
            (0, ticks(100, 0, 0, 0)),
            (1, ticks(100, 0, 0, 0)),
        ]));
        // 第 0 核消失，只剩原来的第 1 核：按位置匹配会张冠李戴，按编号才对
        let out = c.ingest(CpuStat::from_cores(vec![(1, ticks(150, 0, 50, 0))]));
        let core1 = out
            .iter()
            .find(|s| s.labels == vec![(label::CORE, "1".to_string())])
            .expect("cpu1");
        // 差分总量 = 200 − 100 = 100，其中 idle 增了 50 → 非空闲占一半
        assert!(
            (core1.value - 50.0).abs() < 1e-9,
            "按编号匹配到的应是 cpu1 自己的基线"
        );
    }

    #[test]
    fn 本机两轮采集值域合理() {
        let mut c = CpuCollector::new();
        let first = c
            .collect(Instant::now())
            .expect("逐核 CPU 时间在任何 Windows 上都该可读");
        assert!(first.is_empty(), "第一轮无基线");
        std::thread::sleep(Duration::from_millis(200));
        let out = c.collect(Instant::now()).expect("第二轮");

        let get = |m: &str| {
            out.iter()
                .find(|s| s.metric == m)
                .map(|s| s.value)
                .expect(m)
        };
        let usage = get(cat::CPU_USAGE);
        let system = get(cat::CPU_SYSTEM);
        let irq = get(cat::CPU_IRQ);
        assert!(
            usage + 1e-6 >= system + irq,
            "非空闲时间应当装得下内核态与中断：usage={usage} system={system} irq={irq}"
        );
        for s in &out {
            assert!(
                (0.0..=100.0).contains(&s.value),
                "{} = {}",
                s.metric,
                s.value
            );
        }
        let cores = out
            .iter()
            .filter(|s| s.metric == cat::CPU_CORE_USAGE)
            .count();
        assert!(cores >= 1, "至少一个核");
        // Windows 没有这两态，绝不能凭空产出
        for absent in [cat::CPU_IOWAIT, cat::CPU_STEAL] {
            assert!(
                !out.iter().any(|s| s.metric == absent),
                "{absent} 在 Windows 上不该存在"
            );
        }
        eprintln!(
            "本机 CPU：usage={usage:.2}% system={system:.2}% irq={irq:.2}% 核数={cores}"
        );
    }
}
