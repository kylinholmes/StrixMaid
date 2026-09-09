//! 进程磁盘 IO 速率的差分计算，套路同 [`super::cpu`]：
//! 内核只给累计字节数（Linux `/proc/<pid>/io`，macOS `proc_pid_rusage`），
//! 速率 = Δbytes / Δ墙钟。基线按 `(pid, starttime)` 匹配，pid 复用不会串。
//!
//! 与 CPU% 的一个差别：CPU% 测不到就是 0，而 IO 计数器**可能整个读不到**
//! （无权限）。「读不到」由调用方直接映射成 `None`，不进本模块——这里只处理
//! 读得到的差分。

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use super::cpu::MIN_INTERVAL;

#[derive(Debug, Clone)]
struct Sample {
    starttime: u64,
    read: u64,
    write: u64,
    at: Instant,
    last: Option<(f64, f64)>,
}

/// 所有进程的 IO 计数快照。
#[derive(Debug, Default)]
pub struct IoSamples {
    samples: HashMap<u32, Sample>,
}

impl IoSamples {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次观察，返回（读速率, 写速率），bytes/s。
    /// 首次观察到该 `(pid, starttime)` 时返回 `None`（调用方按语义填 `Some(0.0)`）。
    pub fn observe(
        &mut self,
        pid: u32,
        starttime: u64,
        read: u64,
        write: u64,
        now: Instant,
    ) -> Option<(f64, f64)> {
        match self.samples.get_mut(&pid) {
            Some(s) if s.starttime == starttime => {
                let elapsed = now.saturating_duration_since(s.at);
                if elapsed < MIN_INTERVAL {
                    return s.last;
                }
                let secs = elapsed.as_secs_f64();
                let rate = |delta: u64| (delta as f64 / secs).round();
                let r = rate(read.saturating_sub(s.read));
                let w = rate(write.saturating_sub(s.write));
                s.read = read;
                s.write = write;
                s.at = now;
                s.last = Some((r, w));
                Some((r, w))
            }
            _ => {
                self.samples.insert(
                    pid,
                    Sample {
                        starttime,
                        read,
                        write,
                        at: now,
                        last: None,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 两轮差分出速率() {
        let mut s = IoSamples::new();
        let t0 = Instant::now();
        assert_eq!(s.observe(1, 100, 1000, 0, t0), None);
        let t1 = t0 + Duration::from_secs(2);
        // 2 秒读了 4096 字节 → 2048 B/s
        assert_eq!(s.observe(1, 100, 5096, 512, t1), Some((2048.0, 256.0)));
    }

    #[test]
    fn 采样过密沿用上次() {
        let mut s = IoSamples::new();
        let t0 = Instant::now();
        s.observe(1, 7, 0, 0, t0);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(s.observe(1, 7, 100, 0, t1), Some((100.0, 0.0)));
        let t2 = t1 + Duration::from_millis(50);
        assert_eq!(s.observe(1, 7, 999, 0, t2), Some((100.0, 0.0)));
    }

    #[test]
    fn pid_复用重置基线() {
        let mut s = IoSamples::new();
        let t0 = Instant::now();
        s.observe(9, 100, 1_000_000, 0, t0);
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(s.observe(9, 200, 10, 0, t1), None);
    }

    #[test]
    fn 清理消失的_pid() {
        let mut s = IoSamples::new();
        let t0 = Instant::now();
        s.observe(1, 1, 0, 0, t0);
        s.observe(2, 1, 0, 0, t0);
        s.retain_seen(&HashSet::from([1]));
        let t1 = t0 + Duration::from_secs(1);
        // pid 2 再出现是首次观察
        assert_eq!(s.observe(2, 1, 100, 0, t1), None);
    }
}
