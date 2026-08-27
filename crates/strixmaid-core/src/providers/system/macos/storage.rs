//! 文件系统列表：`getfsstat(2)`。
//!
//! 与指标采集器（[`crate::metrics::collect::macos::fs`]）共用
//! [`crate::platform::macos::mounts`] 取数，但**过滤口径不同**：
//! 采集器要为每个挂载点长期维护五条曲线，只留真正有容量意义的；
//! 这里是 `GET /system/info` 的一次性快照，用户想看到的是「`df` 会列出什么」，
//! 因此只排掉伪文件系统与零容量项，网络挂载照列。
//!
//! 网络挂载在这里不构成阻塞风险：`getfsstat(MNT_NOWAIT)` 用的是内核缓存，
//! 不会向远端发起同步请求。

use strixmaid_types::system::FilesystemInfo;

use crate::platform::macos::mounts;

/// 伪文件系统类型：没有容量概念，列出来只是噪声。
const PSEUDO_FSTYPES: &[&str] = &["devfs", "autofs", "lifs", "nullfs", "fdesc", "kernfs"];

/// 是否列出这个挂载点。
pub fn should_list(fstype: &str, total: u64) -> bool {
    !PSEUDO_FSTYPES.contains(&fstype) && total > 0
}

/// 本机文件系统列表，按挂载点排序。
pub fn read_filesystems() -> Vec<FilesystemInfo> {
    let Some(all) = mounts() else {
        return Vec::new();
    };
    let mut out: Vec<FilesystemInfo> = all
        .into_iter()
        .filter(|m| should_list(&m.fstype, m.total))
        .map(|m| FilesystemInfo {
            used_bytes: m.used(),
            inodes_used: m.inodes_used(),
            inodes_total: (m.inodes_total > 0).then_some(m.inodes_total),
            mount_point: m.mount_point,
            device: m.device,
            fs_type: m.fstype,
            total_bytes: m.total,
            available_bytes: m.available,
            read_only: m.read_only,
        })
        .collect();
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 过滤规则() {
        assert!(should_list("apfs", 100));
        assert!(should_list("smbfs", 100), "网络挂载在快照里照列");
        assert!(!should_list("devfs", 100));
        assert!(!should_list("apfs", 0), "零容量的挂载没有展示价值");
    }

    #[test]
    fn 本机列表含根且已排序() {
        let all = read_filesystems();
        assert!(!all.is_empty());
        let root = all
            .iter()
            .find(|f| f.mount_point == "/")
            .expect("根文件系统");
        assert!(root.total_bytes > 0);
        assert!(root.used_bytes <= root.total_bytes);
        assert!(!root.fs_type.is_empty());
        assert!(!root.device.is_empty());

        let mounts: Vec<&str> = all.iter().map(|f| f.mount_point.as_str()).collect();
        let mut sorted = mounts.clone();
        sorted.sort_unstable();
        assert_eq!(mounts, sorted, "必须按挂载点排序");
        eprintln!("本机文件系统：{mounts:?}");
    }
}
