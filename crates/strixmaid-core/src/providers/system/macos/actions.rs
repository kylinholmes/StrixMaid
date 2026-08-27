//! 三个写操作：改主机名、改时区、重启 / 关机。
//!
//! # 这里必须调外部命令
//!
//! 读路径上一个子进程都不起（见 [`super`] 的模块文档），写路径则绕不开：
//!
//! - 主机名有三份（`HostName` / `LocalHostName` / `ComputerName`），存在
//!   SystemConfiguration 的动态存储里，唯一的公开写入口是 `scutil --set`；
//!   `sethostname(2)` 只改运行时的那一份，重启即丢，还会被 `configd` 随时改回去。
//! - 时区由 `systemsetup -settimezone` 管，它会同时更新 `/etc/localtime` 与
//!   SystemConfiguration 里的记录；只换软链会与系统状态不一致。
//! - 关机重启走 `shutdown(8)`。
//!
//! 三者都要 root。非 root 时命令本身会失败，按其输出映射成
//! [`ErrorCode::PermissionDenied`](strixmaid_types::ErrorCode::PermissionDenied)
//! 并置 `can_retry_elevated`，与 Linux 侧 polkit 被拒时的表现一致。
//!
//! # 参数校验在前
//!
//! 主机名与时区都先做与 Linux 版**完全相同**的校验再交给命令，
//! 拒绝路径穿越与非法字符。命令参数走 `Command::arg`，不经过 shell。

use std::io;
use std::path::Path;

use strixmaid_types::system::{PowerAction, SetHostnameReq};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};

/// macOS 的 zoneinfo 目录。新系统在 `/var/db/timezone/zoneinfo`，
/// 老系统在 `/usr/share/zoneinfo`（现在是指向前者的软链）。
const ZONEINFO_DIRS: &[&str] = &["/var/db/timezone/zoneinfo", "/usr/share/zoneinfo"];

// ================================ 主机名 ================================

/// 校验静态主机名：仅 `[A-Za-z0-9-.]`，1–64 字节，每个标签 1–63 字节且不以 `-` 开头或结尾。
///
/// 规则与 Linux 版逐条一致——同一个 API 在两个平台上必须接受同一组输入。
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
/// `HostName` 是 Unix 意义上的主机名（`hostname(1)` 看到的那个），
/// `pretty_hostname` 写入 `ComputerName`（Finder 与共享服务显示的名字），
/// 与 Linux 侧 `/etc/machine-info` 的 `PRETTY_HOSTNAME` 对应。
/// `LocalHostName`（Bonjour 名）不动——它有自己的字符集限制，
/// 强行同步反而可能失败。
pub fn set_hostname(req: &SetHostnameReq) -> ApiResult<()> {
    validate_hostname(&req.hostname)?;
    scutil_set("HostName", &req.hostname)?;
    if let Some(pretty) = &req.pretty_hostname {
        scutil_set("ComputerName", pretty)?;
    }
    Ok(())
}

/// `scutil --set <key> <value>`。
fn scutil_set(key: &str, value: &str) -> ApiResult<()> {
    let out = std::process::Command::new("scutil")
        .arg("--set")
        .arg(key)
        .arg(value)
        .output()
        .map_err(|e| spawn_error(&e, "scutil"))?;
    if out.status.success() {
        return Ok(());
    }
    Err(command_error(
        &format!("设置 {key}"),
        &String::from_utf8_lossy(&out.stderr),
        out.status,
    ))
}

// ================================= 时区 =================================

/// 校验时区名并确认它在 zoneinfo 里存在。
///
/// 拒绝任何形式的路径穿越；文件必须存在且以 `TZif` 魔数开头
/// （挡住 `zone.tab` 这类非时区文件）。规则与 Linux 版一致。
pub fn validate_timezone(tz: &str) -> ApiResult<()> {
    if tz.is_empty() || tz.len() > 128 {
        return Err(ApiError::invalid_request("时区名不能为空且不超过 128 字节"));
    }
    if tz.starts_with('/')
        || tz.ends_with('/')
        || tz
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
        || !tz
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
    {
        return Err(ApiError::invalid_request(
            "时区名必须是 IANA 形式（如 Asia/Shanghai），只允许字母、数字、`/`、`_`、`-`、`+`",
        ));
    }
    if ZONEINFO_DIRS
        .iter()
        .any(|dir| is_tzif(&Path::new(dir).join(tz)))
    {
        return Ok(());
    }
    Err(ApiError::invalid_request(format!(
        "时区 `{tz}` 不存在于 {}（或不是有效的 TZif 文件）",
        ZONEINFO_DIRS.join(" / ")
    )))
}

/// 文件存在且以 `TZif` 魔数开头。
fn is_tzif(path: &Path) -> bool {
    use std::io::Read as _;
    let mut magic = [0u8; 4];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic))
        .is_ok()
        && &magic == b"TZif"
}

/// 改时区：`systemsetup -settimezone <tz>`。
pub fn set_timezone(tz: &str) -> ApiResult<()> {
    validate_timezone(tz)?;
    let out = std::process::Command::new("systemsetup")
        .arg("-settimezone")
        .arg(tz)
        .output()
        .map_err(|e| spawn_error(&e, "systemsetup"))?;
    // systemsetup 有个恼人的习惯：权限不足时照样退出码 0，把错误写进 stdout。
    // 因此不能只看退出码。
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() && !looks_denied(&stdout) && !looks_denied(&stderr) {
        return Ok(());
    }
    let detail = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    Err(command_error("设置时区", &detail, out.status))
}

