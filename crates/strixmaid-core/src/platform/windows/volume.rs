//! 卷、挂载点与物理盘。
//!
//! # 「文件系统」与「磁盘」是两件事
//!
//! 与 Linux 侧一样（`SystemInfo::disks` vs `filesystems`）：
//!
//! - [`logical_volumes`] 枚举**卷**（有容量、有挂载点、`df` 会列出来的东西）；
//! - [`physical_disks`] 枚举**物理盘**（`\\.\PhysicalDriveN`，有型号、有 IO 计数）。
//!
//! 一块盘可以挂多个卷，也可以一个都不挂，所以两者各自枚举，再用
//! [`Volume::disk_numbers`] 建立关联（对应 Linux 的 `backing_dev`）。
//!
//! # 为什么不走 WMI
//!
//! `Win32_LogicalDisk` / `Win32_DiskDrive` 要起 COM、连 WMI 服务，
//! 而 WMI 在负载高时可能几秒才应答——一个每 2 秒采集一轮的指标管线不能赌这个。
//! 这里全部走同步的 Win32 与 `DeviceIoControl`，与「不依赖守护进程」一致
//! （`design.md` §1）。
//!
//! # 挂载点用的是卷枚举而不是盘符
//!
//! `GetLogicalDriveStringsW` 只给盘符（`C:\`），看不到**挂载到目录上的卷**
//! （`C:\Data\` 这种，Windows 的 mount point）。因此这里走
//! `FindFirstVolumeW` 枚举全部卷、再用 `GetVolumePathNamesForVolumeNameW`
//! 问每个卷挂在哪些路径上——那才是完整的挂载表。

use std::io;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, MAX_PATH};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_READONLY, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW, GetDriveTypeW,
    GetVolumeInformationW, GetVolumePathNamesForVolumeNameW, OPEN_EXISTING,
};
// `GetVolumeInformationW` 的文件系统标志位住在 `SystemServices` 而不是
// `FileSystem`（windows-sys 按头文件而非用途分模块，这一项来自 winnt.h）。
use windows_sys::Win32::System::SystemServices::FILE_READ_ONLY_VOLUME;
use windows_sys::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, DISK_PERFORMANCE, GET_LENGTH_INFORMATION,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_DISK_PERFORMANCE, IOCTL_STORAGE_GET_HOTPLUG_INFO,
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
    STORAGE_HOTPLUG_INFO, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty,
};

use super::handle::Owned;
use super::wide::{from_wide_multi, from_wide_nul, to_wide};
use super::last_error;

/// `DRIVE_*` 的取值，见 `GetDriveTypeW`。
pub mod drive_type {
    pub const UNKNOWN: u32 = 0;
    pub const NO_ROOT_DIR: u32 = 1;
    pub const REMOVABLE: u32 = 2;
    pub const FIXED: u32 = 3;
    pub const REMOTE: u32 = 4;
    pub const CDROM: u32 = 5;
    pub const RAMDISK: u32 = 6;
}

/// 一个已挂载的卷。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// 挂载路径，如 `C:\` 或 `C:\Data\`。这是给人看、也给 API 用的「挂载点」。
    pub mount_point: String,
    /// 卷 GUID 路径（`\\?\Volume{…}\`）。同一个卷可以挂在多处，这个是它的身份。
    pub guid: String,
    /// 文件系统类型，如 `NTFS` / `FAT32` / `exFAT`。
    pub fs_type: String,
    /// 卷标。
    pub label: String,
    /// 总容量，字节。
    pub total: u64,
    /// 总空闲，字节（含只有管理员能用的保留部分）。
    pub free: u64,
    /// 当前用户可用，字节（受配额影响）。
    pub available: u64,
    /// 是否只读挂载。
    pub read_only: bool,
    /// `DRIVE_*`，见 [`drive_type`]。
    pub drive_type: u32,
}

