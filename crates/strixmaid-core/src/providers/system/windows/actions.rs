//! 三个写操作：改主机名、改时区、重启 / 关机。
//!
//! # 一个子进程都不起
//!
//! 与 macOS 侧不同——那边改主机名 / 时区绕不开 `scutil` 与 `systemsetup`
//! （唯一的公开写入口）。Windows 三个操作都有正规的 Win32 API：
//!
//! | 操作 | API | 需要的特权 |
//! |---|---|---|
//! | 改主机名 | `SetComputerNameExW(ComputerNamePhysicalDnsHostname, …)` | 管理员 |
//! | 改时区 | `SetDynamicTimeZoneInformation` | `SeTimeZonePrivilege` |
//! | 重启 / 关机 | `InitiateSystemShutdownExW` | `SeShutdownPrivilege` |
//!
//! 后两个特权即便在管理员令牌里也**默认是禁用状态**，必须先用
//! `AdjustTokenPrivileges` 打开，见 [`enable_privilege`]。这是 Windows 与
//! Unix 最不一样的一点：Unix 是「你是不是 root」，Windows 是「你的令牌里
//! 这一条特权有没有、启没启用」。
//!
//! # 参数校验在前
//!
//! 主机名的校验规则与 Linux / macOS **逐条一致**——同一个 API 在三个平台上
//! 必须接受同一组输入。时区的校验规则则必然不同：Windows 时区名不是 IANA
//! 形式（`China Standard Time` 有空格、`Central Standard Time (Mexico)` 有括号），
//! 见 [`validate_timezone`]。
//!
//! # 权限不足一律映射成可提权重试的 403
//!
//! `ERROR_ACCESS_DENIED` / `ERROR_PRIVILEGE_NOT_HELD` / `ERROR_NOT_ALL_ASSIGNED`
//! 都映射成 [`ErrorCode::PermissionDenied`] 并置 `can_retry_elevated`，
//! 与 Linux 侧 polkit 被拒、macOS 侧命令报 `Permission denied` 时的表现一致。

use std::io;

use strixmaid_types::system::{PowerAction, SetHostnameReq};
use strixmaid_types::{ApiError, ApiResult};

use crate::platform::windows::wide::{to_wide, from_wide_nul};
use crate::platform::windows::{HKLM, error_from_code, last_error, reg_subkeys};

/// 时区定义所在的注册表键。
pub const TIME_ZONES_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones";

// ================================ 主机名 ================================

/// 校验静态主机名：仅 `[A-Za-z0-9-.]`，1–64 字节，每个标签 1–63 字节且不以 `-` 开头或结尾。
///
/// 规则与 Linux / macOS 版逐条一致。
pub fn validate_hostname(name: &str) -> ApiResult<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::invalid_request("主机名长度必须在 1–64 字节之间"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(ApiError::invalid_request(
            "主机名只能包含字母、数字、`-` 和 `.`",
        ));
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(ApiError::invalid_request(
                "主机名的每一段必须是 1–63 字节，且不能有连续的 `.`",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ApiError::invalid_request(
                "主机名的每一段不能以 `-` 开头或结尾",
            ));
        }
    }
    Ok(())
}

/// 设置主机名。
///
/// `ComputerNamePhysicalDnsHostname` 是「这台机器的 DNS 主机名」，
/// 对应 Linux 的 `/etc/hostname`（`uname -n` 看到的那个）。写进去之后
/// `SetComputerNameExW` 会连带更新 NetBIOS 名，两者从此保持一致。
///
/// # ⚠ 要重启才生效
///
/// Windows 的主机名改动写在注册表的 `ComputerName\ComputerName` 下，
/// **重启后**才会拷进 `ActiveComputerName` 生效。也就是说这次调用成功返回后，
/// [`collect_system_info`](super::collect_system_info) 报的 `hostname`
/// **仍然是旧名字**（它读的是 `ComputerNamePhysicalDnsHostname`，即活动名），
/// 直到机器重启。这与 Linux 的 `sethostname(2)` 立即生效完全不同，
/// 前端应在改名成功后提示「重启后生效」。
///
/// # `pretty_hostname` 没有对应物
///
/// Linux 的 `PRETTY_HOSTNAME`（`/etc/machine-info`）允许任意 UTF-8，
/// macOS 对应 `ComputerName`。Windows **没有**这么一个「另一个更好看的名字」：
/// 计算机描述（`Services\LanmanServer\Parameters\srvcomment`）只在网上邻居里
/// 显示，语义与生命周期都对不上。因此传了 `pretty_hostname` 时如实返回
/// `capability_unavailable`，**而且在改主机名之前就返回**——
/// 半成功（名字改了、漂亮名没改）比整个失败更难解释。
pub fn set_hostname(req: &SetHostnameReq) -> ApiResult<()> {
    validate_hostname(&req.hostname)?;
    if req.pretty_hostname.is_some() {
        return Err(ApiError::capability_unavailable(
            "host",
            "Windows 没有「漂亮主机名」这个概念，无法设置 pretty_hostname",
        ));
    }
    set_computer_name(&req.hostname).map_err(|e| win_to_api(&e, "修改主机名"))
}

