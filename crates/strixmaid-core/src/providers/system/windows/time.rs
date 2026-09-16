//! 时间与时区：`GetDynamicTimeZoneInformation` + W32Time 的注册表配置。
//!
//! # ⚠ `timezone` 返回的是 Windows 时区名，不是 IANA 名
//!
//! **这是本模块与 DTO 注释的一处明确偏离，必须记录在案。**
//!
//! [`TimeInfo::timezone`](strixmaid_types::system::TimeInfo::timezone) 的注释写的是
//! 「IANA 时区名」，Linux 与 macOS 侧从 `/etc/localtime` 的软链目标里截出
//! `Asia/Shanghai` 这种串。Windows 没有 IANA 时区数据库——它有自己的一套
//! 时区标识（注册表 `…\Time Zones` 下的键名，如 `China Standard Time`、
//! `W. Europe Standard Time`），两套命名**不是一一对应**
//! （一个 Windows 时区常对应多个 IANA 时区）。
//!
//! 可选的做法只有三种：
//!
//! 1. 内置一张 CLDR 的 `windowsZones.xml` 映射表——几百行数据，还要跟着
//!    CLDR 的发布节奏更新，且反向映射本就是多对一，只能挑一个「代表」IANA 名，
//!    那等于替用户做了一个他没做过的选择；
//! 2. 找系统里的现成映射——**没有**。注册表的时区键里只有显示名与 `TZI` 偏移
//!    数据，没有任何 IANA 标识；ICU 的 `ucal_getTimeZoneIDForWindowsID` 要链
//!    `icu.dll`（Win10 1703+ 才有），为一个展示字段引入一个可选系统 DLL 不划算；
//! 3. **如实返回 Windows 时区的 `TimeZoneKeyName`**，并把这处差异写清楚。
//!
//! 选 3。`utc_offset_secs` 在三个平台上口径完全一致（都是真实的秒数偏移），
//! 前端做时间换算只需要它；`timezone` 是给人看的标识，显示
//! `China Standard Time` 不会让任何人误解，而显示一个凭映射表猜出来的
//! `Asia/Shanghai` 反倒可能与用户实际选的时区不符。
//!
//! # `Bias` 的符号与直觉相反
//!
//! Windows 的定义是 **`UTC = 本地时间 + Bias`**（单位分钟）。东八区的 `Bias`
//! 是 `-480`，不是 `+480`。加上当前生效的 `StandardBias` / `DaylightBias` 之后
//! 取负、乘 60，才得到 DTO 要的「相对 UTC 的偏移，东为正，秒」。
//! 见 [`utc_offset_secs`]，那是个纯函数，用固定输入单测。
//!
//! # NTP 状态
//!
//! | 字段 | 取法 |
//! |---|---|
//! | `ntp_enabled` | `…\Services\W32Time\Parameters` 的 `Type`：`NTP` / `NT5DS`（域同步）/ `AllSync` 为启用，`NoSync` 为关闭，读不到为 `None` |
//! | `ntp_service` | 固定 `W32Time`（Windows Time 服务），仅在该服务的注册表键存在时报 |
//! | `ntp_synchronized` | **`None`**：W32Time 没有一个可读的「已同步」位。`w32tm /query /status` 的「上次成功同步时间」是服务通过 RPC 现算的，不落注册表；`…\W32Time\Config` 下全是配置项，没有状态。读不到就报 `None`，**不编 `false`**——那会被前端显示成「时钟未同步」 |

use strixmaid_types::system::TimeInfo;

use crate::platform::windows::{HKLM, reg_dword, reg_string};
use crate::platform::windows::wide::from_wide_nul;

/// Windows Time 服务的参数键。
pub const W32TIME_PARAMETERS: &str = r"SYSTEM\CurrentControlSet\Services\W32Time\Parameters";
/// Windows Time 服务本身的键（存在即说明这台机器有这个服务）。
pub const W32TIME_SERVICE: &str = r"SYSTEM\CurrentControlSet\Services\W32Time";
/// 当前时区信息的注册表镜像。
pub const TIMEZONE_INFORMATION: &str = r"SYSTEM\CurrentControlSet\Control\TimeZoneInformation";

/// `TIME_ZONE_ID_DAYLIGHT`：当前处于夏令时。
const TIME_ZONE_ID_DAYLIGHT: u32 = 2;
/// `TIME_ZONE_ID_INVALID`：调用失败。
const TIME_ZONE_ID_INVALID: u32 = u32::MAX;

