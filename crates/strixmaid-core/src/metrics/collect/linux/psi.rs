//! PSI（Pressure Stall Information）：`/proc/pressure/{cpu,memory,io}` 的 `avg10`。
//!
//! design.md §7.1 的关键差异化项。每个文件形如：
//!
//! ```text
//! some avg10=0.00 avg60=0.06 avg300=0.15 total=114292666622
//! full avg10=0.00 avg60=0.00 avg300=0.00 total=0
//! ```
//!
//! 4.20 之前的内核没有这些文件；`psi=0` 启动参数下文件存在但读取报 `EOPNOTSUPP`；
//! 容器里可能整个目录都不可见。三种情况都**不是错误**：本采集器只产出能读到的
//! 项，首次失败记一条 info，之后保持安静。

use std::path::PathBuf;
use std::time::Instant;

use super::{CollectError, Collector, Sample, read_text};
use crate::metrics::catalog as cat;

const PRESSURE_DIR: &str = "/proc/pressure";

/// 资源 → (文件名, some 指标, full 指标)。
///
/// cpu 的 full 为 `None`：内核在整机层面对 cpu 的 `full` 没有定义，那一行恒为 0，
/// 存它等于存一条恒零曲线（roadmap/08 §4.3）。
const RESOURCES: [(&str, &str, Option<&str>); 3] = [
    ("cpu", cat::PSI_CPU_SOME, None),
    ("memory", cat::PSI_MEMORY_SOME, Some(cat::PSI_MEMORY_FULL)),
    ("io", cat::PSI_IO_SOME, Some(cat::PSI_IO_FULL)),
];

/// 一行压力数据。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PsiLine {
    /// `true` 为 `full` 行，`false` 为 `some` 行。
    pub full: bool,
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    /// 累计停滞微秒。
    pub total: u64,
}

/// 解析一个 pressure 文件。无法识别的行跳过。
pub fn parse_pressure(text: &str) -> Vec<PsiLine> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let full = match it.next() {
            Some("some") => false,
            Some("full") => true,
            _ => continue,
        };
        let mut l = PsiLine {
            full,
            avg10: 0.0,
            avg60: 0.0,
            avg300: 0.0,
            total: 0,
        };
        for kv in it {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            match k {
                "avg10" => l.avg10 = v.parse().unwrap_or(0.0),
                "avg60" => l.avg60 = v.parse().unwrap_or(0.0),
                "avg300" => l.avg300 = v.parse().unwrap_or(0.0),
                "total" => l.total = v.parse().unwrap_or(0),
                _ => {}
            }
        }
        out.push(l);
    }
    out
}

/// PSI 采集器。
pub struct PsiCollector {
    dir: PathBuf,
    /// 每种资源是否已经为「不可用」记过日志，避免每 2s 刷一条。
    logged: [bool; 3],
}

impl Default for PsiCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl PsiCollector {
    /// 读 `/proc/pressure/*`。
    pub fn new() -> Self {
        Self::with_dir(PRESSURE_DIR)
    }

    /// 改读别的目录（测试用）。
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        PsiCollector {
            dir: dir.into(),
            logged: [false; 3],
        }
    }
}

impl Collector for PsiCollector {
    fn name(&self) -> &'static str {
        "psi"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let mut out = Vec::with_capacity(6);
        for (i, (res, some_metric, full_metric)) in RESOURCES.iter().enumerate() {
            let path = self.dir.join(res);
            let text = match read_text(&path) {
                Ok(t) => t,
                Err(e) => {
                    if !self.logged[i] {
                        self.logged[i] = true;
                        tracing::info!(path = %path.display(), error = %e, "PSI 不可用，跳过该项");
                    }
                    continue;
                }
            };
            for line in parse_pressure(&text) {
                let metric = if line.full {
                    *full_metric
                } else {
                    Some(*some_metric)
                };
                let Some(metric) = metric else { continue };
                out.push(Sample::new(metric, line.avg10.clamp(0.0, 100.0)));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析() {
        let lines = parse_pressure(
            "some avg10=1.50 avg60=0.06 avg300=0.15 total=114292666622\n\
             full avg10=0.25 avg60=0.00 avg300=0.00 total=7\n\
             garbage line\n",
        );
        assert_eq!(lines.len(), 2);
        assert!(!lines[0].full);
        assert_eq!(lines[0].avg10, 1.5);
        assert_eq!(lines[0].total, 114292666622);
        assert!(lines[1].full);
        assert_eq!(lines[1].avg10, 0.25);
    }

    #[test]
    fn 目录不存在时不报错() {
        let mut c = PsiCollector::with_dir("/nonexistent/strixmaid-psi");
        assert!(c.collect(Instant::now()).unwrap().is_empty());
        // 第二次仍然安静地返回空
        assert!(c.collect(Instant::now()).unwrap().is_empty());
        assert_eq!(c.logged, [true; 3]);
    }

    #[test]
    fn 本机值域合理() {
        let out = PsiCollector::new().collect(Instant::now()).unwrap();
        for s in &out {
            assert!(
                (0.0..=100.0).contains(&s.value),
                "{} = {}",
                s.metric,
                s.value
            );
            assert!(s.metric.starts_with("psi."));
        }
        if std::path::Path::new("/proc/pressure/io").exists() {
            assert!(out.iter().any(|s| s.metric == cat::PSI_IO_SOME));
        }
        // 整机层面的 cpu full 恒为 0，已从常量表裁掉，绝不能再产出。
        assert!(!out.iter().any(|s| s.metric == "psi.cpu.full"));
    }
}