/// `SetComputerNameExW(ComputerNamePhysicalDnsHostname, name)`。
fn set_computer_name(name: &str) -> io::Result<()> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, SetComputerNameExW,
    };
    let wide = to_wide(name);
    // SAFETY: wide 是以 NUL 结尾的 UTF-16 缓冲，在调用期间有效；
    // API 只读取它。
    let ok = unsafe { SetComputerNameExW(ComputerNamePhysicalDnsHostname, wide.as_ptr()) };
    if ok == 0 { Err(last_error()) } else { Ok(()) }
}

// ================================= 时区 =================================

/// 校验 Windows 时区名，并确认它在注册表里存在。
///
/// # 与 Linux / macOS 的规则不同，且必须不同
///
/// 那两个平台校验的是 IANA 名（`Asia/Shanghai`），字符集是
/// `[A-Za-z0-9/_+-]`。Windows 时区名是**另一套命名**，合法值里有空格
/// （`China Standard Time`）、句点（`W. Europe Standard Time`）、
/// 括号（`Central Standard Time (Mexico)`）、加号（`UTC+13`）。
/// 硬套 IANA 的字符集会把所有合法输入都拒掉。
///
/// 安全性由两层保证：字符白名单挡掉路径分隔符与控制字符，
/// 存在性检查（枚举 `…\Time Zones` 的子键并**全等比较**）挡掉一切其余情况——
/// 后者比 Linux 侧「拼路径再看文件在不在」更强，因为这里根本不拼路径。
pub fn validate_timezone(tz: &str) -> ApiResult<()> {
    if tz.is_empty() || tz.len() > 128 {
        return Err(ApiError::invalid_request("时区名不能为空且不超过 128 字节"));
    }
    if tz != tz.trim() {
        return Err(ApiError::invalid_request("时区名首尾不能有空白"));
    }
    if !tz.bytes().all(is_timezone_char) {
        return Err(ApiError::invalid_request(
            "时区名只能包含字母、数字、空格与 `.`、`-`、`+`、`(`、`)`、`_`、`&`、`'`",
        ));
    }
    // 注册表键名不区分大小写，比较时也不区分。
    if reg_subkeys(HKLM, TIME_ZONES_KEY)
        .iter()
        .any(|k| k.eq_ignore_ascii_case(tz))
    {
        return Ok(());
    }
    Err(ApiError::invalid_request(format!(
        "时区 `{tz}` 不存在。Windows 用自己的时区名（如 `China Standard Time`），不是 IANA 名"
    )))
}

/// Windows 时区名允许的字符。
fn is_timezone_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b' ' | b'.' | b'-' | b'+' | b'(' | b')' | b'_' | b'&' | b'\'')
}

