//! 进程数：进程表里的线程状态 → `procs.running` / `procs.total`。
//!
//! # 为什么没有 `load.1m`
//!
//! **Windows 内核没有负载均值这个概念。** Unix 的 `loadavg` 是内核每 5 秒
//! 对「可运行 + 不可中断等待」的任务数做一次指数移动平均（1/5/15 分钟三个
//! 衰减常数），这个累加器就在内核里，`/proc/loadavg` 只是把它打印出来。
//! Windows 的调度器不维护任何这样的累加器。
//!
//! 常见的冒充办法是 `\System\Processor Queue Length`，那是**错的**：
//!
//! | | Unix `load.1m` | `Processor Queue Length` |
//! |---|---|---|
//! | 时间特性 | 1 分钟指数移动平均 | 采样瞬间的即时值 |
//! | 计入正在运行的线程 | 是 | **否**（只数在排队等 CPU 的） |
//! | 计入 IO 等待 | 是（不可中断等待） | 否 |
//!
//! 把一个瞬时队列长度画在标着「1 分钟负载均值」的位置上，读数的人会按 Unix
//! 的经验去解释它（「超过核数就是过载」），而它的量纲与量级都不是那回事。
//! 自己在用户态维护一个 1 分钟 EMA 也不行：那样得出的数只反映采集器自己的
//! 采样节奏，重启一次就断档，且与任何系统工具都对不上。
//!
//! 所以 `load.1m` 在 Windows 上如实缺席（`design.md` §1 第 2 条）。
//!
//! # `procs.running` 的口径
//!
//! 与 Linux `/proc/stat` 的 `procs_running` 对齐：**可运行的调度实体（线程）数**
//! = `Running`（正占着某颗核）+ `Ready` / `Standby`（在就绪队列里排着）。
//! `procs.total` 是线程总数，同样与 `/proc/loadavg` 第四列的分母对齐——
//! 两者都是线程而不是进程，这是 Linux 侧既有的口径，不在这里改。
//!
//! # 必须排除 pid 0
//!
//! 「System Idle Process」（pid 0）在进程表里有**每颗逻辑处理器一个**空闲线程，
//! 而空闲线程在它那颗核空着的时候状态正是 `Running`。不滤掉它，一台 16 核的
//! 闲置机器会报出 `procs.running ≈ 16`——看上去像满载，实际是满闲，正好反了。
//! Linux 的 `procs_running` 同样不含 per-CPU 的 idle 任务（`swapper/N`）。
//! [`crate::platform::windows::ntdll::system_processes`] 按约定不替调用方做取舍，
//! 这个过滤就落在这里。

use std::time::Instant;

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog as cat;
use crate::platform::windows::ntdll::{self, ProcessEntry};

/// 系统空闲进程的 pid。它的线程不是真实负载，见模块文档。
const IDLE_PID: u32 = 0;

/// 一次线程统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadCounts {
    /// 可运行线程数（`Running` + `Ready` + `Standby`）。
    pub running: u32,
    /// 线程总数。
    pub total: u32,
}

/// 从一张进程表算出线程统计，**排除系统空闲进程**。
///
/// 先让 [`ntdll::runnable_and_total_threads`] 把整张表加一遍，再把空闲进程那一项
/// 减掉——而不是先过滤出一份新表：几百个 `ProcessEntry` 每个都带 `String`，
/// 为了滤掉一行而整表克隆，每 2 秒一次不划算。
pub fn counts_from(procs: &[ProcessEntry]) -> ThreadCounts {
    let (all_running, all_total) = ntdll::runnable_and_total_threads(procs);
    let idle = procs.iter().find(|p| p.pid == IDLE_PID);
    ThreadCounts {
        running: all_running.saturating_sub(idle.map_or(0, |p| p.thread_states.runnable())),
        total: all_total.saturating_sub(idle.map_or(0, |p| p.threads)),
    }
}

/// 进程数采集器。无状态——两条都是瞬时量。
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
        let procs = ntdll::system_processes()
            .map_err(|e| CollectError::new(self.name(), format!("进程表读取失败: {e}")))?;
        if procs.is_empty() {
            return Err(CollectError::new(self.name(), "进程表为空"));
        }
        let c = counts_from(&procs);
        // load.1m 在 Windows 上不存在，见模块文档。
        Ok(vec![
            Sample::new(cat::PROCS_RUNNING, f64::from(c.running)),
            Sample::new(cat::PROCS_TOTAL, f64::from(c.total)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::windows::ntdll::ThreadStates;

    fn entry(pid: u32, running: u32, ready: u32, threads: u32) -> ProcessEntry {
        ProcessEntry {
            pid,
            ppid: 0,
            name: format!("p{pid}.exe"),
            session_id: 0,
            threads,
            thread_states: ThreadStates {
                running,
                ready,
                waiting: threads.saturating_sub(running + ready),
                terminated: 0,
                total: threads,
            },
            start_ts: 0,
            cpu_100ns: 0,
            working_set: 0,
            virtual_size: 0,
            private_bytes: 0,
            base_priority: 8,
            handles: 0,
            read_bytes: 0,
            write_bytes: 0,
        }
    }

    #[test]
    fn 可运行是运行加就绪() {
        let procs = vec![entry(4, 1, 2, 10)];
        assert_eq!(
            counts_from(&procs),
            ThreadCounts {
                running: 3,
                total: 10
            }
        );
    }

    /// 这条是本模块最重要的一条：空闲进程的线程不能算进可运行数。
    #[test]
    fn 系统空闲进程被排除() {
        // 一台 16 核的闲置机器：idle 进程 16 个线程全在 Running
        let procs = vec![entry(IDLE_PID, 16, 0, 16), entry(4, 1, 0, 120)];
        let c = counts_from(&procs);
        assert_eq!(c.running, 1, "闲置机器的可运行线程不该是 16");
        assert_eq!(c.total, 120, "线程总数也不含空闲线程");
    }

    #[test]
    fn 本机采集() {
        let mut c = LoadCollector::new();
        let out = c.collect(Instant::now()).expect("进程表应当可读");
        let get = |m: &str| {
            out.iter()
                .find(|s| s.metric == m)
                .map(|s| s.value)
                .expect(m)
        };
        let running = get(cat::PROCS_RUNNING);
        let total = get(cat::PROCS_TOTAL);
        assert!(total > 50.0, "线程总数只有 {total}，明显不对");
        assert!(running >= 1.0, "至少本测试线程在跑");
        assert!(running <= total);

        // Windows 没有负载均值
        assert!(!out.iter().any(|s| s.metric == cat::LOAD_1M));
        assert!(out.iter().all(|s| s.labels.is_empty()));

        // 对照：不排除空闲进程时可运行数会被抬到接近核数
        let procs = ntdll::system_processes().unwrap();
        let (raw_running, raw_total) = ntdll::runnable_and_total_threads(&procs);
        eprintln!(
            "本机线程：可运行 {running} / 总 {total}（含空闲进程时：{raw_running} / {raw_total}，逻辑处理器 {} 个）",
            std::thread::available_parallelism().map_or(0, std::num::NonZero::get)
        );
    }
}
