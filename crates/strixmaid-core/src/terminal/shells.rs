//! 本机可用的登录 shell 清单（`GET /terminals/shells`，`roadmap/12-workspace.md` §4.6）。
//!
//! 跑在**主进程**而不是 worker：清单的两个来源——Unix 的 `/etc/shells` 与
//! 目标用户的 passwd 项、Windows 的固定白名单加文件存在性——都是世界可读的，
//! 为一个只读列表付一次 RPC 往返不值得。真正开 shell 时 worker 仍按自己的
//! 规则复核（`worker/terminal/{unix,windows}.rs` 的 `resolve_shell`），
//! 这里给出的只是「下拉里展示什么」，不是准入判定。

use strixmaid_types::terminal::ShellInfo;

/// Unix：`/etc/shells` 里列出的 shell。注释与空行跳过。
///
/// **不做路径规范化**：`/etc/shells` 写的是什么就比什么（与 worker 侧
/// `resolve_shell` 的白名单判定同一条规则，理由见那里）。
#[cfg(unix)]
pub fn listed_shells() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string("/etc/shells") else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// 列出 `username` 可选的 shell，默认项排最前。
///
/// 默认 = 该用户 passwd 里的登录 shell（查不到用户或字段为空则 `/bin/sh`）。
/// 默认 shell 不在 `/etc/shells` 里时**也列出**（管理员写进 passwd 的决定
/// 不需要我们复核，worker 对「未显式指定」的请求同样放行）。
#[cfg(unix)]
pub fn available_shells(username: &str) -> Vec<ShellInfo> {
    let login = nix::unistd::User::from_name(username)
        .ok()
        .flatten()
        .map(|u| u.shell)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/bin/sh".to_owned());

    let mut out = vec![shell_info(&login, true)];
    for path in listed_shells() {
        if path != login {
            out.push(shell_info(&path, false));
        }
    }
    out
}

/// Windows：固定白名单逐个探测存在性，默认 = `%COMSPEC%`（几乎总是 cmd.exe）。
///
/// 白名单与 worker 侧 `ALLOWED_SHELLS` 保持一致（那边按文件名判定，这边给
/// 完整路径）；`pwsh` / `bash` / `wsl` 不在固定位置，按 PATH 查找。
#[cfg(windows)]
pub fn available_shells(_username: &str) -> Vec<ShellInfo> {
    use std::path::{Path, PathBuf};

    let system32 = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32");
    let default = std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .unwrap_or_else(|| system32.join("cmd.exe"));

    let mut out = Vec::new();
    if default.is_file() {
        out.push(shell_info(&default.to_string_lossy(), true));
    }
    let fixed = [
        system32.join("cmd.exe"),
        system32.join(r"WindowsPowerShell\v1.0\powershell.exe"),
        system32.join("wsl.exe"),
        system32.join("bash.exe"),
    ];
    let from_path = ["pwsh.exe", "bash.exe"].iter().filter_map(|name| which(name));
    for p in fixed.into_iter().chain(from_path) {
        let already = out
            .iter()
            .any(|s: &ShellInfo| Path::new(&s.path) == p.as_path());
        if !already && p.is_file() {
            out.push(shell_info(&p.to_string_lossy(), false));
        }
    }
    out
}

/// 按 PATH 找一个可执行文件（Windows 专用的最小 which）。
#[cfg(windows)]
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

fn shell_info(path: &str, default: bool) -> ShellInfo {
    let name = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_owned());
    ShellInfo {
        path: path.to_owned(),
        name,
        default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn 默认_shell_排最前且清单来自_etc_shells() {
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap();
        let shells = available_shells(&me.name);
        assert!(!shells.is_empty());
        assert!(shells[0].default, "第一项必须是默认 shell");
        assert_eq!(
            shells[0].path,
            me.shell.to_string_lossy(),
            "默认 = passwd 的登录 shell"
        );
        assert_eq!(shells.iter().filter(|s| s.default).count(), 1);
        // 除默认外都来自 /etc/shells，且不重复列默认。
        let listed = listed_shells();
        for s in &shells[1..] {
            assert!(listed.contains(&s.path), "{} 不在 /etc/shells", s.path);
            assert_ne!(s.path, shells[0].path);
        }
    }

    #[cfg(unix)]
    #[test]
    fn 未知用户回落到_bin_sh() {
        let shells = available_shells("这个用户不存在-strixmaid-test");
        assert_eq!(shells[0].path, "/bin/sh");
        assert!(shells[0].default);
    }

    #[cfg(windows)]
    #[test]
    fn 默认是_comspec_且都真实存在() {
        let shells = available_shells("whoever");
        assert!(!shells.is_empty(), "cmd.exe 任何 Windows 都有");
        assert!(shells[0].default);
        assert!(shells[0].name.eq_ignore_ascii_case("cmd.exe"));
        for s in &shells {
            assert!(std::path::Path::new(&s.path).is_file(), "{} 不存在", s.path);
        }
        assert_eq!(shells.iter().filter(|s| s.default).count(), 1);
    }
}