impl Volume {
    /// 已用字节，口径同 `df`：`total − free`。
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// 是不是本地固定盘（`fs` 指标只采这一类，网络盘会阻塞采集）。
    pub fn is_fixed(&self) -> bool {
        self.drive_type == drive_type::FIXED
    }

    /// 承载这个卷的物理盘号（跨盘的动态卷 / 存储空间会有多个）。
    ///
    /// 取不到（网络盘、没有权限）时返回空表。
    pub fn disk_numbers(&self) -> Vec<u32> {
        volume_disk_extents(&self.guid).unwrap_or_default()
    }
}

/// 枚举全部已挂载的卷。
///
/// 没有挂载点的卷（刚插上还没分配盘符的）会被跳过——它在文件系统视图里
/// 不存在，列出来只会是一行没有路径的空记录。
pub fn logical_volumes() -> Vec<Volume> {
    let mut out = Vec::new();
    let mut name = [0u16; 64]; // 卷 GUID 路径固定 49 个字符
    // SAFETY: name 有 64 个 u16 可写，长度如实给出。
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if find == super::handle::invalid_handle() {
        return out;
    }
    loop {
        let guid = from_wide_nul(&name);
        for mount in volume_mount_points(&guid) {
            if let Some(v) = describe_volume(&guid, &mount) {
                out.push(v);
            }
        }
        // SAFETY: find 是上面拿到的有效句柄；name 仍有 64 个 u16 可写。
        let more = unsafe { FindNextVolumeW(find, name.as_mut_ptr(), name.len() as u32) };
        if more == 0 {
            break;
        }
    }
    // SAFETY: find 有效且只关一次。
    unsafe {
        FindVolumeClose(find);
    }
    out.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    out
}

/// 一个卷挂在哪些路径上。
fn volume_mount_points(guid: &str) -> Vec<String> {
    let w = to_wide(guid);
    let mut len: u32 = 0;
    // SAFETY: 缓冲为空时只回填所需长度。
    unsafe {
        GetVolumePathNamesForVolumeNameW(w.as_ptr(), std::ptr::null_mut(), 0, &raw mut len);
    }
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: buf 有 len 个 u16 可写。
    let ok = unsafe {
        GetVolumePathNamesForVolumeNameW(w.as_ptr(), buf.as_mut_ptr(), len, &raw mut len)
    };
    if ok == 0 {
        return Vec::new();
    }
    from_wide_multi(&buf)
}

/// 取一个挂载点的文件系统信息与容量。
fn describe_volume(guid: &str, mount: &str) -> Option<Volume> {
    let root = to_wide(mount);
    // SAFETY: root 以 NUL 结尾。
    let dtype = unsafe { GetDriveTypeW(root.as_ptr()) };

    let mut label = [0u16; MAX_PATH as usize + 1];
    let mut fs_name = [0u16; MAX_PATH as usize + 1];
    let mut flags: u32 = 0;
    // SAFETY: 两个缓冲各有 MAX_PATH+1 个 u16，长度如实给出；其余输出参数可为空。
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            label.len() as u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    // 读不到卷信息（光驱没碟、网络盘断开）就不列它——一行全 0 的记录没有意义。
    if ok == 0 {
        return None;
    }

    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut free: u64 = 0;
    // SAFETY: 三个输出参数都指向本栈帧上的 u64。
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            root.as_ptr(),
            &raw mut available,
            &raw mut total,
            &raw mut free,
        )
    };
    if ok == 0 {
        return None;
    }

    Some(Volume {
        mount_point: mount.to_owned(),
        guid: guid.to_owned(),
        fs_type: from_wide_nul(&fs_name),
        label: from_wide_nul(&label),
        total,
        free,
        available,
        read_only: flags & FILE_READ_ONLY_VOLUME != 0,
        drive_type: dtype,
    })
}

