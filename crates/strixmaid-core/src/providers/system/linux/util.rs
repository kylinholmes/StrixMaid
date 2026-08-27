//! system / process provider 共用的小工具：读 procfs / sysfs 小文件、当前时间、`uname`。
//!
//! 这里的每个函数都遵守 `docs/design.md` §1 的降级原则：**读不到就返回 `None`**，
//! 绝不 panic、绝不让整个采集失败——容器、非 root、精简发行版里缺文件是常态。

use std::ffi::CStr;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// 当前 unix 秒。
///
/// 系统时钟早于 1970 只可能是 RTC 没电，退回 0 比 panic 好。
pub(crate) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 读一个小文本文件并去掉首尾空白；读不到或内容为空时返回 `None`。
pub(crate) fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// 读一个只含一个无符号整数的文件（sysfs 里大量存在）。
pub(crate) fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_trimmed(path)?.parse().ok()
}

/// 读一个 `0` / `1` 形式的布尔文件（sysfs 的 `removable` / `ro` / `rotational`）。
pub(crate) fn read_bool(path: impl AsRef<Path>) -> Option<bool> {
    read_u64(path).map(|v| v != 0)
}

/// 运行期 `uname -m`。
///
/// **不用编译期的 `std::env::consts::ARCH`**：i686 二进制跑在 x86_64 内核上、
/// 或 armv7 二进制跑在 aarch64 上时二者不同，而运维关心的是内核在跑什么。
pub(crate) fn uname_machine() -> Option<String> {
    // SAFETY: utsname 是纯 C 结构体，全零是合法初始状态；uname(2) 只向该缓冲区写入
    // 以 NUL 结尾的字符串，成功后 `machine` 保证 NUL 终止。
    let mut uts: libc::utsname = unsafe { std::mem::zeroed() };
    if unsafe { libc::uname(&mut uts) } != 0 {
        return None;
    }
    let machine = unsafe { CStr::from_ptr(uts.machine.as_ptr()) };
    let s = machine.to_string_lossy();
    if s.is_empty() {
        None
    } else {
        Some(s.into_owned())
    }
}

/// 解析 `/proc/meminfo` 风格的 `Key:   123 kB` 行，返回**字节数**。
///
/// 只认 `kB`（内核实际只用这个单位）；没有单位的行按字节处理。
pub(crate) fn meminfo_value(raw: &str, key: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        if k.trim() != key {
            return None;
        }
        let mut parts = v.split_whitespace();
        let num: u64 = parts.next()?.parse().ok()?;
        Some(match parts.next() {
            Some("kB") | Some("KB") | Some("kb") => num.saturating_mul(1024),
            _ => num,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meminfo_解析带单位的行() {
        let raw = "MemTotal:       16384 kB\nMemFree:  100 kB\nHugePagesTotal:  0\n";
        assert_eq!(meminfo_value(raw, "MemTotal"), Some(16384 * 1024));
        assert_eq!(meminfo_value(raw, "HugePagesTotal"), Some(0));
        assert_eq!(meminfo_value(raw, "Nope"), None);
    }

    #[test]
    fn uname_machine_在本机可读() {
        let m = uname_machine().expect("uname 必然成功");
        assert!(!m.is_empty());
    }
}
