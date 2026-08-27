//! CPU：mach `host_processor_info(PROCESSOR_CPU_LOAD_INFO)` → 总 + 每核的 4 态百分比。
//!
//! # 与 Linux 的差异
//!
//! `/proc/stat` 给 8 态，mach 只给 **user / system / idle / nice** 四态
//! （`CPU_STATE_MAX == 4`）：没有 iowait、irq、softirq、steal。这不是采集方式的取舍，
//! XNU 内核就没有分别统计这几类时间。因此本采集器**只产出这四态加 usage**，
//! `cpu.iowait` / `cpu.irq` / `cpu.softirq` / `cpu.steal` 在 macOS 上不存在。
//!
//! 连带影响 usage 的定义：Linux 是 `100 − idle − iowait`，这里是 `100 − idle`。
//! 两者在各自平台上都表示「非空闲时间占比」，语义一致。
//!
//! # 没有「总」这一行
//!
//! mach 只按核返回，没有 `/proc/stat` 的 `cpu` 汇总行。总量由本模块把各核的
//! **原始 tick 逐态相加**得到，再走同一套差分——不是「各核百分比取平均」。
//! 两者在核数不变时等价，但前者在热插拔（VM 里加减 vCPU）时不会算错。

use std::time::Instant;

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog::{self as cat, label};

/// mach 的四个 CPU 状态，索引即 `CPU_STATE_*` 常量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub user: u64,
    pub system: u64,
    pub idle: u64,
    pub nice: u64,
}

impl CpuTicks {
    /// 四态之和。
    pub fn total(&self) -> u64 {
        self.user + self.system + self.idle + self.nice
    }

    /// 逐态相加，用于把各核合成总量。
    fn add(&mut self, other: &CpuTicks) {
        self.user += other.user;
        self.system += other.system;
        self.idle += other.idle;
        self.nice += other.nice;
    }
}

/// 一次采样：总量与各核。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuStat {
    pub total: CpuTicks,
    /// 按 mach 返回的顺序，`(核编号, ticks)`。
    pub cores: Vec<(u32, CpuTicks)>,
}

impl CpuStat {
    /// 由各核 ticks 合成，总量为逐态求和。
    pub fn from_cores(cores: Vec<(u32, CpuTicks)>) -> CpuStat {
        let mut total = CpuTicks::default();
        for (_, t) in &cores {
            total.add(t);
        }
        CpuStat { total, cores }
    }
}

/// 两轮之间各状态的百分比。字段与 Linux 版同名的部分含义相同。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuPercent {
    /// `100 − idle`。
    pub usage: f64,
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub idle: f64,
}

/// 差分求百分比。总 tick 无增长（或倒退）时返回 `None`。
pub fn percent_between(prev: &CpuTicks, cur: &CpuTicks) -> Option<CpuPercent> {
    let total = cur.total().checked_sub(prev.total())?;
    if total == 0 {
        return None;
    }
    let pct =
        |c: u64, p: u64| (c.saturating_sub(p) as f64 * 100.0 / total as f64).clamp(0.0, 100.0);
    let idle = pct(cur.idle, prev.idle);
    Some(CpuPercent {
        usage: (100.0 - idle).clamp(0.0, 100.0),
        user: pct(cur.user, prev.user),
        nice: pct(cur.nice, prev.nice),
        system: pct(cur.system, prev.system),
        idle,
    })
}

