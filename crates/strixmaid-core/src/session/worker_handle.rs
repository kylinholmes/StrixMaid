//! 主进程侧的 worker 句柄：在 helper 经 `SCM_RIGHTS` 传回的 socketpair 上做 RPC。
//!
//! 调用可以并发在途——写端用 `Mutex` 串行化，一个后台 task 读回应并按 `id` 唤醒
//! 对应的 `oneshot`。worker 断开时所有在途调用都会收到 [`ErrorCode::Unavailable`]。
//!
//! # 订阅（`roadmap/01-worker-execution.md` §4.4）
//!
//! [`WorkerHandle::subscribe`] 在同一条 socket 上开一条长流：主进程发一帧
//! `Subscribe`，worker 起一个任务把流转成 `Event` 帧送回，读循环按 `id` 投进
//! 一个容量 [`SUBSCRIPTION_QUEUE_CAP`] 的 `mpsc`。调用方拿到的就是这个
//! `Receiver`，用完直接 drop——**退订是自动的**，见下。
//!
//! ## 生命周期：谁负责退订
//!
//! 返回的 `Receiver` 一旦 drop，worker 里那个 `journalctl -f` 子进程就该跟着死。
//! 靠调用方记得调一个 `unsubscribe()` 是不可靠的：WS 连接断开、任务被 abort、
//! panic 展开，任何一条路径漏掉都会在 worker 里留下一个永远跑着的子进程。
//!
//! 因此每个订阅配一个后台守望任务，它拿着 [`SubscriptionGuard`]，只做一件事：
//! 等 `Sender::closed()`（即接收端被 drop）。一旦等到，guard 的 `Drop` 把订阅从
//! 表里摘掉并发出 `Unsubscribe`。守望任务本身被 abort 或运行时关停时，guard 同样
//! 会 drop，清理照做——把清理绑在 `Drop` 上而不是绑在某条代码路径上，就是为了
//! 覆盖这些非正常路径。
//!
//! 反方向（worker 先结束流）由 `End` 帧触发：读循环把订阅从表里摘掉，守望任务
//! 随之醒来并**解除武装**——流已经结束了，再发 `Unsubscribe` 只是噪音。
//!
//! ## 背压
//!
//! `mpsc` 容量 [`SUBSCRIPTION_QUEUE_CAP`]。消费者跟不上时队列填满，读循环停在
//! `send().await` 上、不再从 socket 取帧，socket 缓冲随之填满，worker 那一侧
//! 该订阅的写就被顶住——全链路没有一处无界缓冲，慢消费者只会让流变慢，不会
//! 让内存涨。
//!
//! 代价是读循环被占住期间，这个 worker 上其它 RPC 的响应也读不出来。所以等待
//! 有上限 [`SUBSCRIBER_STALL_LIMIT`]：超过这个时间还塞不进去，就判定该订阅者
//! 已经死了，退订并关掉它的流，让整条连接恢复。宁可丢一个订阅，也不能让一个
//! 不读数据的客户端把整个会话拖住。

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use serde_json::Value;
use strixmaid_types::ipc::{FromWorker, METHOD_PING, METHOD_WHOAMI, ToWorker, WhoAmI};
use strixmaid_types::{ApiError, ErrorCode};
use tokio::net::UnixStream;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;

use super::SessionError;
use super::framing::{self, FdFrameReader};

/// 等 worker 第一帧 `Hello` 的上限。exec 一个静态二进制并起 tokio 用不了 1 秒，
/// 这里放宽到 15 秒兜底负载很高的机器。
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// `Shutdown` 之后等 worker 自行退出的时间，超过则 SIGTERM。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// SIGTERM 之后再等这么久，还不退就 SIGKILL。
const TERM_GRACE: Duration = Duration::from_secs(2);

/// 每个订阅的队列深度。够吸收一次 GC 停顿或一轮慢渲染，又不至于在内存里
/// 攒下几秒钟的日志——`logs.follow` 的批次已经在 provider 里合并过了。
pub const SUBSCRIPTION_QUEUE_CAP: usize = 64;

/// 订阅队列写满后最多顶多久。超过即判定订阅者已死（见模块文档「背压」）。
const SUBSCRIBER_STALL_LIMIT: Duration = Duration::from_secs(10);

