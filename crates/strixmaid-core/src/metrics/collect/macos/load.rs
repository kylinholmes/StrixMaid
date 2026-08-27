//! 负载：`getloadavg(3)` + 由 `sysctl KERN_PROC_ALL` 的返回长度推算的进程数。
//!
//! # 为什么没有 `procs.running`
//!
//! `/proc/loadavg` 第四个字段是 `运行中/总数`，一次读文件就有。XNU 不导出运行队列长度：
//! 要拿到「此刻有多少线程可运行」得遍历全部任务的全部线程逐个看调度状态，
//! 代价与收益完全不成比例。故本采集器只产出 `procs.total`。
//!
//! # 进程数的取法
//!
//! 不真的把 pid 列表读出来——`proc_listpids` 在 `buffer = NULL` 时只返回
//! 「装得下需要多少字节」，除以 `pid_t` 的大小即得进程数。一次系统调用，不拷贝数据。
//! 这个数字会随两次调用之间进程的增减而抖动，作为一条曲线足够。
//!
//! 没走 `sysctl KERN_PROC_ALL` 的同款技巧，是因为 `libc` 没有为 Apple 目标声明
//! `kinfo_proc`，那样就得自己复刻一个几百字节、随系统版本变动的结构体只为取它的
//! `size_of`——`proc_listpids` 的元素是 `pid_t`，大小是确定的 4 字节。

use std::time::Instant;

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog as cat;

/// 三个负载均值。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LoadAvg {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

/// `getloadavg(3)`。返回值是实际填充的个数，不足 3 个视为失败。
pub fn read_loadavg() -> Option<LoadAvg> {
    let mut buf = [0f64; 3];
    // SAFETY: buf 是 3 个 c_double 的可写数组，nelem 如实描述其长度。
    let n = unsafe { libc::getloadavg(buf.as_mut_ptr(), 3) };
    (n == 3).then_some(LoadAvg {
        one: buf[0],
        five: buf[1],
        fifteen: buf[2],
    })
}

/// `libproc.h` 的 `PROC_ALL_PIDS`：列出全部 pid，不按类型过滤。
///
/// `libc` 没有为 Apple 目标导出这个常量，值取自 SDK 头文件
/// `/usr/include/libproc.h`（`sys/proc_info.h` 里的 `PROC_ALL_PIDS = 1`）。
const PROC_ALL_PIDS: u32 = 1;

/// 当前进程数。读不到返回 `None`。
pub fn read_proc_count() -> Option<u64> {
    // SAFETY: buffer 为空、buffersize 为 0 时 proc_listpids 不写任何内存，
    // 只返回「装下全部 pid 需要多少字节」。
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return None;
    }
    Some(bytes as u64 / std::mem::size_of::<libc::pid_t>() as u64)
}

/// 负载采集器。无状态。
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadCollector;

impl LoadCollector {
    pub fn new() -> Self {
        LoadCollector
    }
}

impl Collector for LoadCollector {
    fn name(&self) -> &'static str {
        "load"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let load = read_loadavg()
            .ok_or_else(|| CollectError::new(self.name(), "getloadavg 未返回三个值"))?;
        let mut out = vec![
            Sample::new(cat::LOAD_1M, load.one),
            Sample::new(cat::LOAD_5M, load.five),
            Sample::new(cat::LOAD_15M, load.fifteen),
        ];
        // 进程数读不到只是少一条曲线，不该让整轮失败。
        if let Some(n) = read_proc_count() {
            out.push(Sample::new(cat::PROCS_TOTAL, n as f64));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 本机负载() {
        let l = read_loadavg().expect("getloadavg");
        for v in [l.one, l.five, l.fifteen] {
            assert!(v.is_finite() && v >= 0.0, "负载不应为负或非有限值：{v}");
        }
    }

    #[test]
    fn 本机进程数() {
        let n = read_proc_count().expect("KERN_PROC_ALL");
        assert!(n > 1, "至少有 launchd 与本测试进程，实际 {n}");
    }

    #[test]
    fn 采集一轮() {
        let mut c = LoadCollector::new();
        let out = c.collect(Instant::now()).unwrap();
        assert!(out.iter().any(|s| s.metric == cat::LOAD_1M));
        assert!(out.iter().any(|s| s.metric == cat::PROCS_TOTAL));
        // XNU 不导出运行队列长度
        assert!(!out.iter().any(|s| s.metric == cat::PROCS_RUNNING));
    }
}