/// 卷跨在哪些物理盘上。
fn volume_disk_extents(guid: &str) -> Option<Vec<u32>> {
    // IOCTL 要的是不带尾部反斜杠的设备路径。
    let device = guid.trim_end_matches('\\');
    let h = open_device(device).ok()?;

    // VOLUME_DISK_EXTENTS 是变长的（NumberOfDiskExtents 个 DISK_EXTENT）。
    // 一次给 8 个的空间，跨 8 块盘的卷在本项目的场景里不存在。
    let mut buf = [0u8; 512];
    let mut returned: u32 = 0;
    // SAFETY: buf 有 512 字节可写；输入缓冲为空是该 IOCTL 的约定。
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            h.raw(),
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            std::ptr::null(),
            0,
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            buf.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    // 布局：u32 NumberOfDiskExtents + 4 字节填充 + DISK_EXTENT[]，
    // DISK_EXTENT = { u32 DiskNumber, u32 填充, i64 StartingOffset, i64 ExtentLength }
    let count = u32::from_ne_bytes(buf[0..4].try_into().ok()?) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count.min(8) {
        let at = 8 + i * 24;
        if at + 4 > buf.len() {
            break;
        }
        out.push(u32::from_ne_bytes(buf[at..at + 4].try_into().ok()?));
    }
    Some(out)
}

use windows_sys::Win32::Storage::FileSystem::IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS;

// ===========================================================================
// 物理盘
// ===========================================================================

/// 一块物理盘。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalDisk {
    /// 盘号，`\\.\PhysicalDrive3` 里的 3。
    pub number: u32,
    /// 对外用的设备名，形如 `PhysicalDrive3`，作为指标的 `dev` 标签。
    pub name: String,
    /// 型号（厂商 + 产品），读不到为 `None`。
    pub model: Option<String>,
    /// 容量，字节。
    pub size_bytes: u64,
    /// 是否机械盘（有寻道开销）。判定不了为 `None`。
    pub rotational: Option<bool>,
    /// 是否可移动介质。
    pub removable: bool,
}

/// 枚举物理盘。
///
/// 没有「列出所有盘」的 API，标准做法就是从 0 开始逐个试着打开
/// `\\.\PhysicalDriveN`。盘号**不保证连续**（拔掉中间一块盘会留下空洞），
/// 因此连续 [`PROBE_GAP`] 个都打不开才停，而不是遇到第一个失败就停。
pub fn physical_disks() -> Vec<PhysicalDisk> {
    /// 连续多少个盘号打不开就认为到头了。
    const PROBE_GAP: u32 = 4;
    /// 盘号上界，防御性兜底。
    const MAX_DISK: u32 = 64;

    let mut out = Vec::new();
    let mut miss = 0;
    for n in 0..MAX_DISK {
        match describe_disk(n) {
            Some(d) => {
                out.push(d);
                miss = 0;
            }
            None => {
                miss += 1;
                if miss >= PROBE_GAP && !out.is_empty() {
                    break;
                }
            }
        }
    }
    out
}

fn describe_disk(number: u32) -> Option<PhysicalDisk> {
    let h = open_device(&format!(r"\\.\PhysicalDrive{number}")).ok()?;
    let size_bytes = disk_size(&h)?;

    Some(PhysicalDisk {
        number,
        name: format!("PhysicalDrive{number}"),
        model: disk_model(&h),
        size_bytes,
        rotational: disk_rotational(&h),
        removable: disk_removable(&h),
    })
}

