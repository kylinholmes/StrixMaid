//! 磁盘（macOS）：`IOBlockStorageDriver` 的 `Statistics` 差分 → 吞吐 / IOPS / util% / await。
//!
//! 这正是 `iostat` 的数据源。与 Linux 版（`/proc/diskstats`）产出完全相同的五个指标，
//! 语义对齐：
//!
//! - `disk.util`：繁忙时间占比。IOKit 给的 `Total Time (Read/Write)` 是**每个 IO 的
//!   处理时长累计**（纳秒），并发深队列下差分可超过墙钟——封顶 100，与 iostat 一致；
//! - `disk.await`：`Δ总时长 / Δ操作数`（毫秒）。无 IO 的轮次**不产出**这条样本，
//!   宁缺毋滥（roadmap/08 §6.2：await 不可合计，也不可编造）；
//! - 设备名取驱动子节点 `IOMedia` 的 `BSD Name`（`disk0` 等物理盘；APFS 合成盘
//!   `disk3` 是虚拟设备，没有 `IOBlockStorageDriver`，天然不会出现——正确）。
//!
//! 首轮只建基线不产出——速率必须来自两次采样的差。

use std::collections::HashMap;
use std::time::Instant;

use super::{CollectError, Collector, Sample, elapsed_secs, rate, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::iokit;

/// 一轮读到的累计计数。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Counters {
    read_bytes: u64,
    write_bytes: u64,
    read_ops: u64,
    write_ops: u64,
    /// 读 + 写处理时长累计，纳秒。
    busy_ns: u64,
}

/// 读当前全部块设备的计数。
fn snapshot() -> Vec<(String, Counters)> {
    let mut out = Vec::new();
    for svc in iokit::services("IOBlockStorageDriver") {
        let Some(dev) = svc.child_string("IOMedia", "BSD Name") else {
            continue;
        };
        let g = |key: &str| svc.props.sub_i64("Statistics", key).unwrap_or(0).max(0) as u64;
        out.push((
            dev,
            Counters {
                read_bytes: g("Bytes (Read)"),
                write_bytes: g("Bytes (Write)"),
                read_ops: g("Operations (Read)"),
                write_ops: g("Operations (Write)"),
                busy_ns: g("Total Time (Read)") + g("Total Time (Write)"),
            },
        ));
    }
    out
}

/// 磁盘采集器。
pub struct DiskCollector {
    prev: HashMap<String, (Instant, Counters)>,
}

impl DiskCollector {
    pub fn new() -> Self {
        DiskCollector {
            prev: HashMap::new(),
        }
    }
}

impl Default for DiskCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector for DiskCollector {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError> {
        let mut out = Vec::new();
        for (dev_raw, cur) in snapshot() {
            let dev = sanitize_label(&dev_raw);
            let prev = self.prev.insert(dev.clone(), (now, cur));
            let Some((prev_at, p)) = prev else {
                continue; // 首轮：只建基线
            };
            let secs = elapsed_secs(prev_at, now);
            let (Some(rd), Some(wr), Some(rops), Some(wops), Some(busy)) = (
                rate(p.read_bytes, cur.read_bytes, secs),
                rate(p.write_bytes, cur.write_bytes, secs),
                rate(p.read_ops, cur.read_ops, secs),
                rate(p.write_ops, cur.write_ops, secs),
                rate(p.busy_ns, cur.busy_ns, secs),
            ) else {
                continue; // 计数回绕（设备重连）：这一轮作废，基线已更新
            };
            let mk = |m, v| Sample::labeled(m, label::DEV, dev.clone(), v);
            out.push(mk(cat::DISK_READ_BYTES, rd));
            out.push(mk(cat::DISK_WRITE_BYTES, wr));
            out.push(mk(cat::DISK_IOPS, rops + wops));
            // busy 是 ns/s，除 1e9 即占比；深队列可超 1，封顶
            out.push(mk(cat::DISK_UTIL, (busy / 1e9 * 100.0).min(100.0)));
            let ops_delta =
                (cur.read_ops + cur.write_ops).saturating_sub(p.read_ops + p.write_ops);
            if ops_delta > 0 {
                let busy_delta_ms =
                    cur.busy_ns.saturating_sub(p.busy_ns) as f64 / 1e6;
                out.push(mk(cat::DISK_AWAIT, busy_delta_ms / ops_delta as f64));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn 首轮不产出_第二轮有速率() {
        let mut c = DiskCollector::new();
        let t0 = Instant::now();
        let first = c.collect(t0).expect("采集不应失败");
        assert!(first.is_empty(), "首轮只建基线");
        // 第二轮：真机上未必有 IO 变化，但不应 panic 且值非负
        let second = c.collect(t0 + Duration::from_secs(2)).expect("采集不应失败");
        for s in &second {
            assert!(s.value >= 0.0, "{} = {}", s.metric, s.value);
            if s.metric == cat::DISK_UTIL {
                assert!(s.value <= 100.0);
            }
        }
    }
}
