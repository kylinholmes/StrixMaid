//! 三个写操作：改主机名、改时区、重启 / 关机。
//!
//! 现在**没有权限体系**（认证由另一模块提供，最后接线）。这里只实现逻辑：
//! 以当前进程身份执行，非 root 时由内核 / 文件权限拒绝，映射为
//! [`ErrorCode::PermissionDenied`](strixmaid_types::ErrorCode::PermissionDenied) 并标记
//! `can_retry_elevated`。`systemctl reboot|poweroff` 是本模块唯一允许调用的外部命令。

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use strixmaid_types::system::{PowerAction, SetHostnameReq};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};

use super::os_release::quote_shell;

const HOSTNAME_FILE: &str = "/etc/hostname";
const MACHINE_INFO_FILE: &str = "/etc/machine-info";
const LOCALTIME: &str = "/etc/localtime";
const LOCALTIME_TMP: &str = "/etc/.localtime.strixmaid-tmp";
const TIMEZONE_FILE: &str = "/etc/timezone";
const ZONEINFO_DIR: &str = "/usr/share/zoneinfo";

// ================================ 主机名 ================================

/// 校验静态主机名：仅 `[A-Za-z0-9-.]`，1–64 字节，每个标签 1–63 字节且不以 `-` 开头或结尾。
pub fn validate_hostname(name: &str) -> ApiResult<()> {
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::invalid_request("主机名长度必须在 1–64 字节之间"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
    {
        return Err(ApiError::invalid_request("主机名只能包含字母、数字、`-` 和 `.`"));
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(ApiError::invalid_request("主机名的每一段必须是 1–63 字节，且不能有连续的 `.`"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(ApiError::invalid_request("主机名的每一段不能以 `-` 开头或结尾"));
        }
    }
    Ok(())
}

/// 设置主机名：`sethostname(2)` 立即生效 + 写 `/etc/hostname` 持久化；
/// 可选地更新 `/etc/machine-info` 的 `PRETTY_HOSTNAME`。
pub fn set_hostname(req: &SetHostnameReq) -> ApiResult<()> {
    validate_hostname(&req.hostname)?;

    sethostname(&req.hostname).map_err(|e| io_to_api(&e, "修改运行中的主机名"))?;
    fs::write(HOSTNAME_FILE, format!("{}\n", req.hostname))
        .map_err(|e| io_to_api(&e, "写入 /etc/hostname"))?;

    if let Some(pretty) = &req.pretty_hostname {
        write_pretty_hostname(pretty).map_err(|e| io_to_api(&e, "写入 /etc/machine-info"))?;
    }
    Ok(())
}

/// `sethostname(2)`。libc crate 没给 Linux 声明这个包装函数，走 `syscall(2)`。
fn sethostname(name: &str) -> io::Result<()> {
    // SAFETY: 指针与长度来自同一个 &str，内核只读取该缓冲区，不保留引用。
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sethostname,
            name.as_ptr() as *const libc::c_char,
            name.len() as libc::size_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 更新 / 写入 `/etc/machine-info` 的 `PRETTY_HOSTNAME`，其它行原样保留；空串表示删除该项。
fn write_pretty_hostname(pretty: &str) -> io::Result<()> {
    let existing = match fs::read_to_string(MACHINE_INFO_FILE) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    fs::write(MACHINE_INFO_FILE, render_machine_info(&existing, pretty))
}

/// 纯函数：把 `PRETTY_HOSTNAME` 合并进 machine-info 文本。
pub fn render_machine_info(existing: &str, pretty: &str) -> String {
    let mut out = String::with_capacity(existing.len() + pretty.len() + 32);
    let mut written = false;
    for line in existing.lines() {
        let is_key = line
            .trim_start()
            .split_once('=')
            .is_some_and(|(k, _)| k.trim() == "PRETTY_HOSTNAME");
        if is_key {
            if !written && !pretty.is_empty() {
                out.push_str(&format!("PRETTY_HOSTNAME={}\n", quote_shell(pretty)));
            }
            written = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !written && !pretty.is_empty() {
        out.push_str(&format!("PRETTY_HOSTNAME={}\n", quote_shell(pretty)));
    }
    out
}

// ================================= 时区 =================================

/// 校验时区名并返回 zoneinfo 文件路径。
///
/// 拒绝任何形式的路径穿越；文件必须存在且以 `TZif` 魔数开头（挡住 `zone.tab` 这类非时区文件）。
pub fn validate_timezone(tz: &str) -> ApiResult<PathBuf> {
    if tz.is_empty() || tz.len() > 128 {
        return Err(ApiError::invalid_request("时区名不能为空且不超过 128 字节"));
    }
    if tz.starts_with('/')
        || tz.ends_with('/')
        || tz.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
        || !tz
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'+'))
    {
        return Err(ApiError::invalid_request(
            "时区名必须是 IANA 形式（如 Asia/Shanghai），只允许字母、数字、`/`、`_`、`-`、`+`",
        ));
    }
    let path = Path::new(ZONEINFO_DIR).join(tz);
    let mut magic = [0u8; 4];
    let ok = fs::File::open(&path)
        .and_then(|mut f| {
            use std::io::Read as _;
            f.read_exact(&mut magic)
        })
        .is_ok()
        && &magic == b"TZif";
    if !ok {
        return Err(ApiError::invalid_request(format!(
            "时区 `{tz}` 不存在于 {ZONEINFO_DIR}（或不是有效的 TZif 文件）"
        )));
    }
    Ok(path)
}

/// 改时区：原子替换 `/etc/localtime` 软链；若 `/etc/timezone` 存在（Debian 系）则一并更新。
pub fn set_timezone(tz: &str) -> ApiResult<()> {
    validate_timezone(tz)?;
    let target = format!("../usr/share/zoneinfo/{tz}");

    // 先建临时链接再 rename：rename 是原子的，任何时刻 /etc/localtime 都有效。
    let _ = fs::remove_file(LOCALTIME_TMP);
    symlink(&target, LOCALTIME_TMP).map_err(|e| io_to_api(&e, "创建 /etc/localtime 软链"))?;
    if let Err(e) = fs::rename(LOCALTIME_TMP, LOCALTIME) {
        let _ = fs::remove_file(LOCALTIME_TMP);
        return Err(io_to_api(&e, "替换 /etc/localtime"));
    }

    if Path::new(TIMEZONE_FILE).exists() {
        fs::write(TIMEZONE_FILE, format!("{tz}\n")).map_err(|e| io_to_api(&e, "写入 /etc/timezone"))?;
    }
    Ok(())
}

// ================================= 电源 =================================

/// 重启 / 关机：调 `systemctl reboot|poweroff`（本模块唯一允许的外部命令）。
///
/// 命令返回即视为已受理——真正的关机是异步的。
pub async fn power(action: PowerAction) -> ApiResult<()> {
    let verb = match action {
        PowerAction::Reboot => "reboot",
        PowerAction::Poweroff => "poweroff",
    };
    let output = tokio::process::Command::new("systemctl")
        .arg(verb)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                ApiError::capability_unavailable("systemd", "找不到 systemctl，无法执行电源操作")
            } else {
                ApiError::internal(format!("启动 systemctl {verb} 失败")).with_detail(e.to_string())
            }
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("authentication")
        || lower.contains("denied")
        || lower.contains("not authorized")
        || lower.contains("permission")
        || lower.contains("operation not permitted")
    {
        return Err(ApiError::permission_denied(format!("系统拒绝执行 {verb}：需要 root 权限"))
            .with_detail(stderr)
            .retry_elevated());
    }
    Err(ApiError::internal(format!("systemctl {verb} 退出码 {}", output.status))
        .with_detail(stderr))
}

// ================================ 错误映射 ================================

/// 把 I/O 错误映射成 API 错误：EACCES / EPERM / EROFS → 权限拒绝（可提权重试），其余 → 内部错误。
pub(crate) fn io_to_api(e: &io::Error, what: &str) -> ApiError {
    let rofs = e.raw_os_error() == Some(libc::EROFS);
    match e.kind() {
        io::ErrorKind::PermissionDenied => {
            ApiError::permission_denied(format!("{what}需要 root 权限"))
                .with_detail(e.to_string())
                .retry_elevated()
        }
        _ if rofs => ApiError::new(ErrorCode::PermissionDenied, format!("{what}失败：文件系统只读"))
            .with_detail(e.to_string()),
        io::ErrorKind::NotFound => ApiError::not_found(format!("{what}失败：目标不存在")).with_detail(e.to_string()),
        _ => ApiError::internal(format!("{what}失败")).with_detail(e.to_string()),
    }
}

/// 解析 machine-info 里已有的 PRETTY_HOSTNAME（测试用，与 `render_machine_info` 配对）。
#[cfg(test)]
fn parse_pretty(raw: &str) -> Option<String> {
    use super::os_release::unquote_shell;
    raw.lines()
        .find_map(|l| l.split_once('=').filter(|(k, _)| *k == "PRETTY_HOSTNAME"))
        .map(|(_, v)| unquote_shell(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 主机名校验() {
        assert!(validate_hostname("web-01").is_ok());
        assert!(validate_hostname("web01.example.com").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-web").is_err());
        assert!(validate_hostname("web-").is_err());
        assert!(validate_hostname("web..01").is_err());
        assert!(validate_hostname("web 01").is_err());
        assert!(validate_hostname("主机").is_err());
        assert!(validate_hostname(&"a".repeat(65)).is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err(), "单个标签不能超过 63 字节");
        assert!(validate_hostname(&"a".repeat(63)).is_ok());
        let sixty_four = format!("{}.bcd", "a".repeat(60));
        assert!(validate_hostname(&sixty_four).is_ok(), "总长 64 字节、标签合法");
    }

    #[test]
    fn 时区校验() {
        assert!(validate_timezone("../etc/passwd").is_err());
        assert!(validate_timezone("/Asia/Shanghai").is_err());
        assert!(validate_timezone("Asia//Shanghai").is_err());
        assert!(validate_timezone("").is_err());
        assert!(validate_timezone("zone.tab").is_err(), "不是 TZif 文件");
        // 仅当本机有 zoneinfo 时才断言正例
        if Path::new(ZONEINFO_DIR).join("UTC").exists() {
            assert!(validate_timezone("UTC").is_ok());
            assert!(validate_timezone("Asia/Shanghai").is_ok());
            assert!(validate_timezone("Etc/GMT+8").is_ok());
        }
        assert!(validate_timezone("Not/AZone").is_err());
    }

    #[test]
    fn machine_info_合并() {
        let out = render_machine_info("ICON_NAME=computer\nPRETTY_HOSTNAME=\"old\"\n", "新 \"名字\"");
        assert_eq!(out, "ICON_NAME=computer\nPRETTY_HOSTNAME=\"新 \\\"名字\\\"\"\n");
        assert_eq!(parse_pretty(&out).as_deref(), Some("新 \"名字\""));
        // 追加
        assert_eq!(render_machine_info("", "x"), "PRETTY_HOSTNAME=\"x\"\n");
        // 删除
        assert_eq!(render_machine_info("PRETTY_HOSTNAME=\"old\"\nA=b\n", ""), "A=b\n");
    }

    #[test]
    fn 非_root_改主机名被拒且可提权重试() {
        // 测试以普通用户跑时走 EPERM 路径；以 root 跑时本测试不改动系统（用当前主机名回写会真的写文件），故跳过。
        // SAFETY: geteuid 无副作用。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let err = set_hostname(&SetHostnameReq {
            hostname: "strixmaid-test".into(),
            pretty_hostname: None,
        })
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert!(err.can_retry_elevated);
    }

    #[test]
    fn io_错误映射() {
        let e = io::Error::from_raw_os_error(libc::EACCES);
        assert_eq!(io_to_api(&e, "x").code, ErrorCode::PermissionDenied);
        let e = io::Error::from_raw_os_error(libc::EROFS);
        assert_eq!(io_to_api(&e, "x").code, ErrorCode::PermissionDenied);
        let e = io::Error::from_raw_os_error(libc::ENOENT);
        assert_eq!(io_to_api(&e, "x").code, ErrorCode::NotFound);
        let e = io::Error::from_raw_os_error(libc::EIO);
        assert_eq!(io_to_api(&e, "x").code, ErrorCode::Internal);
    }
}
