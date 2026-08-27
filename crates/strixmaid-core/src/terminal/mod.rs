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
//! 所以每个终端从创建起就有一个 [`pump`] 任务无条件地读，输出一律进 [`RingBuf`]，
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
//! [`Terminal::closed`] 这一个原子的 `swap` 裁决，输的一方直接返回——既不会 panic，
//! 也不会向 worker 发第二次 `term.close`（那会打到一个已经被别人复用的 pid 上）。

use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, Weak};
use std::time::Duration;

use rand::Rng as _;
use strixmaid_types::rpc::{
    TERM_CLOSE, TERM_OPEN, TERM_RESIZE, TermCloseParams, TermOpenParams, TermOpenResult,
    TermResizeParams,
};
use strixmaid_types::terminal::TerminalInfo;
use strixmaid_types::{ApiError, ErrorCode};
use tokio::net::UnixStream;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::config::TerminalConfig;
use crate::session::WorkerHandle;
use crate::store::now_unix;

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

// ===========================================================================
// RingBuf
// ===========================================================================

/// 固定容量的字节环形缓冲：写满后覆盖最旧的字节。
///
/// 用它而不是 `VecDeque<u8>` + `truncate`：回看缓冲的写入是高频的（每次 PTY 输出一次），
/// 而读出只在附着时发生一次。环形缓冲让写入恒定是两次 `copy_from_slice`，不搬运已有数据，
/// 也不重新分配——代价只是读出时要拼两段。
#[derive(Debug)]
pub struct RingBuf {
    buf: Box<[u8]>,
    /// 最旧那个字节的下标。
    start: usize,
    /// 已用字节数，永远 `<= buf.len()`。
    len: usize,
}

impl Default for RingBuf {
    fn default() -> Self {
        Self::with_capacity(SCROLLBACK_CAP)
    }
}

impl RingBuf {
    /// 容量为 [`SCROLLBACK_CAP`] 的缓冲。
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定容量。容量 0 表示「不保留任何回看」，写入被直接丢弃。
    pub fn with_capacity(cap: usize) -> Self {
        RingBuf {
            buf: vec![0u8; cap].into_boxed_slice(),
            start: 0,
            len: 0,
        }
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// 当前保留的字节数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 追加一段字节，必要时覆盖最旧的。
    pub fn push(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        if cap == 0 || data.is_empty() {
            return;
        }
        // 一次写入就超过容量：旧内容注定一个字节都留不下，直接取尾部重置。
        // 单独处理这一支不只是为了快——下面的两段拷贝假设 `data.len() < cap`，
        // 混在一起写会在「一次写入正好跨越自己」时出错。
        if data.len() >= cap {
            self.buf.copy_from_slice(&data[data.len() - cap..]);
            self.start = 0;
            self.len = cap;
            return;
        }

        let write_at = (self.start + self.len) % cap;
        let first = std::cmp::min(data.len(), cap - write_at);
        self.buf[write_at..write_at + first].copy_from_slice(&data[..first]);
        let rest = data.len() - first;
        if rest > 0 {
            self.buf[..rest].copy_from_slice(&data[first..]);
        }

        let filled = self.len + data.len();
        if filled > cap {
            // 溢出多少就丢掉多少个最旧的字节。
            self.start = (self.start + (filled - cap)) % cap;
            self.len = cap;
        } else {
            self.len = filled;
        }
    }

    /// 按写入顺序读出全部内容。
    pub fn to_vec(&self) -> Vec<u8> {
        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);
        if self.len == 0 {
            return out;
        }
        let first = std::cmp::min(self.len, cap - self.start);
        out.extend_from_slice(&self.buf[self.start..self.start + first]);
        out.extend_from_slice(&self.buf[..self.len - first]);
        out
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }
}

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
    /// 会话登出或超时，见 [`TerminalRegistry::close_all_for`]。
    Logout,
    /// socketpair 出错，终端已经不可用。
    Failed,
    /// **只用于附着**：同一个终端来了新的 WS，旧的被顶掉。终端本身没有关闭。
    Replaced,
    /// **只用于附着**：这个 WS 长时间不消费，被判定为死连接（见 [`ATTACH_STALL_LIMIT`]）。
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
    /// 附着结束。宿主应据此发 WS close 帧（`Replaced` / `Stalled` 之外的原因还意味着
    /// 终端本身没了，`roadmap/03-terminal.md` §4.4 的 `{"t":"exit"}` 由宿主补发）。
    Closed(CloseReason),
}

/// 当前附着在终端上的那一个 WS。
struct AttachHandle {
    /// 附着序号。解除附着时用它认人：一个迟到的 `Drop` 不能把**后来者**摘掉。
    seq: u64,
    tx: mpsc::Sender<AttachEvent>,
}

/// 一次附着。`Drop` 即解除附着（终端继续跑）。
///
/// 把解除绑在 `Drop` 上而不是某个 `detach()` 方法：WS 可能因为任务被 abort、panic 展开、
/// 运行时关停而消失，只有 `Drop` 在这些路径上都会执行。漏掉一次解除，终端就会永远
/// 显示 `attached = true`，从而躲过空闲回收——一个不会自愈的泄漏。
pub struct Attachment {
    terminal: Arc<Terminal>,
    seq: u64,
    rx: mpsc::Receiver<AttachEvent>,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("terminal", &self.terminal.id)
            .field("seq", &self.seq)
            .finish()
    }
}

impl Attachment {
    /// 取下一个事件；`None` 表示这次附着彻底结束（通道被关闭）。
    ///
    /// 注意 [`AttachEvent::Closed`] 是**尽力而为**的：队列满时投不进去，此时附着方
    /// 只会看到 `None`。因此宿主判断「结束」必须以 `None` 为准，`Closed` 只用来
    /// 决定 close 帧里写什么原因。
    pub async fn next(&mut self) -> Option<AttachEvent> {
        self.rx.recv().await
    }

