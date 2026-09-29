//! 主进程侧的终端注册表（`roadmap/03-terminal.md` §4.3）。
//!
//! ```text
//! 浏览器 ⇄ WS /ws/terminal/{id} ⇄ 本模块（泵 + 回看缓冲）⇄ socketpair ⇄ worker ⇄ PTY ⇄ shell
//! ```
//!
//! 放在 core 而不是 server：Agent 端也要托管终端，它没有 axum。本模块因此**不认识
//! WS**——它只交出一个 [`Attachment`]（一个字节事件的接收端 + 一个写入方法），
//! 谁来驱动它由宿主决定。
//!
//! # 为什么终端必须有一个常驻的泵任务
//!
//! WS 断开只是「解除附着」，PTY 与 shell 继续跑（`roadmap/03-terminal.md` §4.3）。
//! 如果只在有 WS 时才读 socketpair，没人看的那段时间里 shell 的输出会把内核缓冲填满，
//! 然后 shell 被写阻塞——用户回来时看到的是一个卡死的终端，而不是「跑完了的编译」。
//! 所以每个终端从创建起就有一个 `pump` 任务无条件地读，输出一律进 [`RingBuf`]，
//! 有人附着时顺带转发一份。
//!
//! # 锁的边界
//!
//! 注册表要被多个 handler 并发使用，用的是**同步** `Mutex`，并且**任何一处都不在
//! 持锁期间 `await`**。理由不是性能洁癖：`term.close` 要等 worker 应答，worker 若因
//! 磁盘 IO 卡住几秒，持锁 await 会让整张表连同所有其它会话的 `GET /terminals` 一起停摆，
//! 而这种停摆只在压测或线上才暴露。因此所有跨进程的等待（`term.open` / `term.resize` /
//! `term.close`、向 WS 投递字节）都发生在锁外，锁内只做内存操作。
//!
//! # 关闭的四个来源与幂等
//!
//! 显式 `DELETE`、shell 退出（socketpair EOF）、空闲超时、会话登出。前两者天然会撞车：
//! 用户点「关闭」的同一毫秒 shell 正好 `exit`。因此「谁真正执行关闭」由
//! `Terminal::closed` 这一个原子的 `swap` 裁决，输的一方直接返回——既不会 panic，
//! 也不会向 worker 发第二次 `term.close`（那会打到一个已经被别人复用的 pid 上）。

mod attachment;
mod events;
mod pump;
mod registry;
mod ring;
pub mod shells;

pub use attachment::Attachment;
pub use events::{AttachEvent, CloseReason, TerminalClosed, TerminalObserver, TerminalOwner};
pub use registry::TerminalRegistry;
pub use ring::RingBuf;

use crate::session::{WorkerHandle, channel::IpcChannel};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};
use std::time::Duration;
use strixmaid_types::{rpc::TermExit, terminal::TerminalInfo};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

// ===========================================================================
// 常量
// ===========================================================================

/// 回看缓冲容量，256 KiB（`roadmap/03-terminal.md` §4.3）。
///
/// 上限是**每个终端**的常驻内存，`max_per_session`（默认 8）个终端即 2 MiB／会话。
/// 定死而不做成配置：它同时决定了刷新页面后能恢复多少历史，调小了体验会莫名变差，
/// 调大了内存占用会随会话数线性膨胀——两个方向都不该交给部署者去猜。
pub const SCROLLBACK_CAP: usize = 256 * 1024;

/// 终端 id 的随机字节数（hex 后 32 字符）。
///
/// id 是 `WS /ws/terminal/{id}` 的一部分，鉴权虽然另有 token 把关，但它仍然不该可枚举：
/// 16 字节的随机量让「猜别人的终端 id」不成立。
const TERMINAL_ID_BYTES: usize = 16;

/// 一次从 socketpair 读取的上限。够装下 `ls -R /` 这类爆发输出的一大口，
/// 又不至于让每个终端常驻一个大缓冲。
const READ_CHUNK: usize = 16 * 1024;

/// 附着通道的队列深度（每项是一次 read 的结果，最大 [`READ_CHUNK`]）。
///
/// 有界是关键：无界队列会让一个不读数据的浏览器把主进程的内存吃光。队列满了之后
/// 泵停下来，socketpair 与 PTY 的缓冲随之填满，最终顶到 shell 身上——全链路没有
/// 一处无界缓冲，慢客户端只会让自己的终端变慢。
const ATTACH_QUEUE_CAP: usize = 32;

/// 附着队列写满后最多顶多久，超过即判定这个 WS 已经死了并强制解除附着。
///
/// 没有这个上限，一个 TCP 层面还没超时的僵尸连接能让 shell 永远阻塞在写上：
/// 它「有附着」所以躲过空闲回收，又没人读所以永远不动——那是一个不会自愈的死局。
const ATTACH_STALL_LIMIT: Duration = Duration::from_secs(10);

/// 当前附着在终端上的那一个 WS。
struct AttachHandle {
    /// 附着序号。解除附着时用它认人：一个迟到的 `Drop` 不能把**后来者**摘掉。
    seq: u64,
    tx: mpsc::Sender<AttachEvent>,
}

// ===========================================================================
// Terminal
// ===========================================================================

/// 需要整体保持一致的那部分终端状态。
///
/// 三者放在同一把锁下不是偷懒：`scrollback` 与 `attached` 必须原子地一起动。
/// 附着时要「快照回看内容」+「装上新的发送端」，两步之间只要漏进一个字节，
/// 那个字节就会**排在回放之前**到达浏览器——屏幕上表现为历史与新输出交错，
/// 而且只在恰好有输出时才复现。
struct TerminalState {
    info: TerminalInfo,
    scrollback: RingBuf,
    attached: Option<AttachHandle>,
    /// 最近一次有字节流经过的单调时刻。空闲判定用它而不是 `info.last_active_ts`：
    /// 后者是给前端看的 unix 秒，会被系统改时间（NTP 回拨）扭曲。
    last_active: Instant,
}

