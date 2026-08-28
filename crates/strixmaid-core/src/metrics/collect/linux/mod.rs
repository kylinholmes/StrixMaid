//! Linux 采集器：直读 `/proc` 与 `/sys`，覆盖 roadmap/08 §4.2 的全部采集项。
//!
//! 下面那行 `pub(crate) use super::{…}` 把共享条目转发给子模块：子模块里的
//! `use super::{CollectError, Collector, Sample, …}` 因此解析得到同一批条目。

// 共享条目的转发。顺序与子模块的 use 列表一致，不要精简——少一项就会让某个
// 子模块的 `use super::…` 失败，而失败信息指向的是子模块而非这里。
pub(crate) use super::{
    CollectError, Collector, Sample, elapsed_secs, rate, read_text, sanitize_label,
};

pub mod cpu;
pub mod disk;
pub mod fs;
pub mod gpu;
pub mod load;
pub mod mem;
pub mod net;
pub mod psi;

pub use cpu::CpuCollector;
pub use disk::DiskCollector;
pub use fs::FsCollector;
pub use gpu::GpuCollector;
pub use load::LoadCollector;
pub use mem::MemCollector;
pub use net::NetCollector;
pub use psi::PsiCollector;

/// 全部默认采集器（roadmap/08 §4.2 的八类），按资源顺序。
pub fn default_collectors() -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(CpuCollector::new()),
        Box::new(GpuCollector::new()),
        Box::new(MemCollector::new()),
        Box::new(LoadCollector::new()),
        Box::new(PsiCollector::new()),
        Box::new(DiskCollector::new()),
        Box::new(FsCollector::new()),
        Box::new(NetCollector::new()),
    ]
}