// ================================= 电源 =================================

/// 重启 / 关机：`shutdown -r now` / `shutdown -h now`。
///
/// 命令返回即视为已受理——真正的关机是异步的。
pub async fn power(action: PowerAction) -> ApiResult<()> {
    let flag = match action {
        PowerAction::Reboot => "-r",
        PowerAction::Poweroff => "-h",
    };
    let output = tokio::process::Command::new("shutdown")
        .arg(flag)
        .arg("now")
        .output()
        .await
        .map_err(|e| spawn_error(&e, "shutdown"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(command_error(
        match action {
            PowerAction::Reboot => "重启",
            PowerAction::Poweroff => "关机",
        },
        &String::from_utf8_lossy(&output.stderr),
        output.status,
    ))
}

// ================================ 错误映射 ================================

/// 命令起不来。
fn spawn_error(e: &io::Error, program: &str) -> ApiError {
    if e.kind() == io::ErrorKind::NotFound {
        ApiError::capability_unavailable("host", format!("找不到 {program}，无法执行该操作"))
    } else {
        ApiError::internal(format!("启动 {program} 失败")).with_detail(e.to_string())
    }
}

/// 命令跑了但失败了。输出里带权限字样的映射成可提权重试的 403。
fn command_error(what: &str, detail: &str, status: std::process::ExitStatus) -> ApiError {
    let detail = detail.trim().to_owned();
    if looks_denied(&detail) {
        return ApiError::new(
            ErrorCode::PermissionDenied,
            format!("系统拒绝{what}：需要 root 权限"),
        )
        .with_detail(detail)
        .retry_elevated();
    }
    ApiError::internal(format!("{what}失败（退出码 {status}）")).with_detail(detail)
}

/// 输出看起来是权限被拒。
fn looks_denied(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "permission",
        "denied",
        "not authorized",
        "operation not permitted",
        "must be run as root",
        "requires root",
        "you need administrator",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 主机名校验与_linux_一致() {
        assert!(validate_hostname("web-01").is_ok());
        assert!(validate_hostname("a.b.c").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname(&"a".repeat(65)).is_err());
        assert!(validate_hostname("has space").is_err());
        assert!(validate_hostname("-lead").is_err());
        assert!(validate_hostname("trail-").is_err());
        assert!(validate_hostname("a..b").is_err(), "连续的点");
        assert!(validate_hostname("中文").is_err());
    }

    #[test]
    fn 时区校验拒绝路径穿越() {
        assert!(validate_timezone("../../etc/passwd").is_err());
        assert!(validate_timezone("/absolute").is_err());
        assert!(validate_timezone("Asia/").is_err());
        assert!(validate_timezone("Asia//Shanghai").is_err());
        assert!(validate_timezone("").is_err());
        assert!(validate_timezone("Asia/Shang hai").is_err());
        // 存在但不是 TZif
        assert!(validate_timezone("zone.tab").is_err());
    }

    #[test]
    fn 本机已知时区通过校验() {
        assert!(validate_timezone("UTC").is_ok());
        assert!(
            validate_timezone("Asia/Shanghai").is_ok(),
            "zoneinfo 目录：{ZONEINFO_DIRS:?}"
        );
    }

    #[test]
    fn 权限拒绝识别() {
        assert!(looks_denied("setting the timezone requires root"));
        assert!(looks_denied("Operation not permitted"));
        assert!(looks_denied("scutil: Permission denied"));
        assert!(!looks_denied("Time Zone: Asia/Shanghai"));
        assert!(!looks_denied(""));
    }

    /// 权限被拒时必须是可提权重试的 403，而不是 500。
    ///
    /// 只测映射函数，**不真的去调 `scutil` / `systemsetup`**：单测跑过之后系统状态
    /// 必须和跑之前一样。这一条在 macOS 上尤其要当心——`scutil --set` 对
    /// `admin` 组成员是放行的（macOS 的授权走 SystemConfiguration 而不是 uid 0），
    /// 一个「反正非 root 会失败」的测试在开发者自己的机器上会真的改掉主机名。
    #[test]
    fn 权限拒绝映射为可提权重试的_403() {
        let status = std::process::Command::new("false")
            .status()
            .expect("/usr/bin/false");
        let e = command_error("设置主机名", "scutil: Permission denied", status);
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);
        assert!(e.detail.is_some());

        // 不像权限问题的失败仍是 500，不能一律当成「提权就能好」
        let e = command_error("设置时区", "unknown time zone", status);
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(!e.can_retry_elevated);
    }

    /// 命令不存在时报能力缺失，而不是 500。
    #[test]
    fn 命令不存在报能力缺失() {
        let e = spawn_error(
            &io::Error::new(io::ErrorKind::NotFound, "no such file"),
            "scutil",
        );
        assert_eq!(e.code, ErrorCode::CapabilityUnavailable);
        let e = spawn_error(&io::Error::other("boom"), "scutil");
        assert_eq!(e.code, ErrorCode::Internal);
    }
}
