//! 文件系统：`/proc/self/mounts` + `statvfs` → 每挂载点的空间用量。
//!
//! 时序库里只有 `fs.used` / `fs.total` 两条（roadmap/08 §4.2）：使用率是派生量
//! （前端做一次除法），inode 是慢变量，走健康检查 `disk.inodes`
//! （`providers/system/health.rs`）而不是曲线。[`FsUsage`] 仍保留 inode 字段与
//! [`FsUsage::usage_percent`]——那是 `statvfs` 口径的唯一定义处，健康检查同源。
//!
//! 过滤规则（参考 node_exporter 的默认排除表并加上本项目的取舍）：
//!
//! - 伪文件系统按类型排除：proc / sysfs / tmpfs / devtmpfs / cgroup / overlay / squashfs …；
//! - **网络文件系统也排除**（nfs / cifs / 9p / ceph / `fuse.*` …）：`statvfs` 打在一个挂死的
//!   NFS 上会无限期阻塞，把整轮采集卡住。这是 P0 的保守取舍，后续给网络挂载单独加超时
//!   后再放开；
//! - 挂载点前缀排除：`/proc` `/sys` `/dev` `/run` `/snap` 以及容器存储目录；
//! - 同一挂载点被多次挂载时取**最后一次**（statvfs 看到的就是它），同一设备多处挂载
//!   （bind mount）时取**第一次**。
//!
//! 用量口径与 `df` 一致：`used = total − free`，`usage% = used / (used + avail)`——
//! 分母不含 root 保留块，所以 ext4 上 95% 时普通用户已经写不进去，与体感一致。

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use super::{CollectError, Collector, Sample, read_text, sanitize_label};
use crate::metrics::catalog::{self as cat, label};

const MOUNTS_PATH: &str = "/proc/self/mounts";

/// 一条挂载。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// 设备或来源（`/dev/sda1` / `tmpfs` / `server:/export`）。
    pub spec: String,
    /// 挂载点，已解码 `\040` 等八进制转义。
    pub mount_point: String,
    /// 文件系统类型。
    pub fstype: String,
}

/// 伪文件系统类型。
const PSEUDO_FSTYPES: &[&str] = &[
    "autofs",
    "binfmt_misc",
    "bpf",
    "cgroup",
    "cgroup2",
    "configfs",
    "debugfs",
    "devfs",
    "devpts",
    "devtmpfs",
    "efivarfs",
    "fusectl",
    "hugetlbfs",
    "iso9660",
    "mqueue",
    "nsfs",
    "overlay",
    "proc",
    "procfs",
    "pstore",
    "ramfs",
    "rootfs",
    "rpc_pipefs",
    "securityfs",
    "selinuxfs",
    "squashfs",
    "sysfs",
    "tmpfs",
    "tracefs",
    "udf",
    "nfsd",
    "sunrpc",
];

/// 网络 / 远程文件系统类型（`fuse.*` 另按前缀排除）。
const NETWORK_FSTYPES: &[&str] = &[
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "smbfs",
    "9p",
    "ceph",
    "glusterfs",
    "afs",
    "lustre",
    "ncpfs",
    "coda",
    "ocfs2",
    "gfs2",
];

/// 排除的挂载点前缀（本身或其子路径）。
const EXCLUDED_MOUNT_PREFIXES: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/snap",
    "/var/lib/docker",
    "/var/lib/containers/storage",
];