/// 当前 unix 时间戳（秒）。
pub fn unix_now() -> i64 {
    crate::platform::windows::unix_now()
}

/// 采集时间信息。任何一项读不到都退化成兜底值，不会失败。
pub fn read_time_info() -> TimeInfo {
    let tz = read_time_zone();
    TimeInfo {
        ts: unix_now(),
        timezone: tz
            .as_ref()
            .map(|t| t.key_name.clone())
            .filter(|s| !s.is_empty())
            // API 没给键名时退回注册表里的同名值，再不行才是 UTC
            // （与 Linux / macOS 读不到 `/etc/localtime` 时的兜底一致）。
            .or_else(|| reg_string(HKLM, TIMEZONE_INFORMATION, "TimeZoneKeyName"))
            .unwrap_or_else(|| "UTC".to_owned()),
        utc_offset_secs: tz.as_ref().map_or(0, |t| t.offset_secs),
        ntp_enabled: ntp_enabled(),
        // 见模块文档「NTP 状态」
        ntp_synchronized: None,
        ntp_service: crate::platform::windows::registry::open_read(HKLM, W32TIME_SERVICE)
            .is_ok()
            .then(|| "W32Time".to_owned()),
        rtc_local: Some(rtc_local()),
    }
}

/// 从 `GetDynamicTimeZoneInformation` 解出的时区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeZone {
    /// `TimeZoneKeyName`，如 `China Standard Time`。见模块文档的偏离说明。
    pub key_name: String,
    /// 当前生效的 UTC 偏移，秒，东为正，含夏令时。
    pub offset_secs: i32,
}

/// 读一次当前时区。调用失败返回 `None`。
pub fn read_time_zone() -> Option<TimeZone> {
    use windows_sys::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, GetDynamicTimeZoneInformation,
    };

    let mut info = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: info 是一个完整的、已零初始化的结构体，API 只往里写。
    let id = unsafe { GetDynamicTimeZoneInformation(&raw mut info) };
    if id == TIME_ZONE_ID_INVALID {
        return None;
    }
    Some(TimeZone {
        key_name: from_wide_nul(&info.TimeZoneKeyName),
        offset_secs: utc_offset_secs(info.Bias, info.StandardBias, info.DaylightBias, id),
    })
}

/// `TIME_ZONE_INFORMATION` 的三个 bias → DTO 要的「东为正的秒数偏移」。
///
/// Windows 的定义是 `UTC = 本地 + Bias + 当前生效的 XxxBias`（分钟），
/// 所以要取负。纯函数，可用固定输入单测。
///
/// 结果夹在 ±14 小时内：地球上没有超出这个范围的时区，超出只能是数据异常，
/// 与其把一个荒唐的偏移发给前端，不如夹到边界。
pub fn utc_offset_secs(bias: i32, standard_bias: i32, daylight_bias: i32, tz_id: u32) -> i32 {
    let current = if tz_id == TIME_ZONE_ID_DAYLIGHT {
        daylight_bias
    } else {
        // TIME_ZONE_ID_STANDARD 与 TIME_ZONE_ID_UNKNOWN（该时区没有夏令时规则）
        // 都按标准时间算。
        standard_bias
    };
    let minutes = bias.saturating_add(current);
    minutes
        .saturating_neg()
        .saturating_mul(60)
        .clamp(-14 * 3600, 14 * 3600)
}

/// 是否启用了网络时间同步，见模块文档。
fn ntp_enabled() -> Option<bool> {
    let ty = reg_string(HKLM, W32TIME_PARAMETERS, "Type")?;
    match ty.trim() {
        // NTP：直接对 NTP 服务器同步；NT5DS：跟域控同步；AllSync：两者都试。
        "NTP" | "NT5DS" | "AllSync" => Some(true),
        "NoSync" => Some(false),
        // 未知取值：不猜。
        _ => None,
    }
}

