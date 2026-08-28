//! 文件系统：`getfsstat(2)` → 每挂载点的空间用量（`fs.used` / `fs.total` 两条，
//! roadmap/08 §4.2：使用率由前端做除法，inode 走健康检查而非曲线）。
//!
//! 取数在 [`crate::platform::macos::mounts`]，本文件只决定**哪些挂载点值得画成曲线**
//! 以及怎么摊成样本。用量口径（`used = total − free`、`usage% = used / (used + avail)`）
//! 由 [`Mount`] 提供，与 Linux 版、与 `df` 一致。
//!
//! # 过滤
//!
//! - 伪文件系统按类型排除：`devfs` / `autofs` / `lifs` / `nullfs`；
//! - **网络文件系统排除**（`nfs` / `smbfs` / `webdav` / `ftp`）。理由同 Linux 版：
//!   对一个挂死的远程挂载做 `statfs` 会无限期阻塞，把整轮采集卡住；
//! - 挂载点前缀排除 `/dev` 与 `/System/Volumes/Preboot`（引导辅助卷，容量固定且与用户无关）。
//!
//! `/System/Volumes/Data`、`/System/Volumes/VM` 这些**保留**：它们是 APFS 容器里
//! 真实的卷。同容器的卷共享物理空间，因此几条曲线数值高度接近，这是 APFS 的事实，
//! 不是采集错误——`df` 的输出也是这样。

use std::time::Instant;

use super::{CollectError, Collector, Sample, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::macos::{Mount, mounts};

/// 伪文件系统类型。
const PSEUDO_FSTYPES: &[&str] = &["devfs", "autofs", "lifs", "nullfs", "fdesc", "kernfs"];

/// 网络 / 远程文件系统类型。
const NETWORK_FSTYPES: &[&str] = &["nfs", "smbfs", "cifs", "webdav", "ftp", "afpfs"];

/// 排除的挂载点前缀（本身或其子路径）。
const EXCLUDED_MOUNT_PREFIXES: &[&str] = &["/dev", "/System/Volumes/Preboot"];

/// 是否应当采集这个挂载点。
pub fn should_collect(mount_point: &str, fstype: &str) -> bool {
    if PSEUDO_FSTYPES.contains(&fstype) || NETWORK_FSTYPES.contains(&fstype) {
        return false;
    }
    !EXCLUDED_MOUNT_PREFIXES
        .iter()
        .any(|p| mount_point == *p || mount_point.starts_with(&format!("{p}/")))
}

/// 文件系统采集器。无状态——全是瞬时量。
#[derive(Debug, Clone, Copy, Default)]
pub struct FsCollector;

impl FsCollector {
    pub fn new() -> Self {
        FsCollector
    }

    /// 把挂载列表摊成样本，顺带过滤。
    pub fn samples(all: &[Mount]) -> Vec<Sample> {
        let mut out = Vec::with_capacity(all.len() * 5);
        for m in all {
            if !should_collect(&m.mount_point, &m.fstype) || m.total == 0 {
                continue;
            }
            let mount = sanitize_label(&m.mount_point);
            let s = |metric, v| Sample::labeled(metric, label::MOUNT, mount.clone(), v);
            out.push(s(cat::FS_USED, m.used() as f64));
            out.push(s(cat::FS_TOTAL, m.total as f64));
        }
        out
    }
}

impl Collector for FsCollector {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let all = mounts().ok_or_else(|| CollectError::new(self.name(), "getfsstat 调用失败"))?;
        Ok(Self::samples(&all))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount(mount_point: &str, fstype: &str, total: u64, free: u64, avail: u64) -> Mount {
        Mount {
            mount_point: mount_point.into(),
            device: "/dev/disk1".into(),
            fstype: fstype.into(),
            total,
            free,
            available: avail,
            inodes_total: 0,
            inodes_free: 0,
            read_only: false,
        }
    }

    #[test]
    fn 过滤规则() {
        assert!(should_collect("/", "apfs"));
        assert!(should_collect("/System/Volumes/Data", "apfs"));
        assert!(!should_collect("/dev", "devfs"));
        assert!(!should_collect("/System/Volumes/Preboot", "apfs"));
        assert!(!should_collect("/net", "autofs"));
        assert!(!should_collect("/mnt/share", "smbfs"), "网络挂载会阻塞采集");
        // 前缀匹配必须按路径段，不能把 /devel 也排除掉
        assert!(should_collect("/devel", "apfs"));
    }

    #[test]
    fn 摊平与过滤() {
        let all = vec![
            mount("/", "apfs", 100, 40, 30),
            mount("/dev", "devfs", 100, 0, 0),
            mount("/empty", "apfs", 0, 0, 0),
        ];
        let out = FsCollector::samples(&all);
        // 只剩 / 的两条（used / total）
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|s| s.labels == vec![(label::MOUNT, "/".to_string())])
        );
        let used = out.iter().find(|s| s.metric == cat::FS_USED).unwrap();
        assert_eq!(used.value, 60.0);

        // inode 属健康检查而非曲线（roadmap/08 §4.4），即便有数据也不产出。
        let with_inodes = Mount {
            inodes_total: 1000,
            inodes_free: 900,
            ..mount("/", "apfs", 100, 40, 30)
        };
        let out = FsCollector::samples(&[with_inodes]);
        assert_eq!(out.len(), 2);
        assert!(!out.iter().any(|s| s.metric.starts_with("fs.inodes")));
    }

    #[test]
    fn 本机采集包含根文件系统() {
        let mut c = FsCollector::new();
        let out = c.collect(Instant::now()).expect("getfsstat");
        let root: Vec<&Sample> = out
            .iter()
            .filter(|s| s.labels == vec![(label::MOUNT, "/".to_string())])
            .collect();
        assert!(!root.is_empty(), "必须采到根文件系统");
        let get = |m: &str| root.iter().find(|s| s.metric == m).map(|s| s.value).expect(m);
        assert!(get(cat::FS_USED) <= get(cat::FS_TOTAL));
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
        }
    }
}