/// 改时区：`SetDynamicTimeZoneInformation`。
///
/// 结构体的偏移与夏令时规则从注册表的时区定义里取（`TZI` 是一段 44 字节的
/// `REG_TZI_FORMAT`），`TimeZoneKeyName` 填时区键名——填了它系统才会继续按
/// 该时区的**动态**夏令时规则（`Dynamic DST` 子键，历年规则不同的时区靠它）
/// 走，只填偏移的话以后年份的夏令时会算错。
///
/// 需要 `SeTimeZonePrivilege`，本函数会先启用它。
pub fn set_timezone(tz: &str) -> ApiResult<()> {
    use windows_sys::Win32::Security::SE_TIME_ZONE_NAME;
    use windows_sys::Win32::System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, SetDynamicTimeZoneInformation,
    };

    validate_timezone(tz)?;

    let key = format!("{TIME_ZONES_KEY}\\{tz}");
    let tzi = read_tzi(&key).ok_or_else(|| {
        ApiError::internal(format!("时区 `{tz}` 的注册表定义缺少 TZI 数据，无法应用"))
    })?;

    let info = DYNAMIC_TIME_ZONE_INFORMATION {
        Bias: tzi.bias,
        StandardBias: tzi.standard_bias,
        DaylightBias: tzi.daylight_bias,
        StandardDate: tzi.standard_date,
        DaylightDate: tzi.daylight_date,
        // 显示名读不到就留空：它只影响 `GetTimeZoneInformation` 回报的名字，
        // 不影响时间换算，不值得为它失败。
        StandardName: wide_array::<32>(
            &crate::platform::windows::reg_string(HKLM, &key, "Std").unwrap_or_default(),
        ),
        DaylightName: wide_array::<32>(
            &crate::platform::windows::reg_string(HKLM, &key, "Dlt").unwrap_or_default(),
        ),
        TimeZoneKeyName: wide_array::<128>(tz),
        // 不禁用动态夏令时——禁用它等于把时区钉死在当前的夏令时规则上。
        DynamicDaylightTimeDisabled: false,
    };

    // SAFETY: 常量来自 windows-sys，是一个静态的以 NUL 结尾的宽串。
    unsafe { enable_privilege(SE_TIME_ZONE_NAME) }.map_err(|e| win_to_api(&e, "启用时区特权"))?;

    // SAFETY: info 是一个完整初始化的结构体，API 只读取它。
    let ok = unsafe { SetDynamicTimeZoneInformation(&raw const info) };
    if ok == 0 {
        return Err(win_to_api(&last_error(), "设置时区"));
    }
    Ok(())
}

/// 注册表时区定义里的 `TZI`（`REG_TZI_FORMAT`，44 字节）。
///
/// 不 derive `Debug` / `PartialEq`：内嵌的 `SYSTEMTIME` 来自 windows-sys，
/// 那边没有为它实现这两个 trait，而本结构只在解析途中存在，不需要它们。
#[derive(Clone, Copy)]
struct Tzi {
    bias: i32,
    standard_bias: i32,
    daylight_bias: i32,
    standard_date: windows_sys::Win32::Foundation::SYSTEMTIME,
    daylight_date: windows_sys::Win32::Foundation::SYSTEMTIME,
}

/// 读一个时区键的 `TZI` 二进制值。
///
/// `platform/windows/registry.rs` 只封了字符串与定长标量的读取，没有二进制读取
/// ——整个工程里只有这一处需要它。为一个调用点去改共享的平台层不划算，
/// 因此在这里就地调一次 `RegGetValueW`。
fn read_tzi(subkey: &str) -> Option<Tzi> {
    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, SYSTEMTIME};
    use windows_sys::Win32::System::Registry::{RRF_RT_REG_BINARY, RegGetValueW};

    /// `REG_TZI_FORMAT` 的字节大小：3 个 LONG + 2 个 SYSTEMTIME。
    const TZI_SIZE: usize = 12 + 32;

    let sub = to_wide(subkey);
    let val = to_wide("TZI");
    let mut buf = [0u8; TZI_SIZE];
    let mut len = TZI_SIZE as u32;
    // SAFETY: 两个路径串以 NUL 结尾；buf 有 len 字节可写，len 如实描述其大小。
    let rc = unsafe {
        RegGetValueW(
            HKLM.raw(),
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            &raw mut len,
        )
    };
    if rc != ERROR_SUCCESS || len as usize != TZI_SIZE {
        return None;
    }

    let i32_at = |at: usize| i32::from_ne_bytes(buf[at..at + 4].try_into().unwrap_or([0; 4]));
    let systemtime_at = |at: usize| {
        let w = |i: usize| u16::from_ne_bytes(buf[at + i * 2..at + i * 2 + 2].try_into().unwrap_or([0; 2]));
        SYSTEMTIME {
            wYear: w(0),
            wMonth: w(1),
            wDayOfWeek: w(2),
            wDay: w(3),
            wHour: w(4),
            wMinute: w(5),
            wSecond: w(6),
            wMilliseconds: w(7),
        }
    };
    Some(Tzi {
        bias: i32_at(0),
        standard_bias: i32_at(4),
        daylight_bias: i32_at(8),
        standard_date: systemtime_at(12),
        daylight_date: systemtime_at(28),
    })
}

/// 把字符串写进一个定长 UTF-16 数组（末尾留 NUL，超长截断）。
fn wide_array<const N: usize>(s: &str) -> [u16; N] {
    let mut out = [0u16; N];
    for (slot, c) in out.iter_mut().take(N - 1).zip(s.encode_utf16()) {
        *slot = c;
    }
    out
}

