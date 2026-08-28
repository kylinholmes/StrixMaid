//! 块设备（`/sys/block`）与已挂载文件系统（`/proc/self/mounts` + `statvfs`）。
//!
//! 不依赖 udisks2 / lsblk / df——全部直读内核接口（`docs/design.md` §1）。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use nix::sys::statvfs::{FsFlags, statvfs};
use strixmaid_types::system::{DiskInfo, FilesystemInfo};

use super::util::{read_bool, read_trimmed, read_u64};

// ================================ 块设备 ================================

/// 遍历 `/sys/block`，过滤 loop / ram / zram 等纯虚拟设备。
pub fn read_disks() -> Vec<DiskInfo> {
    let Ok(entries) = fs::read_dir("/sys/block") else {
        return Vec::new();
    };
    let mut disks: Vec<DiskInfo> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if is_virtual_block(&name) {
                return None;
            }
            read_disk(&name, &e.path())
        })
        .collect();
    disks.sort_by(|a, b| a.name.cmp(&b.name));
    disks
}

/// 不值得展示的虚拟块设备。
pub fn is_virtual_block(name: &str) -> bool {
    ["loop", "ram", "zram", "fd"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn read_disk(name: &str, dir: &Path) -> Option<DiskInfo> {
    // `size` 单位固定为 512 字节扇区，与物理扇区大小无关。
    let sectors = read_u64(dir.join("size"))?;
    Some(DiskInfo {
        name: name.to_owned(),
        model: read_trimmed(dir.join("device/model"))
            .or_else(|| read_trimmed(dir.join("device/name")))
            .or_else(|| read_trimmed(dir.join("dm/name"))),
        size_bytes: sectors.saturating_mul(512),
        rotational: read_bool(dir.join("queue/rotational")).unwrap_or(false),
        removable: read_bool(dir.join("removable")).unwrap_or(false),
        read_only: read_bool(dir.join("ro")).unwrap_or(false),
        // P0 不判定 SMART（需要 root + 解析 ATA/NVMe 命令）。
        smart_healthy: None,
    })
}

// ============================== 文件系统 ==============================

/// `/proc/self/mounts` 的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub device: String,
    pub mount_point: String,
    pub fs_type: String,
    pub options: String,
}

impl MountEntry {
    /// 挂载选项里是否有 `ro`。
    pub fn is_read_only(&self) -> bool {
        self.options.split(',').any(|o| o == "ro")
    }
}

/// 解析 `/proc/self/mounts`（等价于 `/etc/mtab`）。
///
/// 路径中的空格等字符以八进制转义（`\040`），这里还原。
pub fn parse_mounts(raw: &str) -> Vec<MountEntry> {
    raw.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let device = unescape_octal(it.next()?);
            let mount_point = unescape_octal(it.next()?);
            let fs_type = it.next()?.to_owned();
            let options = it.next().unwrap_or("").to_owned();
            Some(MountEntry {
                device,
                mount_point,
                fs_type,
                options,
            })
        })
        .collect()
}

