//! 内存：`/proc/meminfo`。全部换算成字节。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use super::{CollectError, Collector, Sample, read_text};
use crate::metrics::catalog as cat;

const MEMINFO_PATH: &str = "/proc/meminfo";

/// 解析 `/proc/meminfo`：`Key:  值 kB` → 字节；无单位的行（`HugePages_Total`）原样。
pub fn parse_meminfo(text: &str) -> HashMap<&str, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let mut it = rest.split_whitespace();
        let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) else {
            continue;
        };
        let bytes = match it.next() {
            Some("kB") => v.saturating_mul(1024),
            Some("MB") => v.saturating_mul(1024 * 1024),
            _ => v,
        };
        map.insert(key.trim(), bytes);
    }
    map
}

/// 由解析结果产出样本。缺 `MemTotal` 时返回 `None`。
pub fn samples_from(info: &HashMap<&str, u64>) -> Option<Vec<Sample>> {
    let get = |k: &str| info.get(k).copied();
    let total = get("MemTotal")?;
    let free = get("MemFree").unwrap_or(0);
    let buffers = get("Buffers").unwrap_or(0);
    let cached = get("Cached").unwrap_or(0);
    // 3.14 之前的内核没有 MemAvailable，用经典近似。
    let available = get("MemAvailable").unwrap_or(free + buffers + cached);
    let swap_total = get("SwapTotal").unwrap_or(0);
    let swap_free = get("SwapFree").unwrap_or(0);
    Some(vec![
        Sample::new(cat::MEM_TOTAL, total as f64),
        Sample::new(cat::MEM_AVAILABLE, available as f64),
        Sample::new(cat::MEM_USED, total.saturating_sub(available) as f64),
        Sample::new(cat::MEM_FREE, free as f64),
        Sample::new(cat::MEM_BUFFERS, buffers as f64),
        Sample::new(cat::MEM_CACHED, cached as f64),
        Sample::new(cat::MEM_DIRTY, get("Dirty").unwrap_or(0) as f64),
        Sample::new(cat::MEM_SWAP_TOTAL, swap_total as f64),
        Sample::new(cat::MEM_SWAP_FREE, swap_free as f64),
        Sample::new(
            cat::MEM_SWAP_USED,
            swap_total.saturating_sub(swap_free) as f64,
        ),
    ])
}

/// 内存采集器（无状态）。
pub struct MemCollector {
    path: PathBuf,
}

impl Default for MemCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl MemCollector {
    /// 读 `/proc/meminfo`。
    pub fn new() -> Self {
        MemCollector {
            path: PathBuf::from(MEMINFO_PATH),
        }
    }
}

impl Collector for MemCollector {
    fn name(&self) -> &'static str {
        "mem"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let text =
            read_text(&self.path).map_err(|e| CollectError::io(self.name(), &self.path, &e))?;
        samples_from(&parse_meminfo(&text))
            .ok_or_else(|| CollectError::new(self.name(), "/proc/meminfo 缺少 MemTotal"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
MemTotal:        1000 kB
MemFree:          100 kB
MemAvailable:     600 kB
Buffers:           50 kB
Cached:           400 kB
Dirty:              8 kB
SwapTotal:        200 kB
SwapFree:         150 kB
HugePages_Total:       0
";

    #[test]
    fn 解析并换算() {
        let m = parse_meminfo(SAMPLE);
        assert_eq!(m["MemTotal"], 1_024_000);
        assert_eq!(m["HugePages_Total"], 0);
        let s = samples_from(&m).unwrap();
        let get = |k: &str| s.iter().find(|x| x.metric == k).unwrap().value;
        assert_eq!(get(cat::MEM_USED), (1000 - 600) as f64 * 1024.0);
        assert_eq!(get(cat::MEM_SWAP_USED), 50.0 * 1024.0);
        assert_eq!(get(cat::MEM_DIRTY), 8.0 * 1024.0);
    }

    #[test]
    fn 缺_memavailable_时近似() {
        let m = parse_meminfo("MemTotal: 100 kB\nMemFree: 10 kB\nBuffers: 5 kB\nCached: 20 kB\n");
        let s = samples_from(&m).unwrap();
        let avail = s
            .iter()
            .find(|x| x.metric == cat::MEM_AVAILABLE)
            .unwrap()
            .value;
        assert_eq!(avail, 35.0 * 1024.0);
        assert!(samples_from(&parse_meminfo("MemFree: 1 kB\n")).is_none());
    }

    #[test]
    fn 本机值域合理() {
        let out = MemCollector::new()
            .collect(Instant::now())
            .expect("读 /proc/meminfo");
        let get = |k: &str| out.iter().find(|x| x.metric == k).unwrap().value;
        assert!(get(cat::MEM_TOTAL) > 0.0);
        assert!(get(cat::MEM_AVAILABLE) <= get(cat::MEM_TOTAL));
        assert!(get(cat::MEM_FREE) <= get(cat::MEM_TOTAL));
        assert!(get(cat::MEM_SWAP_FREE) <= get(cat::MEM_SWAP_TOTAL));
        assert_eq!(
            get(cat::MEM_USED),
            get(cat::MEM_TOTAL) - get(cat::MEM_AVAILABLE)
        );
    }
}
