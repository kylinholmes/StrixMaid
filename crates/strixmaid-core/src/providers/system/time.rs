//! 时间与时区：`/etc/localtime` / `/etc/timezone`、`adjtimex(2)` 同步状态、NTP 服务探测。
//!
//! **不调 `timedatectl`**：它需要 systemd-timedated 与 DBus；这里的每一项都能直接读出来。

use std::fs;
use std::path::Path;

use strixmaid_types::system::TimeInfo;

use super::util::{read_trimmed, unix_now};

/// 采集时间信息。
pub fn read_time_info() -> TimeInfo {
    let ts = unix_now();
    let ntp = detect_ntp();
    TimeInfo {
        ts,
        timezone: read_timezone(),
        utc_offset_secs: utc_offset_secs(ts),
        ntp_enabled: ntp.enabled,
        ntp_synchronized: ntp_synchronized(),
        ntp_service: ntp.service,
        rtc_local: rtc_local(),
    }
}

/// IANA 时区名。`/etc/localtime` 软链目标 → `/etc/timezone` → `"UTC"`。
pub fn read_timezone() -> String {
    fs::read_link("/etc/localtime")
        .ok()
        .and_then(|target| timezone_from_link_target(&target))
        .or_else(|| read_trimmed("/etc/timezone"))
        .unwrap_or_else(|| "UTC".to_owned())
}

/// 从 `/etc/localtime` 的链接目标里截出 `Area/City`。
///
/// 目标可能是绝对路径（`/usr/share/zoneinfo/Asia/Shanghai`）或相对路径
/// （`../usr/share/zoneinfo/Asia/Shanghai`），统一找 `zoneinfo/` 之后的部分。
pub fn timezone_from_link_target(target: &Path) -> Option<String> {
    let s = target.to_str()?;
    let idx = s.rfind("zoneinfo/")?;
    let tz = &s[idx + "zoneinfo/".len()..];
    // 去掉 posix/ 与 right/ 这两个变体目录前缀
    let tz = tz
        .strip_prefix("posix/")
        .or_else(|| tz.strip_prefix("right/"))
        .unwrap_or(tz);
    if tz.is_empty() {
        None
    } else {
        Some(tz.to_owned())
    }
}

/// 当前时区相对 UTC 的偏移（秒，东为正），含夏令时。
///
/// 走 libc 的 `localtime_r`，它读 `/etc/localtime`（glibc 与 musl 都支持，且 glibc 会按
/// 文件 mtime 自动重读，改时区后无需重启）。
pub fn utc_offset_secs(now: i64) -> i32 {
    let t: libc::time_t = now as libc::time_t;
    // SAFETY: tm 是纯 C 结构体，全零合法；localtime_r 只写入该结构体，失败返回空指针。
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // glibc / musl 的 localtime_r 内部会按需重读 /etc/localtime（等价于 tzset），
    // 不需要额外调用；libc crate 也没有为 Linux 导出 tzset。
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i32
}

/// 内核时钟是否已被 NTP 同步：`adjtimex(2)` 的 `STA_UNSYNC` 位。
///
/// `modes = 0` 是纯读取，不需要任何特权，容器内同样可用。调用失败返回 `None`。
pub fn ntp_synchronized() -> Option<bool> {
    // SAFETY: timex 是纯 C 结构体，全零（modes = 0）表示只读不改；adjtimex 只写入该结构体。
    let mut tx: libc::timex = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::adjtimex(&mut tx) };
    if rc < 0 {
        return None;
    }
    // TIME_ERROR(5) 表示时钟未同步；STA_UNSYNC 位是同一件事的另一个表达。
    Some(rc != libc::TIME_ERROR && (tx.status & libc::STA_UNSYNC) == 0)
}

/// NTP 服务探测结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NtpStatus {
    /// `None`：机器上没有任何已知的 NTP 实现；`Some(false)`：装了但没 enable。
    pub enabled: Option<bool>,
    /// 已 enable 的那个 unit；没有 enable 的则是第一个已安装的。
    pub service: Option<String>,
}

/// 已知的 NTP 实现对应的 systemd unit 名，按优先级排列。
pub const NTP_UNITS: &[&str] = &[
    "systemd-timesyncd.service",
    "chronyd.service",
    "chrony.service",
    "ntpd.service",
    "ntp.service",
    "ntpsec.service",
    "openntpd.service",
];

/// unit 文件可能所在的目录。
const UNIT_DIRS: &[&str] = &[
    "/etc/systemd/system",
    "/run/systemd/system",
    "/usr/local/lib/systemd/system",
    "/usr/lib/systemd/system",
    "/lib/systemd/system",
];

/// 探测 NTP：unit 文件存在即「已安装」，`/etc/systemd/system/*.wants/<unit>` 存在即「已 enable」。
pub fn detect_ntp() -> NtpStatus {
    let mut installed = Vec::new();
    for unit in NTP_UNITS {
        if UNIT_DIRS
            .iter()
            .any(|d| Path::new(d).join(unit).exists())
        {
            installed.push(*unit);
        }
    }
    if installed.is_empty() {
        return NtpStatus::default();
    }
    let enabled = installed.iter().find(|u| unit_is_enabled(u));
    NtpStatus {
        enabled: Some(enabled.is_some()),
        service: Some((*enabled.unwrap_or(&installed[0])).to_owned()),
    }
}

/// `/etc/systemd/system/*.wants/<unit>` 是否存在（`systemctl enable` 的产物）。
fn unit_is_enabled(unit: &str) -> bool {
    let Ok(entries) = fs::read_dir("/etc/systemd/system") else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        (name.ends_with(".wants") || name.ends_with(".requires"))
            && fs::symlink_metadata(e.path().join(unit)).is_ok()
    })
}

/// RTC 是否按本地时间存储：`/etc/adjtime` 第三行为 `LOCAL`。文件不存在时为 `None`。
pub fn rtc_local() -> Option<bool> {
    let raw = fs::read_to_string("/etc/adjtime").ok()?;
    let third = raw.lines().nth(2)?.trim();
    match third {
        "LOCAL" => Some(true),
        "UTC" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn 从链接目标截时区() {
        assert_eq!(
            timezone_from_link_target(&PathBuf::from("/usr/share/zoneinfo/Asia/Shanghai")).as_deref(),
            Some("Asia/Shanghai")
        );
        assert_eq!(
            timezone_from_link_target(&PathBuf::from("../usr/share/zoneinfo/Europe/Berlin")).as_deref(),
            Some("Europe/Berlin")
        );
        assert_eq!(
            timezone_from_link_target(&PathBuf::from("/usr/share/zoneinfo/posix/UTC")).as_deref(),
            Some("UTC")
        );
        assert_eq!(timezone_from_link_target(&PathBuf::from("/etc/foo")), None);
        assert_eq!(timezone_from_link_target(&PathBuf::from("/usr/share/zoneinfo/")), None);
    }

    #[test]
    fn 本机时间信息() {
        let t = read_time_info();
        assert!(!t.timezone.is_empty());
        assert!(t.utc_offset_secs.abs() <= 14 * 3600);
        // adjtimex 在普通 Linux 上总能调用成功
        assert!(t.ntp_synchronized.is_some());
    }
}
