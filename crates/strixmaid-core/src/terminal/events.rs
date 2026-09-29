//! 终端与宿主之间的事件、身份和观察者契约。

use strixmaid_types::rpc::TermExit;

// ===========================================================================
// 关闭原因 / 附着
// ===========================================================================

/// 终端结束或附着结束的原因。[`as_str`](CloseReason::as_str) 的取值直接进审计的
/// `detail`（`roadmap/03-terminal.md` §4.6），因此是**稳定的字符串契约**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// `DELETE /terminals/{id}`。
    Deleted,
    /// shell 退出，socketpair 读到 EOF。
    Exited,
    /// 无附着且无输出超过 `idle_timeout_secs`。
    Idle,
    /// 会话登出或超时，见 [`super::TerminalRegistry::close_all_for`]。
    Logout,
    /// socketpair 出错，终端已经不可用。
    Failed,
    /// **只用于附着**：同一个终端来了新的 WS，旧的被顶掉。终端本身没有关闭。
    Replaced,
    /// **只用于附着**：这个 WS 长时间不消费，被判定为死连接（见 `ATTACH_STALL_LIMIT`）。
    /// 终端本身没有关闭。
    Stalled,
}

impl CloseReason {
    /// 写进审计与日志的稳定标识。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deleted => "deleted",
            Self::Exited => "exited",
            Self::Idle => "idle",
            Self::Logout => "logout",
            Self::Failed => "failed",
            Self::Replaced => "replaced",
            Self::Stalled => "stalled",
        }
    }

    /// 这个原因是否意味着终端本身没了（相对于「只是换了个 WS」）。
    pub const fn is_terminal_gone(self) -> bool {
        !matches!(self, Self::Replaced | Self::Stalled)
    }
}

impl std::fmt::Display for CloseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 附着方（WS）收到的事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachEvent {
    /// PTY 的原始字节，原样作为 WS 二进制帧发出。
    ///
    /// 第一条一定是回看缓冲的**全量回放**（可能为空时不发），之后才是实时输出。
    Data(Vec<u8>),
    /// 附着结束。宿主应据此发 WS close 帧。`Replaced` / `Stalled` 之外的原因还
    /// 意味着终端本身没了，`roadmap/03-terminal.md` §4.4 的 `{"t":"exit"}` 由宿主
    /// 补发；`exit` 是 shell 的退出状态——worker 真取到了才有值，取不到就没有，
    /// 宿主不得编一个顶替。
    Closed {
        reason: CloseReason,
        exit: Option<TermExit>,
    },
}

/// 开出这个终端的会话主体（登录用户），随终端保存，关闭时交给 [`TerminalObserver`]。
///
/// 单独存一份而不是关闭时反查会话：登出路径上会话正在拆除，等观察者要用时
/// 可能已经查无此人，而审计恰恰最需要这条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalOwner {
    /// 登录用户名（审计的 actor）。
    pub username: String,
    /// 登录用户 uid。
    pub uid: u32,
    /// 开终端那一刻会话是否处于提权状态。
    pub elevated: bool,
}

/// 一次真正执行了的终端关闭。
#[derive(Debug, Clone)]
pub struct TerminalClosed {
    /// 终端 id。
    pub id: String,
    /// worker 内 shell 的 pid。
    pub pid: u32,
    /// 归属会话（token 的 sha256）。
    pub session_hash: String,
    /// 开终端的登录用户。
    pub owner: TerminalOwner,
    /// shell 实际运行身份的用户名（提权终端上与 `owner.username` 不同）。
    pub target_user: String,
    /// shell 实际运行身份的 uid。
    pub target_uid: u32,
    /// 关闭原因。
    pub reason: CloseReason,
    /// shell 的退出状态（若 worker 取到了）。
    pub exit: Option<TermExit>,
}

/// 终端关闭的观察者钩子（`roadmap/03-terminal.md` §7 的审计要求）。
///
/// 空闲回收、shell 自行退出、会话登出这几种关闭发生在 core 内部，而审计的
/// `Store` 在宿主手里、也该留在宿主手里——这个钩子把「发生了什么」交出去，
/// 「记到哪里」由宿主决定。**每次真正执行的关闭恰好回调一次**（幂等裁决
/// 输掉的那一方不回调），`Deleted` 也回调——要不要跳过由实现者定夺。
///
/// 回调是同步的且在关闭路径上执行，实现者不得阻塞；要写库就 `spawn` 出去。
pub trait TerminalObserver: Send + Sync {
    /// 一个终端被真正关闭了。
    fn on_closed(&self, event: &TerminalClosed);
}
