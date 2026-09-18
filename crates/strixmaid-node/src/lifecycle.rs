//! 进程生命周期的两个接口：关停档位与启动进度上报。
//!
//! 这两样本来在 `strixmaid-server` 的 `main.rs` 里，是 `pub(crate)` 的。搬到 node
//! 是因为 Windows 服务托管（[`crate::winsvc`]）要用它们，而托管也归 node——
//! 两者必须同 crate，否则要互相 re-export。搬家时由 `pub(crate)` 转 `pub`，
//! 语义未变。

use std::time::Duration;

/// 正常关停时留给 axum 排空在途连接的上限。
///
/// axum 的 graceful shutdown 会等**所有**连接自然结束，而本服务的 WebSocket
/// （指标推送、终端）在客户端不主动断开时永远不会结束，无上限地等下去等于不退出。
const GRACEFUL_DRAIN: Duration = Duration::from_secs(20);

/// 紧急关停时留给 axum 排空在途连接的上限。见 [`ShutdownKind::Urgent`]。
const URGENT_DRAIN: Duration = Duration::from_millis(800);

/// 紧急关停时留给「落盘 + 关库」的上限。
///
/// 系统在 `CTRL_CLOSE_EVENT` / `SERVICE_CONTROL_SHUTDOWN` 之后只给几秒，
/// 超时即 `TerminateProcess`。这里取一个明显小于那个时限的值，宁可少收几个
/// 后台任务，也要保证 SQLite 有机会正常关闭。
pub const URGENT_CLEANUP: Duration = Duration::from_secs(2);

/// 关停的紧迫程度。
///
/// Unix 上只有一档：`SIGTERM` / `SIGINT` 之后进程想跑多久跑多久，systemd 的
/// `TimeoutStopSec` 默认 90 秒。Windows 不是——控制台的
/// `CTRL_CLOSE_EVENT`、`CTRL_SHUTDOWN_EVENT` 与 SCM 的 `SERVICE_CONTROL_SHUTDOWN`
/// 都只给**几秒**（受 `WaitToKillServiceTimeout` 等策略约束，默认 5 秒），
/// 到点直接 `TerminateProcess`，处理函数里做多少事都是徒劳。
///
/// 因此把「收到什么」与「还能做多少」分开：信号源负责判定档位，关停路径按档位
/// 决定做全套还是只保命。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownKind {
    /// 完整关停：排空在途请求、落盘、逐个关掉 worker 与 helper、正常关库。
    ///
    /// 对应 Unix 的 `SIGTERM` / `SIGINT`、Windows 的 Ctrl-C / Ctrl-Break，
    /// 以及 SCM 的 `SERVICE_CONTROL_STOP` / `SERVICE_CONTROL_PRESHUTDOWN`。
    Graceful,
    /// 紧急关停：只做「不做就会丢数据」的两件事——把未满分钟的指标落盘、
    /// 正常关闭 SQLite——并各自带超时。**不等** worker 退出：进程一死，
    /// IPC 管道断开，worker 读到 EOF 会自行结束，内核也会回收句柄。
    ///
    /// 对应 Windows 的 Ctrl-Close / Ctrl-Shutdown 与 SCM 的
    /// `SERVICE_CONTROL_SHUTDOWN`。Unix 侧不产生这一档。
    Urgent,
}

impl ShutdownKind {
    /// 这一档给 axum 多少时间排空在途连接。
    pub const fn drain_budget(self) -> Duration {
        match self {
            ShutdownKind::Graceful => GRACEFUL_DRAIN,
            ShutdownKind::Urgent => URGENT_DRAIN,
        }
    }
}

/// 启动进度的观察者。
///
/// 前台运行时没人关心进度，所以默认实现全是空的。Windows 服务模式下不一样：
/// SCM 要求进程在 `SERVICE_START_PENDING` 期间持续上报**递增的**
/// `dwCheckPoint`，否则会在 `dwWaitHint` 到期后判定服务启动失败并把进程杀掉。
/// 启动里最慢的几步（开库、起 helper、探测能力）都埋在 [`crate::Node::start`]
/// 内部，与其让宿主在外面猜时间，不如把上报点做成回调交给它。
pub trait StartupReporter: Send + Sync {
    /// 进入下一个启动阶段。
    fn stage(&self, _name: &str) {}
    /// 监听套接字已就绪、开始接受请求。
    fn ready(&self) {}
}

/// 前台运行时的空实现。
pub struct NoReporter;

impl StartupReporter for NoReporter {}