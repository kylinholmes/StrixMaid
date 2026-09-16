//! Windows 平台原语：宽字符、句柄 RAII、注册表、令牌/SID、卷、ntdll、PDH。
//!
//! 与 [`crate::platform::macos`] 同一定位：这一层刻意很薄，只放**不含业务判断**
//! 的东西——一次 `RegGetValueW`、一次 `LookupAccountSidW`、一次
//! `NtQuerySystemInformation`。业务口径（哪些卷该采、内存怎么算「可用」、
//! 服务状态怎么映射成 `UnitActiveState`）一律留在调用方。
//!
//! # 为什么是 `windows-sys` 而不是 `windows`
//!
//! 与 helper 的自写 PAM FFI、[`crate::platform::iokit`] 同一取向：只声明用得到
//! 的那部分表面。`windows` crate 的 COM/WinRT 投影会把一个只读观测工具的依赖树
//! 撑大一个数量级，而我们用到的全是 C 函数。
//!
//! # ntdll 的两个函数是手写的
//!
//! `NtQuerySystemInformation` / `NtQueryInformationProcess` 不在 `windows-sys` 的
//! 公开表面里（它们属于「半公开」API）。但**没有它们就没有一次取全的进程表**：
//! 替代方案 `CreateToolhelp32Snapshot` 拿不到 CPU 时间与 IO 计数，只能对每个进程
//! 再开一次句柄、调三四个 API——几百个进程就是上千次系统调用。
//! `SystemProcessInformation` 一次调用给出全部进程的 pid/ppid/名字/线程数/
//! CPU 时间/内存/IO 计数，这正是 Linux 侧读 `/proc` 的等价物。见 [`ntdll`]。
//!
//! 每处 `unsafe` 都单独标注其安全前提。

pub mod handle;
pub mod ntdll;
pub mod pdh;
pub mod registry;
pub mod token;
pub mod volume;
pub mod wide;

pub use handle::{OwnedHandleExt, close_handle, invalid_handle};
pub use ntdll::{boot_time_unix, system_processes};
pub use registry::{HKLM, RegRoot, reg_dword, reg_qword, reg_string, reg_subkeys};
pub use token::{
    AccountName, SID_ADMINISTRATORS, TokenIdentity, account_by_sid, current_identity, is_elevated,
    rid_of_sid, sid_to_string,
};
pub use volume::{Volume, logical_volumes};
pub use wide::{from_wide, from_wide_nul, to_wide};

use std::io;

use windows_sys::Win32::Foundation::GetLastError;

/// 最近一次 Win32 错误，转成 [`io::Error`]。
///
/// 只在紧挨着失败的那一行调用——`GetLastError` 是线程局部的，中间隔一次
/// 别的系统调用就可能被覆盖。
pub fn last_error() -> io::Error {
    // SAFETY: GetLastError 无参数、无副作用，只读线程局部的错误码。
    io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)
}

/// 按 Win32 错误码构造 [`io::Error`]（用于直接返回错误码的 API，如注册表函数）。
pub fn error_from_code(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code as i32)
}

/// 100 纳秒为单位的 Windows 文件时间 → unix 秒。
///
/// FILETIME 的纪元是 1601-01-01，unix 是 1970-01-01，相差 11644473600 秒。
/// 时间早于 unix 纪元（理论上只有系统时钟被改到 1970 之前才会出现）时返回 0，
/// 而不是一个负数时间戳——DTO 里的 `ts` 全是「unix 秒」，负值只会让前端显示成
/// 1969 年，比 0 更难解释。
pub const fn filetime_to_unix(ft: u64) -> i64 {
    const EPOCH_DIFF_100NS: u64 = 116_444_736_000_000_000;
    if ft < EPOCH_DIFF_100NS {
        return 0;
    }
    ((ft - EPOCH_DIFF_100NS) / 10_000_000) as i64
}

/// 把 `FILETIME` 的高低两半拼成 u64（Win32 结构体里它常以两个 u32 出现）。
pub const fn filetime_u64(low: u32, high: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

/// 当前 unix 时间戳（秒）。
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 文件时间换算() {
        // 1970-01-01T00:00:00Z 正好是 FILETIME 的 116444736000000000
        assert_eq!(filetime_to_unix(116_444_736_000_000_000), 0);
        // 加一天
        assert_eq!(
            filetime_to_unix(116_444_736_000_000_000 + 864_000_000_000),
            86_400
        );
        // 早于 unix 纪元：夹到 0，不产生负时间戳
        assert_eq!(filetime_to_unix(0), 0);
        assert_eq!(filetime_to_unix(1), 0);
    }

    #[test]
    fn 高低位拼接() {
        assert_eq!(filetime_u64(0x3333_4444, 0x1111_2222), 0x1111_2222_3333_4444);
        assert_eq!(filetime_u64(0, 0), 0);
        assert_eq!(filetime_u64(u32::MAX, 0), u64::from(u32::MAX));
    }

    #[test]
    fn 本机时间戳合理() {
        // 2020-01-01 之后、2100 年之前
        let now = unix_now();
        assert!(now > 1_577_836_800, "时间戳明显不对：{now}");
        assert!(now < 4_102_444_800, "时间戳明显不对：{now}");
    }

    #[test]
    fn 错误码转换保留原值() {
        // ERROR_FILE_NOT_FOUND
        assert_eq!(error_from_code(2).raw_os_error(), Some(2));
        assert_eq!(error_from_code(5).kind(), std::io::ErrorKind::PermissionDenied);
    }
}
