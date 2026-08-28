//! 负载与进程数：`/proc/loadavg`。
//!
//! 格式：`load1 load5 load15 running/total last_pid`。第四列的两个数分别是
//! 可运行的调度实体数与系统内调度实体（线程）总数，不是「进程数」——
//! 但这就是 `/proc/loadavg` 能给出的、也是 top/uptime 展示的数。
//!
//! `load.5m` / `load.15m` 不入库（roadmap/08 §4.3）：它们本来就是 `load.1m` 的
//! 移动平均——内核替我们平滑，是因为 `uptime` 没有历史；而我们存着五层完整
//! 曲线，趋势直接看图。[`parse_loadavg`] 仍解析全部三个值，裁剪只在产出处。

use std::path::PathBuf;
use std::time::Instant;

use super::{CollectError, Collector, Sample, read_text};
use crate::metrics::catalog as cat;

const LOADAVG_PATH: &str = "/proc/loadavg";

/// `/proc/loadavg` 解析结果。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAvg {
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub running: u64,
    pub total: u64,
}

/// 解析。字段不全或格式不对返回 `None`。
pub fn parse_loadavg(text: &str) -> Option<LoadAvg> {
    let mut it = text.split_whitespace();
    let load1 = it.next()?.parse().ok()?;
    let load5 = it.next()?.parse().ok()?;
    let load15 = it.next()?.parse().ok()?;
    let (running, total) = it.next()?.split_once('/')?;
    Some(LoadAvg {
        load1,
        load5,
        load15,
        running: running.parse().ok()?,
        total: total.parse().ok()?,
    })
}

/// 负载采集器（无状态）。
pub struct LoadCollector {
    path: PathBuf,
}

impl Default for LoadCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl LoadCollector {
    /// 读 `/proc/loadavg`。
    pub fn new() -> Self {
        LoadCollector {
            path: PathBuf::from(LOADAVG_PATH),
        }
    }
}

impl Collector for LoadCollector {
    fn name(&self) -> &'static str {
        "load"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let text =
            read_text(&self.path).map_err(|e| CollectError::io(self.name(), &self.path, &e))?;
        let l = parse_loadavg(&text).ok_or_else(|| {
            CollectError::new(self.name(), format!("无法解析 /proc/loadavg: {text:?}"))
        })?;
        Ok(vec![
            Sample::new(cat::LOAD_1M, l.load1),
            Sample::new(cat::PROCS_RUNNING, l.running as f64),
            Sample::new(cat::PROCS_TOTAL, l.total as f64),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析() {
        let l = parse_loadavg("25.58 27.17 25.84 23/25130 1242712\n").unwrap();
        assert_eq!(l.load1, 25.58);
        assert_eq!(l.load15, 25.84);
        assert_eq!(l.running, 23);
        assert_eq!(l.total, 25130);
        assert!(parse_loadavg("1.0 2.0\n").is_none());
        assert!(parse_loadavg("1.0 2.0 3.0 x 5\n").is_none());
    }

    #[test]
    fn 本机值域合理() {
        let out = LoadCollector::new()
            .collect(Instant::now())
            .expect("读 /proc/loadavg");
        assert_eq!(out.len(), 3);
        for s in &out {
            assert!(s.value >= 0.0, "{} = {}", s.metric, s.value);
        }
        let get = |k: &str| out.iter().find(|x| x.metric == k).unwrap().value;
        assert!(get(cat::PROCS_TOTAL) >= 1.0);
        assert!(get(cat::PROCS_RUNNING) <= get(cat::PROCS_TOTAL));
    }
}
