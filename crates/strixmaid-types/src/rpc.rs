//! worker RPC 契约：方法名常量与参数类型（`roadmap/01-worker-execution.md` §4.1）。
//!
//! 放在 types 里是因为**两侧都要用同一份**：worker 侧（`strixmaid-core` 的
//! `worker::providers`）按这些名字注册处理器，主进程侧（`strixmaid-server` 的
//! `auth::exec`）按同样的名字发起调用。名字写成常量而不是字面量，改名时编译器
//! 会把两边一起指出来。
//!
//! # 参数与结果的形状
//!
//! 单参数的方法直接用现成的请求 DTO（`ProcessListQuery` / `LogQuery` / …），
//! 多参数的才在这里定一个小结构体。结果一律是对应的响应 DTO，
//! 错误以 [`crate::ApiError`] 原样回传——**worker 里产生的错误码、`detail`、
//! `can_retry_elevated` 都要原封不动到达前端**，中间任何一层都不该重新包装。
//!
//! # 为什么不复用 HTTP 的路径
//!
//! RPC 方法名以 provider id 为前缀（`host.` / `proc.` / `service.` / `log.` / `caps.`），
//! 与 HTTP 路径无关。两者的生命周期不同：HTTP 路径是对外契约、改动要谨慎；
//! 方法名是内部协议，helper、worker 与主进程同版本发布，可以随时调整。

use serde::{Deserialize, Serialize};

use crate::process::SignalName;
use crate::service::{UnitAction, UnitScope};

// ===========================================================================
// 方法名
// ===========================================================================

/// 主机信息（读）。参数无，结果 [`crate::system::SystemInfo`]。
pub const HOST_INFO: &str = "host.info";
/// 健康聚合（读）。参数无，结果 [`crate::system::HealthReport`]。
pub const HOST_HEALTH: &str = "host.health";
/// 时间与时区（读）。参数无，结果 [`crate::system::TimeInfo`]。
pub const HOST_TIME: &str = "host.time";
/// 改主机名（写）。参数 [`crate::system::SetHostnameReq`]。
pub const HOST_SET_HOSTNAME: &str = "host.set_hostname";
/// 改时区（写）。参数 [`crate::system::SetTimezoneReq`]。
pub const HOST_SET_TIMEZONE: &str = "host.set_timezone";
/// 重启 / 关机（写）。参数 [`crate::system::PowerReq`]。
pub const HOST_POWER: &str = "host.power";

/// 进程列表（读）。参数 [`crate::process::ProcessListQuery`]。
pub const PROC_LIST: &str = "proc.list";
/// 进程详情（读）。参数 [`PidParams`]。
pub const PROC_DETAIL: &str = "proc.detail";
/// 发信号（写*）。参数 [`SignalParams`]，见 [`crate::rpc`] 模块文档里的「写\*」说明。
pub const PROC_SIGNAL: &str = "proc.signal";
/// 调整优先级（写*）。参数 [`ReniceParams`]。
pub const PROC_RENICE: &str = "proc.renice";
/// 进程实时流（订阅）。参数 [`crate::process::ProcessListQuery`] 加 `interval_secs`。
pub const PROC_LIVE: &str = "proc.live";

/// unit 列表（读）。参数 [`crate::service::UnitListQuery`]。
pub const SERVICE_LIST: &str = "service.list";
/// unit 详情（读）。参数 [`UnitParams`]。
pub const SERVICE_DETAIL: &str = "service.detail";
/// unit 文件（读）。参数 [`UnitParams`]。
pub const SERVICE_FILE: &str = "service.file";
/// unit 依赖（读）。参数 [`UnitParams`]。
pub const SERVICE_DEPS: &str = "service.deps";
/// unit 操作（写）。参数 [`UnitActionParams`]。
pub const SERVICE_ACTION: &str = "service.action";

/// 日志查询（读）。参数 [`crate::log::LogQuery`]。
pub const LOG_QUERY: &str = "log.query";
/// 单条日志详情（读）。参数 [`CursorParams`]。
pub const LOG_ENTRY: &str = "log.entry";
/// boot 列表（读）。参数无。
pub const LOG_BOOTS: &str = "log.boots";
/// 日志跟随（订阅）。参数 [`crate::log::LogQuery`]。
pub const LOG_FOLLOW: &str = "log.follow";

/// user 层能力实测（读）。参数无，结果 [`crate::capability::UserProbe`]。
pub const CAPS_PROBE_USER: &str = "caps.probe_user";

/// 开一个 PTY（`roadmap/03-terminal.md` §4.5）。
///
/// 参数 [`TermOpenParams`]，结果 [`TermOpenResult`]，**并附带 1 个 fd**——
/// 那是主进程与 PTY 之间的 socketpair 一端。因此它注册为
/// `Dispatcher::register_fd` 而不是普通处理器。
pub const TERM_OPEN: &str = "term.open";

/// 改 PTY 窗口大小。参数 [`TermResizeParams`]，无结果。
pub const TERM_RESIZE: &str = "term.resize";