/// 物理盘容量，字节。
///
/// # 为什么先问几何而不是直接问长度
///
/// `IOCTL_DISK_GET_LENGTH_INFO` 的访问要求是 `FILE_READ_ACCESS`，而
/// [`open_device`] 刻意用 `dwDesiredAccess = 0` 打开设备（那是「只查属性、
/// 不读写数据」的用法，不需要管理员）。两者对不上：非特权进程能把
/// `\\.\PhysicalDriveN` 打开，却会在这个 IOCTL 上拿到 `ERROR_ACCESS_DENIED`。
///
/// `IOCTL_DISK_GET_DRIVE_GEOMETRY_EX` 是 `FILE_ANY_ACCESS` 的，同样给出总字节数
/// （`DiskSize`），非特权可用。所以把它放在前面，`GET_LENGTH_INFO` 退化成兜底
/// ——两者在能拿到结果时给的是同一个数。
///
/// 这个顺序不是优化，是**功能正确性**：颠倒过来会让非管理员运行时
/// [`physical_disks`] 恒为空表，而本项目的读路径必须在非特权下也能工作。
fn disk_size(h: &Owned) -> Option<u64> {
    use windows_sys::Win32::System::Ioctl::{DISK_GEOMETRY_EX, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX};

    // DISK_GEOMETRY_EX 尾部是变长的分区信息，多给一些空间，只读前面固定的部分。
    let mut geo = [0u8; 256];
    let mut returned: u32 = 0;
    // SAFETY: 输出缓冲有 256 字节可写，长度如实给出；该 IOCTL 无输入缓冲。
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            h.raw(),
            IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
            std::ptr::null(),
            0,
            geo.as_mut_ptr().cast::<core::ffi::c_void>(),
            geo.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok != 0 && returned as usize >= std::mem::size_of::<DISK_GEOMETRY_EX>() {
        // SAFETY: 刚确认过返回长度足够容纳结构体的固定部分。
        let g = unsafe { std::ptr::read_unaligned(geo.as_ptr().cast::<DISK_GEOMETRY_EX>()) };
        if g.DiskSize > 0 {
            return Some(g.DiskSize as u64);
        }
    }

    let mut length = GET_LENGTH_INFORMATION { Length: 0 };
    let mut returned: u32 = 0;
    // SAFETY: 输出缓冲是一个完整的 GET_LENGTH_INFORMATION。
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            h.raw(),
            IOCTL_DISK_GET_LENGTH_INFO,
            std::ptr::null(),
            0,
            (&raw mut length).cast::<core::ffi::c_void>(),
            std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 || length.Length <= 0 {
        return None;
    }
    Some(length.Length as u64)
}

/// `STORAGE_DEVICE_DESCRIPTOR` 里的厂商 + 产品串。
fn disk_model(h: &Owned) -> Option<String> {
    let mut buf = [0u8; 1024];
    let descriptor = storage_query(h, StorageDeviceProperty, &mut buf)?;
    if descriptor < std::mem::size_of::<STORAGE_DEVICE_DESCRIPTOR>() {
        return None;
    }
    // SAFETY: 刚确认过返回长度足够容纳结构体头部。
    let d = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<STORAGE_DEVICE_DESCRIPTOR>()) };

    // 三个 *Offset 是相对缓冲起点的字节偏移，0 表示没有这一项。
    let at = |offset: u32| -> Option<String> {
        if offset == 0 || offset as usize >= buf.len() {
            return None;
        }
        let bytes = &buf[offset as usize..];
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        let s = String::from_utf8_lossy(&bytes[..end]).trim().to_owned();
        (!s.is_empty()).then_some(s)
    };
    let vendor = at(d.VendorIdOffset);
    let product = at(d.ProductIdOffset);
    match (vendor, product) {
        (Some(v), Some(p)) => Some(format!("{v} {p}")),
        (None, Some(p)) => Some(p),
        (Some(v), None) => Some(v),
        (None, None) => None,
    }
}

/// 有没有寻道开销：`IncursSeekPenalty` 为真即机械盘。
fn disk_rotational(h: &Owned) -> Option<bool> {
    let mut buf = [0u8; 256];
    let got = storage_query(h, StorageDeviceSeekPenaltyProperty, &mut buf)?;
    if got < std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() {
        return None;
    }
    // SAFETY: 刚确认过返回长度足够。
    let d =
        unsafe { std::ptr::read_unaligned(buf.as_ptr().cast::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()) };
    // windows-sys 把 `BOOLEAN` 映射成 Rust 的 `bool`，直接用即可。
    Some(d.IncursSeekPenalty)
}