    /// 把浏览器发来的字节写进 PTY。
    ///
    /// 不加写锁：同一时刻只有一个附着，而键盘输入只从附着方来。
    ///
    /// 走 `writable` + `try_write` 而不是 `AsyncWriteExt::write_all`：后者要 `&mut`，
    /// 而这条 stream 是与泵任务共享的 `Arc`（tokio 的就绪 API 都取 `&self`，
    /// 这正是能一边读一边写而不 split 的原因）。
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        let stream = &*self.terminal.stream;
        let mut sent = 0;
        while sent < data.len() {
            stream.writable().await?;
            match stream.try_write(&data[sent..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "终端 socket 拒绝写入",
                    ));
                }
                Ok(n) => sent += n,
                // 就绪是一个提示而不是保证：另一个写者可能抢先填满了缓冲。
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// 所附着终端的 id。
    pub fn terminal_id(&self) -> &str {
        &self.terminal.id
    }

    /// 所附着的终端。
    pub fn terminal(&self) -> &Arc<Terminal> {
        &self.terminal
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.terminal.detach_if(self.seq);
    }
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
    /// worker 内的句柄：shell 的 pid（`roadmap/03-terminal.md` §4.5）。
    pid: u32,
    /// 开这个终端的 worker。持有一份句柄（`Clone` 只是 `Arc` 自增）是为了
    /// 关闭时还能发出 `term.close`——那时会话可能已经在拆了。
    worker: WorkerHandle,
    /// worker 经 `SCM_RIGHTS` 交回的 socketpair 一端。
    ///
    /// 用 `Arc` 共享而不是拆成读写两半：泵任务独占读，附着方并发写，
    /// tokio 允许 `&UnixStream` 同时做这两件事。
    stream: Arc<UnixStream>,
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
            let _ = old.tx.try_send(AttachEvent::Closed(CloseReason::Replaced));
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
            self.finish_attach(seq, CloseReason::Exited);
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
    fn finish_attach(&self, seq: u64, reason: CloseReason) {
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
            let _ = h.tx.try_send(AttachEvent::Closed(reason));
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

// ===========================================================================
// TerminalRegistry
// ===========================================================================

/// 注册表的内部表。**只在同步 `Mutex` 下访问，绝不跨 `await` 持有**。
#[derive(Default)]
struct Inner {
    terms: HashMap<String, Arc<Terminal>>,
    /// 会话 → 已通过上限检查但还没插进 `terms` 的数量。
    ///
    /// 没有它就有 TOCTOU：`term.open` 是一次跨进程往返，两个并发的
    /// `POST /terminals` 会双双读到「还差一个到上限」，于是一起建起来。
    /// 上限存在的意义就是挡住跑飞的前端，一个能被并发绕过的上限等于没有。
    reserving: HashMap<String, usize>,
}

impl Inner {
    fn count_for(&self, session_hash: &str) -> usize {
        self.terms
            .values()
            .filter(|t| t.session_hash == session_hash)
            .count()
    }

    fn release(&mut self, session_hash: &str) {
        if let Some(n) = self.reserving.get_mut(session_hash) {
            *n -= 1;
            if *n == 0 {
                self.reserving.remove(session_hash);
            }
        }
    }
}

/// 一个已通过上限检查、尚未落表的名额。`Drop` 即归还——`term.open` 失败、
/// fd 有问题、future 被取消，任何一条路径都不会把名额漏掉。
struct Reservation<'a> {
    registry: &'a TerminalRegistry,
    session_hash: String,
    held: bool,
}

impl Reservation<'_> {
    /// 落表：插入 `terms` 与归还名额必须在**同一把锁**里完成，
    /// 否则中间那一瞬会少算一个，正好够让第 N+1 个终端挤进来。
    fn commit(mut self, term: Arc<Terminal>) {
        let mut inner = self.registry.lock();
        inner.terms.insert(term.id.clone(), term);
        inner.release(&self.session_hash);
        self.held = false;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.held {
            self.registry.lock().release(&self.session_hash);
        }
    }
}

/// 主进程持有的全部终端。
///
/// 用 `Arc<TerminalRegistry>` 共享给各 handler；内部可变见 [`Inner`]。
pub struct TerminalRegistry {
    cfg: TerminalConfig,
    inner: StdMutex<Inner>,
}

impl std::fmt::Debug for TerminalRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalRegistry")
            .field("live", &self.lock().terms.len())
            .finish()
    }
}

