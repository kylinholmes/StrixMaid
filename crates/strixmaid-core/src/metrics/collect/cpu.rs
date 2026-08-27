//! CPU：`/proc/stat` 的 `cpu` / `cpuN` 行 → 总 + 每核的 8 态百分比（两轮差分）。
//!
//! `/proc/stat` 的列：`user nice system idle iowait irq softirq steal guest guest_nice`
//! （单位 jiffies）。内核把 `guest` 计在 `user` 里、`guest_nice` 计在 `nice` 里，
//! 求百分比时从 user / nice 中扣除，避免重复计数；8 态之和 = 100。
//! 老内核列数更少，缺的列按 0 处理。

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{CollectError, Collector, Sample, read_text};
use crate::metrics::catalog::{self as cat, label};

const STAT_PATH: &str = "/proc/stat";

/// 一行 `cpu` 的累计 jiffies。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user: u64,
    pub nice: u64,
    pub system: u64,
    pub idle: u64,
    pub iowait: u64,
    pub irq: u64,
    pub softirq: u64,
    pub steal: u64,
    pub guest: u64,
    pub guest_nice: u64,
}

impl CpuTimes {
    /// 从 `cpu` 行去掉标签后的数字字段解析。前四列必须存在，其余缺省为 0。
    pub fn parse_fields<'a>(mut fields: impl Iterator<Item = &'a str>) -> Option<CpuTimes> {
        let mut next = || fields.next().and_then(|f| f.parse::<u64>().ok());
        let user = next()?;
        let nice = next()?;
        let system = next()?;
        let idle = next()?;
        Some(CpuTimes {
            user,
            nice,
            system,
            idle,
            iowait: next().unwrap_or(0),
            irq: next().unwrap_or(0),
            softirq: next().unwrap_or(0),
            steal: next().unwrap_or(0),
            guest: next().unwrap_or(0),
            guest_nice: next().unwrap_or(0),
        })
    }

    /// 8 态之和（guest 已包含在 user / nice 内，不重复相加）。
    pub fn total(&self) -> u64 {
        self.user
            + self.nice
            + self.system
            + self.idle
            + self.iowait
            + self.irq
            + self.softirq
            + self.steal
    }
}

/// 一次 `/proc/stat` 的 CPU 部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuStat {
    /// `cpu` 行（全部核之和）。
    pub total: CpuTimes,
    /// `cpuN` 行，按出现顺序，`(N, times)`。
    pub cores: Vec<(u32, CpuTimes)>,
}

/// 解析 `/proc/stat` 文本。没有 `cpu` 总行时返回 `None`。
pub fn parse_stat(text: &str) -> Option<CpuStat> {
    let mut total = None;
    let mut cores = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(tag) = fields.next() else { continue };
        if tag == "cpu" {
            total = CpuTimes::parse_fields(fields);
        } else if let Some(idx) = tag.strip_prefix("cpu") {
            if let (Ok(idx), Some(t)) = (idx.parse::<u32>(), CpuTimes::parse_fields(fields)) {
                cores.push((idx, t));
            }
        } else if tag == "intr" {
            // cpu 行都在文件头部，看到 intr 就可以停了。
            break;
        }
    }
    Some(CpuStat {
        total: total?,
        cores,
    })
}

/// 两轮之间各状态的百分比。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuPercent {
    /// `100 − idle − iowait`。
    pub usage: f64,
    pub user: f64,
    pub nice: f64,
    pub system: f64,
    pub idle: f64,
    pub iowait: f64,
    pub irq: f64,
    pub softirq: f64,
    pub steal: f64,
}

