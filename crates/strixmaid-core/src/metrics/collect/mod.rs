//! 采集器：直读 `/proc` 与 `/sys`，产出一轮 [`Sample`]（design.md §7.1）。
//!
//! # 约定
//!
//! - 每个采集器把「读文件」与「解析文本」分开：`collect()` 只负责 IO，然后交给
//!   纯函数 `parse_*` / `ingest()`。差分逻辑因此可以用固定文本样本做单测。
//! - **可选文件缺失不是错误**（容器里没有 `/proc/pressure`、`/sys/block` 受限），
//!   采集器返回少几个样本即可；只有核心文件（`/proc/stat` 这类）读不到才返回 `Err`。
//!   调度器对 `Err` 只记 warn，不退出。
//! - 速率类指标（`*_bytes` / `*_iops` / `*_packets` …）由采集器用两轮之间的
//!   **单调时钟**差分，第一轮没有基线时不产出这些样本。计数器回绕或设备被替换
//!   （当前值小于上一轮）时跳过该设备本轮的速率样本。
//! - 指标名一律引用 [`crate::metrics::catalog`] 里的常量。

use std::path::Path;
use std::time::Instant;

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

/// 标签集合：键为静态字符串（见 [`crate::metrics::catalog::label`]），值由采集器生成。
/// 绝大多数指标只有 0–1 个标签，`Vec` 足够。
pub type Labels = Vec<(&'static str, String)>;

/// 一个采样值。
#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    /// 指标名，取自 [`crate::metrics::catalog`]。
    pub metric: &'static str,
    /// 标签。
    pub labels: Labels,
    /// 值。速率类已差分，百分比类在 `0..=100`。
    pub value: f64,
}

impl Sample {
    /// 无标签样本。
    pub fn new(metric: &'static str, value: f64) -> Self {
        Sample {
            metric,
            labels: Vec::new(),
            value,
        }
    }

    /// 单标签样本。
    pub fn labeled(
        metric: &'static str,
        key: &'static str,
        label: impl Into<String>,
        value: f64,
    ) -> Self {
        Sample {
            metric,
            labels: vec![(key, label.into())],
            value,
        }
    }

    /// 规范化标签串（`k=v` 按键排序、逗号连接），与 `series.labels` 列一致。
    pub fn canonical_labels(&self) -> String {
        let pairs: Vec<(&str, &str)> = self.labels.iter().map(|(k, v)| (*k, v.as_str())).collect();
        crate::store::canonical_labels(&pairs)
    }
}

/// 采集失败。只用于「核心输入读不到 / 解析不出」这类整轮失效的情况。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("采集器 {collector} 失败: {message}")]
pub struct CollectError {
    /// 采集器名，见 [`Collector::name`]。
    pub collector: &'static str,
    /// 原因。
    pub message: String,
}

impl CollectError {
    /// 构造。
    pub fn new(collector: &'static str, message: impl Into<String>) -> Self {
        CollectError {
            collector,
            message: message.into(),
        }
    }

    /// 读文件失败。
    pub fn io(collector: &'static str, path: &Path, error: &std::io::Error) -> Self {
        CollectError::new(collector, format!("读取 {} 失败: {error}", path.display()))
    }
}

/// 采集器接口。
///
/// `collect` 是**同步阻塞**的：目标全是几百字节的 procfs 小文件，一次 `read` 远快于
/// 把任务丢进阻塞线程池的开销。调度器把整轮采集放进一个 `spawn_blocking` 里跑。
pub trait Collector: Send {
    /// 稳定的短名，用于日志（`cpu` / `mem` / `disk` …）。
    fn name(&self) -> &'static str;

    /// 采一轮。`now` 是单调时钟，速率类指标用它与上一轮做差分。
    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError>;
}

/// design.md §7.1 全部 P0 采集器，按类别顺序。
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

// ============================ 公共小工具 ============================

/// 读整个文本文件。procfs 文件 `stat` 报的大小是 0，必须用 `read_to_string` 而不是
/// 按大小预分配。
pub(crate) fn read_text(path: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(path)
}

/// 标签值消毒：`,` 与 `=` 是规范标签串的分隔符，不能出现在值里。
pub(crate) fn sanitize_label(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == ',' || c == '=' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// 两次采样之间的秒数（单调时钟）。
pub(crate) fn elapsed_secs(prev: Instant, now: Instant) -> f64 {
    now.saturating_duration_since(prev).as_secs_f64()
}

/// 计数器差分速率。当前值小于上一轮（回绕 / 设备被替换）或时间差非正时返回 `None`。
pub(crate) fn rate(prev: u64, cur: u64, secs: f64) -> Option<f64> {
    if secs <= 0.0 {
        return None;
    }
    let delta = cur.checked_sub(prev)?;
    Some(delta as f64 / secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 标签消毒() {
        assert_eq!(sanitize_label("dev=sda,1"), "dev_sda_1");
        assert_eq!(sanitize_label("/boot/efi"), "/boot/efi");
    }

    #[test]
    fn 速率差分() {
        assert_eq!(rate(100, 300, 2.0), Some(100.0));
        assert_eq!(rate(300, 100, 2.0), None, "计数器回退");
        assert_eq!(rate(100, 300, 0.0), None, "时间差为 0");
        let a = Instant::now();
        let b = a + Duration::from_millis(1500);
        assert!((elapsed_secs(a, b) - 1.5).abs() < 1e-9);
        assert_eq!(elapsed_secs(b, a), 0.0, "时钟倒退不会出负数");
    }

    #[test]
    fn 样本的规范标签串() {
        let s = Sample::labeled("x", "dev", "sda", 1.0);
        assert_eq!(s.canonical_labels(), "dev=sda");
        assert_eq!(Sample::new("x", 1.0).canonical_labels(), "");
    }

    /// 所有默认采集器在本机跑两轮不 panic，且产出的指标名全部登记在常量表里。
    #[test]
    fn 默认采集器产出的指标都在常量表中() {
        let mut collectors = default_collectors(true);
        for round in 0..2 {
            if round == 1 {
                std::thread::sleep(Duration::from_millis(120));
            }
            let now = Instant::now();
            for c in collectors.iter_mut() {
                let samples = match c.collect(now) {
                    Ok(s) => s,
                    Err(e) => {
                        // 本机没有该输入（例如容器）——不是本用例要断言的东西。
                        eprintln!("跳过 {}: {e}", c.name());
                        continue;
                    }
                };
                for s in &samples {
                    let def = crate::metrics::catalog::find(s.metric)
                        .unwrap_or_else(|| panic!("{} 产出了未登记的指标 {}", c.name(), s.metric));
                    let keys: Vec<&str> = s.labels.iter().map(|(k, _)| *k).collect();
                    assert_eq!(keys, def.labels, "{} 的标签键与常量表不符", s.metric);
                    assert!(s.value.is_finite(), "{} 出现非有限值", s.metric);
                }
            }
        }
    }
}