impl TerminalRegistry {
    /// 建一个空注册表。
    pub fn new(cfg: TerminalConfig) -> Arc<Self> {
        Arc::new(TerminalRegistry {
            cfg,
            inner: StdMutex::new(Inner::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 生效的配置。
    pub fn config(&self) -> &TerminalConfig {
        &self.cfg
    }

    /// 开一个终端：向 `worker` 发 `term.open`，接管它交回的 fd，起泵任务。
    ///
    /// **以谁的身份跑由「发给哪个 worker」决定**（见 `strixmaid_types::rpc::TermOpenParams`
    /// 的注释）。本模块不做鉴权判断——`user` 是否需要提权由调用方在选 worker 时决定，
    /// 这里再判一次就是第二套鉴权，正是 `design.md` §5.1 要避免的。
    pub async fn open(
        self: &Arc<Self>,
        session_hash: &str,
        worker: &WorkerHandle,
        params: TermOpenParams,
    ) -> Result<TerminalInfo, ApiError> {
        if params.cols == 0 || params.rows == 0 {
            return Err(ApiError::invalid_request("终端尺寸的行列数都必须大于 0"));
        }
        // 先占名额再发 RPC：反过来的话，超限时已经在 worker 里 fork 出了一个 shell，
        // 还得再发一次 `term.close` 去收拾。
        let slot = self.reserve(session_hash)?;

        let (cols, rows) = (params.cols, params.rows);
        let value = serde_json::to_value(&params)
            .map_err(|e| ApiError::internal(format!("term.open 参数序列化失败: {e}")))?;
        let (result, fds) = worker.call_with_fds(TERM_OPEN, value).await?;
        let result: TermOpenResult = serde_json::from_value(result)
            .map_err(|e| ApiError::internal(format!("term.open 响应格式错误: {e}")))?;

        // fd 数量不对就是协议错。`fds` 在这个作用域结束时全部关闭，不会泄漏；
        // worker 侧的 shell 会因为 socketpair 断开而收到 EOF 自行退出。
        let mut fds = fds;
        if fds.len() != 1 {
            return Err(ApiError::internal(format!(
                "term.open 应附带 1 个 fd，实际 {}",
                fds.len()
            )));
        }
        let stream = wrap_stream(fds.remove(0))?;

        let now = now_unix();
        let id = random_hex(TERMINAL_ID_BYTES);
        let info = TerminalInfo {
            id: id.clone(),
            shell: result.shell,
            user: result.user,
            uid: result.uid,
            cols,
            rows,
            created_ts: now,
            last_active_ts: now,
            attached: false,
        };
        let (stop_tx, stop_rx) = oneshot::channel();
        let term = Arc::new(Terminal {
            id,
            session_hash: session_hash.to_string(),
            pid: result.pid,
            worker: worker.clone(),
            stream: Arc::new(stream),
            state: StdMutex::new(TerminalState {
                info: info.clone(),
                scrollback: RingBuf::new(),
                attached: None,
                last_active: Instant::now(),
            }),
            closed: AtomicBool::new(false),
            next_seq: AtomicU64::new(1),
            stop: StdMutex::new(Some(stop_tx)),
        });

        slot.commit(term.clone());
        // 泵只持有注册表的 `Weak`：宿主放掉注册表时（进程关停）泵不该反过来
        // 把它吊着不放，那会让一堆终端连同 worker 句柄一起活到进程结束。
        tokio::spawn(pump(term, Arc::downgrade(self), stop_rx));

        tracing::info!(
            id = %info.id,
            pid = result.pid,
            user = %info.user,
            shell = %info.shell,
            "终端已创建"
        );
        Ok(info)
    }

    /// 占一个名额。超限时返回 [`ErrorCode::Conflict`]（→ HTTP 409，
    /// `roadmap/03-terminal.md` §7 的验收标准）。
    fn reserve(&self, session_hash: &str) -> Result<Reservation<'_>, ApiError> {
        let mut inner = self.lock();
        let live = inner.count_for(session_hash);
        let reserving = inner.reserving.get(session_hash).copied().unwrap_or(0);
        if live + reserving >= self.cfg.max_per_session {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!(
                    "本会话的终端数已达上限 {}，请先关闭一个",
                    self.cfg.max_per_session
                ),
            ));
        }
        *inner.reserving.entry(session_hash.to_string()).or_insert(0) += 1;
        Ok(Reservation {
            registry: self,
            session_hash: session_hash.to_string(),
            held: true,
        })
    }

    /// 按 id 取本会话的终端。
    ///
    /// `session_hash` 不匹配时返回 [`ErrorCode::NotFound`] 而不是 403：
    /// 别的会话不该能通过错误码的差异**探测出某个 id 是否存在**。
    pub fn get(&self, session_hash: &str, id: &str) -> Result<Arc<Terminal>, ApiError> {
        self.lock()
            .terms
            .get(id)
            .filter(|t| t.session_hash == session_hash)
            .cloned()
            .ok_or_else(|| ApiError::not_found("终端不存在或已关闭"))
    }

    /// 本会话的全部终端，按创建时间排序。
    pub fn list_for(&self, session_hash: &str) -> Vec<TerminalInfo> {
        let terms: Vec<Arc<Terminal>> = self
            .lock()
            .terms
            .values()
            .filter(|t| t.session_hash == session_hash)
            .cloned()
            .collect();
        // 取 `info()` 要拿每个终端自己的锁，因此先放掉注册表的锁再取——
        // 两把锁嵌套是死锁的经典配方，即便当前顺序安全也不留这个隐患。
        let mut out: Vec<TerminalInfo> = terms.iter().map(|t| t.info()).collect();
        out.sort_by(|a, b| a.created_ts.cmp(&b.created_ts).then(a.id.cmp(&b.id)));
        out
    }

    /// 本会话当前的终端数。
    pub fn count_for(&self, session_hash: &str) -> usize {
        self.lock().count_for(session_hash)
    }

    /// 附着一个 WS。
    pub fn attach(&self, session_hash: &str, id: &str) -> Result<Attachment, ApiError> {
        Ok(self.get(session_hash, id)?.attach())
    }

    /// 改窗口大小。
    pub async fn resize(
        &self,
        session_hash: &str,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), ApiError> {
        if cols == 0 || rows == 0 {
            return Err(ApiError::invalid_request("终端尺寸的行列数都必须大于 0"));
        }
        let term = self.get(session_hash, id)?;
        let params = serde_json::to_value(TermResizeParams {
            pid: term.pid,
            cols,
            rows,
        })
        .map_err(|e| ApiError::internal(format!("term.resize 参数序列化失败: {e}")))?;
        // 先等 worker 真的 ioctl 成功再改本地记录：反过来会让 `GET /terminals`
        // 报告一个 PTY 上并不成立的尺寸。
        term.worker.call(TERM_RESIZE, params).await?;
        let mut st = term.lock();
        st.info.cols = cols;
        st.info.rows = rows;
        Ok(())
    }

    /// 关闭一个终端（`DELETE /terminals/{id}`）。
    pub async fn close(
        &self,
        session_hash: &str,
        id: &str,
        reason: CloseReason,
    ) -> Result<(), ApiError> {
        let term = self.get(session_hash, id)?;
        self.finish(&term, reason).await;
        Ok(())
    }

    /// 关闭一个会话的**全部**终端，返回实际关掉的个数。
    ///
    /// 会话登出与会话超时都要调它（`roadmap/03-terminal.md` §4.3）：终端跑的是
    /// 用户身份的 shell，会话都没了还留着它，等于留下一个没有主人的登录态。
    pub async fn close_all_for(&self, session_hash: &str, reason: CloseReason) -> usize {
        // 先在锁内把它们从表里全部摘走，再逐个关。摘表这一步必须是原子的，
        // 否则一个并发的 `POST /terminals` 会在登出中途插进来一个新终端。
        let doomed: Vec<Arc<Terminal>> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .terms
                .values()
                .filter(|t| t.session_hash == session_hash)
                .map(|t| t.id.clone())
                .collect();
            ids.iter().filter_map(|id| inner.terms.remove(id)).collect()
        };
        let mut closed = 0;
        for term in doomed {
            if shutdown(&term, reason).await {
                closed += 1;
            }
        }
        closed
    }

