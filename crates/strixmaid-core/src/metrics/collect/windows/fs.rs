//! 文件系统：卷枚举 → 每挂载点的空间用量（`fs.used` / `fs.total` 两条，
//! roadmap/08 §4.2：使用率由前端做除法，inode 走健康检查而非曲线）。
//!
//! 取数在 [`crate::platform::windows::volume::logical_volumes`]，本文件只决定
//! **哪些卷值得画成曲线**以及怎么摊成样本。用量口径（`used = total − free`）
//! 由 [`Volume::used`] 提供，与 Linux 版、与 `df` 一致。
//!
//! # 挂载点，不是盘符
//!
//! 标签 `mount` 的值是卷的挂载路径（`C:\`、`C:\Data\`），来自
//! `GetVolumePathNamesForVolumeNameW` 而不是 `GetLogicalDriveStringsW`——
//! 后者看不见挂载到目录上的卷。理由见 `volume.rs` 的模块文档。
//!
//! # 只采固定盘
//!
//! 判据是 `GetDriveTypeW` 返回 `DRIVE_FIXED`（[`Volume::is_fixed`]），
//! 其余类型一律排除，与 Linux / macOS 版排除网络挂载与伪文件系统是同一理由：
//!
//! | 类型 | 为什么不采 |
//! |---|---|
//! | `DRIVE_REMOTE`（网络盘） | 对一个断开的 SMB 挂载做 `GetDiskFreeSpaceExW` 会阻塞到超时（默认几十秒），把整轮采集卡死。这与 Linux 版排除 `nfs`/`cifs`、macOS 版排除 `smbfs` 完全同因 |
//! | `DRIVE_CDROM` | 容量是碟片决定的常数，换碟时整条曲线跳变，没有观测价值 |
//! | `DRIVE_REMOVABLE`（U 盘 / 读卡器） | 插拔即整条 series 生灭，画出来全是断线 |
//! | `DRIVE_RAMDISK` | 第三方虚拟盘，容量恒定 |
//!
//! 真要看网络盘的容量，那是「远端那台机器的文件系统」，应当由远端的 agent 上报，
//! 而不是从挂载它的这台机器上隔着网络问——那样一个卷会在两个节点上各出现一次。
//!
//! 容量为 0 的卷同样跳过（没插碟的光驱、刚初始化还没格式化的卷）：一条恒为 0
//! 的曲线不是观测，是噪声。

use std::time::Instant;

use super::{CollectError, Collector, Sample, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::windows::volume::{Volume, logical_volumes};

/// 是否应当把这个卷画成曲线。
pub fn should_collect(v: &Volume) -> bool {
    v.is_fixed() && v.total > 0
}

/// 文件系统采集器。无状态——全是瞬时量。
#[derive(Debug, Clone, Copy, Default)]
pub struct FsCollector;

impl FsCollector {
    pub fn new() -> Self {
        FsCollector
    }

    /// 把卷列表摊成样本，顺带过滤。
    pub fn samples(all: &[Volume]) -> Vec<Sample> {
        let mut out = Vec::with_capacity(all.len() * 2);
        for v in all {
            if !should_collect(v) {
                continue;
            }
            let mount = sanitize_label(&v.mount_point);
            out.push(Sample::labeled(
                cat::FS_USED,
                label::MOUNT,
                mount.clone(),
                v.used() as f64,
            ));
            out.push(Sample::labeled(
                cat::FS_TOTAL,
                label::MOUNT,
                mount,
                v.total as f64,
            ));
        }
        out
    }
}

impl Collector for FsCollector {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let all = logical_volumes();
        // 一台 Windows 至少有一个系统卷；一个都枚举不到说明 FindFirstVolumeW 失败了
        //（`logical_volumes` 分不清「失败」与「空」，这里按核心输入读不到处理）。
        if all.is_empty() {
            return Err(CollectError::new(self.name(), "未枚举到任何已挂载的卷"));
        }
        Ok(Self::samples(&all))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::windows::volume::drive_type;

    fn vol(mount: &str, dtype: u32, total: u64, free: u64) -> Volume {
        Volume {
            mount_point: mount.into(),
            guid: r"\\?\Volume{00000000-0000-0000-0000-000000000000}\".into(),
            fs_type: "NTFS".into(),
            label: String::new(),
            total,
            free,
            available: free,
            read_only: false,
            drive_type: dtype,
        }
    }

    #[test]
    fn 过滤规则() {
        assert!(should_collect(&vol(r"C:\", drive_type::FIXED, 100, 40)));
        assert!(should_collect(&vol(
            r"C:\Data\",
            drive_type::FIXED,
            100,
            40
        )));
        assert!(
            !should_collect(&vol(r"Z:\", drive_type::REMOTE, 100, 40)),
            "网络盘会阻塞采集"
        );
        assert!(!should_collect(&vol(r"D:\", drive_type::CDROM, 100, 40)));
        assert!(!should_collect(&vol(r"E:\", drive_type::REMOVABLE, 100, 40)));
        assert!(!should_collect(&vol(r"R:\", drive_type::RAMDISK, 100, 40)));
        assert!(
            !should_collect(&vol(r"F:\", drive_type::FIXED, 0, 0)),
            "容量为 0 的卷不画"
        );
    }

    #[test]
    fn 摊平与过滤() {
        let all = vec![
            vol(r"C:\", drive_type::FIXED, 100, 40),
            vol(r"Z:\", drive_type::REMOTE, 100, 40),
            vol(r"D:\", drive_type::CDROM, 0, 0),
        ];
        let out = FsCollector::samples(&all);
        // 只剩 C:\ 的两条（used / total）
        assert_eq!(out.len(), 2);
        assert!(
            out.iter()
                .all(|s| s.labels == vec![(label::MOUNT, r"C:\".to_string())])
        );
        let used = out.iter().find(|s| s.metric == cat::FS_USED).unwrap();
        assert_eq!(used.value, 60.0, "used = total − free");
        let total = out.iter().find(|s| s.metric == cat::FS_TOTAL).unwrap();
        assert_eq!(total.value, 100.0);
    }

    #[test]
    fn 挂载点里的反斜杠不被消毒掉() {
        // 标签消毒只替换 `,` `=` 与控制字符，Windows 路径必须原样保留
        let out = FsCollector::samples(&[vol(r"C:\Data\", drive_type::FIXED, 10, 5)]);
        assert_eq!(out[0].labels[0].1, r"C:\Data\");
    }

    #[test]
    fn 本机采集包含系统盘() {
        let mut c = FsCollector::new();
        let out = c.collect(Instant::now()).expect("卷枚举");
        let sys: Vec<&Sample> = out
            .iter()
            .filter(|s| s.labels[0].1.eq_ignore_ascii_case(r"C:\"))
            .collect();
        assert!(!sys.is_empty(), "必须采到系统盘");
        let get = |m: &str| sys.iter().find(|s| s.metric == m).map(|s| s.value).expect(m);
        assert!(get(cat::FS_USED) <= get(cat::FS_TOTAL));
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
            assert_eq!(s.labels.len(), 1);
            assert_eq!(s.labels[0].0, label::MOUNT);
        }
        // 只有两条文件系统指标（inode 走健康检查）
        assert!(
            out.iter()
                .all(|s| [cat::FS_USED, cat::FS_TOTAL].contains(&s.metric))
        );
        eprintln!(
            "本机文件系统：{}",
            out.iter()
                .filter(|s| s.metric == cat::FS_TOTAL)
                .map(|s| format!("{}={:.0}GiB", s.labels[0].1, s.value / (1u64 << 30) as f64))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }
}
