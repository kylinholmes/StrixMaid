//! 磁盘：`/proc/diskstats` 差分 → 每个**整盘**的吞吐 / IOPS / util% / await。
//!
//! `disk.iops` 是**读写合计**（roadmap/08 §4.2）：方向已由两条 bytes 给出，
//! IOPS 在面板上只回答「打满的是大 IO 还是小 IO」。[`rates_between`] 仍分别
//! 算读写，合并只在 [`DiskCollector::ingest`] 的产出处。
//!
//! `/proc/diskstats` 每行：`major minor name` 后接 11 个累计计数
//! （新内核再追加 discard / flush 各若干列，本采集器不用）：
//!
//! ```text
//! reads reads_merged sectors_read ms_reading
//! writes writes_merged sectors_written ms_writing
//! in_flight ms_io ms_weighted
//! ```
//!
//! 扇区固定按 512 字节计（内核在此文件里的约定，与设备物理扇区大小无关）。
//!
//! 「整盘」判定优先用 `/sys/block` 目录列表（分区不在里面）；`/sys` 受限时退化为
//! 按命名规则猜（`sda1` / `nvme0n1p1` / `mmcblk0p1` / `md127p1` 是分区）。
//! `loop` / `ram` / `zram` / `fd` / `sr` 一律排除。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use super::{CollectError, Collector, Sample, elapsed_secs, rate, read_text, sanitize_label};
use crate::metrics::catalog::{self as cat, label};

const DISKSTATS_PATH: &str = "/proc/diskstats";
const SYS_BLOCK_PATH: &str = "/sys/block";
const SECTOR_BYTES: f64 = 512.0;

/// 一行 diskstats。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiskStat {
    pub name: String,
    pub reads: u64,
    pub reads_merged: u64,
    pub sectors_read: u64,
    pub ms_reading: u64,
    pub writes: u64,
    pub writes_merged: u64,
    pub sectors_written: u64,
    pub ms_writing: u64,
    pub in_flight: u64,
    pub ms_io: u64,
    pub ms_weighted: u64,
}

/// 解析 `/proc/diskstats`。列数不足 14 的行跳过。
pub fn parse_diskstats(text: &str) -> Vec<DiskStat> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 14 {
            continue;
        }
        let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
        out.push(DiskStat {
            name: f[2].to_owned(),
            reads: n(3),
            reads_merged: n(4),
            sectors_read: n(5),
            ms_reading: n(6),
            writes: n(7),
            writes_merged: n(8),
            sectors_written: n(9),
            ms_writing: n(10),
            in_flight: n(11),
            ms_io: n(12),
            ms_weighted: n(13),
        });
    }
    out
}

/// 无论如何都不采的设备：回环、内存盘、软驱、光驱。
pub fn is_excluded_name(name: &str) -> bool {
    ["loop", "ram", "zram", "fd", "sr"]
        .iter()
        .any(|p| name.starts_with(p) && name[p.len()..].chars().all(|c| c.is_ascii_digit()))
}

/// 按命名规则猜是否是分区（`/sys/block` 不可读时的退路）。
pub fn looks_like_partition(name: &str) -> bool {
    let stripped = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if stripped.len() == name.len() {
        // 没有尾随数字：sda / dm-0 之类，一定是整盘
        return false;
    }
    let mut chars = stripped.chars().rev();
    let last = chars.next();
    let before = chars.next();
    // nvme0n1p1 / mmcblk0p1 / md127p1：数字 + p + 数字
    if last == Some('p') && before.is_some_and(|c| c.is_ascii_digit()) {
        return true;
    }
    // sda1 / hda1 / vda1 / xvda1：字母盘符 + 数字
    ["sd", "hd", "vd", "xvd"]
        .iter()
        .any(|p| name.starts_with(p) && last.is_some_and(|c| c.is_ascii_alphabetic()))
}

/// 一轮差分得到的速率。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskRates {
    pub read_bytes: f64,
    pub write_bytes: f64,
    pub read_iops: f64,
    pub write_iops: f64,
    pub util: f64,
    pub await_ms: f64,
}