    /// 回收空闲终端，返回关掉的个数（`roadmap/03-terminal.md` §4.3「空闲」）。
    ///
    /// `idle_timeout_secs = 0` 视为**关闭空闲回收**：字面理解「0 秒即空闲」
    /// 会让每个新终端在下一次扫描时立刻被关掉，那不是任何人想要的配置。
    pub async fn sweep_idle(&self) -> usize {
        let limit = self.cfg.idle_timeout();
        if limit.is_zero() {
            return 0;
        }
        let now = Instant::now();
        let doomed: Vec<Arc<Terminal>> = {
            let candidates: Vec<Arc<Terminal>> = self.lock().terms.values().cloned().collect();
            // 判定要拿每个终端自己的锁，所以放掉注册表的锁之后再筛。
            let ids: Vec<String> = candidates
                .iter()
                .filter(|t| t.idle_for(now).is_some_and(|idle| idle >= limit))
                .map(|t| t.id.clone())
                .collect();
            let mut inner = self.lock();
            ids.iter().filter_map(|id| inner.terms.remove(id)).collect()
        };
        let mut closed = 0;
        for term in doomed {
            tracing::info!(id = %term.id, pid = term.pid, "终端空闲超时，关闭");
            if shutdown(&term, CloseReason::Idle).await {
                closed += 1;
            }
        }
        closed
    }

    /// 摘表 + 关闭。返回 `true` 表示本次调用是真正执行关闭的那一个。
    async fn finish(&self, term: &Arc<Terminal>, reason: CloseReason) -> bool {
        // 先摘表：`GET /terminals` 立刻不再列出它，不必等 worker 应答。
        self.lock().terms.remove(&term.id);
        shutdown(term, reason).await
    }
}

/// 真正的关闭动作，幂等。返回 `false` 表示别人已经关过了。
async fn shutdown(term: &Arc<Terminal>, reason: CloseReason) -> bool {
    // 唯一的裁决点：显式 DELETE 与 shell 退出撞车是**正常**竞态，
    // 输的一方必须安静地返回，尤其不能重发 `term.close`——pid 可能已被复用。
    if term.closed.swap(true, Ordering::AcqRel) {
        return false;
    }

    // 通知并摘掉附着方（锁内只做内存操作）。
    let handle = {
        let mut st = term.lock();
        st.info.attached = false;
        st.attached.take()
    };
    if let Some(h) = handle {
        let _ = h.tx.try_send(AttachEvent::Closed(reason));
    }

    // 叫醒泵任务。它可能就是当前调用者（EOF 那条路径），那也没关系：
    // 它会在 `select!` 里看到 stop 已就绪，但那时它已经走出循环了。
    term.stop.lock().unwrap_or_else(|e| e.into_inner()).take();

    // 跨进程的等待放在所有锁之外。
    let params = serde_json::json!(TermCloseParams { pid: term.pid });
    match term.worker.call(TERM_CLOSE, params).await {
        Ok(_) => tracing::info!(id = %term.id, pid = term.pid, %reason, "终端已关闭"),
        // worker 先走一步是常态（会话登出时 worker 与终端一起拆），
        // 那种情况下 PTY 已随 worker 进程消失，没有需要补救的资源。
        Err(e) => tracing::debug!(
            id = %term.id, pid = term.pid, %reason, error = %e,
            "向 worker 发送 term.close 失败"
        ),
    }
    true
}

/// 常驻泵：把 socketpair 的输出写进回看缓冲，并转发给当前附着方。
async fn pump(
    term: Arc<Terminal>,
    registry: Weak<TerminalRegistry>,
    mut stop: oneshot::Receiver<()>,
) {
    let stream = Arc::clone(&term.stream);
    let mut buf = vec![0u8; READ_CHUNK];
    let reason = loop {
        // 用就绪 API 而不是 `AsyncReadExt::read`：后者要 `&mut UnixStream`，
        // 而这条 stream 与附着方的写共享同一个 `Arc`（见 [`Attachment::write`]）。
        // `readable()` 是取消安全的，被 `select!` 丢掉不会吞字节。
        let ready = tokio::select! {
            // 关闭优先：已经决定要关的终端不必再多等一轮。
            biased;
            _ = &mut stop => return,
            r = stream.readable() => r,
        };
        if let Err(e) = ready {
            tracing::warn!(id = %term.id, pid = term.pid, error = %e, "终端 socket 不可读");
            break CloseReason::Failed;
        }
        match stream.try_read(&mut buf) {
            Ok(0) => break CloseReason::Exited,
            Ok(n) => forward(&term, &buf[..n]).await,
            // 就绪只是提示，假唤醒要接着等。
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => {
                tracing::warn!(id = %term.id, pid = term.pid, error = %e, "终端读取失败");
                break CloseReason::Failed;
            }
        }
    };

    match registry.upgrade() {
        Some(reg) => {
            reg.finish(&term, reason).await;
        }
        // 注册表没了（进程在关停）：表已经不存在，直接走关闭动作。
        None => {
            shutdown(&term, reason).await;
        }
    }
}