fn disk_removable(h: &Owned) -> bool {
    let mut info = STORAGE_HOTPLUG_INFO {
        Size: std::mem::size_of::<STORAGE_HOTPLUG_INFO>() as u32,
        MediaRemovable: false,
        MediaHotplug: false,
        DeviceHotplug: false,
        WriteCacheEnableOverride: false,
    };
    let mut returned: u32 = 0;
    // SAFETY: 输出缓冲是一个完整的 STORAGE_HOTPLUG_INFO。
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            h.raw(),
            IOCTL_STORAGE_GET_HOTPLUG_INFO,
            std::ptr::null(),
            0,
            (&raw mut info).cast::<core::ffi::c_void>(),
            std::mem::size_of::<STORAGE_HOTPLUG_INFO>() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    ok != 0 && info.MediaRemovable
}

/// 一次 `IOCTL_STORAGE_QUERY_PROPERTY`，返回写入的字节数。
fn storage_query(h: &Owned, property: i32, buf: &mut [u8]) -> Option<usize> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: property,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0; 1],
    };
    let mut returned: u32 = 0;
    // SAFETY: 输入是一个完整的 STORAGE_PROPERTY_QUERY；输出缓冲有 buf.len() 字节可写。
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            h.raw(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            (&raw const query).cast::<core::ffi::c_void>(),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            buf.len() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(returned as usize)
}

/// 一块盘的累计 IO 计数（`iostat` 的数据源）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskCounters {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ops: u64,
    pub write_ops: u64,
    /// 读 + 写的累计处理时长，100 纳秒。
    pub busy_100ns: u64,
    /// 空闲累计时长，100 纳秒。`util% = 1 − Δidle/Δ墙钟`。
    pub idle_100ns: u64,
}

/// 读一块盘的 `IOCTL_DISK_PERFORMANCE`。
///
/// # 这个计数器默认就是开的吗
///
/// 是。物理盘的性能计数自 Windows Vista 起默认启用（`diskperf -y` 那套是
/// XP 时代的事）。取不到时返回 `None`——调用方少一条曲线，不报错。
pub fn disk_counters(number: u32) -> Option<DiskCounters> {
    let h = open_device(&format!(r"\\.\PhysicalDrive{number}")).ok()?;
    let mut perf: DISK_PERFORMANCE = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    // SAFETY: 输出缓冲是一个完整的 DISK_PERFORMANCE。
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            h.raw(),
            IOCTL_DISK_PERFORMANCE,
            std::ptr::null(),
            0,
            (&raw mut perf).cast::<core::ffi::c_void>(),
            std::mem::size_of::<DISK_PERFORMANCE>() as u32,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    Some(DiskCounters {
        read_bytes: perf.BytesRead.max(0) as u64,
        write_bytes: perf.BytesWritten.max(0) as u64,
        read_ops: u64::from(perf.ReadCount),
        write_ops: u64::from(perf.WriteCount),
        busy_100ns: (perf.ReadTime.max(0) as u64).saturating_add(perf.WriteTime.max(0) as u64),
        idle_100ns: perf.IdleTime.max(0) as u64,
    })
}

/// 打开一个设备用于 `DeviceIoControl`。
///
/// # 为什么 `dwDesiredAccess` 传 0
///
/// 0 表示「只查询设备属性，不读写数据」。这类打开**不需要管理员**——
/// `IOCTL_DISK_PERFORMANCE`、`IOCTL_STORAGE_QUERY_PROPERTY`、
/// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS` 都在这个访问级别下工作。
/// 要 `GENERIC_READ` 才需要管理员，而那是读扇区才用得着的东西。
fn open_device(path: &str) -> io::Result<Owned> {
    let w = to_wide(path);
    // SAFETY: path 以 NUL 结尾；其余参数按文档给出。
    let h = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            w.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: h 刚由 CreateFileW 返回；Owned::new 会挡掉 INVALID_HANDLE_VALUE。
    unsafe { Owned::new(h) }
}

/// 一个路径上的文件属性里有没有只读位（供 fs provider 合成 mode）。
pub fn is_readonly_attr(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_READONLY != 0
}