/// 硬件时钟是否按本地时间存储。
///
/// Windows **默认把 RTC 当本地时间**（这正是 Windows/Linux 双系统时钟对不上的
/// 根源）。只有显式设置了 `RealTimeIsUniversal = 1` 时 RTC 才走 UTC。
/// 注册表里没有这一项等于用的是默认值，所以报 `true` 不是编数据，
/// 而是如实反映平台的默认行为。
fn rtc_local() -> bool {
    reg_dword(HKLM, TIMEZONE_INFORMATION, "RealTimeIsUniversal") != Some(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TZ_UNKNOWN: u32 = 0;
    const TZ_STANDARD: u32 = 1;

    #[test]
    fn 偏移换算的符号() {
        // 东八区（中国 / 新加坡）：Bias = -480，没有夏令时
        assert_eq!(utc_offset_secs(-480, 0, 0, TZ_UNKNOWN), 8 * 3600);
        assert_eq!(utc_offset_secs(-480, 0, 0, TZ_STANDARD), 8 * 3600);
        // 西五区（美东）标准时间：Bias = 300
        assert_eq!(utc_offset_secs(300, 0, -60, TZ_STANDARD), -5 * 3600);
        // 美东夏令时：DaylightBias = -60 生效 → UTC-4
        assert_eq!(
            utc_offset_secs(300, 0, -60, TIME_ZONE_ID_DAYLIGHT),
            -4 * 3600
        );
        // 中欧夏令时：Bias = -60、DaylightBias = -60 → UTC+2
        assert_eq!(
            utc_offset_secs(-60, 0, -60, TIME_ZONE_ID_DAYLIGHT),
            2 * 3600
        );
        // UTC 本身
        assert_eq!(utc_offset_secs(0, 0, 0, TZ_UNKNOWN), 0);
        // 半小时时区（印度 UTC+5:30）
        assert_eq!(utc_offset_secs(-330, 0, 0, TZ_UNKNOWN), 19_800);
        // 非零 StandardBias（少见，但规范允许）
        assert_eq!(utc_offset_secs(-480, -30, 0, TZ_STANDARD), 8 * 3600 + 1800);
    }

    #[test]
    fn 异常偏移被夹住而不是溢出() {
        assert_eq!(utc_offset_secs(i32::MIN, 0, 0, TZ_UNKNOWN), 14 * 3600);
        assert_eq!(utc_offset_secs(i32::MAX, 0, 0, TZ_UNKNOWN), -14 * 3600);
        assert_eq!(utc_offset_secs(i32::MIN, i32::MIN, 0, TZ_STANDARD), 14 * 3600);
    }

    #[test]
    fn 本机时区() {
        let tz = read_time_zone().expect("GetDynamicTimeZoneInformation 应可用");
        assert!(!tz.key_name.is_empty(), "本机应有时区键名");
        assert!(
            (-14 * 3600..=14 * 3600).contains(&tz.offset_secs),
            "偏移越界：{}",
            tz.offset_secs
        );
        // 偏移必须是整分钟（世界上没有秒级偏移的现行时区）
        assert_eq!(tz.offset_secs % 60, 0);
        eprintln!("本机时区：{} / {} 秒", tz.key_name, tz.offset_secs);
    }

    /// 本机偏移必须与标准库算出来的一致——这是对 `Bias` 符号最硬的一条检验。
    #[test]
    fn 偏移与系统本地时间自洽() {
        use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

        // SAFETY: 两个 API 只往传入的结构体里写。
        let (local, utc) = unsafe {
            let mut l = std::mem::zeroed();
            let mut u = std::mem::zeroed();
            GetLocalTime(&raw mut l);
            GetSystemTime(&raw mut u);
            (l, u)
        };
        // 只比小时与分钟，避开跨日的复杂度：把两者都折算成「当天的分钟数」，
        // 差值对 1440 取模即偏移的分钟数。
        let to_min = |t: &windows_sys::Win32::Foundation::SYSTEMTIME| {
            i32::from(t.wHour) * 60 + i32::from(t.wMinute)
        };
        let diff = (to_min(&local) - to_min(&utc)).rem_euclid(1440);
        let expect = read_time_info().utc_offset_secs / 60;
        let expect = expect.rem_euclid(1440);
        assert!(
            (diff - expect).abs() <= 1,
            "本地时间与 UTC 相差 {diff} 分钟，但报告的偏移是 {expect} 分钟"
        );
    }

    #[test]
    fn 本机时间信息() {
        let t = read_time_info();
        assert!(t.ts > 1_700_000_000, "时间戳明显不对：{}", t.ts);
        assert!(!t.timezone.is_empty());
        assert!((-14 * 3600..=14 * 3600).contains(&t.utc_offset_secs));
        assert_eq!(t.ntp_synchronized, None, "读不到就该是 None，不能编 false");
        assert_eq!(
            t.ntp_service.as_deref(),
            Some("W32Time"),
            "任何 Windows 都有 Windows Time 服务"
        );
        assert!(t.rtc_local.is_some());
        eprintln!("本机时间：{}", serde_json::to_string(&t).unwrap());
    }
}