/// 差分求百分比。总 jiffies 无增长（或倒退）时返回 `None`。
pub fn percent_between(prev: &CpuTimes, cur: &CpuTimes) -> Option<CpuPercent> {
    let total = cur.total().checked_sub(prev.total())?;
    if total == 0 {
        return None;
    }
    let pct =
        |c: u64, p: u64| (c.saturating_sub(p) as f64 * 100.0 / total as f64).clamp(0.0, 100.0);
    let idle = pct(cur.idle, prev.idle);
    let iowait = pct(cur.iowait, prev.iowait);
    Some(CpuPercent {
        usage: (100.0 - idle - iowait).clamp(0.0, 100.0),
        user: pct(
            cur.user.saturating_sub(cur.guest),
            prev.user.saturating_sub(prev.guest),
        ),
        nice: pct(
            cur.nice.saturating_sub(cur.guest_nice),
            prev.nice.saturating_sub(prev.guest_nice),
        ),
        system: pct(cur.system, prev.system),
        idle,
        iowait,
        irq: pct(cur.irq, prev.irq),
        softirq: pct(cur.softirq, prev.softirq),
        steal: pct(cur.steal, prev.steal),
    })
}

/// CPU 采集器。
pub struct CpuCollector {
    prev: Option<CpuStat>,
    /// 每核是否也产出 8 态（默认开，对应 design.md §7.1「总 + 每核」）。
    /// 关掉后每核只剩 `cpu.core.usage`，大核数机器可借此把 series 数砍到 1/9。
    per_core_states: bool,
    path: PathBuf,
}

impl Default for CpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuCollector {
    /// 读 `/proc/stat`。
    pub fn new() -> Self {
        CpuCollector {
            prev: None,
            per_core_states: true,
            path: PathBuf::from(STAT_PATH),
        }
    }

    /// 是否为每个核产出全部 8 态。
    #[must_use]
    pub fn per_core_states(mut self, on: bool) -> Self {
        self.per_core_states = on;
        self
    }