/// `ERROR_MORE_DATA` 的转发，供调用方判断缓冲不足。
pub const MORE_DATA: u32 = ERROR_MORE_DATA;

/// 最近一次错误，转发以省去调用方的 `use`。
pub fn device_error() -> io::Error {
    last_error()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 本机至少有一个卷且含系统盘() {
        let vols = logical_volumes();
        assert!(!vols.is_empty(), "一台 Windows 至少有一个卷");
        let sys = vols
            .iter()
            .find(|v| v.mount_point.eq_ignore_ascii_case("C:\\"))
            .expect("本机应当有 C 盘");
        assert!(sys.total > 0);
        assert!(sys.used() <= sys.total);
        assert!(sys.available <= sys.free);
        assert!(!sys.fs_type.is_empty(), "文件系统类型不该为空");
        assert!(sys.is_fixed(), "C 盘应当是固定盘");
        assert!(sys.guid.starts_with(r"\\?\Volume{"), "卷 GUID：{}", sys.guid);
        eprintln!(
            "本机卷：{}",
            vols.iter()
                .map(|v| format!("{}({} {}/{})", v.mount_point, v.fs_type, v.used(), v.total))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    #[test]
    fn 挂载点按路径排序且不重复() {
        let vols = logical_volumes();
        let mounts: Vec<&str> = vols.iter().map(|v| v.mount_point.as_str()).collect();
        let mut sorted = mounts.clone();
        sorted.sort_unstable();
        assert_eq!(mounts, sorted, "必须按挂载点排序");
        let unique: std::collections::HashSet<&&str> = mounts.iter().collect();
        assert_eq!(unique.len(), mounts.len(), "挂载点重复了");
    }

    #[test]
    fn 物理盘可枚举并带容量() {
        let disks = physical_disks();
        assert!(!disks.is_empty(), "一台 Windows 至少有一块物理盘");
        for d in &disks {
            assert!(d.size_bytes > 0, "{} 容量为 0", d.name);
            assert_eq!(d.name, format!("PhysicalDrive{}", d.number));
        }
        eprintln!(
            "本机物理盘：{}",
            disks
                .iter()
                .map(|d| format!(
                    "{}({}, {} GiB, rot={:?})",
                    d.name,
                    d.model.as_deref().unwrap_or("未知型号"),
                    d.size_bytes / (1 << 30),
                    d.rotational
                ))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    #[test]
    fn 系统盘能关联到物理盘() {
        let vols = logical_volumes();
        let Some(sys) = vols.iter().find(|v| v.is_fixed()) else {
            return;
        };
        let disks = sys.disk_numbers();
        assert!(!disks.is_empty(), "固定盘应当能问出承载它的物理盘号");
        let known: Vec<u32> = physical_disks().into_iter().map(|d| d.number).collect();
        for n in &disks {
            assert!(known.contains(n), "盘号 {n} 不在枚举出的物理盘里");
        }
    }

    #[test]
    fn 磁盘计数器单调增长() {
        let Some(first) = physical_disks().first().map(|d| d.number) else {
            return;
        };
        let a = disk_counters(first).expect("物理盘的性能计数默认是开的");
        std::thread::sleep(std::time::Duration::from_millis(120));
        let b = disk_counters(first).unwrap();
        assert!(b.read_bytes >= a.read_bytes, "读字节数倒退");
        assert!(b.write_bytes >= a.write_bytes, "写字节数倒退");
        assert!(b.idle_100ns >= a.idle_100ns, "空闲时间倒退");
        assert!(
            b.idle_100ns > a.idle_100ns || b.busy_100ns > a.busy_100ns,
            "120ms 里既没空闲也没忙碌，计数器大概率没工作"
        );
    }

    #[test]
    fn 不存在的设备打不开而不是_panic() {
        assert!(open_device(r"\\.\PhysicalDrive999").is_err());
        assert!(disk_counters(999).is_none());
    }
}