/// 在途调用的应答通道。
///
/// 载荷带上 `Vec<OwnedFd>`：`term.open` 的结果**必须**连同 PTY 的 fd 一起交给
/// 调用方，而 fd 只在读循环那一刻存在。若在这里把它丢掉，之后无论如何都补不回来。
type Pending = HashMap<u64, oneshot::Sender<Result<(Value, Vec<OwnedFd>), ApiError>>>;

/// 一个活着的订阅在主进程侧的登记。
struct Sub {
    /// 投递 `Event` 用的发送端。
    tx: mpsc::Sender<Value>,
    /// 只为它的 `Drop` 而存在：本项从表里被移除时，守望任务的
    /// `oneshot::Receiver` 立刻醒来，从而知道「流是 worker 结束的」。
    _ended: oneshot::Sender<()>,
}

struct Inner {
    /// 整条连接。读端要用 `recvmsg` 收 fd，因而不能 `into_split`——
    /// 见 [`FdFrameReader`]：普通 `read()` 读过带 fd 的字节会让内核**静默丢掉** fd。
    /// tokio 的 `UnixStream` 允许 `&self` 并发读写，共享一个 `Arc` 即可。
    stream: Arc<UnixStream>,
    /// 写端互斥。一帧要原子地写完：两个并发写交错到一起就是流上的字节乱码，
    /// 而这种损坏在读端表现为莫名其妙的解析错误，极难追。
    write_lock: Mutex<()>,
    pending: StdMutex<Pending>,
    subs: StdMutex<HashMap<u64, Sub>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    reader: StdMutex<Option<JoinHandle<()>>>,
}

impl Inner {
    /// 串行地写一帧给 worker。主进程方向从不发 fd。
    async fn send(&self, msg: &ToWorker) -> strixmaid_types::ipc::IpcResult<()> {
        let _guard = self.write_lock.lock().await;
        framing::write_msg_with_fds(&self.stream, msg, &[]).await
    }
}

/// 一个已连上的 worker。`Clone` 代价 = `Arc` 自增。
#[derive(Clone)]
pub struct WorkerHandle {
    /// helper `fork` 出来的 pid（`WorkerSpawned.pid`）。这是终止 worker 时用的权威值；
    /// `<= 1` 表示未知（测试用的进程内 worker），此时绝不发信号。
    pid: i32,
    /// worker 实际运行的 uid（来自 `Hello`，即内核眼中的 `getuid()`）。
    uid: u32,
    inner: Arc<Inner>,
}

