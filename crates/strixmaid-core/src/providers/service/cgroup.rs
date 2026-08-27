//! cgroup v2 用量直读（`/sys/fs/cgroup/<ControlGroup>/`）。
//!
//! 这是 Cockpit 没有而我们要的差异化项：服务详情页直接显示该 unit 的 CPU / 内存 / 任务数，
//! 数据来自内核而不是 systemd 的缓存属性（后者在 `MemoryAccounting=no` 时是 `[not set]`）。
//!
//! 只实现 cgroup v2（unified）。v1 的文件布局完全不同（`memory/…/memory.usage_in_bytes`），
//! 2024 年后的发行版默认都是 v2；v1 或容器内目录不可读时返回 `None`，由调用方回落到
//! systemd 属性（`MemoryCurrent` / `CPUUsageNSec` …）。
//!
//! # CPU 百分比
//!
//! `cpu.stat` 的 `usage_usec` 是单调累加值，百分比需要两次采样求差。为了不给详情接口加
//! 固定延迟，这里按 cgroup 路径缓存上一次采样：首次读取 `cpu_percent = None`，
//! 之后每次读取都相对上一次计算——前端轮询详情时自然得到实时占用。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use strixmaid_types::service::CgroupUsage;

/// 两次采样间隔低于此值时不算百分比（噪声太大）。
const MIN_SAMPLE_GAP: Duration = Duration::from_millis(100);
/// 采样缓存里超过此时长没再读过的 cgroup 会被清掉。
const SAMPLE_TTL: Duration = Duration::from_secs(600);
/// 缓存条目超过此数时触发清理。
const PRUNE_THRESHOLD: usize = 256;

/// 上一次 CPU 采样。
#[derive(Debug, Clone, Copy)]
struct CpuSample {
    at: Instant,
    usage_usec: u64,
}

/// cgroup 读取器，持有 CPU 采样缓存。
#[derive(Debug)]
pub struct CgroupReader {
    root: PathBuf,
    samples: Mutex<HashMap<String, CpuSample>>,
}

impl Default for CgroupReader {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupReader {
    /// 以 `/sys/fs/cgroup` 为根。
    pub fn new() -> Self {
        Self::with_root("/sys/fs/cgroup")
    }

    /// 指定根目录（测试用）。
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            samples: Mutex::new(HashMap::new()),
        }
    }

    /// 读取 `control_group`（systemd 的 `ControlGroup` 属性，如 `/system.slice/nginx.service`）
    /// 的用量。目录不存在或不是目录时返回 `None`；单个文件读不到只让对应字段为 `None`。
    ///
    /// 同步 I/O：目标全是 sysfs 里几十字节的小文件，一次读取远快于丢进阻塞线程池。
    pub fn read(&self, control_group: &str) -> Option<CgroupUsage> {
        let dir = self.root.join(control_group.trim_start_matches('/'));
        if !dir.is_dir() {
            return None;
        }

        let cpu_usec = read_cpu_stat_usage(&dir.join("cpu.stat"));
        Some(CgroupUsage {
            cpu_usage_nsec: cpu_usec.map(|u| u.saturating_mul(1000)),
            cpu_percent: cpu_usec.and_then(|u| self.sample_cpu(control_group, u)),
            memory_current_bytes: read_u64(&dir.join("memory.current")),
            memory_peak_bytes: read_u64(&dir.join("memory.peak")),
            memory_limit_bytes: read_u64(&dir.join("memory.max")),
            tasks_current: read_u64(&dir.join("pids.current")),
            tasks_limit: read_u64(&dir.join("pids.max")),
            path: Some(control_group.to_owned()),
        })
    }

    /// 记录本次采样并相对上一次算百分比。
    fn sample_cpu(&self, key: &str, usage_usec: u64) -> Option<f64> {
        let now = Instant::now();
        // 锁中毒只意味着别的线程在持锁时 panic 了，缓存本身仍然一致，照用。
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let prev = samples.insert(
            key.to_owned(),
            CpuSample {
                at: now,
                usage_usec,
            },
        );

        if samples.len() > PRUNE_THRESHOLD {
            samples.retain(|_, s| now.duration_since(s.at) < SAMPLE_TTL);
        }

        let prev = prev?;
        let elapsed = now.duration_since(prev.at);
        if elapsed < MIN_SAMPLE_GAP || elapsed > SAMPLE_TTL {
            return None;
        }
        // usage 单调递增；cgroup 被删除又重建时可能回退，此时放弃这一次。
        let delta = usage_usec.checked_sub(prev.usage_usec)?;
        Some(delta as f64 / elapsed.as_micros() as f64 * 100.0)
    }
}