/// 解码 `/proc/self/mounts` 里的 `\ooo` 八进制转义（空格是 `\040`）。
pub fn unescape_mount(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `\` 后面要恰好跟三位八进制数字才是转义
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析 `/proc/self/mounts`。
pub fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let spec = f.next()?;
            let mp = f.next()?;
            let fstype = f.next()?;
            Some(Mount {
                spec: unescape_mount(spec),
                mount_point: unescape_mount(mp),
                fstype: fstype.to_owned(),
            })
        })
        .collect()
}

/// 该类型是否值得采。
pub fn is_selected_fstype(fstype: &str) -> bool {
    !(PSEUDO_FSTYPES.contains(&fstype)
        || NETWORK_FSTYPES.contains(&fstype)
        || fstype.starts_with("fuse."))
}

/// 挂载点是否在排除前缀下。
pub fn is_excluded_mount_point(mp: &str) -> bool {
    EXCLUDED_MOUNT_PREFIXES
        .iter()
        .any(|p| mp == *p || mp.strip_prefix(p).is_some_and(|rest| rest.starts_with('/')))
}

/// 应用全部过滤与去重规则，保持原有顺序。
pub fn select_mounts(mounts: Vec<Mount>) -> Vec<Mount> {
    // 同一挂载点取最后一次：从后往前扫，首次见到的挂载点即最终生效者。
    let mut seen_mp = HashSet::new();
    let mut keep = vec![false; mounts.len()];
    for (i, m) in mounts.iter().enumerate().rev() {
        if is_selected_fstype(&m.fstype)
            && !is_excluded_mount_point(&m.mount_point)
            && seen_mp.insert(m.mount_point.as_str())
        {
            keep[i] = true;
        }
    }
    // 同一设备取第一次：正向扫。
    let mut seen_spec = HashSet::new();
    mounts
        .into_iter()
        .zip(keep)
        .filter(|(m, k)| *k && (!m.spec.starts_with('/') || seen_spec.insert(m.spec.clone())))
        .map(|(m, _)| m)
        .collect()
}

/// 一个挂载点的用量。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsUsage {
    pub total: u64,
    pub free: u64,
    pub avail: u64,
    pub inodes_total: u64,
    pub inodes_free: u64,
}

impl FsUsage {
    /// `total − free`。
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// `df` 口径的使用率。
    pub fn usage_percent(&self) -> f64 {
        let used = self.used();
        let denom = used.saturating_add(self.avail);
        if denom == 0 {
            0.0
        } else {
            (used as f64 / denom as f64 * 100.0).clamp(0.0, 100.0)
        }
    }

    /// 由 `statvfs` 结果换算。
    pub fn from_statvfs(st: &nix::sys::statvfs::Statvfs) -> FsUsage {
        let frsize = widen(st.fragment_size());
        FsUsage {
            total: widen(st.blocks()).saturating_mul(frsize),
            free: widen(st.blocks_free()).saturating_mul(frsize),
            avail: widen(st.blocks_available()).saturating_mul(frsize),
            inodes_total: widen(st.files()),
            inodes_free: widen(st.files_free()),
        }
    }

    /// 转成样本。
    pub fn samples(&self, mount_point: &str) -> Vec<Sample> {
        let mp = sanitize_label(mount_point);
        let mk = |m, v| Sample::labeled(m, label::MOUNT, mp.clone(), v);
        vec![
            mk(cat::FS_USED, self.used() as f64),
            mk(cat::FS_TOTAL, self.total as f64),
        ]
    }
}

/// `fsblkcnt_t` / `c_ulong` 在 32 位与 64 位平台宽度不同，统一提升到 u64。
fn widen(v: impl Into<u64>) -> u64 {
    v.into()
}

/// 文件系统采集器（无状态）。
pub struct FsCollector {
    path: PathBuf,
}

impl Default for FsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl FsCollector {
    /// 读 `/proc/self/mounts`。
    pub fn new() -> Self {
        FsCollector {
            path: PathBuf::from(MOUNTS_PATH),
        }
    }
}

impl Collector for FsCollector {
    fn name(&self) -> &'static str {
        "fs"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let text =
            read_text(&self.path).map_err(|e| CollectError::io(self.name(), &self.path, &e))?;
        let mut out = Vec::new();
        for m in select_mounts(parse_mounts(&text)) {
            let st = match nix::sys::statvfs::statvfs(m.mount_point.as_str()) {
                Ok(st) => st,
                Err(e) => {
                    tracing::debug!(mount = %m.mount_point, error = %e, "statvfs 失败，跳过");
                    continue;
                }
            };
            let usage = FsUsage::from_statvfs(&st);
            if usage.total == 0 {
                continue;
            }
            out.extend(usage.samples(&m.mount_point));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
sysfs /sys sysfs rw,nosuid 0 0
proc /proc proc rw 0 0
udev /dev devtmpfs rw 0 0
tmpfs /run tmpfs rw 0 0
/dev/mapper/vg-root / ext4 rw,relatime 0 0
/dev/sda1 /boot/efi vfat rw 0 0
/dev/sda1 /mnt/efi-bind vfat rw 0 0
/dev/loop3 /snap/core/1 squashfs ro 0 0
overlay /var/lib/docker/overlay2/abc/merged overlay rw 0 0
server:/export /mnt/nfs nfs4 rw 0 0
sshfs#u@h:/ /mnt/ssh fuse.sshfs rw 0 0
/dev/sdb1 /mnt/with\\040space xfs rw 0 0
/dev/sdc1 /data ext4 rw 0 0
/dev/sdd1 /data ext4 rw 0 0
";

    #[test]
    fn 转义解码() {
        assert_eq!(unescape_mount("/mnt/with\\040space"), "/mnt/with space");
        assert_eq!(unescape_mount("/plain"), "/plain");
        assert_eq!(unescape_mount("/tail\\"), "/tail\\");
        assert_eq!(unescape_mount("/x\\12"), "/x\\12", "不足三位不解码");
    }

    #[test]
    fn 过滤与去重() {
        let selected = select_mounts(parse_mounts(SAMPLE));
        let mps: Vec<&str> = selected.iter().map(|m| m.mount_point.as_str()).collect();
        assert_eq!(mps, ["/", "/boot/efi", "/mnt/with space", "/data"]);
        // /data 被挂了两次，生效的是最后一次
        assert_eq!(selected.last().unwrap().spec, "/dev/sdd1");
    }

    #[test]
    fn 前缀排除() {
        assert!(is_excluded_mount_point("/sys"));
        assert!(is_excluded_mount_point("/sys/fs/cgroup"));
        assert!(!is_excluded_mount_point("/sysdata"));
        assert!(!is_excluded_mount_point("/"));
        assert!(is_excluded_mount_point("/var/lib/docker/overlay2/x"));
    }

    #[test]
    fn 用量换算() {
        let u = FsUsage {
            total: 1000,
            free: 300,
            avail: 200,
            inodes_total: 50,
            inodes_free: 10,
        };
        assert_eq!(u.used(), 700);
        // 700 / (700 + 200)
        assert!((u.usage_percent() - 700.0 / 9.0).abs() < 1e-9);
        let s = u.samples("/");
        assert_eq!(s.len(), 2, "只有 used 与 total 两条曲线");
        assert_eq!(s[0].value, 700.0);
        assert_eq!(s[1].value, 1000.0);
        assert_eq!(
            FsUsage {
                total: 0,
                free: 0,
                avail: 0,
                inodes_total: 0,
                inodes_free: 0
            }
            .usage_percent(),
            0.0
        );
    }

    #[test]
    fn 本机值域合理() {
        let out = FsCollector::new()
            .collect(Instant::now())
            .expect("读 /proc/self/mounts");
        let mounts: HashSet<&str> = out.iter().map(|s| s.labels[0].1.as_str()).collect();
        assert!(mounts.contains("/"), "根文件系统必须在: {mounts:?}");
        for mp in &mounts {
            let get = |m: &str| {
                out.iter()
                    .find(|s| s.metric == m && s.labels[0].1 == *mp)
                    .unwrap()
                    .value
            };
            assert!(get(cat::FS_USED) <= get(cat::FS_TOTAL), "{mp}");
            assert!(get(cat::FS_TOTAL) > 0.0, "{mp}");
        }
    }
}
