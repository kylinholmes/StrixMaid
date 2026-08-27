//! 指标链路：采集 → 内存环形缓冲 → 每分钟落盘 → 查询 / 实时广播（design.md §7）。
//!
//! ```text
//! collect::*  ──每 interval_secs──▶  ring::RingSet  ──每分钟──▶  store (m_1m → 五层聚合)
//!      │                                   │
//!      └──── broadcast<MetricSnapshot> ────┴──▶  engine::MetricsEngine（HTTP / WS 用）
//! ```
//!
//! - [`catalog`]：指标常量表（名字 / 单位 / 说明 / 标签键）；
//! - [`collect`]：各采集器与 [`Collector`] trait；
//! - [`ring`]：固定容量环与桶统计（含真中位数）；
//! - [`scheduler`]：tokio 调度任务；
//! - [`engine`]：对外句柄 [`MetricsEngine`]。
//!
//! design.md §3 里列的 `rollup.rs`（分层聚合与保留期清理）已经由 [`crate::store`] 的
//! `rollup_all` / `prune_all` / `maintain` 实现，本模块只负责按分钟调用它。

pub mod catalog;
pub mod collect;
pub mod engine;
pub mod ring;
pub mod scheduler;

pub use catalog::{CATALOG, MetricDef};
pub use collect::{CollectError, Collector, Sample};
pub use engine::{MetricsEngine, Selector, parse_selectors};
pub use ring::{Bucket, Point, Ring, RingSet, RingStats, SeriesKey};
pub use scheduler::{RoundInfo, Scheduler, SchedulerConfig};

/// MVP 里本机节点的 id（design.md §8 `series.node` / §11）。
pub const LOCAL_NODE: &str = "local";
