//! 物理盘与文件系统：复用 [`crate::platform::windows::volume`] 的枚举。
//!
//! # 「磁盘」与「文件系统」是两件事
//!
//! 与 Linux / macOS 侧同一分工（`SystemInfo::disks` vs `filesystems`）：
//!
//! - [`read_disks`] 来自 `physical_disks()`（`\\.\PhysicalDriveN`，有型号、有容量）；
//! - [`read_filesystems`] 来自 `logical_volumes()`（有挂载点、有容量，`df` 会列的东西）。
//!
//! 两者用 `Volume::disk_numbers()` 关联：卷跨在哪些物理盘上由
//! `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` 给出，这是 Linux `backing_dev`
//! （挂载点 → 整盘块设备）的等价物。
//!
//! # 列出口径
//!
//! 与 macOS 侧 `storage.rs` 一致：这是 `GET /system/info` 的**一次性快照**，
//! 用户想看到的是「资源管理器/`df` 会列出什么」，因此**网络盘照列**，
//! 只排掉零容量的项（没插盘的光驱、没格式化的卷、`NO_ROOT_DIR`）。
//! 指标采集器的口径更窄（只采本地固定盘，见 `Volume::is_fixed`），那是因为
//! 它要为每个挂载点长期维护曲线，而网络盘随时会卡住整轮采集。
//!
//! # 拿不到的字段
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `DiskInfo::smart_healthy` | 判定 SMART 要用 `IOCTL_ATA_PASS_THROUGH` 发 ATA 直通命令，**需要管理员**（`GENERIC_READ` 级别打开设备）。P0 不做，健康报告的 `skipped` 里如实标出 `smart` |
//! | `FilesystemInfo::inodes_total` / `inodes_used` | NTFS / ReFS / exFAT **没有 inode 这个概念**。NTFS 的 MFT 记录数会随卷增长而动态扩张，不存在「用完就建不了文件」的固定上限，拿 MFT 记录数冒充 inode 只会让前端算出一个没有意义的百分比 |
//!
//! # `device` 字段用卷 GUID
//!
//! Linux 的 `device` 是 `/dev/nvme0n1p2` 这种设备路径。Windows 上与之语义最接近的
//! 是**卷 GUID 路径**（`\\?\Volume{…}\`）——它是卷的身份，盘符只是挂载点之一
//! （一个卷可以同时挂在 `D:\` 和 `C:\Data\`）。因此 `device` 用 GUID，
//! `mount_point` 用挂载路径。

use strixmaid_types::system::{DiskInfo, FilesystemInfo};

use crate::platform::windows::volume::{Volume, physical_disks};
use crate::platform::windows::logical_volumes;

/// 是否把这个卷列进快照，见模块文档「列出口径」。
pub fn should_list(total: u64) -> bool {
    total > 0
}

/// 物理盘列表，按盘号排序（`physical_disks` 本就是按盘号枚举的）。
pub fn read_disks() -> Vec<DiskInfo> {
    physical_disks()
        .into_iter()
        .map(|d| DiskInfo {
            name: d.name,
            model: d.model,
            size_bytes: d.size_bytes,
            // `IOCTL_STORAGE_QUERY_PROPERTY` 判定不出寻道开销时按「非机械盘」处理：
            // DTO 这一项是 `bool` 而不是 `Option<bool>`，没有表达「不知道」的余地。
            // 判定不出的多是虚拟盘与 U 盘，它们确实没有寻道开销，按 false 走
            // 比按 true 走离事实更近。
            rotational: d.rotational.unwrap_or(false),
            removable: d.removable,
            // 物理盘一级没有「只读」这个属性（只读是卷/介质的属性，
            // 写保护的 SD 卡体现在卷的 `FILE_READ_ONLY_VOLUME` 上）。
            read_only: false,
            // 见模块文档「拿不到的字段」
            smart_healthy: None,
        })
        .collect()
}

