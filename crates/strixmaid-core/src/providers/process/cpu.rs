//! 进程 CPU% 的差分计算。
//!
//! `/proc/<pid>/stat` 只有累计 tick（`utime + stime`），要得到「最近一段时间的占用率」
//! 必须保留上一轮快照：`Δticks / hz / Δ墙钟 × 100`。第一次观察某个 pid 时没有基线，
//! 返回 `None`（调用方填 0.0，与 types 文档一致）。
//!
//! 快照按 `(pid, starttime)` 匹配——pid 被复用时 starttime 必然不同，不会把新进程的
//! tick 减去旧进程的基线。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// 两次采样间隔短于此值时不更新基线、沿用上次结果：tick 是 10ms 粒度，
/// 几十毫秒内的差分只会得到 0% / 100% 这类噪声。
pub const MIN_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone)]
struct Sample {
    starttime: u64,
    ticks: u64,
    at: Instant,
    last_percent: Option<f64>,
}

/// 所有进程的 CPU 快照。
#[derive(Debug, Default)]
pub struct CpuSamples {
    samples: HashMap<u32, Sample>,
}

impl CpuSamples {
    pub fn new() -> Self {
        Self::default()
    }

    /// 是否已经有过一轮采样（即下一轮能算出非零 CPU%）。
    pub fn has_baseline(&self) -> bool {
        !self.samples.is_empty()
    }

    /// 记录一次观察，返回本轮 CPU%。首次观察到该 `(pid, starttime)` 时返回 `None`。
    pub fn observe(
        &mut self,
        pid: u32,
        starttime: u64,
        ticks: u64,
        now: Instant,
        hz: u64,
    ) -> Option<f64> {
        match self.samples.get_mut(&pid) {
            Some(s) if s.starttime == starttime => {
                let elapsed = now.saturating_duration_since(s.at);
                if elapsed < MIN_INTERVAL {
                    return s.last_percent;
                }
                let pct = cpu_percent(ticks.saturating_sub(s.ticks), elapsed, hz);
                s.ticks = ticks;
                s.at = now;
                s.last_percent = Some(pct);
                Some(pct)
            }
            _ => {
                self.samples.insert(
                    pid,
                    Sample {
                        starttime,
                        ticks,
                        at: now,
                        last_percent: None,
                    },
                );
                None
            }
        }
    }

    /// 清理本轮没见到的 pid（进程已退出）。
    pub fn retain_seen(&mut self, seen: &HashSet<u32>) {
        self.samples.retain(|pid, _| seen.contains(pid));
    }

    /// 当前跟踪的 pid 数。
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// `Δticks / hz / Δ秒 × 100`。单核跑满 = 100，多核可超过 100。
pub fn cpu_percent(delta_ticks: u64, elapsed: Duration, hz: u64) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 || hz == 0 {
        return 0.0;
    }
    let cpu_secs = delta_ticks as f64 / hz as f64;
    let pct = cpu_secs / secs * 100.0;
    // 两位小数足够展示，也避免 JSON 里一串 33.333333333
    (pct * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ: u64 = 100;

    #[test]
    fn 两轮快照差分() {
        let mut s = CpuSamples::new();
        let t0 = Instant::now();
        // 第一轮：无基线
        assert_eq!(s.observe(42, 1000, 500, t0, HZ), None);
        assert!(s.has_baseline());
        // 1 秒后多用了 50 tick = 0.5 CPU 秒 → 50%
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(s.observe(42, 1000, 550, t1, HZ), Some(50.0));
        // 再 2 秒用了 400 tick = 4 CPU 秒 → 200%（多核）
        let t2 = t1 + Duration::from_secs(2);
        assert_eq!(s.observe(42, 1000, 950, t2, HZ), Some(200.0));
    }

    #[test]
    fn 采样过密时沿用上次结果且不动基线() {
        let mut s = CpuSamples::new();
        let t0 = Instant::now();
        s.observe(1, 7, 0, t0, HZ);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(s.observe(1, 7, 100, t1, HZ), Some(100.0));
        // 50ms 后再问：返回上次的 100%，基线仍在 t1
        let t2 = t1 + Duration::from_millis(50);
        assert_eq!(s.observe(1, 7, 105, t2, HZ), Some(100.0));
        // 1 秒后：相对 t1 的差分 = 100 tick / 1s
        let t3 = t1 + Duration::from_secs(1);
        assert_eq!(s.observe(1, 7, 200, t3, HZ), Some(100.0));
    }

    #[test]
    fn pid_复用时重置基线() {
        let mut s = CpuSamples::new();
        let t0 = Instant::now();
        s.observe(9, 100, 9_000_000, t0, HZ);
        // 同一 pid、不同 starttime → 新进程，首次观察
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(s.observe(9, 200, 10, t1, HZ), None);
        let t2 = t1 + Duration::from_secs(1);
        assert_eq!(s.observe(9, 200, 20, t2, HZ), Some(10.0));
    }

    #[test]
    fn 清理消失的_pid() {
        let mut s = CpuSamples::new();
        let t0 = Instant::now();
        s.observe(1, 1, 0, t0, HZ);
        s.observe(2, 1, 0, t0, HZ);
        s.observe(3, 1, 0, t0, HZ);
        s.retain_seen(&HashSet::from([1, 3]));
        assert_eq!(s.len(), 2);
        // pid 2 再出现时是首次观察
        assert_eq!(s.observe(2, 1, 0, t0 + Duration::from_secs(1), HZ), None);
    }

    #[test]
    fn 百分比计算边界() {
        assert_eq!(cpu_percent(0, Duration::from_secs(1), HZ), 0.0);
        assert_eq!(cpu_percent(100, Duration::ZERO, HZ), 0.0);
        assert_eq!(cpu_percent(100, Duration::from_secs(1), 0), 0.0);
        assert_eq!(cpu_percent(1, Duration::from_secs(3), HZ), 0.33);
    }
}