impl std::fmt::Debug for WorkerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerHandle")
            .field("pid", &self.pid)
            .field("uid", &self.uid)
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl WorkerHandle {
    /// 接管 fd、等 `Hello`、起读循环。
    ///
    /// `expected_uid` 为 `Some` 时 `Hello.uid` 必须等于它——不等说明 helper 声称的身份切换
    /// 没有发生，这种 worker 绝不能用。admin worker 传 `None`（由 root helper 直接 fork）。
    pub async fn connect(
        fd: OwnedFd,
        pid: i32,
        expected_uid: Option<u32>,
    ) -> Result<Self, SessionError> {
        let std_stream = std::os::unix::net::UnixStream::from(fd);
        std_stream
            .set_nonblocking(true)
            .map_err(|e| SessionError::Worker(format!("worker socket 设置非阻塞失败: {e}")))?;
        let stream = UnixStream::from_std(std_stream)
            .map_err(|e| SessionError::Worker(format!("worker socket 注册到 tokio 失败: {e}")))?;
        let stream = Arc::new(stream);
        let mut reader = FdFrameReader::new(stream.clone());

        let hello = tokio::time::timeout(HELLO_TIMEOUT, reader.read())
            .await
            .map_err(|_| SessionError::Worker("等待 worker Hello 超时".into()))??;
        // Hello 不该带 fd；带了就是协议错，`fds` 在这里 drop 掉不会泄漏。
        let hello: Option<FromWorker> = match hello {
            Some((payload, _fds)) => Some(strixmaid_types::ipc::decode(&payload)?),
            None => None,
        };
        let uid = match hello {
            Some(FromWorker::Hello {
                pid: hello_pid,
                uid,
                ..
            }) => {
                if let Some(expected) = expected_uid
                    && uid != expected
                {
                    return Err(SessionError::Worker(format!(
                        "worker 报告的 uid={uid} 与 helper 声明的 uid={expected} 不一致，拒绝使用"
                    )));
                }
                tracing::debug!(pid, hello_pid, uid, "worker 已连接");
                uid
            }
            Some(other) => {
                return Err(SessionError::Worker(format!(
                    "worker 第一帧应为 Hello，实际 {other:?}"
                )));
            }
            None => {
                return Err(SessionError::Worker(
                    "worker 在发出 Hello 之前就退出了".into(),
                ));
            }
        };

        let inner = Arc::new(Inner {
            stream,
            write_lock: Mutex::new(()),
            pending: StdMutex::new(HashMap::new()),
            subs: StdMutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            reader: StdMutex::new(None),
        });

        let reader_task = tokio::spawn(read_loop(reader, inner.clone()));
        *inner.reader.lock().unwrap_or_else(|e| e.into_inner()) = Some(reader_task);

        Ok(WorkerHandle { pid, uid, inner })
    }

    /// helper 报告的 worker pid。
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// worker 运行的 uid。
    pub fn uid(&self) -> u32 {
        self.uid
    }

    /// 连接是否还在。
    pub fn is_alive(&self) -> bool {
        !self.inner.closed.load(Ordering::Acquire)
    }

    /// 发起一次 RPC。
    ///
    /// 应答若附带 fd，这里会把它关掉并告警——需要 fd 的调用请用
    /// [`call_with_fds`](Self::call_with_fds)。宁可吵，也不要让 fd 无声无息地漏掉。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let (value, fds) = self.call_with_fds(method, params).await?;
        if !fds.is_empty() {
            tracing::warn!(
                method,
                count = fds.len(),
                "worker 的应答附带了 fd，但调用方没有接收，已关闭"
            );
        }
        Ok(value)
    }

    /// 发起一次 RPC 并取走应答附带的 fd（`term.open` 用）。
    pub async fn call_with_fds(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Value, Vec<OwnedFd>), ApiError> {
        if !self.is_alive() {
            return Err(ApiError::new(ErrorCode::Unavailable, "worker 已退出"));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);

        let msg = ToWorker::Call {
            id,
            method: method.to_string(),
            params,
        };
        if let Err(e) = self.inner.send(&msg).await {
            self.inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(
                ApiError::new(ErrorCode::Unavailable, "无法向 worker 发送请求")
                    .with_detail(e.to_string()),
            );
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ApiError::new(ErrorCode::Unavailable, "worker 在应答前断开")),
        }
    }

    /// 建立一条订阅流（`roadmap/01-worker-execution.md` §4.4）。
    ///
    /// 返回的 `Receiver` 在流结束或 worker 断开时给出 `None`。**drop 它即退订**，
    /// 无需（也没有）显式的 `unsubscribe`——生命周期管理见模块文档。
    ///
    /// 返回 `Ok` 只表示 `Subscribe` 帧发出去了，不表示 worker 那边把流建起来了：
    /// 频道不存在、provider 不可用这类失败会以一帧 `End { error }` 回来，表现为
    /// 「流立刻结束」，原因记在日志里。要在这里同步拿到失败原因就得给协议加一个
    /// 订阅确认帧，为一个几乎只在配置错误时才发生的分支不值得。
    pub async fn subscribe(
        &self,
        channel: &str,
        params: Value,
    ) -> Result<mpsc::Receiver<Value>, ApiError> {
        if !self.is_alive() {
            return Err(ApiError::new(ErrorCode::Unavailable, "worker 已退出"));
        }
        // 与 `call` 共用序号空间：一条连接上一个 id 只对应一件事。
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(SUBSCRIPTION_QUEUE_CAP);
        let (ended_tx, ended_rx) = oneshot::channel();
        self.inner
            .subs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                Sub {
                    tx: tx.clone(),
                    _ended: ended_tx,
                },
            );

        let msg = ToWorker::Subscribe {
            id,
            channel: channel.to_string(),
            params,
        };
        if let Err(e) = self.inner.send(&msg).await {
            // 还没起守望任务，这里自己收拾干净即可。
            self.inner
                .subs
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(
                ApiError::new(ErrorCode::Unavailable, "无法向 worker 发起订阅")
                    .with_detail(e.to_string()),
            );
        }

        tokio::spawn(watch_subscription(
            SubscriptionGuard {
                inner: self.inner.clone(),
                id,
                armed: true,
            },
            tx,
            ended_rx,
        ));
        Ok(rx)
    }

    /// `ping`。
    pub async fn ping(&self) -> Result<(), ApiError> {
        self.call(METHOD_PING, Value::Null).await.map(|_| ())
    }

    /// `whoami`。
    pub async fn whoami(&self) -> Result<WhoAmI, ApiError> {
        let v = self.call(METHOD_WHOAMI, Value::Null).await?;
        serde_json::from_value(v)
            .map_err(|e| ApiError::internal(format!("whoami 响应格式错误: {e}")))
    }

    /// 请 worker 退出：先 `Shutdown`，超时后 SIGTERM，再超时 SIGKILL。
    pub async fn shutdown(&self) {
        {
            let _ = self.inner.send(&ToWorker::Shutdown).await;
            // 半关写方向让 worker 的读端看到 EOF。用 nix 而不是 `AsyncWriteExt::shutdown`
            // ——后者要 `&mut`，而这里只有共享的 `Arc<UnixStream>`。
            use std::os::fd::AsRawFd;
            let _ = nix::sys::socket::shutdown(
                self.inner.stream.as_raw_fd(),
                nix::sys::socket::Shutdown::Write,
            );
        }
        if self.wait_closed(SHUTDOWN_GRACE).await {
            return;
        }
        if self.signal(Signal::SIGTERM) && self.wait_closed(TERM_GRACE).await {
            return;
        }
        self.signal(Signal::SIGKILL);
        self.inner.closed.store(true, Ordering::Release);
        self.fail_pending();
    }

    /// 等读循环结束（即 worker 关闭了连接）。
    async fn wait_closed(&self, limit: Duration) -> bool {
        let task = self
            .inner
            .reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        match task {
            Some(task) => tokio::time::timeout(limit, task).await.is_ok(),
            None => self.inner.closed.load(Ordering::Acquire),
        }
    }

    /// 给 worker 发信号。pid 未知（`<= 1`）时什么都不做——给 0 / -1 / 1 发信号是灾难。
    fn signal(&self, sig: Signal) -> bool {
        if self.pid <= 1 {
            return false;
        }
        match kill(Pid::from_raw(self.pid), sig) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!(pid = self.pid, ?sig, error = %e, "向 worker 发信号失败");
                false
            }
        }
    }

    fn fail_pending(&self) {
        fail_all(&self.inner);
    }
}

