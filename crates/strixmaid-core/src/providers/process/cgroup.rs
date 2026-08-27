//! `/proc/<pid>/cgroup` → 所属 systemd unit 的反查（`docs/design.md` §13 步骤 16）。
//!
//! cgroup v2 只有一行 `0::/system.slice/nginx.service`；v1 / hybrid 时取 `name=systemd`
//! 那一行。unit 名是路径里最深的 `*.service` / `*.socket` / `*.scope` 段；位于用户管理器
//! 之下的（`user@1000.service/app.slice/xxx.service`）返回从 `user@N.service` 起的完整相对路径，
//! 这样前端能区分「系统 unit」与「某用户的 user unit」。

/// 从 `/proc/<pid>/cgroup` 原文里取出 cgroup 路径（以 `/` 开头）。
///
/// 优先 v2 的 `0::` 行，其次 v1 的 `name=systemd` 控制器行。
pub fn parse_cgroup_path(raw: &str) -> Option<String> {
    let mut systemd_v1: Option<&str> = None;
    for line in raw.lines() {
        let mut parts = line.splitn(3, ':');
        let id = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        if id == "0" && controllers.is_empty() {
            return Some(path.to_owned());
        }
        if controllers.split(',').any(|c| c == "name=systemd") {
            systemd_v1 = Some(path);
        }
    }
    systemd_v1.map(str::to_owned)
}

/// 段名是否是 unit（不含 `.slice`——slice 是分组，不是可管理的 unit）。
fn is_unit_segment(seg: &str) -> bool {
    seg.ends_with(".service") || seg.ends_with(".socket") || seg.ends_with(".scope")
}

/// 是否是用户管理器 `user@<uid>.service`。
fn is_user_manager(seg: &str) -> bool {
    seg.starts_with("user@") && seg.ends_with(".service")
}

/// 从 cgroup 路径推导所属 unit。不在任何 unit 下（`/`、纯 slice、内核线程）时为 `None`。
pub fn unit_from_cgroup_path(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let last = segments.iter().rposition(|s| is_unit_segment(s))?;
    let user_manager = segments[..=last].iter().position(|s| is_user_manager(s));
    match user_manager {
        Some(um) if um < last => Some(segments[um..=last].join("/")),
        _ => Some(segments[last].to_owned()),
    }
}

/// 一步到位：原文 → unit。
pub fn unit_from_cgroup_file(raw: &str) -> Option<String> {
    unit_from_cgroup_path(&parse_cgroup_path(raw)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v2_路径() {
        assert_eq!(
            parse_cgroup_path("0::/system.slice/nginx.service\n").as_deref(),
            Some("/system.slice/nginx.service")
        );
        assert_eq!(parse_cgroup_path("0::/\n").as_deref(), Some("/"));
        assert_eq!(parse_cgroup_path(""), None);
    }

    #[test]
    fn v1_hybrid_取_name_systemd() {
        let raw = "12:pids:/system.slice/nginx.service\n1:name=systemd:/system.slice/nginx.service\n0::/system.slice/nginx.service\n";
        assert_eq!(
            parse_cgroup_path(raw).as_deref(),
            Some("/system.slice/nginx.service")
        );
        let v1_only = "12:pids:/foo\n1:name=systemd:/system.slice/sshd.service\n";
        assert_eq!(
            parse_cgroup_path(v1_only).as_deref(),
            Some("/system.slice/sshd.service")
        );
        let no_systemd = "12:pids:/foo\n";
        assert_eq!(parse_cgroup_path(no_systemd), None);
    }

    #[test]
    fn 系统_unit() {
        assert_eq!(
            unit_from_cgroup_path("/system.slice/nginx.service").as_deref(),
            Some("nginx.service")
        );
        assert_eq!(
            unit_from_cgroup_path("/system.slice/system-getty.slice/getty@tty1.service").as_deref(),
            Some("getty@tty1.service")
        );
        assert_eq!(
            unit_from_cgroup_path("/system.slice/docker-0123abcd.scope").as_deref(),
            Some("docker-0123abcd.scope")
        );
        assert_eq!(
            unit_from_cgroup_path("/system.slice/ssh.socket").as_deref(),
            Some("ssh.socket")
        );
        assert_eq!(unit_from_cgroup_path("/init.scope").as_deref(), Some("init.scope"));
    }

    #[test]
    fn 用户_unit_带完整相对路径() {
        assert_eq!(
            unit_from_cgroup_path("/user.slice/user-1000.slice/user@1000.service/app.slice/app-foo.service").as_deref(),
            Some("user@1000.service/app.slice/app-foo.service")
        );
        assert_eq!(
            unit_from_cgroup_path("/user.slice/user-3346.slice/user@3346.service/app.slice/dbus.socket").as_deref(),
            Some("user@3346.service/app.slice/dbus.socket")
        );
        // 用户管理器自己
        assert_eq!(
            unit_from_cgroup_path("/user.slice/user-1000.slice/user@1000.service").as_deref(),
            Some("user@1000.service")
        );
        // 登录会话 scope 不在 user@ 之下
        assert_eq!(
            unit_from_cgroup_path("/user.slice/user-1000.slice/session-3.scope").as_deref(),
            Some("session-3.scope")
        );
    }

    #[test]
    fn 不在_unit_下() {
        assert_eq!(unit_from_cgroup_path("/"), None);
        assert_eq!(unit_from_cgroup_path(""), None);
        assert_eq!(unit_from_cgroup_path("/system.slice"), None);
        assert_eq!(unit_from_cgroup_path("/user.slice/user-1000.slice"), None);
    }

    #[test]
    fn 本进程() {
        // 只要求读得到且不 panic；具体值取决于启动方式（ssh.service / user@N.service / session scope）。
        if let Ok(raw) = std::fs::read_to_string("/proc/self/cgroup") {
            let _ = unit_from_cgroup_file(&raw);
        }
    }
}
