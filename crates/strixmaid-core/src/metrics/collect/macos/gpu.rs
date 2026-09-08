//! GPU（macOS）：`IOAccelerator` 的 `PerformanceStatistics` → 占用率与显存。
//!
//! Apple Silicon 的 GPU 驱动（`AGXAccelerator*`）在注册表里按秒级刷新
//! `Device Utilization %` 与 `In use system memory`，普通用户可读
//! （`ioreg -r -c IOAccelerator` 同源）。统一内存架构下没有「显存总量」——
//! **不产出 `gpu.mem_total`**，也不拿整机内存假冒（spec §6：测不到就不出现）；
//! 显存条的分母用 `Alloc system memory`（GPU 已向系统申请的量，used ≤ alloc）。
//! 引擎细分（`gpu.engine.usage`）取 `Renderer/Tiler Utilization %`。
//!
//! 标签 `gpu=<序号>`：枚举顺序在一次开机内稳定；多卡 Mac（Intel + 独显）
//! 各占一个序号。全是瞬时量，无需差分。

use std::time::Instant;

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::iokit;

/// GPU 采集器。无状态。
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuCollector;

impl GpuCollector {
    pub fn new() -> Self {
        GpuCollector
    }
}

impl Collector for GpuCollector {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let mut out = Vec::new();
        for (idx, svc) in iokit::services("IOAccelerator").iter().enumerate() {
            let stat = |key: &str| svc.props.sub_i64("PerformanceStatistics", key);
            // 没有占用率键的（老驱动 / 虚拟机）整卡跳过——半张卡没有意义
            let Some(usage) = stat("Device Utilization %") else {
                continue;
            };
            let id = idx.to_string();
            out.push(Sample::labeled(
                cat::GPU_USAGE,
                label::GPU,
                id.clone(),
                (usage.max(0) as f64).min(100.0),
            ));
            for (key, engine) in [
                ("Renderer Utilization %", "renderer"),
                ("Tiler Utilization %", "tiler"),
            ] {
                if let Some(v) = stat(key) {
                    out.push(Sample {
                        metric: cat::GPU_ENGINE_USAGE,
                        // 键序 = canonical 序（engine < gpu），与常量表 GPU_ENGINE 一致
                        labels: vec![
                            (label::ENGINE, engine.to_string()),
                            (label::GPU, id.clone()),
                        ],
                        value: (v.max(0) as f64).min(100.0),
                    });
                }
            }
            if let Some(mem) = stat("In use system memory") {
                out.push(Sample::labeled(
                    cat::GPU_MEM_USED,
                    label::GPU,
                    id.clone(),
                    mem.max(0) as f64,
                ));
            }
            if let Some(alloc) = stat("Alloc system memory") {
                out.push(Sample::labeled(
                    cat::GPU_MEM_ALLOC,
                    label::GPU,
                    id,
                    alloc.max(0) as f64,
                ));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 值域() {
        let mut c = GpuCollector::new();
        let out = c.collect(Instant::now()).expect("采集不应失败");
        for s in &out {
            assert!(s.value >= 0.0, "{} = {}", s.metric, s.value);
            if s.metric == cat::GPU_USAGE {
                assert!(s.value <= 100.0);
            }
        }
    }
}