/// 一个活着的终端。
pub struct Terminal {
    /// 16 字节随机 hex。
    id: String,
    /// 归属会话（`sessions.id`，即 token 的 sha256）。
    session_hash: String,
    /// 开这个终端的登录用户，见 [`TerminalOwner`]。
    owner: TerminalOwner,
    /// worker 内的句柄：shell 的 pid（`roadmap/03-terminal.md` §4.5）。
    pid: u32,
    /// 开这个终端的 worker。持有一份句柄（`Clone` 只是 `Arc` 自增）是为了
    /// 关闭时还能发出 `term.close`——那时会话可能已经在拆了。
    worker: WorkerHandle,
    /// worker 经 `SCM_RIGHTS` 交回的 socketpair 一端。
    ///
    /// 用 `Arc` 共享而不是拆成读写两半：泵任务独占读，附着方并发写，
    /// 两个平台的 `IpcChannel` 都允许 `&self` 同时做这两件事。
    stream: Arc<IpcChannel>,
    state: StdMutex<TerminalState>,
    /// 关闭的唯一裁决点，见模块文档「关闭的四个来源与幂等」。
    closed: AtomicBool,
    /// 下一个附着序号。
    next_seq: AtomicU64,
    /// 丢掉发送端即通知泵任务收工。用 `oneshot` 而不是标志位：泵大部分时间
    /// 停在 `readable()` 上，只有一个能进 `select!` 的 future 才叫得醒它。
    stop: StdMutex<Option<oneshot::Sender<()>>>,
}

impl std::fmt::Debug for Terminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Terminal")
            .field("id", &self.id)
            .field("pid", &self.pid)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Terminal {
    /// 终端 id。
    pub fn id(&self) -> &str {
        &self.id
    }

    /// 归属会话。
    pub fn session_hash(&self) -> &str {
        &self.session_hash
    }

    /// worker 内 shell 的 pid。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 当前元信息快照。
    pub fn info(&self) -> TerminalInfo {
        self.lock().info.clone()
    }

    /// 是否已经关闭（或正在关闭）。
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn lock(&self) -> MutexGuard<'_, TerminalState> {
        // 与本项目其余部分一致：锁内只有内存操作，不可能在持锁时 panic 破坏不变量，
        // 因此中毒的锁直接接管，而不是把一次无关的 panic 放大成整个注册表不可用。
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 附着一个新的 WS，顶掉旧的。
    ///
    /// 语义见 `roadmap/03-terminal.md` §4.3：先关旧的（原因 `replaced`），
    /// 再回放全部回看内容，然后转实时。
    pub fn attach(self: &Arc<Self>) -> Attachment {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(ATTACH_QUEUE_CAP);

        let previous = {
            let mut st = self.lock();
            let replay = st.scrollback.to_vec();
            if !replay.is_empty() {
                // 通道是刚建的，必然有位置；这里的 `try_send` 不会失败。
                // 之所以在锁内先把回放塞进队列：见 [`TerminalState`] 的注释——
                // 它和「装上发送端」必须是同一个原子步骤。
                let _ = tx.try_send(AttachEvent::Data(replay));
            }
            st.info.attached = true;
            st.attached.replace(AttachHandle { seq, tx })
        };

        if let Some(old) = previous {
            // 尽力告知原因；即便队列满了投不进去，`old.tx` 在这里 drop，
            // 旧 WS 的接收端仍会看到流结束，不会挂着。
            let _ = old.tx.try_send(AttachEvent::Closed {
                reason: CloseReason::Replaced,
                exit: None,
            });
        }

        let attachment = Attachment {
            terminal: self.clone(),
            seq,
            rx,
        };

        // 与关闭撞车的窗口：关闭是「先置 closed，再摘 attached」，若我们恰好在这两步
        // 之间装上，摘的就是我们、没问题；若在之后装上，摘的人已经走了，得自己收拾。
        // 不补这一手，附着方会永远等一个已经死掉的终端。
        if self.is_closed() {
            // 撞上关闭时拿不到退出状态（它在赢家那条路径手里），只能不带。
            self.finish_attach(seq, CloseReason::Exited, None);
        }
        attachment
    }

    /// 解除附着（仅当当前附着确实是 `seq` 那一次）。
    fn detach_if(&self, seq: u64) {
        let mut st = self.lock();
        if st.attached.as_ref().is_some_and(|a| a.seq == seq) {
            st.attached = None;
            st.info.attached = false;
        }
    }

    /// 通知并解除某一次附着。
    fn finish_attach(&self, seq: u64, reason: CloseReason, exit: Option<TermExit>) {
        let handle = {
            let mut st = self.lock();
            match st.attached.as_ref() {
                Some(a) if a.seq == seq => {
                    st.info.attached = false;
                    st.attached.take()
                }
                _ => None,
            }
        };
        if let Some(h) = handle {
            let _ = h.tx.try_send(AttachEvent::Closed { reason, exit });
        }
    }

    /// 距最近一次输出过去了多久。
    fn idle_for(&self, now: Instant) -> Option<Duration> {
        let st = self.lock();
        // 有 WS 挂着就不算空闲——用户可能正盯着一个跑了半小时的编译。
        if st.attached.is_some() {
            return None;
        }
        Some(now.saturating_duration_since(st.last_active))
    }
}

#[cfg(test)]
mod tests;
