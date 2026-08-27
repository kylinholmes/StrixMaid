//! Linux 采集器：直读 `/proc` 与 `/sys`，覆盖 `design.md` §7.1 的全部 P0 采集项。
//!
//! 七个子模块是 StrixMaid 的目标平台实现，**内容与平台化改造之前逐字节相同**——
//! 它们原本直接位于 `metrics/collect/` 下，为了给 macOS 让出并列位置才整体移入本目录。
//! 下面那行 `pub(crate) use super::{…}` 就是为此存在的：子模块里的
//! `use super::{CollectError, Collector, Sample, …}` 因此仍然解析得到同一批共享条目，
//! 移动没有产生任何内容改动，diff 是纯重命名。

// 共享条目的转发。顺序与子模块的 use 列表一致，不要精简——少一项就会让某个
// 子模块的 `use super::…` 失败，而失败信息指向的是子模块而非这里。
pub(crate) use super::{
    CollectError, Collector, Sample, elapsed_secs, rate, read_text, sanitize_label,
};

pub mod cpu;
pub mod disk;
pub mod fs;
pub mod load;
pub mod mem;
pub mod net;
pub mod psi;

pub use cpu::CpuCollector;
pub use disk::DiskCollector;
pub use fs::FsCollector;
pub use load::LoadCollector;
pub use mem::MemCollector;
pub use net::NetCollector;
pub use psi::PsiCollector;

/// `design.md` §7.1 全部 P0 采集器，按类别顺序。
pub fn default_collectors(per_core_detail: bool) -> Vec<Box<dyn Collector>> {
    vec![
        Box::new(CpuCollector::new().per_core_states(per_core_detail)),
        Box::new(MemCollector::new()),
        Box::new(LoadCollector::new()),
        Box::new(PsiCollector::new()),
        Box::new(DiskCollector::new()),
        Box::new(FsCollector::new()),
        Box::new(NetCollector::new()),
    ]
}