/// 读循环：把回应按 `id` 派发给等待者；连接断开时唤醒所有在途调用。
async fn read_loop(mut reader: FdFrameReader, inner: Arc<Inner>) {
    loop {
        // 先取出 fd 再解码：解码失败时 `fds` 随作用域 drop，不会泄漏。
        let framed = match reader.read().await {
            Ok(Some((payload, fds))) => match strixmaid_types::ipc::decode::<FromWorker>(&payload) {
                Ok(msg) => Some((msg, fds)),
                Err(e) => {
                    tracing::warn!(error = %e, "解析 worker 响应失败");
                    break;
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::warn!(error = %e, "读取 worker 响应失败");
                break;
            }
        };
        match framed {
            Some((FromWorker::Result { id, value }, fds)) => {
                deliver(&inner, id, Ok((value, fds)));
            }
            Some((FromWorker::Error { id, error }, _)) => deliver(&inner, id, Err(error)),
            Some((FromWorker::Event { id, data }, _)) => deliver_event(&inner, id, data).await,
            Some((FromWorker::End { id, error }, _)) => {
                // 把登记摘掉：`tx` 与 `_ended` 随之 drop，接收端看到流结束，
                // 守望任务醒来并解除武装（不再回 `Unsubscribe`）。
                let known = take_sub(&inner, id).is_some();
                match error {
                    // 错误只能记日志：`subscribe` 交出去的是 `Receiver<Value>`，
                    // 没有承载错误的位置。频道名与参数都在这条日志的上下文里。
                    Some(e) => tracing::warn!(
                        id,
                        known,
                        code = ?e.code,
                        error = %e.message,
                        detail = ?e.detail,
                        "worker 的订阅以错误结束"
                    ),
                    None => tracing::debug!(id, known, "worker 的订阅正常结束"),
                }
            }
            Some((FromWorker::Hello { .. }, _)) => {
                tracing::debug!("worker 重复发送 Hello，忽略");
            }
            None => {
                tracing::debug!("worker 关闭了连接");
                break;
            }
        }
    }
    inner.closed.store(true, Ordering::Release);
    fail_all(&inner);
    // 清空订阅表让每个接收端看到流结束——worker 没了，等下去没有意义。
    inner
        .subs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// 把一帧 `Event` 投给订阅者。
///
/// 快路径是 `try_send`，队列有位置时一次同步调用就完事、读循环不让出。
/// 只有队列满了才真的等——那正是背压该起作用的时刻（见模块文档）。
async fn deliver_event(inner: &Arc<Inner>, id: u64, data: Value) {
    let tx = inner
        .subs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .map(|sub| sub.tx.clone());
    let Some(tx) = tx else {
        // 常见且无害：`Unsubscribe` 与在途的 `Event` 撞车。
        tracing::trace!(id, "收到已退订订阅的事件，丢弃");
        return;
    };

    let pending = match tx.try_send(data) {
        Ok(()) => return,
        Err(TrySendError::Closed(_)) => {
            // 接收端刚 drop，守望任务马上会发 `Unsubscribe`，这里不重复动作。
            tracing::debug!(id, "订阅的接收端已关闭，丢弃事件");
            return;
        }
        Err(TrySendError::Full(v)) => v,
    };

    match tokio::time::timeout(SUBSCRIBER_STALL_LIMIT, tx.send(pending)).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => tracing::debug!(id, "等待期间订阅的接收端关闭，丢弃事件"),
        Err(_) => {
            // 订阅者十秒没取走一帧，判定它死了。摘掉登记并退订，否则整条连接
            // 上的其它 RPC 都要陪着它一起卡住。
            tracing::warn!(
                id,
                stall_secs = SUBSCRIBER_STALL_LIMIT.as_secs(),
                "订阅者长时间不消费，强制退订"
            );
            take_sub(inner, id);
            send_unsubscribe(inner, id).await;
        }
    }
}

/// 从订阅表里摘掉一项。
fn take_sub(inner: &Inner, id: u64) -> Option<Sub> {
    inner
        .subs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
}

/// 尽力发一帧 `Unsubscribe`。worker 已经走了就什么都不做——它连着的资源
/// 会随进程一起消失。
async fn send_unsubscribe(inner: &Inner, id: u64) {
    if inner.closed.load(Ordering::Acquire) {
        return;
    }
    if let Err(e) = inner.send(&ToWorker::Unsubscribe { id }).await {
        tracing::debug!(id, error = %e, "发送 Unsubscribe 失败");
    }
}

/// 订阅的退订守卫：`Drop` 时摘掉登记并（在仍处于武装状态时）发出 `Unsubscribe`。
///
/// 之所以把退订放在 `Drop` 而不是某个显式方法里：持有它的守望任务可能被 abort、
/// 可能随运行时一起停，只有 `Drop` 在这些路径上都会执行。
struct SubscriptionGuard {
    inner: Arc<Inner>,
    id: u64,
    /// 为 `false` 时只清理、不发 `Unsubscribe`——流已经由 worker 结束了。
    armed: bool,
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        let id = self.id;
        take_sub(&self.inner, id);
        if !self.armed {
            return;
        }
        let inner = Arc::clone(&self.inner);
        // `Drop` 不能 await，把最后一帧交给一个短任务。运行时已经在关停时
        // 拿不到 handle，那种情况下 worker 也活不过主进程，无需退订。
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { send_unsubscribe(&inner, id).await });
            }
            Err(_) => tracing::debug!(id, "运行时已停止，跳过 Unsubscribe"),
        }
    }
}

/// 守望一个订阅：等到「接收端 drop」或「worker 结束了流」，然后让 guard 收尾。
async fn watch_subscription(
    mut guard: SubscriptionGuard,
    tx: mpsc::Sender<Value>,
    ended: oneshot::Receiver<()>,
) {
    tokio::select! {
        // 接收端被 drop（或 close）：这才是需要向 worker 退订的情形。
        _ = tx.closed() => {}
        // 登记被摘掉（`End` 帧或连接断开）：流已经没了，解除武装。
        _ = ended => guard.armed = false,
    }
    // 这里 drop `tx`：它是最后一个发送端，接收端因此看到流结束。
}

fn deliver(inner: &Inner, id: u64, result: Result<(Value, Vec<OwnedFd>), ApiError>) {
    let tx = inner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id);
    match tx {
        Some(tx) => {
            let _ = tx.send(result);
        }
        None => tracing::debug!(id, "收到没有等待者的 worker 响应"),
    }
}

fn fail_all(inner: &Inner) {
    let drained: Vec<_> = inner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain()
        .collect();
    for (_, tx) in drained {
        let _ = tx.send(Err(ApiError::new(ErrorCode::Unavailable, "worker 已断开")));
    }
}