/// 两行 diskstats 之间的速率。任一计数器回退返回 `None`。
pub fn rates_between(prev: &DiskStat, cur: &DiskStat, secs: f64) -> Option<DiskRates> {
    let read_iops = rate(prev.reads, cur.reads, secs)?;
    let write_iops = rate(prev.writes, cur.writes, secs)?;
    let read_bytes = rate(prev.sectors_read, cur.sectors_read, secs)? * SECTOR_BYTES;
    let write_bytes = rate(prev.sectors_written, cur.sectors_written, secs)? * SECTOR_BYTES;
    let d_ms_io = cur.ms_io.checked_sub(prev.ms_io)?;
    let d_ms_rw = cur.ms_reading.checked_sub(prev.ms_reading)?
        + cur.ms_writing.checked_sub(prev.ms_writing)?;
    let d_ios = (cur.reads - prev.reads) + (cur.writes - prev.writes);
    Some(DiskRates {
        read_bytes,
        write_bytes,
        read_iops,
        write_iops,
        util: (d_ms_io as f64 / (secs * 1000.0) * 100.0).clamp(0.0, 100.0),
        await_ms: if d_ios == 0 {
            0.0
        } else {
            d_ms_rw as f64 / d_ios as f64
        },
    })
}

/// 磁盘采集器。
pub struct DiskCollector {
    path: PathBuf,
    sys_block: PathBuf,
    prev: HashMap<String, DiskStat>,
    prev_at: Option<Instant>,
    sys_block_warned: bool,
}

impl Default for DiskCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl DiskCollector {
    /// 读 `/proc/diskstats` + `/sys/block`。
    pub fn new() -> Self {
        DiskCollector {
            path: PathBuf::from(DISKSTATS_PATH),
            sys_block: PathBuf::from(SYS_BLOCK_PATH),
            prev: HashMap::new(),
            prev_at: None,
            sys_block_warned: false,
        }
    }

    /// `/sys/block` 里的整盘名；目录不可读时返回 `None`（退化为命名规则）。
    fn whole_disks(&mut self) -> Option<HashSet<String>> {
        match std::fs::read_dir(&self.sys_block) {
            Ok(rd) => Some(
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect(),
            ),
            Err(e) => {
                if !self.sys_block_warned {
                    self.sys_block_warned = true;
                    tracing::info!(path = %self.sys_block.display(), error = %e, "/sys/block 不可读，按设备名规则判定整盘");
                }
                None
            }
        }
    }

    /// 喂入一轮解析结果。`whole` 为 `/sys/block` 列表，`None` 时用命名规则。
    pub fn ingest(
        &mut self,
        stats: Vec<DiskStat>,
        whole: Option<&HashSet<String>>,
        now: Instant,
    ) -> Vec<Sample> {
        let secs = self.prev_at.map(|p| elapsed_secs(p, now));
        let mut out = Vec::new();
        let mut next = HashMap::with_capacity(stats.len());
        for s in stats {
            if is_excluded_name(&s.name) {
                continue;
            }
            let is_whole = match whole {
                Some(set) => set.contains(&s.name),
                None => !looks_like_partition(&s.name),
            };
            if !is_whole {
                continue;
            }
            if let Some(secs) = secs
                && let Some(prev) = self.prev.get(&s.name)
                && let Some(r) = rates_between(prev, &s, secs)
            {
                let dev = sanitize_label(&s.name);
                let mk = |m, v| Sample::labeled(m, label::DEV, dev.clone(), v);
                out.extend([
                    mk(cat::DISK_READ_BYTES, r.read_bytes),
                    mk(cat::DISK_WRITE_BYTES, r.write_bytes),
                    // 合并项（roadmap/08 §4.2）。
                    mk(cat::DISK_IOPS, r.read_iops + r.write_iops),
                    mk(cat::DISK_UTIL, r.util),
                    mk(cat::DISK_AWAIT, r.await_ms),
                ]);
            }
            next.insert(s.name.clone(), s);
        }
        self.prev = next;
        self.prev_at = Some(now);
        out
    }
}

