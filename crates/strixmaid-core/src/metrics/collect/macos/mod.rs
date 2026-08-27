//! macOS 采集器：mach `host_*` 调用、`sysctl`、`getfsstat`。
//!
//! # 定位
//!
//! macOS 是 StrixMaid 的**开发与联调平台**，不是交付目标（`design.md` §2.1 的三个产物
//! 都是 Linux 二进制）。这里的采集器存在的理由只有一个：让本机跑起来的服务返回真实数据，
//! 从而能对照 OpenAPI 逐端点验证 API 契约、WS 推送与前端渲染，而不是对着一堆空数组调试。
//!
//! # 与 Linux 实现的覆盖差异
//!
//! | 采集项 | macOS | 说明 |
//! |---|---|---|
//! | CPU | ✅ 5 态 | mach 只统计 user / system / idle / nice 四态，没有 iowait / irq / softirq / steal |
//! | 内存 | ✅ 部分 | 没有 `Buffers` / `Dirty` 的对应概念；`available` 是估算值，见 [`mem`] |
//! | 负载 | ✅ | `getloadavg(3)`；运行队列长度无对应数据源，只产出 `procs.total` |
//! | PSI | ❌ | `/proc/pressure` 是 Linux 独有的内核特性，无任何等价物 |
//! | 磁盘 IO | ❌ | 需要走 IOKit 逐设备取 statistics，成本高、与联调目的不匹配 |
//! | 文件系统 | ✅ | `getfsstat(2)` |
//! | 网络 | ✅ 部分 | `sysctl NET_RT_IFLIST2`，没有发送方向的丢包计数 |
//!
//! **少产出指标不需要任何额外处理**：某条 series 是否存在本来就由 `GET /metrics/series`
//! 如实报告，前端据此决定画不画。这正是 `design.md` §1 第 2 条「能力探测而非硬依赖」
//! 在指标层的体现——缺 PSI 就是没有 PSI，不是错误。
//!
//! # FFI 约定
//!
//! `sysctl` / `getfsstat` 这类**多处共用**的封装在 [`crate::platform::macos`]，
//! 本目录只写采集口径。mach 的 `host_statistics64` / `host_processor_info` 只有
//! 采集器用得到，就近放在各自的采集器文件里。
//!
//! 绝大多数符号直接取自 `libc`，连 `HOST_VM_INFO64`、`CPU_STATE_*` 这些常量也有；
//! 唯独 `mach_host_self` 与 `mach_task_self` 走 `mach2`——`libc` 里这两个已标
//! `#[deprecated]`（注解明确写着「改用 mach2」），而本项目的质量门要求零 warning，
//! 与其到处撒 `#[allow(deprecated)]`，不如按它说的做。
//!
//! 每处 `unsafe` 都单独标注其安全前提。

// 共享条目的转发，作用同 `linux/mod.rs` 里的同名声明。
pub(crate) use super::{CollectError, Collector, Sample, elapsed_secs, rate, sanitize_label};

pub mod cpu;
pub mod fs;
pub mod load;
pub mod mem;
pub mod net;

pub use cpu::CpuCollector;
pub use fs::FsCollector;
pub use load::LoadCollector;
pub use mem::MemCollector;
pub use net::NetCollector;

/// macOS 上能原生对应的采集器，顺序与 Linux 版一致（缺的两项直接不出现）。
pub fn default_collectors(per_core_detail: bool) -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(CpuCollector::new().per_core_states(per_core_detail)),
        Box::new(MemCollector::new()),
        Box::new(LoadCollector::new()),
        Box::new(FsCollector::new()),
        Box::new(NetCollector::new()),
    ]
}
