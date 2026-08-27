//! macOS 的系统调用原语：`sysctl`、`getfsstat`、以及两个字符串小工具。
//!
//! # FFI 来源
//!
//! 除 `mach_host_self` / `mach_task_self` 外的符号与常量全部取自 `libc`
//! （`libc` 把那两个标了 `#[deprecated]`，注解明确指向 `mach2`，故那两处走 `mach2`）。
//! 每处 `unsafe` 单独标注安全前提。

use std::ffi::CString;

// ============================ sysctl ============================

/// 按名字读一个定长的 sysctl 标量或 POD 结构体
/// （`hw.memsize` / `hw.pagesize` / `vm.swapusage` / `kern.boottime` 这类）。
///
/// 返回 `None` 的情形：名字不存在、内核给出的长度与 `T` 不符。后者必须当失败处理——
/// 长度对不上意味着我们对该 sysctl 的类型判断是错的，读出来的值没有意义。
///
/// 只要求 `T: Copy` 而非 `T: Default`：`libc::xsw_usage` 这类 C 结构体没有 `Default`
/// 实现，而全零对它们都是合法的初始状态，用 `MaybeUninit::zeroed` 起手即可。
pub fn sysctl_scalar<T: Copy>(name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let mut value = std::mem::MaybeUninit::<T>::zeroed();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: cname 是以 NUL 结尾的合法 C 字符串；oldp 指向一块 len 字节的可写内存，
    // 且 len 如实描述其大小；newp 为空表示只读不写。
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            value.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len != std::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: 调用成功且长度与 T 精确相符，内核已把整个 T 写满。
    Some(unsafe { value.assume_init() })
}

/// 按名字读一个字符串型 sysctl（`kern.hostname` / `hw.model` / `kern.osrelease` …）。
///
/// 内核返回的是以 NUL 结尾的 C 字符串，长度包含那个 NUL，这里去掉。
pub fn sysctl_str(name: &str) -> Option<String> {
    let cname = CString::new(name).ok()?;
    let mut len: usize = 0;
    // SAFETY: oldp 为空时内核只把所需字节数写进 len。
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            std::ptr::null_mut(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }

    let mut buf = vec![0u8; len];
    let mut cap = len;
    // SAFETY: buf 有 cap 字节可写，cap 如实描述其大小。
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut cap,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(cap);
    // 去掉尾部 NUL（可能不止一个，取决于该 sysctl 的实现）
    while buf.last() == Some(&0) {
        buf.pop();
    }
    let s = String::from_utf8_lossy(&buf).into_owned();
    (!s.is_empty()).then_some(s)
}

/// 按 MIB 读一段变长的 sysctl 数据。
///
/// 先用 `oldp = NULL` 问所需长度，再按该长度分配并读取。两次调用之间内核数据可能变长
/// （典型如接口列表），因此多要 `slack` 字节的余量；仍然不够时返回 `None` 而不是重试
/// ——调用方都在周期性采集里，漏一轮远好过在这里打转。
pub fn sysctl_raw(mib: &mut [libc::c_int], slack: usize) -> Option<Vec<u8>> {
    let mut len: usize = 0;
    // SAFETY: mib 指向 mib.len() 个 c_int；oldp 为空时内核只把所需字节数写进 len。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            std::ptr::null_mut(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || len == 0 {
        return None;
    }

    let mut buf = vec![0u8; len + slack];
    let mut cap = buf.len();
    // SAFETY: 同上；oldp 指向 cap 字节的可写内存，内核把实际写入的字节数回填进 cap。
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as libc::c_uint,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &raw mut cap,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return None;
    }
    buf.truncate(cap);
    Some(buf)
}

/// 定长 C 字符数组转 `String`，在第一个 NUL 处截断，非 UTF-8 字节用 U+FFFD 替换。
///
/// `statfs` 的 `f_mntonname` 是 `[c_char; 1024]`，绝大多数内容是尾部的 NUL。
pub fn c_array_to_string(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 本机页大小（字节）。mach 的计数几乎全是「页数」，换算成字节都要它。
///
/// Apple Silicon 是 16 KiB，Intel Mac 是 4 KiB；读不到时按 4 KiB 兜底，
/// 只会让数值偏小，不会让采集整轮失败。
pub fn page_size() -> u64 {
    sysctl_scalar::<u32>("hw.pagesize").map_or(4096, u64::from)
}

// ============================ 挂载表 ============================

/// `MNT_RDONLY`：只读挂载。`libc` 在 BSD 公共模块里有这个常量，
/// 这里给它一个本地别名只为让引用点读起来更直白。
pub const MNT_RDONLY: u32 = libc::MNT_RDONLY as u32;

/// 一个挂载点的原始快照，字段直接对应 `struct statfs`。
///
/// 不含任何过滤或口径判断——`used` / `usage_percent` 是 `df` 的定义，
/// 哪些挂载点值得展示由调用方决定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// 挂载点路径，`f_mntonname`。
    pub mount_point: String,
    /// 挂载来源，`f_mntfromname`（`/dev/disk3s1s1` / `map auto_home` …）。
    pub device: String,
    /// 文件系统类型，`f_fstypename`（`apfs` / `devfs` / `autofs` …）。
    pub fstype: String,
    /// 总容量（字节）。
    pub total: u64,
    /// 空闲（含只有 root 能用的保留块，字节）。
    pub free: u64,
    /// 非特权可用（字节）。
    pub available: u64,
    /// inode 总数。APFS 报的是一个极大的理论上限；报 0 表示不统计。
    pub inodes_total: u64,
    /// 空闲 inode 数。
    pub inodes_free: u64,
    /// 是否只读挂载。
    pub read_only: bool,
}