/// 向 mach 要一次逐核 CPU tick。
///
/// `host_processor_info` 在内核里**分配**一块 vm 内存交给调用方，必须用
/// `vm_deallocate` 归还——用 `free` 或干脆不还都是错的（后者是每 2 秒泄漏一次的内存泄漏）。
pub fn read_processor_info() -> Option<CpuStat> {
    let mut ncpu: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = std::ptr::null_mut();
    let mut count: libc::mach_msg_type_number_t = 0;

    // SAFETY: 三个 out 参数都指向本栈帧上合法的可写内存。调用成功后 info 指向内核分配的
    // count 个 integer_t，所有权转移给我们，下面负责 vm_deallocate。
    let rc = unsafe {
        libc::host_processor_info(
            mach2::mach_init::mach_host_self(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &raw mut ncpu,
            &raw mut info,
            &raw mut count,
        )
    };
    if rc != libc::KERN_SUCCESS || info.is_null() {
        return None;
    }

    let per_cpu = libc::CPU_STATE_MAX as usize;
    let expected = ncpu as usize * per_cpu;
    let mut cores = Vec::with_capacity(ncpu as usize);
    if count as usize >= expected {
        for i in 0..ncpu as usize {
            // SAFETY: 上面确认了 count >= ncpu * CPU_STATE_MAX，故 i*per_cpu+3 在界内。
            let at = |state: libc::c_int| unsafe {
                *info.add(i * per_cpu + state as usize) as u32 as u64
            };
            cores.push((
                i as u32,
                CpuTicks {
                    user: at(libc::CPU_STATE_USER),
                    system: at(libc::CPU_STATE_SYSTEM),
                    idle: at(libc::CPU_STATE_IDLE),
                    nice: at(libc::CPU_STATE_NICE),
                },
            ));
        }
    }

    // SAFETY: info / count 正是上面 host_processor_info 返回的那一对，只归还一次。
    unsafe {
        libc::vm_deallocate(
            mach2::traps::mach_task_self(),
            info as libc::vm_address_t,
            count as libc::vm_size_t * std::mem::size_of::<libc::integer_t>(),
        );
    }

    (!cores.is_empty()).then(|| CpuStat::from_cores(cores))
}

/// CPU 采集器。
pub struct CpuCollector {
    prev: Option<CpuStat>,
    /// 每核是否也产出全部状态；关掉后每核只剩 `cpu.core.usage`（design.md §7.2）。
    per_core_states: bool,
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuCollector {
    pub fn new() -> Self {
        CpuCollector {
            prev: None,
            per_core_states: true,
        }
    }

    /// 是否为每个核产出全部状态。
    #[must_use]
    pub fn per_core_states(mut self, on: bool) -> Self {
        self.per_core_states = on;
        self
    }

    /// 喂入一次采样，产出与上一轮的差分样本；第一轮返回空。
    pub fn ingest(&mut self, stat: CpuStat) -> Vec<Sample> {
        let mut out = Vec::new();
        if let Some(prev) = &self.prev {
            if let Some(p) = percent_between(&prev.total, &stat.total) {
                out.extend([
                    Sample::new(cat::CPU_USAGE, p.usage),
                    Sample::new(cat::CPU_USER, p.user),
                    Sample::new(cat::CPU_NICE, p.nice),
                    Sample::new(cat::CPU_SYSTEM, p.system),
                    Sample::new(cat::CPU_IDLE, p.idle),
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
                let core = idx.to_string();
                let s = |metric, v| Sample::labeled(metric, label::CORE, core.clone(), v);
                out.push(s(cat::CPU_CORE_USAGE, p.usage));
                if self.per_core_states {
                    out.extend([
                        s(cat::CPU_CORE_USER, p.user),
                        s(cat::CPU_CORE_NICE, p.nice),
                        s(cat::CPU_CORE_SYSTEM, p.system),
                        s(cat::CPU_CORE_IDLE, p.idle),
                    ]);
                }
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
        let stat = read_processor_info().ok_or_else(|| {
            CollectError::new(self.name(), "host_processor_info 调用失败或未返回任何核")
        })?;
        Ok(self.ingest(stat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn ticks(user: u64, system: u64, idle: u64, nice: u64) -> CpuTicks {
        CpuTicks {
            user,
            system,
            idle,
            nice,
        }
    }

    #[test]
    fn 总量是各核逐态求和() {
        let s = CpuStat::from_cores(vec![(0, ticks(10, 20, 70, 0)), (1, ticks(30, 10, 60, 0))]);
        assert_eq!(s.total, ticks(40, 30, 130, 0));
        assert_eq!(s.total.total(), 200);
    }

    #[test]
    fn 差分百分比() {
        let a = ticks(100, 100, 700, 100);
        // 差分：user 200, system 100, idle 700, nice 0 → 总 1000
        let b = ticks(300, 200, 1400, 100);
        let p = percent_between(&a, &b).unwrap();
        let close = |x: f64, y: f64| (x - y).abs() < 1e-9;
        assert!(close(p.user, 20.0));
        assert!(close(p.system, 10.0));
        assert!(close(p.idle, 70.0));
        assert!(close(p.nice, 0.0));
        assert!(close(p.usage, 30.0), "usage = 100 − idle");
        let sum = p.user + p.system + p.idle + p.nice;
        assert!(close(sum, 100.0), "四态之和应为 100，实际 {sum}");
        assert!(percent_between(&a, &a).is_none(), "无变化");
        assert!(percent_between(&b, &a).is_none(), "计数器倒退");
    }

    #[test]
    fn 采集器两轮差分与每核标签() {
        let a = CpuStat::from_cores(vec![(0, ticks(100, 100, 700, 100))]);
        let b = CpuStat::from_cores(vec![(0, ticks(300, 200, 1400, 100))]);
        let mut c = CpuCollector::new();
        assert!(c.ingest(a.clone()).is_empty(), "第一轮无基线");
        // 总 5 + 每核 5
        let out = c.ingest(b.clone());
        assert_eq!(out.len(), 5 + 5);
        let core0 = out
            .iter()
            .find(|s| {
                s.metric == cat::CPU_CORE_USAGE && s.labels == vec![(label::CORE, "0".to_string())]
            })
            .expect("cpu0 的 usage");
        assert!((core0.value - 30.0).abs() < 1e-9);

        let mut lean = CpuCollector::new().per_core_states(false);
        lean.ingest(a);
        assert_eq!(lean.ingest(b).len(), 5 + 1);
    }

    #[test]
    fn 核数变化时按编号匹配() {
        let mut c = CpuCollector::new().per_core_states(false);
        c.ingest(CpuStat::from_cores(vec![
            (0, ticks(0, 0, 100, 0)),
            (1, ticks(0, 0, 100, 0)),
        ]));
        // 第 0 核消失，只剩原来的第 1 核：按位置匹配会张冠李戴，按编号才对
        let out = c.ingest(CpuStat::from_cores(vec![(1, ticks(50, 0, 150, 0))]));
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
        let first = c.collect(Instant::now()).expect("host_processor_info");
        assert!(first.is_empty(), "第一轮无基线");
        std::thread::sleep(Duration::from_millis(150));
        let out = c.collect(Instant::now()).expect("host_processor_info");
        let get = |m: &str| {
            out.iter()
                .find(|s| s.metric == m)
                .map(|s| s.value)
                .expect(m)
        };
        let sum: f64 = [cat::CPU_USER, cat::CPU_NICE, cat::CPU_SYSTEM, cat::CPU_IDLE]
            .iter()
            .map(|m| get(m))
            .sum();
        assert!((sum - 100.0).abs() < 0.5, "四态之和 ≈ 100，实际 {sum}");
        for s in &out {
            assert!(
                (0.0..=100.0).contains(&s.value),
                "{} = {}",
                s.metric,
                s.value
            );
        }
        assert!(
            out.iter().any(|s| s.metric == cat::CPU_CORE_USAGE),
            "至少一个核"
        );
        // macOS 没有这四态，绝不能凭空产出
        for absent in [
            cat::CPU_IOWAIT,
            cat::CPU_IRQ,
            cat::CPU_SOFTIRQ,
            cat::CPU_STEAL,
        ] {
            assert!(
                !out.iter().any(|s| s.metric == absent),
                "{absent} 在 macOS 上不该存在"
            );
        }
    }
}