// ================================= 电源 =================================

/// 重启 / 关机：`InitiateSystemShutdownExW`。
///
/// 超时传 0 表示立即执行；`bForceAppsClosed` 传 `FALSE`——强制关闭会让没保存的
/// 工作直接丢失，一个远程管理面板不该替用户做这个决定。有应用阻止关机时
/// API 仍会返回成功，由 Windows 自己弹出「以下应用阻止了关机」的界面
/// （在无人值守的服务器上会按组策略超时后继续）。
///
/// 需要 `SeShutdownPrivilege`，本函数会先启用它。调用返回即视为已受理——
/// 真正的关机是异步的，与 Linux / macOS 侧一致。
pub async fn power(action: PowerAction) -> ApiResult<()> {
    tokio::task::spawn_blocking(move || power_blocking(action))
        .await
        .map_err(|e| ApiError::internal("电源操作任务异常终止").with_detail(e.to_string()))?
}

fn power_blocking(action: PowerAction) -> ApiResult<()> {
    use windows_sys::Win32::Security::SE_SHUTDOWN_NAME;
    use windows_sys::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
        SHTDN_REASON_MINOR_OTHER,
    };

    let what = match action {
        PowerAction::Reboot => "重启",
        PowerAction::Poweroff => "关机",
    };
    // SAFETY: 常量来自 windows-sys，是一个静态的以 NUL 结尾的宽串。
    unsafe { enable_privilege(SE_SHUTDOWN_NAME) }
        .map_err(|e| win_to_api(&e, &format!("启用{what}特权")))?;

    let reboot = matches!(action, PowerAction::Reboot);
    // SAFETY: 机器名与提示文本传空表示「本机、不显示消息」，其余是标量。
    let ok = unsafe {
        InitiateSystemShutdownExW(
            std::ptr::null(),
            std::ptr::null(),
            0,
            0,
            i32::from(reboot),
            SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
        )
    };
    if ok == 0 {
        return Err(win_to_api(&last_error(), what));
    }
    Ok(())
}

// ================================ 特权 ================================