    /// 改读别的文件（测试用）。
    #[must_use]
    pub fn with_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.path = path.into();
        self
    }

    /// 喂入一次解析结果，产出与上一轮的差分样本；第一轮返回空。
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
                    Sample::new(cat::CPU_IOWAIT, p.iowait),
                    Sample::new(cat::CPU_IRQ, p.irq),
                    Sample::new(cat::CPU_SOFTIRQ, p.softirq),
                    Sample::new(cat::CPU_STEAL, p.steal),
                ]);
            }
            for (pos, (idx, cur)) in stat.cores.iter().enumerate() {
                // 核的顺序通常两轮一致，先按位置试，热插拔后再退化成查找。
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
                        s(cat::CPU_CORE_IOWAIT, p.iowait),
                        s(cat::CPU_CORE_IRQ, p.irq),
                        s(cat::CPU_CORE_SOFTIRQ, p.softirq),
                        s(cat::CPU_CORE_STEAL, p.steal),
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
        let text =
            read_text(&self.path).map_err(|e| CollectError::io(self.name(), &self.path, &e))?;
        let stat = parse_stat(&text).ok_or_else(|| {
            CollectError::new(
                self.name(),
                format!("{} 里没有 cpu 行", Path::new(&self.path).display()),
            )
        })?;
        Ok(self.ingest(stat))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SAMPLE_A: &str = "\
cpu  100 10 100 700 50 0 20 20 0 0
cpu0 50 5 50 350 25 0 10 10 0 0
cpu1 50 5 50 350 25 0 10 10 0 0
intr 12345 0 0
ctxt 999
";
    const SAMPLE_B: &str = "\
cpu  300 10 200 1400 100 0 30 60 0 0
cpu0 150 5 100 700 50 0 15 30 0 0
cpu1 150 5 100 700 50 0 15 30 0 0
intr 12345 0 0
";

    #[test]
    fn 解析固定文本() {
        let s = parse_stat(SAMPLE_A).unwrap();
        assert_eq!(s.total.user, 100);
        assert_eq!(s.total.iowait, 50);
        assert_eq!(s.total.total(), 1000);
        assert_eq!(s.cores.len(), 2);
        assert_eq!(s.cores[1].0, 1);
        assert!(parse_stat("intr 1\nctxt 2\n").is_none(), "没有 cpu 行");
        // 老内核只有 4 列
        let old = parse_stat("cpu  1 2 3 4\n").unwrap();
        assert_eq!(
            old.total,
            CpuTimes {
                user: 1,
                nice: 2,
                system: 3,
                idle: 4,
                ..Default::default()
            }
        );
    }

    #[test]
    fn 差分百分比() {
        // 差分：user 200, nice 0, system 100, idle 700, iowait 50, irq 0, softirq 10, steal 40 → 总 1100
        let a = parse_stat(SAMPLE_A).unwrap();
        let b = parse_stat(SAMPLE_B).unwrap();
        let p = percent_between(&a.total, &b.total).unwrap();
        let close = |x: f64, y: f64| (x - y).abs() < 1e-9;
        assert!(close(p.user, 200.0 / 11.0));
        assert!(close(p.system, 100.0 / 11.0));
        assert!(close(p.idle, 700.0 / 11.0));
        assert!(close(p.iowait, 50.0 / 11.0));
        assert!(close(p.softirq, 10.0 / 11.0));
        assert!(close(p.steal, 40.0 / 11.0));
        assert!(close(p.usage, 100.0 - 750.0 / 11.0));
        let sum = p.user + p.nice + p.system + p.idle + p.iowait + p.irq + p.softirq + p.steal;
        assert!(close(sum, 100.0), "8 态之和应为 100，实际 {sum}");
        // 无变化 → None
        assert!(percent_between(&a.total, &a.total).is_none());
    }

    #[test]
    fn guest_从_user_中扣除() {
        let prev = CpuTimes::default();
        let cur = CpuTimes {
            user: 300,
            idle: 700,
            guest: 100,
            ..Default::default()
        };
        let p = percent_between(&prev, &cur).unwrap();
        assert!((p.user - 20.0).abs() < 1e-9);
        assert!((p.idle - 70.0).abs() < 1e-9);
        assert!((p.usage - 30.0).abs() < 1e-9, "usage 不受 guest 扣除影响");
    }

    #[test]
    fn 采集器两轮差分与每核标签() {
        let mut c = CpuCollector::new();
        assert!(
            c.ingest(parse_stat(SAMPLE_A).unwrap()).is_empty(),
            "第一轮无基线"
        );
        let out = c.ingest(parse_stat(SAMPLE_B).unwrap());
        // 总 9 + 每核 9 × 2
        assert_eq!(out.len(), 9 + 18);
        let core1 = out
            .iter()
            .find(|s| {
                s.metric == cat::CPU_CORE_USAGE && s.labels == vec![(label::CORE, "1".to_string())]
            })
            .expect("cpu1 的 usage");
        assert!(core1.value > 0.0 && core1.value <= 100.0);

        let mut lean = CpuCollector::new().per_core_states(false);
        lean.ingest(parse_stat(SAMPLE_A).unwrap());
        assert_eq!(lean.ingest(parse_stat(SAMPLE_B).unwrap()).len(), 9 + 2);
    }

    #[test]
    fn 本机两轮采集值域合理() {
        let mut c = CpuCollector::new();
        let first = c.collect(Instant::now()).expect("读 /proc/stat");
        assert!(first.is_empty());
        std::thread::sleep(Duration::from_millis(150));
        let out = c.collect(Instant::now()).expect("读 /proc/stat");
        let get = |m: &str| {
            out.iter()
                .find(|s| s.metric == m)
                .map(|s| s.value)
                .expect(m)
        };
        let states = [
            cat::CPU_USER,
            cat::CPU_NICE,
            cat::CPU_SYSTEM,
            cat::CPU_IDLE,
            cat::CPU_IOWAIT,
            cat::CPU_IRQ,
            cat::CPU_SOFTIRQ,
            cat::CPU_STEAL,
        ];
        let sum: f64 = states.iter().map(|m| get(m)).sum();
        assert!((sum - 100.0).abs() < 0.5, "8 态之和 ≈ 100，实际 {sum}");
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
    }
}