/// 文件系统列表，按挂载点排序。
pub fn read_filesystems() -> Vec<FilesystemInfo> {
    let mut out: Vec<FilesystemInfo> = logical_volumes()
        .into_iter()
        .filter(|v| should_list(v.total))
        .map(to_filesystem)
        .collect();
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

/// 一个卷 → [`FilesystemInfo`]。
fn to_filesystem(v: Volume) -> FilesystemInfo {
    FilesystemInfo {
        used_bytes: v.used(),
        backing_dev: backing_dev(&v),
        total_bytes: v.total,
        available_bytes: v.available,
        read_only: v.read_only,
        fs_type: if v.fs_type.is_empty() {
            // 没格式化 / 驱动不给类型名的卷（罕见，多为网络重定向器）。
            // 给个明确的兜底而不是空串——DTO 这一项是必填的 `String`。
            "unknown".to_owned()
        } else {
            v.fs_type
        },
        device: v.guid,
        mount_point: v.mount_point,
        // 见模块文档「拿不到的字段」
        inodes_total: None,
        inodes_used: None,
    }
}

/// 承载这个卷的整盘设备名（`PhysicalDrive0`）。
///
/// 跨多块盘的卷（跨区卷、镜像卷、存储池）只报**第一块**：DTO 这一项是
/// `Option<String>` 而不是列表，前端用它把同一块盘上的多个挂载点的读写去重求和。
/// 跨盘卷在这个模型下本来就表达不全，取第一块至少能让它归到一个真实存在的盘上。
/// 网络盘、CD-ROM、以及取不到盘号的卷为 `None`——界面显示「无块设备」，
/// 与 Linux 上 tmpfs / nfs 的表现一致。
fn backing_dev(v: &Volume) -> Option<String> {
    v.disk_numbers()
        .first()
        .map(|n| format!("PhysicalDrive{n}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 过滤规则() {
        assert!(should_list(1));
        assert!(!should_list(0), "零容量的卷没有展示价值");
    }

    #[test]
    fn 本机磁盘列表() {
        let disks = read_disks();
        // 任何能跑起本程序的机器都至少有一块盘。
        assert!(!disks.is_empty(), "一块物理盘都没枚举到");
        for d in &disks {
            assert!(d.name.starts_with("PhysicalDrive"), "设备名：{}", d.name);
            assert!(d.size_bytes > 0, "{} 容量为 0", d.name);
            assert_eq!(d.smart_healthy, None, "SMART 需要管理员，P0 不采");
            assert!(!d.read_only);
        }
        eprintln!(
            "本机物理盘：{:?}",
            disks
                .iter()
                .map(|d| (&d.name, &d.model, d.size_bytes, d.rotational))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn 本机文件系统列表() {
        let all = read_filesystems();
        assert!(!all.is_empty(), "一个卷都没枚举到");
        for f in &all {
            assert!(!f.mount_point.is_empty());
            assert!(!f.device.is_empty());
            assert!(!f.fs_type.is_empty());
            assert!(f.total_bytes > 0);
            assert!(f.used_bytes <= f.total_bytes, "{} 已用超过总量", f.mount_point);
            assert!(f.available_bytes <= f.total_bytes);
            assert_eq!(f.inodes_total, None, "NTFS 没有 inode 概念");
            assert_eq!(f.inodes_used, None);
            if let Some(dev) = &f.backing_dev {
                assert!(dev.starts_with("PhysicalDrive"), "承载设备名：{dev}");
            }
        }

        // 必须按挂载点排序
        let mounts: Vec<&str> = all.iter().map(|f| f.mount_point.as_str()).collect();
        let mut sorted = mounts.clone();
        sorted.sort_unstable();
        assert_eq!(mounts, sorted, "必须按挂载点排序");

        // 系统盘一定在列表里
        let sysdrive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
        assert!(
            all.iter().any(|f| f.mount_point.starts_with(&sysdrive)),
            "系统盘 {sysdrive} 不在列表里：{mounts:?}"
        );
        eprintln!(
            "本机文件系统：{:?}",
            all.iter()
                .map(|f| (&f.mount_point, &f.fs_type, f.total_bytes, &f.backing_dev))
                .collect::<Vec<_>>()
        );
    }

    /// 文件系统与物理盘要能对上：每个有 `backing_dev` 的卷，
    /// 它指向的盘必须真的在 `disks` 里。
    #[test]
    fn 卷与盘的关联自洽() {
        let disks = read_disks();
        let names: Vec<&str> = disks.iter().map(|d| d.name.as_str()).collect();
        for f in read_filesystems() {
            if let Some(dev) = &f.backing_dev {
                assert!(
                    names.contains(&dev.as_str()),
                    "{} 指向的 {dev} 不在物理盘列表 {names:?} 里",
                    f.mount_point
                );
            }
        }
    }
}