impl Collector for DiskCollector {
    fn name(&self) -> &'static str {
        "disk"
    }

    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError> {
        let text =
            read_text(&self.path).map_err(|e| CollectError::io(self.name(), &self.path, &e))?;
        let stats = parse_diskstats(&text);
        let whole = self.whole_disks();
        Ok(self.ingest(stats, whole.as_ref(), now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const A: &str = "\
   7       0 loop0 50 0 100 0 0 0 0 0 0 1 0 0 0 0 0 0 0
 259       0 nvme0n1 1000 0 8000 500 2000 0 16000 1500 0 1000 2000
 259       1 nvme0n1p1 100 0 800 50 200 0 1600 150 0 100 200
   8       0 sda 10 0 80 5 20 0 160 15 0 10 20 0 0 0 0
";
    const B: &str = "\
   7       0 loop0 50 0 100 0 0 0 0 0 0 1 0 0 0 0 0 0 0
 259       0 nvme0n1 1100 0 8800 600 2300 0 19200 1900 0 1500 2800
 259       1 nvme0n1p1 100 0 800 50 200 0 1600 150 0 100 200
   8       0 sda 10 0 80 5 20 0 160 15 0 10 20 0 0 0 0
";

    #[test]
    fn 解析固定文本() {
        let s = parse_diskstats(A);
        assert_eq!(s.len(), 4);
        assert_eq!(s[1].name, "nvme0n1");
        assert_eq!(s[1].sectors_written, 16000);
        assert_eq!(s[1].ms_weighted, 2000);
        assert!(parse_diskstats("1 2 short\n").is_empty());
    }

    #[test]
    fn 命名规则() {
        for whole in [
            "sda", "nvme0n1", "mmcblk0", "dm-0", "md0", "vda", "xvda", "sdab",
        ] {
            assert!(!looks_like_partition(whole), "{whole} 应是整盘");
        }
        for part in [
            "sda1",
            "nvme0n1p1",
            "mmcblk0p2",
            "md127p1",
            "vda3",
            "xvda1",
            "sdab12",
        ] {
            assert!(looks_like_partition(part), "{part} 应是分区");
        }
        assert!(is_excluded_name("loop0"));
        assert!(is_excluded_name("ram12"));
        assert!(is_excluded_name("sr0"));
        assert!(!is_excluded_name("sda"));
        assert!(!is_excluded_name("fdisk1"), "fd 后面不是纯数字");
    }

    #[test]
    fn 差分速率() {
        let a = parse_diskstats(A);
        let b = parse_diskstats(B);
        // nvme0n1，2 秒：reads +100, sectors_read +800, writes +300, sectors_written +3200,
        // ms_reading +100, ms_writing +400, ms_io +500
        let r = rates_between(&a[1], &b[1], 2.0).unwrap();
        assert_eq!(r.read_iops, 50.0);
        assert_eq!(r.write_iops, 150.0);
        assert_eq!(r.read_bytes, 800.0 * 512.0 / 2.0);
        assert_eq!(r.write_bytes, 3200.0 * 512.0 / 2.0);
        assert!((r.util - 25.0).abs() < 1e-9, "500ms / 2000ms = 25%");
        assert!((r.await_ms - 500.0 / 400.0).abs() < 1e-9);
        // 无变化的设备：全 0，await 取 0
        let r = rates_between(&a[3], &b[3], 2.0).unwrap();
        assert_eq!(
            r,
            DiskRates {
                read_bytes: 0.0,
                write_bytes: 0.0,
                read_iops: 0.0,
                write_iops: 0.0,
                util: 0.0,
                await_ms: 0.0
            }
        );
        // 计数器回退 → None
        assert!(rates_between(&b[1], &a[1], 2.0).is_none());
    }

    #[test]
    fn 采集器过滤与两轮() {
        let mut c = DiskCollector::new();
        let t0 = Instant::now();
        assert!(c.ingest(parse_diskstats(A), None, t0).is_empty());
        let out = c.ingest(parse_diskstats(B), None, t0 + Duration::from_secs(2));
        let devs: HashSet<String> = out.iter().map(|s| s.labels[0].1.clone()).collect();
        assert_eq!(
            devs,
            HashSet::from(["nvme0n1".to_string(), "sda".to_string()]),
            "loop 与分区被过滤"
        );
        assert_eq!(out.len(), 10);
        // 合并项的算术（roadmap/08 §10）：nvme0n1 读 50 + 写 150 IOPS。
        let iops = out
            .iter()
            .find(|s| s.metric == cat::DISK_IOPS && s.labels[0].1 == "nvme0n1")
            .expect("disk.iops");
        assert!((iops.value - 200.0).abs() < 1e-9, "读写合计，实际 {}", iops.value);

        // 用 /sys/block 列表时以列表为准
        let mut c = DiskCollector::new();
        let whole: HashSet<String> = ["sda".to_string()].into();
        c.ingest(parse_diskstats(A), Some(&whole), t0);
        let out = c.ingest(
            parse_diskstats(B),
            Some(&whole),
            t0 + Duration::from_secs(2),
        );
        assert!(out.iter().all(|s| s.labels[0].1 == "sda"));
    }

    #[test]
    fn 本机两轮值域合理() {
        let mut c = DiskCollector::new();
        c.collect(Instant::now()).expect("读 /proc/diskstats");
        std::thread::sleep(Duration::from_millis(100));
        let out = c.collect(Instant::now()).expect("读 /proc/diskstats");
        for s in &out {
            assert!(s.value >= 0.0, "{} {:?} = {}", s.metric, s.labels, s.value);
            if s.metric == cat::DISK_UTIL {
                assert!(s.value <= 100.0);
            }
            assert!(!s.labels[0].1.starts_with("loop"));
        }
    }
}
