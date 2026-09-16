//! Windows 采集器：`NtQuerySystemInformation`、`GlobalMemoryStatusEx`、
//! `GetIfTable2`、`IOCTL_DISK_PERFORMANCE`、PDH。
//!
//! # 定位
//!
//! 与 [`super::macos`] 不同，Windows 不只是开发平台——`design.md` §2.1 的产物里
//! 有 Windows 侧的 agent，所以这里的口径要经得起当真。原则仍是那一条
//! （`design.md` §1 第 2 条）：**能力探测而非硬依赖，缺什么如实缺席，
//! 绝不拿相近的东西冒充**。
//!
//! # 与 Linux 实现的覆盖差异
//!
//! | 采集项 | Windows | 说明 |
//! |---|---|---|
//! | CPU | ✅ 部分 | `SystemProcessorPerformanceInformation` 给 idle/kernel/user/dpc/interrupt。产出 `cpu.usage` / `cpu.system` / `cpu.irq` + 每核 usage；**没有** `cpu.iowait`（见 [`cpu`]）与 `cpu.steal` |
//! | GPU | ✅ 部分 | PDH 的 `\GPU Engine(*)` / `\GPU Adapter Memory(*)` + 注册表显存总量。**没有** `gpu.temp`（Windows 无统一温度接口）与 `gpu.mem_alloc`（那是统一内存架构的概念） |
//! | 内存 | ✅ | `GlobalMemoryStatusEx` + `GetPerformanceInfo` + `SystemPagefileInformation`，六条全有 |
//! | 负载 | ✅ 部分 | 进程表里的线程状态给 `procs.running` / `procs.total`；**没有** `load.1m`（见 [`load`]） |
//! | PSI | ❌ | `/proc/pressure` 是 Linux 独有的内核特性，无任何等价物 |
//! | 磁盘 IO | ✅ | `IOCTL_DISK_PERFORMANCE`（`typeperf` 与任务管理器同源），五条全有 |
//! | 文件系统 | ✅ | `FindFirstVolumeW` + `GetDiskFreeSpaceExW`，只采固定盘（见 [`fs`]） |
//! | 网络 | ✅ | `GetIfTable2`，`MIB_IF_ROW2` 收发错误与丢包四个计数都有，`net.errors` 是完整的合并项 |
//!
//! **少产出指标不需要任何额外处理**：某条 series 是否存在本来就由
//! `GET /metrics/series` 如实报告，前端据此决定画不画。
//!
//! # 数据源的选择
//!
//! 除 GPU 外一律不走 PDH，理由见 [`crate::platform::windows::pdh`] 的模块文档：
//! 性能计数器要两轮采集、每轮要走一遍计数器名解析，而 CPU / 内存 / 磁盘 / 网络
//! 都有更直接、更便宜、且与内核数据结构同源的入口。GPU 是唯一没有别的路的那一项。
//!
//! # FFI 约定
//!
//! 卷枚举、物理盘、`NtQuerySystemInformation`、PDH、注册表这些**多处共用**的
//! 封装在 [`crate::platform::windows`]，本目录只写采集口径。
//! 只有采集器用得到的调用（`GlobalMemoryStatusEx`、`GetPerformanceInfo`、
//! `GetIfTable2`）就近放在各自的采集器文件里，与 macOS 侧把 mach 调用放在
//! 采集器里是同一取向。
//!
//! 每处 `unsafe` 都单独标注其安全前提。

// 共享条目的转发，作用同 `linux/mod.rs` 与 `macos/mod.rs` 里的同名声明：
// 子模块里的 `use super::{…}` 因此解析到同一批条目。
pub(crate) use super::{CollectError, Collector, Sample, elapsed_secs, rate, sanitize_label};

pub mod cpu;
pub mod disk;
pub mod fs;
pub mod gpu;
pub mod load;
pub mod mem;
pub mod net;

pub use cpu::CpuCollector;
pub use disk::DiskCollector;
pub use fs::FsCollector;
pub use gpu::GpuCollector;
pub use load::LoadCollector;
pub use mem::MemCollector;
pub use net::NetCollector;

/// Windows 上能原生对应的采集器，顺序与 Linux / macOS 版一致（缺的几类直接不出现）。
pub fn default_collectors() -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(CpuCollector::new()),
        Box::new(GpuCollector::new()),
        Box::new(MemCollector::new()),
        Box::new(LoadCollector::new()),
        Box::new(DiskCollector::new()),
        Box::new(FsCollector::new()),
        Box::new(NetCollector::new()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::catalog as cat;
    use std::time::{Duration, Instant};

    /// 本平台没有的那几项，**任何**采集器都不该产出。
    /// 这条是「不编数据」在指标层的机械化检查。
    #[test]
    fn 缺席的指标绝不出现() {
        let 缺席 = [
            cat::CPU_IOWAIT,
            cat::CPU_STEAL,
            cat::LOAD_1M,
            cat::GPU_TEMP,
            cat::GPU_MEM_ALLOC,
            cat::PSI_CPU_SOME,
            cat::PSI_MEMORY_SOME,
            cat::PSI_MEMORY_FULL,
            cat::PSI_IO_SOME,
            cat::PSI_IO_FULL,
        ];
        let mut collectors = default_collectors();
        for round in 0..2 {
            if round == 1 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let now = Instant::now();
            for c in collectors.iter_mut() {
                let Ok(samples) = c.collect(now) else { continue };
                for s in &samples {
                    assert!(
                        !缺席.contains(&s.metric),
                        "{} 产出了 Windows 上不存在的 {}",
                        c.name(),
                        s.metric
                    );
                }
            }
        }
    }

    /// 两轮之后，同一轮里不该出现重名 + 同标签的两条样本——那会让环里的同一条
    /// series 在一个时间点上有两个值。
    #[test]
    fn 同一轮内不出现重复的_series() {
        let mut collectors = default_collectors();
        for round in 0..2 {
            if round == 1 {
                std::thread::sleep(Duration::from_millis(150));
            }
            let now = Instant::now();
            let mut seen = std::collections::HashSet::new();
            for c in collectors.iter_mut() {
                let Ok(samples) = c.collect(now) else { continue };
                for s in &samples {
                    let key = (s.metric, s.canonical_labels());
                    assert!(seen.insert(key.clone()), "重复的 series：{key:?}");
                }
            }
        }
    }

    /// 本机实测输出，给交付报告用。
    #[test]
    fn 本机两轮采集一览() {
        let mut collectors = default_collectors();
        for round in 0..2 {
            if round == 1 {
                std::thread::sleep(Duration::from_millis(1200));
            }
            let now = Instant::now();
            for c in collectors.iter_mut() {
                match c.collect(now) {
                    Ok(samples) if round == 1 => {
                        eprintln!("[{}] {} 条样本", c.name(), samples.len());
                        for s in &samples {
                            eprintln!(
                                "    {}{} = {:.3}",
                                s.metric,
                                if s.labels.is_empty() {
                                    String::new()
                                } else {
                                    format!("{{{}}}", s.canonical_labels())
                                },
                                s.value
                            );
                        }
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("[{}] 失败：{e}", c.name()),
                }
            }
        }
    }
}