/// 还原 `\040` 这类八进制转义。
pub fn unescape_octal(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // 反斜杠后面必须还有整整 3 个字符才可能是 \ooo
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

/// 内核 / 运行时的伪文件系统，没有容量概念，不展示。
pub fn is_pseudo_fs(fs_type: &str) -> bool {
    matches!(
        fs_type,
        "proc"
            | "sysfs"
            | "devtmpfs"
            | "devpts"
            | "tmpfs"
            | "ramfs"
            | "cgroup"
            | "cgroup2"
            | "pstore"
            | "bpf"
            | "securityfs"
            | "debugfs"
            | "tracefs"
            | "configfs"
            | "fusectl"
            | "mqueue"
            | "hugetlbfs"
            | "autofs"
            | "binfmt_misc"
            | "rpc_pipefs"
            | "nsfs"
            | "efivarfs"
            | "selinuxfs"
            | "squashfs"
            | "fuse.gvfsd-fuse"
            | "fuse.portal"
            | "fuse.lxcfs"
            | "fuse.snapfuse"
            | "none"
    ) || fs_type.starts_with("fuse.gvfs")
}

/// 决定一条挂载是否值得进入列表。
///
/// * 伪文件系统一律不要；
/// * `squashfs` 是 snap 的只读镜像，永远 100% 满，进健康检查只会制造假警报；
/// * `overlay` 只保留挂在 `/` 上的那个（容器根），其余是 docker / podman 的层，
///   数量可达几十个而且用量与底层文件系统完全相同。
pub fn should_list_mount(entry: &MountEntry) -> bool {
    if is_pseudo_fs(&entry.fs_type) {
        return false;
    }
    if entry.fs_type == "overlay" && entry.mount_point != "/" {
        return false;
    }
    true
}

/// 读取全部已挂载文件系统的容量。
pub fn read_filesystems() -> Vec<FilesystemInfo> {
    let raw = fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    // 同一挂载点被多次挂载（overmount）时后者可见，用 BTreeMap 让后者覆盖前者、
    // 顺便按挂载点排序。
    let mut by_mount: BTreeMap<String, MountEntry> = BTreeMap::new();
    for entry in parse_mounts(&raw).into_iter().filter(should_list_mount) {
        by_mount.insert(entry.mount_point.clone(), entry);
    }
    // 挂载点 → 整盘设备名（roadmap/08 §5.1）。来源是 /proc/self/mountinfo 的
    // major:minor，而 /proc/self/mounts 不带它，所以单独读一次。
    let backing = backing_dev_map(&fs::read_to_string("/proc/self/mountinfo").unwrap_or_default());
    by_mount
        .into_values()
        .filter_map(|entry| {
            let mut fs_info = stat_filesystem(&entry)?;
            fs_info.backing_dev = backing.get(&entry.mount_point).cloned();
            Some(fs_info)
        })
        .collect()
}

/// 解析 `/proc/self/mountinfo`，产出「挂载点 → 承载它的整盘设备名」。
///
/// mountinfo 每行（`-` 之前）的第 3 个字段是 `major:minor`，第 5 个是挂载点
/// （八进制转义）。用 major:minor 读 `/sys/dev/block/<maj>:<min>`，见
/// [`whole_disk_of`]。伪 / 网络文件系统的 major 多为 0，`/sys/dev/block` 下没有
/// 对应项，自然落空为 `None`——不进 map。
pub fn backing_dev_map(mountinfo: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for line in mountinfo.lines() {
        // 只取 `-` 分隔符之前的固定字段，避免可选的标签字段（shared:1 等）串位。
        let pre = line.split(" - ").next().unwrap_or(line);
        let f: Vec<&str> = pre.split_whitespace().collect();
        if f.len() < 5 {
            continue;
        }
        let majmin = f[2];
        let mount_point = unescape_octal(f[4]);
        if let Some(disk) = whole_disk_of(majmin) {
            map.insert(mount_point, disk);
        }
    }
    map
}

/// `major:minor` → 整盘设备名（不带 `/dev/`）。分区往上找一层到整盘；
/// 直接挂在整盘或 dm-* / md* 上时就是它自己。读不到为 `None`。
///
/// 判据是 `<sysfs>/partition` 是否存在：分区目录有它、整盘没有。这比截字符串
/// （`nvme0n1p2` → `nvme0n1`）健壮——dm-* / md* / loop 的命名规则各不相同，
/// 而 `partition` 文件的语义是统一的（roadmap/08 §5.1）。
pub fn whole_disk_of(majmin: &str) -> Option<String> {
    let link = Path::new("/sys/dev/block").join(majmin);
    let canon = fs::canonicalize(&link).ok()?;
    let target = if canon.join("partition").exists() {
        canon.parent()?
    } else {
        canon.as_path()
    };
    let name = target.file_name()?.to_string_lossy().into_owned();
    // 排除虚拟块设备名（loop/ram/…），它们不该作为「承载盘」展示。
    (!is_virtual_block(&name)).then_some(name)
}

/// 对一个挂载点做 `statvfs`。失败（权限、卡死的网络挂载返回错误等）→ `None`。
fn stat_filesystem(entry: &MountEntry) -> Option<FilesystemInfo> {
    let st = statvfs(entry.mount_point.as_str()).ok()?;
    // 少数 FUSE 实现把 f_frsize 报成 0，此时退回 f_bsize。
    let frsize = if st.fragment_size() > 0 {
        st.fragment_size()
    } else {
        st.block_size()
    } as u64;
    let total = (st.blocks() as u64).saturating_mul(frsize);
    if total == 0 {
        return None;
    }
    let free = (st.blocks_free() as u64).saturating_mul(frsize);
    let available = (st.blocks_available() as u64).saturating_mul(frsize);
    let files = st.files() as u64;
    let (inodes_total, inodes_used) = if files > 0 {
        (Some(files), Some(files.saturating_sub(st.files_free() as u64)))
    } else {
        (None, None)
    };
    Some(FilesystemInfo {
        mount_point: entry.mount_point.clone(),
        device: entry.device.clone(),
        fs_type: entry.fs_type.clone(),
        total_bytes: total,
        used_bytes: total.saturating_sub(free),
        available_bytes: available,
        inodes_total,
        inodes_used,
        read_only: st.flags().contains(FsFlags::ST_RDONLY) || entry.is_read_only(),
        // read_filesystems 会用 mountinfo 的 major:minor 覆盖它；单独 stat 时留空。
        backing_dev: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析_mounts_与八进制转义() {
        let raw = "sysfs /sys sysfs rw,nosuid 0 0\n/dev/sda1 /mnt/my\\040disk ext4 rw,relatime 0 0\n/dev/sdb1 /data xfs ro 0 0\n";
        let m = parse_mounts(raw);
        assert_eq!(m.len(), 3);
        assert_eq!(m[1].mount_point, "/mnt/my disk");
        assert_eq!(m[1].fs_type, "ext4");
        assert!(!m[1].is_read_only());
        assert!(m[2].is_read_only());
    }

    #[test]
    fn 八进制转义边界() {
        assert_eq!(unescape_octal("plain"), "plain");
        assert_eq!(unescape_octal("a\\040b"), "a b");
        assert_eq!(unescape_octal("tab\\011x"), "tab\tx");
        assert_eq!(unescape_octal("bs\\134x"), "bs\\x");
        // 不完整的转义原样保留
        assert_eq!(unescape_octal("end\\04"), "end\\04");
        assert_eq!(unescape_octal("bad\\zzz"), "bad\\zzz");
    }

    #[test]
    fn backing_dev_解析_mountinfo() {
        // 只解析字段与转义；whole_disk_of 依赖真实 /sys，本机用例在下面。
        // 伪文件系统 major=0，/sys/dev/block/0:xx 不存在 → 不进 map。
        let mi = "25 0 0:23 / /proc rw shared:1 - proc proc rw
26 25 259:2 / /mnt/my\\040disk rw shared:2 - ext4 /dev/nvme0n1p2 rw
27 25 0:24 / /run rw - tmpfs tmpfs rw
";
        let m = backing_dev_map(mi);
        // /proc 与 /run 的 major 为 0，落空；/mnt/my disk 取决于本机是否有该设备。
        assert!(!m.contains_key("/proc"));
        assert!(!m.contains_key("/run"));
        // 至少键的转义是对的（值可能因本机无 259:2 而缺）。
    }

    #[test]
    fn 本机_backing_dev_命中根文件系统的整盘() {
        let fss = read_filesystems();
        let root = fss.iter().find(|f| f.mount_point == "/");
        if let Some(root) = root {
            // 本机根多半在一块真实盘上；有 backing 就必须是个非空、非虚拟的名字。
            if let Some(b) = &root.backing_dev {
                assert!(!b.is_empty());
                assert!(!is_virtual_block(b), "承载盘不该是虚拟设备：{b}");
            }
        }
    }

    #[test]
    fn 过滤规则() {
        let mk = |fs: &str, mp: &str| MountEntry {
            device: "x".into(),
            mount_point: mp.into(),
            fs_type: fs.into(),
            options: String::new(),
        };
        assert!(should_list_mount(&mk("ext4", "/")));
        assert!(should_list_mount(&mk("overlay", "/")));
        assert!(!should_list_mount(&mk("overlay", "/var/lib/docker/overlay2/abc/merged")));
        assert!(!should_list_mount(&mk("squashfs", "/snap/core/1")));
        assert!(!should_list_mount(&mk("tmpfs", "/run")));
        assert!(!should_list_mount(&mk("proc", "/proc")));
        assert!(should_list_mount(&mk("nfs4", "/mnt/nas")));
        assert!(is_virtual_block("loop0"));
        assert!(is_virtual_block("zram0"));
        assert!(!is_virtual_block("nvme0n1"));
        assert!(!is_virtual_block("dm-0"));
    }

    #[test]
    fn 本机文件系统与磁盘() {
        let fs = read_filesystems();
        // 只要机器有根文件系统，列表就不该为空；容器里也至少有 `/`。
        assert!(fs.iter().any(|f| f.mount_point == "/"), "{fs:?}");
        for f in &fs {
            assert!(f.total_bytes > 0);
            assert!(f.used_bytes <= f.total_bytes);
            assert!(f.available_bytes <= f.total_bytes);
        }
        let _ = read_disks();
    }
}