/// 读一个单值文件。`max`（无上限）与解析失败都返回 `None`。
fn read_u64(path: &Path) -> Option<u64> {
    let s = fs::read_to_string(path).ok()?;
    s.trim().parse().ok()
}

/// 从 `cpu.stat` 取 `usage_usec`。
fn read_cpu_stat_usage(path: &Path) -> Option<u64> {
    let s = fs::read_to_string(path).ok()?;
    s.lines().find_map(|line| {
        let (k, v) = line.split_once(' ')?;
        (k == "usage_usec").then(|| v.trim().parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在临时目录里搭一棵假 cgroup 树。
    fn fake_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "strixmaid-cgroup-test-{}-{name}",
            std::process::id()
        ));
        let dir = root.join("system.slice/demo.service");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("memory.current"), "268435456\n").unwrap();
        fs::write(dir.join("memory.peak"), "300000000\n").unwrap();
        fs::write(dir.join("memory.max"), "max\n").unwrap();
        fs::write(dir.join("pids.current"), "14\n").unwrap();
        fs::write(dir.join("pids.max"), "4915\n").unwrap();
        fs::write(
            dir.join("cpu.stat"),
            "usage_usec 1000000\nuser_usec 800000\nsystem_usec 200000\n",
        )
        .unwrap();
        root
    }

    #[test]
    fn reads_v2_files_and_handles_max() {
        let root = fake_tree("read");
        let reader = CgroupReader::with_root(&root);
        let u = reader
            .read("/system.slice/demo.service")
            .expect("dir exists");
        assert_eq!(u.memory_current_bytes, Some(268_435_456));
        assert_eq!(u.memory_peak_bytes, Some(300_000_000));
        assert_eq!(u.memory_limit_bytes, None, "`max` 表示无上限");
        assert_eq!(u.tasks_current, Some(14));
        assert_eq!(u.tasks_limit, Some(4915));
        assert_eq!(u.cpu_usage_nsec, Some(1_000_000_000));
        assert_eq!(u.cpu_percent, None, "首次采样没有百分比");
        assert_eq!(u.path.as_deref(), Some("/system.slice/demo.service"));

        assert!(reader.read("/system.slice/missing.service").is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cpu_percent_from_two_samples() {
        let root = fake_tree("cpu");
        let reader = CgroupReader::with_root(&root);
        let cg = "/system.slice/demo.service";
        let stat = root.join("system.slice/demo.service/cpu.stat");

        assert!(reader.read(cg).unwrap().cpu_percent.is_none());
        // 间隔太短：不算。
        fs::write(&stat, "usage_usec 1050000\n").unwrap();
        assert!(reader.read(cg).unwrap().cpu_percent.is_none());

        std::thread::sleep(Duration::from_millis(120));
        // 120ms 里用了 60ms CPU → 约 50%。
        fs::write(&stat, "usage_usec 1110000\n").unwrap();
        let pct = reader.read(cg).unwrap().cpu_percent.expect("second sample");
        assert!((20.0..80.0).contains(&pct), "pct={pct}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn real_cgroup_root_is_readable_when_v2() {
        // 本机若是 cgroup v2，根目录的 cpu.stat 一定可读；v1 时 read() 返回的字段全 None 也不算错。
        let reader = CgroupReader::new();
        if let Some(u) = reader.read("/") {
            assert_eq!(u.path.as_deref(), Some("/"));
        }
    }
}