/// 把一段输出写进回看缓冲并转发给附着方（如果有）。
async fn forward(term: &Arc<Terminal>, data: &[u8]) {
    let target = {
        let mut st = term.lock();
        st.scrollback.push(data);
        st.last_active = Instant::now();
        st.info.last_active_ts = now_unix();
        st.attached.as_ref().map(|a| (a.seq, a.tx.clone()))
    };
    let Some((seq, tx)) = target else {
        // 没人看，写进回看缓冲就够了——这正是「断开后 shell 继续跑」的实现。
        return;
    };

    // 快路径：队列有位置时一次同步调用就完事。
    let pending = match tx.try_send(AttachEvent::Data(data.to_vec())) {
        Ok(()) => return,
        Err(TrySendError::Closed(_)) => {
            // 附着方刚 drop，它的 `Drop` 会（或已经）解除附着，这里不重复动作。
            return;
        }
        Err(TrySendError::Full(ev)) => ev,
    };
    match tokio::time::timeout(ATTACH_STALL_LIMIT, tx.send(pending)).await {
        // 慢路径走到这里就是背压在起作用：泵停住 → socketpair 满 → shell 被顶住。
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(_) => {
            // 十秒都塞不进去，这个 WS 已经死了。解除附着让终端回到「无人附着」，
            // 从而重新受空闲回收管辖，见 [`ATTACH_STALL_LIMIT`]。
            tracing::warn!(
                id = %term.id,
                stall_secs = ATTACH_STALL_LIMIT.as_secs(),
                "附着方长时间不消费，强制解除附着"
            );
            term.finish_attach(seq, CloseReason::Stalled);
        }
    }
}

/// 把 worker 交回的 fd 变成 tokio 的 `UnixStream`。
fn wrap_stream(fd: OwnedFd) -> Result<UnixStream, ApiError> {
    let std_stream = std::os::unix::net::UnixStream::from(fd);
    std_stream
        .set_nonblocking(true)
        .map_err(|e| ApiError::internal(format!("终端 socket 设置非阻塞失败: {e}")))?;
    UnixStream::from_std(std_stream)
        .map_err(|e| ApiError::internal(format!("终端 socket 注册到 tokio 失败: {e}")))
}

