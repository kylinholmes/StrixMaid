//! user 层能力的**实测**（`roadmap/01-worker-execution.md` §4.6）。
//!
//! 与 `capability::derive_user_caps` 的按组推导互补：推导快、离线、但只是猜
//! （「在 `adm` 组」不等于「真能读到系统日志」——ACL 可能被改过，
//! 日志后端可能压根没装）。这里在 **user worker 内**真的去试一次。
//!
//! 必须在 worker 内跑：子进程继承 worker 的 uid，试出来的才是**该用户**的可见范围。
//! 在主进程里试只会得到 root（或服务账户）的结果，那毫无意义。
//!
//! 每个字段测不出结论时留 `None`，由调用方沿用推导值——把「没测出来」当成
//! `false` 会让前端灰掉一项其实可用的能力。

use strixmaid_types::capability::UserProbe;

/// 实测当前 worker 身份的 user 层能力。
pub async fn probe_user() -> UserProbe {
    // SAFETY: getuid 无副作用。
    let uid = unsafe { libc::getuid() };
    UserProbe {
        can_read_journal: probe_system_log().await,
        // polkit 的裁决无法离线探测（`design.md` §6）：只有「已经是 root」这一种
        // 情况可以确定为真，其余留 None 沿用推导值。
        can_manage_units: (uid == 0).then_some(true),
        user_units: probe_user_units(uid),
        uid,
    }
}

/// 能否读到**系统**日志，而不只是自己产生的那些。
///
/// 判据是「读不读得到内核日志」：内核日志一定不属于当前用户，能读到就说明
/// 有系统范围的可见权限。只看 `journalctl` 有没有输出是不够的——非特权用户
/// 跑它也会成功，只是结果里只有自己的条目。
#[cfg(target_os = "linux")]
async fn probe_system_log() -> Option<bool> {
    let out = tokio::process::Command::new("journalctl")
        .args(["-n", "1", "-q", "--no-pager", "--output=cat", "_TRANSPORT=kernel"])
        .env("LC_ALL", "C")
        .env("SYSTEMD_PAGER", "")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    // 命令本身跑不起来（没装 journalctl）→ 测不出结论
    if !out.status.success() && out.stdout.is_empty() && !out.stderr.is_empty() {
        return None;
    }
    Some(!out.stdout.is_empty())
}

/// macOS 版：统一日志里读内核子系统的条目。
///
/// 非特权用户能读到的统一日志本来就比 Linux 宽（没有 journald ACL 那套），
/// 这里仍按同一判据测一次，保持两平台语义一致。
#[cfg(target_os = "macos")]
async fn probe_system_log() -> Option<bool> {
    let out = tokio::process::Command::new("/usr/bin/log")
        .args(["show", "--style", "ndjson", "--last", "1m", "--predicate", "senderImagePath CONTAINS \"/kernel\""])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!out.stdout.is_empty())
}

/// 是否支持用户级 unit：session bus 的 socket 在不在。
///
/// Linux 上是 `/run/user/<uid>/bus`（`pam_systemd` 建的）。macOS 上 launchd 的
/// `gui/<uid>` 域是内建的，恒为真。
#[cfg(target_os = "linux")]
fn probe_user_units(uid: u32) -> Option<bool> {
    Some(std::path::Path::new(&format!("/run/user/{uid}/bus")).exists())
}

/// macOS：launchd 的用户域内建，恒为真。
#[cfg(target_os = "macos")]
fn probe_user_units(_uid: u32) -> Option<bool> {
    Some(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn 本机实测不_panic_且_uid_正确() {
        let p = probe_user().await;
        // SAFETY: getuid 无副作用。
        assert_eq!(p.uid, unsafe { libc::getuid() });

        // 非 root 下 can_manage_units 必须是 None（「测不出」），不能是 Some(false)
        if p.uid != 0 {
            assert_eq!(
                p.can_manage_units, None,
                "polkit 无法离线探测，只能留 None 让推导值生效"
            );
        }
        eprintln!("本机 UserProbe: {}", serde_json::to_string(&p).unwrap());
    }

    #[tokio::test]
    async fn 实测结果可序列化且省略_none() {
        let p = UserProbe {
            can_read_journal: Some(false),
            can_manage_units: None,
            user_units: None,
            uid: 1000,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert!(json.contains("can_read_journal"));
        assert!(
            !json.contains("can_manage_units"),
            "None 应被省略，而不是序列化成 null：{json}"
        );
    }
}