/// 关掉一个 PTY：`SIGHUP` 进程组并回收。参数 [`TermCloseParams`]，无结果。
pub const TERM_CLOSE: &str = "term.close";

// ===========================================================================
// 参数
// ===========================================================================

/// `term.open` 的参数。
///
/// # `user` 是「切到谁」，不是「能不能切」
///
/// 这条区分是本结构最要紧的一点。**该不该切身份，由主进程决定**：它按
/// `roadmap/03-terminal.md` §4.2 判断会话是否已提权，然后据此选择把这次调用
/// 投给 user worker 还是 admin worker。worker 拿到 `user` 之后只管照做，
/// 自己不再判断一次。
///
/// 反过来做——让 worker 看着 `user` 自行决定该不该切——就是在 worker 里
/// 重建一套权限判断，正是 `design.md` §5.1 要避免的「自建鉴权」：
/// 两处判断迟早会不一致，而不一致的那一侧就是提权漏洞。
///
/// user worker 收到与自身不符的 `user` 时不会去切，也不该被当成错误处理：
/// 它压根没有切的能力（非 root），内核会替我们拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermOpenParams {
    /// shell 绝对路径；`None` 表示用目标用户 passwd 项里的登录 shell。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// 目标用户名。仅 admin worker 会用到；user worker 忽略（它只能是自己）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub cols: u16,
    pub rows: u16,
}

/// `term.open` 的结果。
///
/// worker 内以 shell 的 pid 作为终端句柄；主进程侧的 `id` 到 pid 的映射
/// 由 `TerminalRegistry` 保管。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermOpenResult {
    /// shell 进程的 pid，即后续 `term.resize` / `term.close` 的句柄。
    pub pid: u32,
    /// 实际启动的 shell 路径（解析过 `None` 之后的结果）。
    pub shell: String,
    /// 实际运行身份的用户名与 uid，供主进程回填 [`crate::terminal::TerminalInfo`]。
    pub user: String,
    pub uid: u32,
}

/// `term.resize` 的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermResizeParams {
    pub pid: u32,
    pub cols: u16,
    pub rows: u16,
}

/// `term.close` 的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermCloseParams {
    pub pid: u32,
}

/// 只带一个 pid 的参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PidParams {
    pub pid: u32,
}

/// 发信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignalParams {
    pub pid: u32,
    pub signal: SignalName,
}

/// 调整优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReniceParams {
    pub pid: u32,
    pub nice: i32,
}

/// 定位一个 unit。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitParams {
    pub scope: UnitScope,
    pub unit: String,
}

/// 对一个 unit 执行操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitActionParams {
    pub scope: UnitScope,
    pub unit: String,
    pub action: UnitAction,
}

/// 只带一个游标的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorParams {
    pub cursor: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 方法名以_provider_id_为前缀() {
        for m in [HOST_INFO, HOST_HEALTH, HOST_TIME, HOST_POWER] {
            assert!(m.starts_with("host."), "{m}");
        }
        for m in [PROC_LIST, PROC_DETAIL, PROC_SIGNAL, PROC_RENICE, PROC_LIVE] {
            assert!(m.starts_with("proc."), "{m}");
        }
        for m in [SERVICE_LIST, SERVICE_DETAIL, SERVICE_ACTION] {
            assert!(m.starts_with("service."), "{m}");
        }
        for m in [LOG_QUERY, LOG_ENTRY, LOG_BOOTS, LOG_FOLLOW] {
            assert!(m.starts_with("log."), "{m}");
        }
        assert!(CAPS_PROBE_USER.starts_with("caps."));
    }

    #[test]
    fn 方法名互不重复() {
        let all = [
            HOST_INFO,
            HOST_HEALTH,
            HOST_TIME,
            HOST_SET_HOSTNAME,
            HOST_SET_TIMEZONE,
            HOST_POWER,
            PROC_LIST,
            PROC_DETAIL,
            PROC_SIGNAL,
            PROC_RENICE,
            PROC_LIVE,
            SERVICE_LIST,
            SERVICE_DETAIL,
            SERVICE_FILE,
            SERVICE_DEPS,
            SERVICE_ACTION,
            LOG_QUERY,
            LOG_ENTRY,
            LOG_BOOTS,
            LOG_FOLLOW,
            CAPS_PROBE_USER,
            TERM_OPEN,
            TERM_RESIZE,
            TERM_CLOSE,
        ];
        let unique: std::collections::HashSet<&str> = all.iter().copied().collect();
        assert_eq!(unique.len(), all.len(), "方法名有重复");
    }

    #[test]
    fn 参数往返() {
        let p = UnitActionParams {
            scope: UnitScope::System,
            unit: "nginx.service".into(),
            action: UnitAction::Restart,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<UnitActionParams>(&json).unwrap(), p);

        let s = SignalParams {
            pid: 42,
            signal: SignalName::Term,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<SignalParams>(&json).unwrap(), s);
    }
}