impl Mount {
    /// 已用字节，口径同 `df`：`total − free`。
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// 使用率百分比。分母是 `used + available`，**不含 root 保留块**——
    /// 这样 95% 时普通用户已经写不进去，与体感一致。
    /// 分母为 0（空文件系统）时返回 0 而不是 NaN。
    pub fn usage_percent(&self) -> f64 {
        let used = self.used();
        let denom = used.saturating_add(self.available);
        if denom == 0 {
            0.0
        } else {
            used as f64 * 100.0 / denom as f64
        }
    }

    /// 已用 inode 数；`inodes_total` 为 0 时返回 `None`。
    pub fn inodes_used(&self) -> Option<u64> {
        (self.inodes_total > 0).then(|| self.inodes_total.saturating_sub(self.inodes_free))
    }
}

/// 枚举全部挂载点。
///
/// # 为什么是 `getfsstat` 而不是 `getmntinfo`
///
/// `getmntinfo(3)` 把结果放在库内部的**静态缓冲**里，多线程同时调用会互相踩。
/// `getfsstat(2)` 写进调用方给的缓冲，天然可重入。指标采集跑在阻塞线程池里，
/// 与 system provider 的挂载枚举可能并发，必须用后者。
///
/// `MNT_NOWAIT` 也是关键：它表示用内核缓存的统计值、不向文件系统发起同步请求。
/// `MNT_WAIT` 会在挂死的远程挂载上无限期阻塞。
pub fn mounts() -> Option<Vec<Mount>> {
    // SAFETY: buf 为空时 getfsstat 只返回挂载数，不写任何数据。
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return None;
    }

    // 两次调用之间可能有新卷挂上，多留几个位置；getfsstat 只会填满缓冲，不会越界。
    let cap = n as usize + 8;
    let mut buf: Vec<libc::statfs> = Vec::with_capacity(cap);
    let bytes = (cap * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: buf 有 cap 个 statfs 的容量，bufsize 如实描述其字节数。
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if got < 0 {
        return None;
    }
    // SAFETY: getfsstat 已初始化前 got 个元素（got <= cap，由 bufsize 保证）。
    unsafe { buf.set_len(got as usize) };

    Some(
        buf.iter()
            .map(|fs| {
                let block = u64::from(fs.f_bsize);
                Mount {
                    mount_point: c_array_to_string(&fs.f_mntonname),
                    device: c_array_to_string(&fs.f_mntfromname),
                    fstype: c_array_to_string(&fs.f_fstypename),
                    total: fs.f_blocks.saturating_mul(block),
                    free: fs.f_bfree.saturating_mul(block),
                    available: fs.f_bavail.saturating_mul(block),
                    inodes_total: fs.f_files,
                    inodes_free: fs.f_ffree,
                    read_only: fs.f_flags & MNT_RDONLY != 0,
                }
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 标量_sysctl() {
        let mem: u64 = sysctl_scalar("hw.memsize").expect("hw.memsize 在任何 macOS 上都存在");
        assert!(mem > 0);
        assert!(page_size().is_power_of_two());
        assert_eq!(sysctl_scalar::<u64>("no.such.sysctl"), None, "名字不存在");
        // hw.memsize 是 8 字节，按 u32 读必须失败而不是给个截断值
        assert_eq!(sysctl_scalar::<u32>("hw.memsize"), None, "宽度不符");
    }

    #[test]
    fn 字符串_sysctl() {
        let arch = sysctl_str("hw.machine").expect("hw.machine");
        assert!(!arch.is_empty());
        assert!(!arch.contains('\0'), "尾部 NUL 必须去掉：{arch:?}");
        assert!(sysctl_str("kern.osrelease").is_some());
        assert_eq!(sysctl_str("no.such.sysctl"), None);
    }

    #[test]
    fn 变长_sysctl() {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
        assert!(!sysctl_raw(&mut mib, 4096).expect("进程列表").is_empty());
    }

    #[test]
    fn c_数组转字符串() {
        let mut buf = [0 as libc::c_char; 8];
        for (i, b) in b"/tmp".iter().enumerate() {
            buf[i] = *b as libc::c_char;
        }
        assert_eq!(c_array_to_string(&buf), "/tmp");
        assert_eq!(c_array_to_string(&[0 as libc::c_char; 4]), "");
    }

    #[test]
    fn 本机挂载表含根() {
        let all = mounts().expect("getfsstat");
        let root = all.iter().find(|m| m.mount_point == "/").expect("根挂载");
        assert!(root.total > 0);
        assert!(root.used() <= root.total);
        assert!((0.0..=100.0).contains(&root.usage_percent()));
        assert!(!root.fstype.is_empty());
        // 现代 macOS 的根卷是只读的 System 卷
        eprintln!(
            "/ = {} ({}), 只读={}, 使用率={:.1}%",
            root.device,
            root.fstype,
            root.read_only,
            root.usage_percent()
        );
    }

    #[test]
    fn 用量口径与_df_一致() {
        let m = Mount {
            mount_point: "/".into(),
            device: "/dev/disk1".into(),
            fstype: "apfs".into(),
            total: 100,
            free: 40,
            available: 30,
            inodes_total: 0,
            inodes_free: 0,
            read_only: false,
        };
        assert_eq!(m.used(), 60);
        assert!((m.usage_percent() - 200.0 / 3.0).abs() < 1e-9);
        assert_eq!(m.inodes_used(), None, "不统计 inode 时不该编一个数出来");

        let empty = Mount {
            total: 0,
            free: 0,
            available: 0,
            ..m.clone()
        };
        assert_eq!(empty.usage_percent(), 0.0, "空文件系统不产生 NaN");

        let with_inodes = Mount {
            inodes_total: 1000,
            inodes_free: 900,
            ..m
        };
        assert_eq!(with_inodes.inodes_used(), Some(100));
    }
}