/// 随机 hex。用 `rand::rng()`（CSPRNG，OS 熵播种）而不是任何计数器：
/// 终端 id 出现在 URL 里，可枚举的 id 会把「猜 id」变成一条攻击面。
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::sync::atomic::AtomicU32;

    use futures::future::BoxFuture;
    use serde_json::Value;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    use super::*;
    use crate::worker::{self, Dispatcher};

    // -------------------------------------------------------------- RingBuf

    #[test]
    fn 未写满时按写入顺序读出() {
        let mut r = RingBuf::with_capacity(8);
        r.push(b"ab");
        r.push(b"cd");
        assert_eq!(r.len(), 4);
        assert_eq!(r.to_vec(), b"abcd".to_vec());
    }

    #[test]
    fn 环绕后读出的是最后一段() {
        let mut r = RingBuf::with_capacity(8);
        r.push(b"abcde");
        r.push(b"fghij");
        // 一共写了 10 个字节，只该留下最后 8 个，且顺序不变。
        assert_eq!(r.len(), 8);
        assert_eq!(r.to_vec(), b"cdefghij".to_vec());

        // 再绕一圈：start 已经不在 0 上，这一步才真正考验下标计算。
        r.push(b"klm");
        assert_eq!(r.to_vec(), b"fghijklm".to_vec());
    }

    #[test]
    fn 一次写入超过容量只保留末尾() {
        let mut r = RingBuf::with_capacity(8);
        r.push(b"xxxx");
        r.push(b"0123456789abcdefghij");
        assert_eq!(r.len(), 8);
        assert_eq!(r.to_vec(), b"cdefghij".to_vec());
    }

    #[test]
    fn 任意分片写入都与朴素模型一致() {
        // 朴素模型：把所有字节接起来，只留最后 cap 个。环形缓冲的每一步都必须与它相同。
        // 只测「没写满」的用例抓不到环绕下标算错，这个逐步比对能。
        const CAP: usize = 64;
        let mut ring = RingBuf::with_capacity(CAP);
        let mut model: Vec<u8> = Vec::new();
        let mut byte: u8 = 0;
        // 固定种子的 LCG：分片长度覆盖「不跨界」「正好到界」「跨界」「超过容量」四种。
        let mut seed: u32 = 0x9E37_79B9;
        for _ in 0..500 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (seed >> 16) as usize % (CAP * 2 + 3);
            let chunk: Vec<u8> = (0..n)
                .map(|_| {
                    byte = byte.wrapping_add(1);
                    byte
                })
                .collect();
            ring.push(&chunk);
            model.extend_from_slice(&chunk);
            if model.len() > CAP {
                model.drain(..model.len() - CAP);
            }
            assert_eq!(ring.to_vec(), model, "写入 {n} 字节后不一致");
            assert_eq!(ring.len(), model.len());
        }
    }

    #[test]
    fn 零容量的缓冲不保留任何内容() {
        let mut r = RingBuf::with_capacity(0);
        r.push(b"abc");
        assert!(r.is_empty());
        assert_eq!(r.to_vec(), Vec::<u8>::new());
    }

    // ------------------------------------------------------------- 测试脚手架

    /// 进程内的假 worker：`term.open` 用一对 socketpair 冒充 PTY（worker 侧那一端
    /// 留在 `ptys` 里，测试可以往里写来模拟 shell 输出、丢掉它来模拟 shell 退出），
    /// `term.close` / `term.resize` 只记账，供断言「发了几次、发给谁」。
    /// 进程内 worker 的独占锁。
    ///
    /// 这些用例每个都在**同一个进程里**同时扮演主进程与 worker：socketpair 的两端、
    /// `SCM_RIGHTS` 收到的副本、以及每个 `#[tokio::test]` 自己那个用完即弃的 runtime，
    /// 全都挤在同一张 fd 表上。并行跑时能观察到一个用例的终端 socket 莫名其妙地
    /// 变成「读端已关」（对端仍然打开、写得进去，读却返回 0），随之被判为 shell 退出。
    /// 这不是注册表的逻辑问题——`libc::read` 直接读也是 0，socket 在内核里就是那个状态；
    /// 根因在本任务范围之外（怀疑是多 runtime 反复创建销毁时 fd 号被回收所致），
    /// 尚未查清。
    ///
    /// 真实部署里不存在这个前提：worker 是**另一个进程**，fd 号不共享，主进程一辈子
    /// 只有一个 runtime。所以这里用一把锁把「进程内 worker」串起来，而不是去弱化断言。
    ///
    /// 用 tokio 的 `Mutex` 而不是 `std` 的：守卫要跨 `await` 持有整个用例；
    /// 顺带也不会中毒——一个用例 panic 不该把后面所有用例都拖垮。
    static IN_PROCESS_WORKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct FakeWorker {
        handle: WorkerHandle,
        ptys: Arc<StdMutex<HashMap<u32, std::os::unix::net::UnixStream>>>,
        closes: Arc<StdMutex<Vec<u32>>>,
        resizes: Arc<StdMutex<Vec<(u32, u16, u16)>>>,
    }

    impl FakeWorker {
        async fn start() -> FakeWorker {
            let ptys: Arc<StdMutex<HashMap<u32, std::os::unix::net::UnixStream>>> =
                Arc::new(StdMutex::new(HashMap::new()));
            let closes: Arc<StdMutex<Vec<u32>>> = Arc::new(StdMutex::new(Vec::new()));
            let resizes: Arc<StdMutex<Vec<(u32, u16, u16)>>> = Arc::new(StdMutex::new(Vec::new()));
            let next_pid = Arc::new(AtomicU32::new(4000));

            let mut d = Dispatcher::new();
            {
                let ptys = ptys.clone();
                let next_pid = next_pid.clone();
                d.register_fd(
                    TERM_OPEN,
                    Arc::new(move |params: Value| {
                        let ptys = ptys.clone();
                        let next_pid = next_pid.clone();
                        Box::pin(async move {
                            let req: TermOpenParams = serde_json::from_value(params)
                                .map_err(|e| ApiError::invalid_request(e.to_string()))?;
                            let (worker_side, main_side) =
                                std::os::unix::net::UnixStream::pair().unwrap();
                            let pid = next_pid.fetch_add(1, Ordering::Relaxed);
                            ptys.lock().unwrap().insert(pid, worker_side);
                            let result = TermOpenResult {
                                pid,
                                shell: req.shell.unwrap_or_else(|| "/bin/sh".into()),
                                user: req.user.unwrap_or_else(|| "tester".into()),
                                uid: 1000,
                            };
                            Ok((
                                serde_json::to_value(result).unwrap(),
                                vec![OwnedFd::from(main_side)],
                            ))
                        })
                            as BoxFuture<'static, Result<(Value, Vec<OwnedFd>), ApiError>>
                    }),
                );
            }
            {
                let closes = closes.clone();
                d.register_fn(TERM_CLOSE, move |params: Value| {
                    let closes = closes.clone();
                    async move {
                        let p: TermCloseParams = serde_json::from_value(params)
                            .map_err(|e| ApiError::invalid_request(e.to_string()))?;
                        closes.lock().unwrap().push(p.pid);
                        Ok(Value::Null)
                    }
                });
            }
            {
                let resizes = resizes.clone();
                d.register_fn(TERM_RESIZE, move |params: Value| {
                    let resizes = resizes.clone();
                    async move {
                        let p: TermResizeParams = serde_json::from_value(params)
                            .map_err(|e| ApiError::invalid_request(e.to_string()))?;
                        resizes.lock().unwrap().push((p.pid, p.cols, p.rows));
                        Ok(Value::Null)
                    }
                });
            }

            let (main_side, worker_side) = UnixStream::pair().unwrap();
            tokio::spawn(async move {
                let _ = worker::serve(worker_side, Arc::new(d)).await;
            });
            // pid 传 -1：这个 worker 是进程内的，绝不能真去 kill 谁。
            let handle =
                WorkerHandle::connect(OwnedFd::from(main_side.into_std().unwrap()), -1, None)
                    .await
                    .expect("假 worker 握手失败");

            FakeWorker {
                handle,
                ptys,
                closes,
                resizes,
            }
        }

        /// 取走 worker 侧的 PTY 端。丢掉返回值 = shell 退出（主进程读到 EOF）。
        fn pty(&self, pid: u32) -> UnixStream {
            let s = self
                .ptys
                .lock()
                .unwrap()
                .remove(&pid)
                .expect("没有这个 pid 对应的 PTY");
            s.set_nonblocking(true).unwrap();
            UnixStream::from_std(s).unwrap()
        }

        fn closes(&self) -> Vec<u32> {
            self.closes.lock().unwrap().clone()
        }
    }

    fn registry(max_per_session: usize, idle_timeout_secs: u64) -> Arc<TerminalRegistry> {
        TerminalRegistry::new(TerminalConfig {
            idle_timeout_secs,
            max_per_session,
        })
    }

    fn params() -> TermOpenParams {
        TermOpenParams {
            shell: None,
            user: None,
            cols: 80,
            rows: 24,
        }
    }

    async fn open_one(reg: &Arc<TerminalRegistry>, w: &FakeWorker, session: &str) -> TerminalInfo {
        reg.open(session, &w.handle, params())
            .await
            .expect("开终端失败")
    }

    /// 轮询等待某个条件成立；超时即 panic（而不是让断言在竞态下随机失败）。
    async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("等待超时: {what}");
    }

    // ------------------------------------------------------------ 注册表语义

    #[tokio::test]
    async fn 超过上限的终端被拒且错误码映射为_409() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(2, 1800);

        open_one(&reg, &w, "s1").await;
        open_one(&reg, &w, "s1").await;
        let err = reg.open("s1", &w.handle, params()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict);
        assert_eq!(err.http_status(), 409);

        // 上限是「每会话」的，别的会话不受影响。
        open_one(&reg, &w, "s2").await;
        assert_eq!(reg.count_for("s1"), 2);
        assert_eq!(reg.count_for("s2"), 1);
    }

    #[tokio::test]
    async fn 并发创建不会突破上限() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(3, 1800);

        // 八个请求同时进来。名额若只在「发 RPC 之前查一次表」，这里会漏进好几个。
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let reg = reg.clone();
            let handle = w.handle.clone();
            tasks.push(tokio::spawn(async move {
                reg.open("s1", &handle, params()).await
            }));
        }
        let mut ok = 0;
        for t in tasks {
            if t.await.unwrap().is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 3, "成功的创建数必须正好等于上限");
        assert_eq!(reg.count_for("s1"), 3);
    }

    #[tokio::test]
    async fn 关闭是幂等的且只向_worker_发一次_term_close() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let term = reg.get("s1", &info.id).unwrap();

        // 「用户点了 DELETE」与「shell 恰好退出」同时发生，这是正常竞态。
        let (a, b) = tokio::join!(
            reg.finish(&term, CloseReason::Deleted),
            reg.finish(&term, CloseReason::Exited)
        );
        assert!(a ^ b, "只能有一个调用真正执行关闭，实际 a={a} b={b}");
        assert_eq!(
            w.closes(),
            vec![term.pid()],
            "term.close 只能发一次——pid 可能已被复用"
        );
        assert!(reg.list_for("s1").is_empty());

        // 事后再关一次也不能出事。
        assert!(!reg.finish(&term, CloseReason::Deleted).await);
        assert_eq!(w.closes(), vec![term.pid()]);
        // 表里已经没有了，DELETE 走的是 404 而不是第二次关闭。
        let err = reg
            .close("s1", &info.id, CloseReason::Deleted)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn close_all_for_只关本会话的终端() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let a1 = open_one(&reg, &w, "s1").await;
        let a2 = open_one(&reg, &w, "s1").await;
        let b1 = open_one(&reg, &w, "s2").await;
        let pids: HashMap<String, u32> = ["s1", "s2"]
            .iter()
            .flat_map(|s| reg.list_for(s).into_iter().map(|i| i.id))
            .map(|id| {
                let t = reg.get("s1", &id).or_else(|_| reg.get("s2", &id)).unwrap();
                (id, t.pid())
            })
            .collect();

        assert_eq!(reg.close_all_for("s1", CloseReason::Logout).await, 2);
        assert!(reg.list_for("s1").is_empty());
        assert_eq!(
            reg.list_for("s2")
                .iter()
                .map(|i| i.id.clone())
                .collect::<Vec<_>>(),
            vec![b1.id.clone()],
            "别的会话的终端必须原封不动"
        );

        let mut closed = w.closes();
        closed.sort_unstable();
        let mut expect = vec![pids[&a1.id], pids[&a2.id]];
        expect.sort_unstable();
        assert_eq!(closed, expect, "只该关掉 s1 的两个终端");

        // s2 的终端还能正常用。
        assert!(reg.get("s2", &b1.id).is_ok());
    }

    #[tokio::test]
    async fn 别的会话既看不到也拿不到这个终端() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        assert!(reg.list_for("s2").is_empty());
        let err = reg.get("s2", &info.id).unwrap_err();
        // 不能用 403：那等于告诉对方「这个 id 存在」。
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(reg.attach("s2", &info.id).is_err());
    }

    // ---------------------------------------------------------------- 附着

    #[tokio::test]
    async fn 断开后再附着能拿到之前的全部输出() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let term = reg.get("s1", &info.id).unwrap();
        let mut pty = w.pty(term.pid());

        let mut first = reg.attach("s1", &info.id).unwrap();
        assert!(term.info().attached);
        pty.write_all(b"hello").await.unwrap();
        assert_eq!(
            first.next().await,
            Some(AttachEvent::Data(b"hello".to_vec()))
        );

        // 断开只解除附着：PTY 与 shell 继续跑，输出继续进回看缓冲。
        drop(first);
        wait_until(|| !term.info().attached, "解除附着").await;
        assert!(reg.get("s1", &info.id).is_ok(), "断开不该关掉终端");

        pty.write_all(b" world").await.unwrap();
        wait_until(
            || term.lock().scrollback.len() == b"hello world".len(),
            "输出进入回看缓冲",
        )
        .await;

        // 重新附着：第一件事必须是**全量**回放，而且是一整块，不能被后到的字节插队。
        let mut again = reg.attach("s1", &info.id).unwrap();
        assert_eq!(
            again.next().await,
            Some(AttachEvent::Data(b"hello world".to_vec()))
        );
        // 回放之后接着收实时输出。
        pty.write_all(b"!").await.unwrap();
        assert_eq!(again.next().await, Some(AttachEvent::Data(b"!".to_vec())));
    }

    #[tokio::test]
    async fn 新附着顶掉旧附着并给出_replaced() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let term = reg.get("s1", &info.id).unwrap();
        let mut pty = w.pty(term.pid());

        let mut old = reg.attach("s1", &info.id).unwrap();
        let mut new = reg.attach("s1", &info.id).unwrap();

        assert_eq!(
            old.next().await,
            Some(AttachEvent::Closed(CloseReason::Replaced))
        );
        assert_eq!(old.next().await, None, "被顶掉的附着必须彻底结束");

        // 新的那个仍然正常收字节。
        pty.write_all(b"still here").await.unwrap();
        assert_eq!(
            new.next().await,
            Some(AttachEvent::Data(b"still here".to_vec()))
        );

        // 旧附着的 Drop 迟到，也不能把新附着摘掉。
        drop(old);
        assert!(term.info().attached, "迟到的 Drop 摘错了人");
        pty.write_all(b"!").await.unwrap();
        assert_eq!(new.next().await, Some(AttachEvent::Data(b"!".to_vec())));
    }

    #[tokio::test]
    async fn 附着方写入的字节会到达_worker_侧() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let term = reg.get("s1", &info.id).unwrap();
        let mut pty = w.pty(term.pid());

        let att = reg.attach("s1", &info.id).unwrap();
        att.write(b"echo hi\n").await.unwrap();

        let mut buf = [0u8; 32];
        let n = pty.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"echo hi\n");
    }

    #[tokio::test]
    async fn shell_退出时终端被关闭并通知附着方() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let term = reg.get("s1", &info.id).unwrap();
        let pty = w.pty(term.pid());
        let mut att = reg.attach("s1", &info.id).unwrap();

        // shell 退出：socketpair 的另一端关闭，主进程读到 EOF。
        drop(pty);

        assert_eq!(
            att.next().await,
            Some(AttachEvent::Closed(CloseReason::Exited))
        );
        assert_eq!(att.next().await, None);
        wait_until(|| reg.list_for("s1").is_empty(), "终端从表里消失").await;
        // 摘表先于 `term.close` 的往返（列表要立刻反映现实），所以这里得等一下 RPC。
        wait_until(|| !w.closes().is_empty(), "term.close 到达 worker").await;
        assert_eq!(
            w.closes(),
            vec![term.pid()],
            "EOF 也要向 worker 发 term.close"
        );
        assert!(term.is_closed());
    }

    #[tokio::test]
    async fn 关闭后附着的_ws_不会永远挂着() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let mut att = reg.attach("s1", &info.id).unwrap();
        reg.close("s1", &info.id, CloseReason::Deleted)
            .await
            .unwrap();
        assert_eq!(
            att.next().await,
            Some(AttachEvent::Closed(CloseReason::Deleted))
        );
        assert_eq!(att.next().await, None);
    }

    // ------------------------------------------------------------ 空闲与尺寸

    // 空闲超时的配置单位是秒，因此下面几个用例只能用真实时间等一秒出头。
    // 本来该用 `tokio::time::pause`，但那要给 tokio 打开 `test-util` feature，
    // 而本任务不允许改 Cargo.toml——多花一秒钟换不碰别人的文件，值得。
    const IDLE_SECS: u64 = 1;
    /// 比 [`IDLE_SECS`] 多出的余量，抵消调度抖动。
    const IDLE_SLACK: Duration = Duration::from_millis(300);

    #[tokio::test]
    async fn 空闲超时只回收没有附着的终端() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, IDLE_SECS);
        let idle = open_one(&reg, &w, "s1").await;
        let watched = open_one(&reg, &w, "s1").await;
        let idle_pid = reg.get("s1", &idle.id).unwrap().pid();
        let _att = reg.attach("s1", &watched.id).unwrap();

        // 还没到点，一个都不该动。
        assert_eq!(reg.sweep_idle().await, 0);

        tokio::time::sleep(Duration::from_secs(IDLE_SECS) + IDLE_SLACK).await;
        assert_eq!(reg.sweep_idle().await, 1);
        assert_eq!(w.closes(), vec![idle_pid]);
        assert!(reg.get("s1", &idle.id).is_err());
        assert!(
            reg.get("s1", &watched.id).is_ok(),
            "有 WS 挂着的终端不算空闲"
        );
    }

    #[tokio::test]
    async fn 有输出的终端不会被判为空闲() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, IDLE_SECS);
        let info = open_one(&reg, &w, "s1").await;
        let term = reg.get("s1", &info.id).unwrap();
        let mut pty = w.pty(term.pid());

        // 睡过大半个空闲窗口之后来一次输出，计时必须从这次输出重新起算。
        tokio::time::sleep(Duration::from_secs(IDLE_SECS) - Duration::from_millis(400)).await;
        pty.write_all(b"tick").await.unwrap();
        wait_until(|| term.lock().scrollback.len() == 4, "输出到达").await;

        tokio::time::sleep(Duration::from_millis(600)).await;
        // 从创建算已经超过 1 秒了，但从最近一次输出算还没到。
        assert_eq!(reg.sweep_idle().await, 0, "有输出的终端被误判为空闲");
        assert!(w.closes().is_empty());

        tokio::time::sleep(Duration::from_secs(IDLE_SECS) + IDLE_SLACK).await;
        assert_eq!(reg.sweep_idle().await, 1);
        assert_eq!(w.closes(), vec![term.pid()]);
    }

    #[tokio::test]
    async fn 空闲超时为零表示关闭回收() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 0);
        open_one(&reg, &w, "s1").await;
        // 若把 0 当字面值用，「空闲了 0 秒 >= 0 秒」立刻成立，这里会被扫掉。
        assert_eq!(reg.sweep_idle().await, 0);
        assert_eq!(reg.count_for("s1"), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(reg.sweep_idle().await, 0);
        assert!(w.closes().is_empty());
    }

    #[tokio::test]
    async fn resize_先落到_worker_再更新本地记录() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let info = open_one(&reg, &w, "s1").await;
        let pid = reg.get("s1", &info.id).unwrap().pid();

        reg.resize("s1", &info.id, 220, 50).await.unwrap();
        assert_eq!(*w.resizes.lock().unwrap(), vec![(pid, 220, 50)]);
        let after = reg.list_for("s1").remove(0);
        assert_eq!((after.cols, after.rows), (220, 50));

        // 0 是非法尺寸，不该白跑一趟 RPC。
        let err = reg.resize("s1", &info.id, 0, 50).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert_eq!(w.resizes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn 列表只含本会话且带上附着状态() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let a = open_one(&reg, &w, "s1").await;
        let b = open_one(&reg, &w, "s1").await;
        open_one(&reg, &w, "s2").await;

        let list = reg.list_for("s1");
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|i| !i.attached));
        assert!(list.iter().any(|i| i.id == a.id));
        assert!(list.iter().any(|i| i.id == b.id));

        let _att = reg.attach("s1", &a.id).unwrap();
        let list = reg.list_for("s1");
        let a_info = list.iter().find(|i| i.id == a.id).unwrap();
        let b_info = list.iter().find(|i| i.id == b.id).unwrap();
        assert!(a_info.attached);
        assert!(!b_info.attached);
    }

    #[tokio::test]
    async fn 尺寸为零的创建请求不会打扰_worker() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(4, 1800);
        let err = reg
            .open(
                "s1",
                &w.handle,
                TermOpenParams {
                    shell: None,
                    user: None,
                    cols: 0,
                    rows: 24,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(w.ptys.lock().unwrap().is_empty(), "不该开出 PTY");
        // 被拒的请求不能占住名额。
        assert_eq!(reg.count_for("s1"), 0);
        open_one(&reg, &w, "s1").await;
    }

    #[tokio::test]
    async fn 终端_id_是随机且不重复的() {
        let _serial = IN_PROCESS_WORKER.lock().await;
        let w = FakeWorker::start().await;
        let reg = registry(8, 1800);
        let mut ids = std::collections::HashSet::new();
        for _ in 0..8 {
            let info = open_one(&reg, &w, "s1").await;
            assert_eq!(info.id.len(), TERMINAL_ID_BYTES * 2);
            assert!(ids.insert(info.id), "终端 id 重复");
        }
    }
}