/// 在当前进程令牌里启用一条特权。
///
/// Windows 的管理员令牌里带着一堆特权，但**默认全是禁用状态**——这是设计使然
/// （最小权限），每次要用都得显式打开。`AdjustTokenPrivileges` 有个陷阱：
/// 即便一条特权都没能启用，它**也返回 TRUE**，真正的结论在
/// `GetLastError() == ERROR_NOT_ALL_ASSIGNED` 里。
///
/// # Safety
///
/// `name` 必须是一个以 NUL 结尾、在本次调用期间有效的宽字符串
/// （实际用法都是 windows-sys 的 `SE_*_NAME` 静态常量）。
pub unsafe fn enable_privilege(name: *const u16) -> io::Result<()> {
    use windows_sys::Win32::Foundation::{ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use crate::platform::windows::handle::Owned;

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 返回的是永远有效的伪句柄；token 是输出参数。
    let ok = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &raw mut token,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: token 刚由 OpenProcessToken 成功返回，尚无其它持有者。
    let token = unsafe { Owned::new(token) }?;

    let mut luid = LUID {
        LowPart: 0,
        HighPart: 0,
    };
    // SAFETY: 系统名传空表示本机；name 由调用方保证是以 NUL 结尾的宽串。
    let ok = unsafe { LookupPrivilegeValueW(std::ptr::null(), name, &raw mut luid) };
    if ok == 0 {
        return Err(last_error());
    }

    let tp = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: token 有效且带 TOKEN_ADJUST_PRIVILEGES；tp 是一条完整的特权项；
    // 不需要旧状态，按文档把 previousstate 与 returnlength 一并传空。
    let ok = unsafe {
        AdjustTokenPrivileges(
            token.raw(),
            0,
            &raw const tp,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    // 陷阱在这里：返回 TRUE 不代表启用成功。
    let rc = last_error().raw_os_error().unwrap_or(0) as u32;
    if rc == ERROR_NOT_ALL_ASSIGNED {
        return Err(error_from_code(ERROR_NOT_ALL_ASSIGNED));
    }
    Ok(())
}

// ================================ 错误映射 ================================

/// 把 Win32 错误映射成 API 错误。
///
/// 权限类错误（拒绝访问 / 没有这条特权 / 特权没能启用）→ 可提权重试的 403，
/// 与 Linux 侧 EACCES、macOS 侧 `Permission denied` 的处理一致。
pub(crate) fn win_to_api(e: &io::Error, what: &str) -> ApiError {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_NOT_ALL_ASSIGNED,
        ERROR_PRIVILEGE_NOT_HELD,
    };

    let code = e.raw_os_error().unwrap_or(0) as u32;
    match code {
        ERROR_ACCESS_DENIED | ERROR_PRIVILEGE_NOT_HELD | ERROR_NOT_ALL_ASSIGNED => {
            ApiError::permission_denied(format!("{what}需要管理员权限"))
                .with_detail(e.to_string())
                .retry_elevated()
        }
        ERROR_INVALID_PARAMETER => {
            ApiError::invalid_request(format!("{what}失败：系统认为参数不合法"))
                .with_detail(e.to_string())
        }
        _ => ApiError::internal(format!("{what}失败")).with_detail(e.to_string()),
    }
}

/// 当前主机名（`ComputerNamePhysicalDnsHostname`，即**活动**名）。
///
/// 放在本文件是因为它与 [`set_hostname`] 是一对：改名写的是待生效的名字，
/// 这里读的是当前生效的名字，两者在重启之前会不一致，见 [`set_hostname`] 的说明。
pub fn computer_name() -> Option<String> {
    use windows_sys::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };

    // DNS 主机名上限 63 字节，但 API 以字符计且允许更长的缓冲，给足即可。
    let mut buf = [0u16; 256];
    let mut len = buf.len() as u32;
    // SAFETY: buf 有 len 个 u16 可写，len 如实描述其容量；失败时只回填所需长度。
    let ok = unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, buf.as_mut_ptr(), &raw mut len) };
    if ok == 0 {
        return None;
    }
    let name = from_wide_nul(&buf[..(len as usize).min(buf.len())]);
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use strixmaid_types::ErrorCode;

    use super::*;

    #[test]
    fn 主机名校验与另两个平台一致() {
        assert!(validate_hostname("web-01").is_ok());
        assert!(validate_hostname("web01.example.com").is_ok());
        assert!(validate_hostname("a.b.c").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname(&"a".repeat(65)).is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err(), "单个标签不能超过 63 字节");
        assert!(validate_hostname(&"a".repeat(63)).is_ok());
        assert!(validate_hostname("has space").is_err());
        assert!(validate_hostname("-lead").is_err());
        assert!(validate_hostname("trail-").is_err());
        assert!(validate_hostname("a..b").is_err(), "连续的点");
        assert!(validate_hostname("中文").is_err());
        assert!(validate_hostname("WEB_01").is_err(), "下划线不是合法主机名字符");
        let sixty_four = format!("{}.bcd", "a".repeat(60));
        assert!(validate_hostname(&sixty_four).is_ok(), "总长 64 字节、标签合法");
    }

    #[test]
    fn 时区校验拒绝非法字符() {
        assert!(validate_timezone("").is_err());
        assert!(validate_timezone(&"a".repeat(129)).is_err());
        assert!(validate_timezone("../../etc/passwd").is_err(), "路径分隔符");
        assert!(validate_timezone(r"..\..\windows").is_err());
        assert!(validate_timezone("Asia/Shanghai").is_err(), "IANA 名不是 Windows 时区名");
        assert!(validate_timezone(" China Standard Time").is_err(), "首尾空白");
        assert!(validate_timezone("China Standard Time ").is_err());
        assert!(validate_timezone("China\0Standard").is_err());
        assert!(validate_timezone("不存在的时区").is_err());
    }

    #[test]
    fn 本机已知时区通过校验() {
        // 这几个键在任何 Windows 上都存在。
        for tz in ["UTC", "China Standard Time", "W. Europe Standard Time"] {
            assert!(validate_timezone(tz).is_ok(), "{tz} 应当存在于注册表");
        }
        // 大小写不敏感
        assert!(validate_timezone("china standard time").is_ok());
    }

    #[test]
    fn 时区定义可读() {
        let tzi = read_tzi(&format!("{TIME_ZONES_KEY}\\China Standard Time"))
            .expect("China Standard Time 的 TZI 应可读");
        // 东八区：UTC = 本地 + Bias，所以 Bias = -480
        assert_eq!(tzi.bias, -480, "东八区的 Bias 应为 -480");
        // 「不用夏令时」在注册表里的表达是**没有切换点**（`DaylightDate.wMonth == 0`），
        // 不是 `DaylightBias == 0`——微软给几乎每个时区都填了 -60 的 DaylightBias，
        // 它只有在存在切换点时才被用到。拿 DaylightBias 判断会把中国、日本这些
        // 不用夏令时的时区全判错。
        assert_eq!(tzi.daylight_date.wMonth, 0, "中国没有夏令时切换点");
        assert_eq!(tzi.standard_date.wMonth, 0, "没有夏令时也就没有回切点");

        let utc = read_tzi(&format!("{TIME_ZONES_KEY}\\UTC")).expect("UTC 的 TZI 应可读");
        assert_eq!(utc.bias, 0);

        // 不存在的键。`Tzi` 刻意不 derive `Debug` / `PartialEq`（见其定义），
        // 所以这里用 `is_none()` 而不是 `assert_eq!(.., None)`——断言的内容不变。
        assert!(
            read_tzi(&format!("{TIME_ZONES_KEY}\\没有这个时区")).is_none(),
            "不存在的时区键不该读出 TZI"
        );
    }

    #[test]
    fn 定长宽串数组() {
        let a = wide_array::<8>("abc");
        assert_eq!(&a[..3], &['a' as u16, 'b' as u16, 'c' as u16]);
        assert_eq!(a[3], 0, "其余补零");
        // 超长截断，且末位始终留 NUL
        let b = wide_array::<4>("abcdefgh");
        assert_eq!(b, ['a' as u16, 'b' as u16, 'c' as u16, 0]);
        // 空串
        assert_eq!(wide_array::<3>(""), [0, 0, 0]);
    }

    /// 传了 `pretty_hostname` 必须报能力缺失，而且**在动系统之前**就报。
    ///
    /// 用一个非法主机名做前置：校验会先失败，因此这条用例无论如何都不会
    /// 真的改到本机的名字。
    #[test]
    fn 漂亮主机名报能力缺失() {
        // 先确认合法性校验优先级：非法名字优先报 InvalidRequest
        let e = set_hostname(&SetHostnameReq {
            hostname: "不合法的名字".into(),
            pretty_hostname: Some("随便".into()),
        })
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);

        // 合法名字 + pretty：报能力缺失，且不该触碰系统
        let e = set_hostname(&SetHostnameReq {
            hostname: "strixmaid-test".into(),
            pretty_hostname: Some("随便".into()),
        })
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::CapabilityUnavailable);
        assert_eq!(e.capability.as_deref(), Some("host"));
    }

    #[test]
    fn 错误映射() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, ERROR_PRIVILEGE_NOT_HELD,
        };

        let e = win_to_api(&error_from_code(ERROR_ACCESS_DENIED), "改主机名");
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);
        assert!(e.detail.is_some());

        let e = win_to_api(&error_from_code(ERROR_PRIVILEGE_NOT_HELD), "关机");
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);

        let e = win_to_api(&error_from_code(ERROR_INVALID_PARAMETER), "设置时区");
        assert_eq!(e.code, ErrorCode::InvalidRequest);
        assert!(!e.can_retry_elevated);

        // 不像权限问题的失败仍是 500，不能一律当成「提权就能好」
        let e = win_to_api(&io::Error::other("boom"), "改主机名");
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(!e.can_retry_elevated);
    }

    /// 特权启用：本进程可能是管理员、也可能不是，两种结果都合法。
    ///
    /// **不真的去关机 / 改时区**——单测跑完之后系统状态必须和跑之前一样。
    #[test]
    fn 启用特权不_panic() {
        use windows_sys::Win32::Security::SE_SHUTDOWN_NAME;
        // SAFETY: 常量是 windows-sys 提供的静态宽串。
        let r = unsafe { enable_privilege(SE_SHUTDOWN_NAME) };
        match &r {
            Ok(()) => eprintln!("本进程可以启用 SeShutdownPrivilege（多半是管理员）"),
            Err(e) => {
                assert_eq!(
                    win_to_api(e, "关机").code,
                    ErrorCode::PermissionDenied,
                    "启用失败必须映射成可提权重试的 403"
                );
                eprintln!("本进程不能启用 SeShutdownPrivilege：{e}");
            }
        }
    }

    #[test]
    fn 本机主机名可读() {
        let name = computer_name().expect("GetComputerNameExW 应可用");
        assert!(!name.is_empty());
        assert!(!name.contains('\0'), "结尾 NUL 必须去掉：{name:?}");
        eprintln!("本机主机名：{name}");
    }
}
