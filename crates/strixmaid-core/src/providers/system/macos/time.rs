//! 时间与时区：`/etc/localtime` 软链 + `localtime_r(3)`。
//!
//! # NTP 状态在 macOS 上读不到
//!
//! Linux 侧靠 `adjtimex(2)` 的 `STA_UNSYNC` 标志与 `/etc/*.conf` 判断 NTP 是否启用、
//! 是否已同步。macOS 的时间同步由 `timed` 守护进程负责，它既不暴露 `adjtimex` 的
//! 同步位，也没有可读的状态文件；唯一的官方查询方式 `systemsetup -getusingnetworktime`
//! 需要 root，且是个子进程。
//!
//! 因此三个 `ntp_*` 字段一律为 `None`——`TimeInfo` 把它们定义成 `Option` 正是为了
//! 表达「不知道」，填 `Some(false)` 会被前端显示成「NTP 已关闭」，那是撒谎。
//!
//! `rtc_local` 填 `Some(false)`：Mac 的硬件时钟固定走 UTC，这一点是确定的。

use std::path::Path;

use strixmaid_types::system::TimeInfo;

/// 当前 unix 时间戳（秒）。
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 采集时间信息。
pub fn read_time_info() -> TimeInfo {
    let ts = unix_now();
    TimeInfo {
        ts,
        timezone: read_timezone(),
        utc_offset_secs: utc_offset_secs(ts),
        // 见模块文档
        ntp_enabled: None,
        ntp_synchronized: None,
        ntp_service: None,
        rtc_local: Some(false),
    }
}

/// IANA 时区名。
///
/// macOS 的 `/etc/localtime` 是指向 `/var/db/timezone/zoneinfo/Asia/Shanghai`
/// 的软链（老版本是 `/usr/share/zoneinfo/...`），两者都含 `zoneinfo/` 段，
/// 用同一套截取规则。
pub fn read_timezone() -> String {
    std::fs::read_link("/etc/localtime")
        .ok()
        .and_then(|target| timezone_from_link_target(&target))
        .unwrap_or_else(|| "UTC".to_owned())
}

/// 从 `/etc/localtime` 的链接目标里截出 `Area/City`。
pub fn timezone_from_link_target(target: &Path) -> Option<String> {
    let s = target.to_str()?;
    let idx = s.rfind("zoneinfo/")?;
    let tz = &s[idx + "zoneinfo/".len()..];
    // 去掉 posix/ 与 right/ 这两个变体目录前缀
    let tz = tz
        .strip_prefix("posix/")
        .or_else(|| tz.strip_prefix("right/"))
        .unwrap_or(tz);
    (!tz.is_empty()).then(|| tz.to_owned())
}

/// 当前时区相对 UTC 的偏移（秒，东为正），含夏令时。
pub fn utc_offset_secs(now: i64) -> i32 {
    let t = now as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: 两个指针都指向本栈帧上合法的内存；localtime_r 是可重入版本，
    // 不使用全局缓冲，成功时返回指向 tm 的指针。
    let p = unsafe { libc::localtime_r(&raw const t, tm.as_mut_ptr()) };
    if p.is_null() {
        return 0;
    }
    // SAFETY: localtime_r 返回非空即表示 tm 已完整初始化。
    let tm = unsafe { tm.assume_init() };
    i32::try_from(tm.tm_gmtoff).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 从软链目标截时区名() {
        assert_eq!(
            timezone_from_link_target(Path::new("/var/db/timezone/zoneinfo/Asia/Shanghai"))
                .as_deref(),
            Some("Asia/Shanghai")
        );
        assert_eq!(
            timezone_from_link_target(Path::new("/usr/share/zoneinfo/UTC")).as_deref(),
            Some("UTC")
        );
        assert_eq!(
            timezone_from_link_target(Path::new("../usr/share/zoneinfo/posix/Europe/Berlin"))
                .as_deref(),
            Some("Europe/Berlin"),
            "posix/ 变体前缀要去掉"
        );
        assert_eq!(timezone_from_link_target(Path::new("/etc/localtime")), None);
        assert_eq!(timezone_from_link_target(Path::new("/x/zoneinfo/")), None);
    }

    #[test]
    fn 本机时间信息() {
        let t = read_time_info();
        assert!(t.ts > 1_700_000_000, "时间戳明显不对：{}", t.ts);
        assert!(!t.timezone.is_empty());
        // 地球上的 UTC 偏移在 ±14 小时以内
        assert!(
            (-14 * 3600..=14 * 3600).contains(&t.utc_offset_secs),
            "偏移越界：{}",
            t.utc_offset_secs
        );
        assert_eq!(t.rtc_local, Some(false), "Mac 的 RTC 走 UTC");
        assert_eq!(t.ntp_enabled, None, "读不到就该是 None，不能编 false");
        assert_eq!(t.ntp_synchronized, None);
        eprintln!("本机时间：{}", serde_json::to_string(&t).unwrap());
    }
}
