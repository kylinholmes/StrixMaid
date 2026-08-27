//! `strixmaid worker` 模式：以登录用户身份运行的 RPC 服务端（design.md §2.2 / §10）。
//!
//! worker 由 helper 在 `setuid` 之后 `exec` 拉起，fd 3 是与主进程直连的 socketpair
//! （helper 把另一端经 `SCM_RIGHTS` 交给了主进程）。它不读配置、不碰数据库——
//! 所有需要「以用户身份」执行的事都通过这里的 RPC 进来。
//!
//! # 可注册的分发
//!
//! [`Dispatcher`] 按方法名分发到 [`Handler`]，provider 后续以 `register` 挂进来，
//! 不需要改协议枚举。Phase 1 只内置 [`METHOD_PING`] 与 [`METHOD_WHOAMI`]——后者返回
//! 内核眼中的 uid / gid / groups / cwd，用来端到端证明身份切换确实发生了。
//!
//! # 并发模型
//!
//! 一个读循环 + 每个调用一个 task；写端用 `Mutex` 串行化。调用可以并发在途，
//! 回应顺序不保证，主进程按 `id` 配对。
//!
//! # 流式订阅（`roadmap/01-worker-execution.md` §4.4）
//!
//! `logs.follow` 这类长流的可见范围必须随用户，因此 follow 的子进程要在 worker
//! 内运行，请求—响应式的 RPC 不够用。[`Dispatcher::register_stream`] 注册一个
//! **流工厂**：拿订阅参数，返回一个 `Stream<Item = Value>`。
//!
//! 每个订阅一个独立 task（[`run_subscription`]），职责边界是刻意划的：
//!
//! - 读循环只负责起 / 停任务，永远不在流上等待——否则一个安静的 follow 就能
//!   让整个 worker 收不到下一条 `Call`；
//! - 一个订阅被慢消费者堵住时（见下文背压），阻塞的只是它自己那个 task。
//!   其它订阅、其它在途调用各有各的 task，照常推进。
//!
//! ## 背压
//!
//! 主进程侧每个订阅一个容量 64 的 `mpsc`（`session::worker_handle`）。消费者
//! 跟不上时队列填满，主进程停止从 socket 读这一路，socket 缓冲随之填满，worker
//! 这边的 `write_msg().await` 就卡在那里——背压一路顶回到流的源头，中间没有任何
//! 无界缓冲。这正是要的效果：宁可让 follow 慢下来，也不要在 worker 里堆积日志。
//!
//! 代价是这一瞬间写端 `Mutex` 被持有，同一条 socket 上其它帧要排队。这是单条
//! socket 的物理限制，不是实现选择；能做到的是**不让 CPU 侧的工作互相阻塞**，
//! 这一点靠「每个订阅 / 每个调用一个 task」保证。
//!
//! ## 取消
//!
//! `Unsubscribe` 不 `abort` 任务——`abort` 可能落在 `write_msg` 写了一半的位置，
//! 留下半截帧就把整条连接的协议毁了。改用一次性的取消信号，任务只在
//! 「等下一项」这个安全点上响应它，写帧永远是原子的。

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::{BoxStream, Stream, StreamExt};
use serde_json::Value;
use strixmaid_types::ApiError;
use strixmaid_types::ipc::{FromWorker, METHOD_PING, METHOD_WHOAMI, ToWorker, WhoAmI};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::session::framing::{self, FdFrameReader};

pub mod probe;
pub mod providers;
pub mod spawn_as;
pub mod terminal;

/// 一个 RPC 处理器：拿 JSON 参数，返回 JSON 结果或 [`ApiError`]。
pub type Handler =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value, ApiError>> + Send + Sync + 'static>;

/// 一个会交出 fd 的 RPC 处理器。
///
/// 与 [`Handler`] 分开是因为「顺带交出内核资源」是**质**的不同：普通处理器只
/// 产生 JSON，而这类处理器产生的 fd 必须原子地随同一帧发出（见
/// [`crate::session::framing::FdFrameReader`]），漏掉一个就是一个泄漏的 PTY。
/// 让它在类型上就显形，比在文档里叮嘱可靠。
pub type FdHandler = Arc<
    dyn Fn(Value) -> BoxFuture<'static, Result<(Value, Vec<OwnedFd>), ApiError>>
        + Send
        + Sync
        + 'static,
>;

/// 一个订阅流：每一项是要发给主进程的一帧 `data`。
///
/// 流里**没有错误项**：能出错的只有「把流建起来」这一步（provider 不可用、
/// 参数不合法、polkit 拒绝……），那些错误由 [`StreamFactory`] 的 `Result` 表达，
/// 变成一帧 `End { error: Some(..) }`。流一旦建起来，结束就只是结束。
pub type EventStream = BoxStream<'static, Value>;

/// 一个订阅流的工厂：拿订阅参数，异步地把流建起来。
///
/// 建流是 async 的（`LogProvider::follow` 要起子进程），因此工厂返回 future
/// 而不是直接返回流。
pub type StreamFactory = Arc<
    dyn Fn(Value) -> BoxFuture<'static, Result<EventStream, ApiError>> + Send + Sync + 'static,
>;

/// 方法名 → 处理器、频道名 → 流工厂 的注册表。
///
/// 两张表**互相独立**：一个名字可以只是方法、只是频道，或两者都是。分开是因为
/// 二者的语义不同（一次应答 vs. 一条流），混在一起只会让调用方需要靠约定去猜。
#[derive(Clone, Default)]
pub struct Dispatcher {
    handlers: HashMap<String, Handler>,
    fd_handlers: HashMap<String, FdHandler>,
    streams: HashMap<String, StreamFactory>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatcher")
            .field("methods", &self.methods())
            .field("channels", &self.stream_channels())
            .finish()
    }
}

impl Dispatcher {
    /// 带内置方法（`ping` / `whoami`）的分发器。
    pub fn new() -> Self {
        let mut d = Dispatcher::default();
        d.register_fn(METHOD_PING, |_params| async {
            Ok(Value::String("pong".into()))
        });
        d.register_fn(METHOD_WHOAMI, |_params| async {
            serde_json::to_value(whoami()).map_err(|e| ApiError::internal(e.to_string()))
        });
        d
    }

    /// 注册一个处理器；同名覆盖。
    pub fn register(&mut self, method: impl Into<String>, handler: Handler) -> &mut Self {
        self.handlers.insert(method.into(), handler);
        self
    }

    /// 用 `async fn` / 闭包注册处理器的便捷形式。
    pub fn register_fn<F, Fut>(&mut self, method: impl Into<String>, f: F) -> &mut Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, ApiError>> + Send + 'static,
    {
        self.register(method, Arc::new(move |params| Box::pin(f(params))))
    }

    /// 注册一个会交出 fd 的处理器；同名覆盖。
    pub fn register_fd(&mut self, method: impl Into<String>, handler: FdHandler) -> &mut Self {
        self.fd_handlers.insert(method.into(), handler);
        self
    }

    /// 已注册的方法名（排序后），用于探测与调试。
    pub fn methods(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .handlers
            .keys()
            .chain(self.fd_handlers.keys())
            .cloned()
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// 分发一次调用。未知方法 → [`ApiError::not_found`]。
    ///
    /// 交出 fd 的方法**也能**从这里调到，但 fd 会被就地关掉；`serve` 走的是
    /// [`dispatch_with_fds`](Self::dispatch_with_fds)。
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let (value, fds) = self.dispatch_with_fds(method, params).await?;
        if !fds.is_empty() {
            tracing::warn!(method, count = fds.len(), "调用方不接收 fd，已关闭");
        }
        Ok(value)
    }

    /// 分发一次调用，连同处理器交出的 fd。
    ///
    /// 两张表都查：先普通处理器，再 fd 处理器。同名不会出现——注册时就该二选一。
    pub async fn dispatch_with_fds(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Value, Vec<OwnedFd>), ApiError> {
        if let Some(h) = self.handlers.get(method) {
            return h(params).await.map(|v| (v, Vec::new()));
        }
        match self.fd_handlers.get(method) {
            Some(h) => h(params).await,
            None => Err(ApiError::not_found(format!(
                "worker 没有名为 `{method}` 的方法"
            ))),
        }
    }

    /// 注册一个订阅频道；同名覆盖。
    pub fn register_stream_boxed(
        &mut self,
        channel: impl Into<String>,
        factory: StreamFactory,
    ) -> &mut Self {
        self.streams.insert(channel.into(), factory);
        self
    }

    /// 用 `async fn` / 闭包注册订阅频道的便捷形式。
    ///
    /// 闭包返回 `Result<impl Stream<Item = Value>, ApiError>`：建流失败即订阅失败，
    /// 主进程会收到一帧 `End { error: Some(..) }`。
    pub fn register_stream<F, Fut, S>(&mut self, channel: impl Into<String>, f: F) -> &mut Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<S, ApiError>> + Send + 'static,
        S: Stream<Item = Value> + Send + 'static,
    {
        self.register_stream_boxed(
            channel,
            Arc::new(move |params| {
                let fut = f(params);
                Box::pin(async move { fut.await.map(StreamExt::boxed) })
            }),
        )
    }

    /// 已注册的订阅频道名（排序后）。
    pub fn stream_channels(&self) -> Vec<String> {
        let mut names: Vec<String> = self.streams.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    /// 建立一次订阅。未知频道 → [`ApiError::not_found`]。
    pub async fn open_stream(
        &self,
        channel: &str,
        params: Value,
    ) -> Result<EventStream, ApiError> {
        match self.streams.get(channel) {
            Some(f) => f(params).await,
            None => Err(ApiError::not_found(format!(
                "worker 没有名为 `{channel}` 的订阅频道"
            ))),
        }
    }
}

/// 当前进程的补充组列表。
///
/// `nix` 在 Apple 目标上把 `getgroups` 编译掉了（macOS 的 `getgroups(2)` 有个
/// 历史包袱：它返回的可能是登录时的**静态**组列表而非当前真实的组集合，
/// `nix` 不愿意暴露这个语义不一致的接口）。这里直接调 libc：
/// 对本项目的用途——把 `whoami` 报给主进程做展示与 `elevate_groups` 判断——
/// 静态列表也是可用的，何况 macOS 只是开发平台。
///
/// 调用两次：第一次问个数，第二次取内容。个数在两次之间变大时返回空表而不是
/// 截断的结果，避免报出一个「看起来完整、其实少了几项」的组列表。
#[cfg(target_os = "macos")]
fn current_groups() -> Vec<u32> {
    // SAFETY: gidsetsize 为 0 时 getgroups 不写 grouplist，只返回组数。
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0 as libc::gid_t; n as usize];
    // SAFETY: buf 有 n 个 gid_t 的容量，gidsetsize 如实描述其长度。
    let got = unsafe { libc::getgroups(n, buf.as_mut_ptr()) };
    if got < 0 || got > n {
        return Vec::new();
    }
    buf.truncate(got as usize);
    buf
}

/// 当前进程的补充组列表。
#[cfg(not(target_os = "macos"))]
fn current_groups() -> Vec<u32> {
    nix::unistd::getgroups()
        .map(|gs| gs.into_iter().map(|g| g.as_raw()).collect())
        .unwrap_or_default()
}

/// 当前进程的身份快照，直接问内核。
pub fn whoami() -> WhoAmI {
    let groups = current_groups();
    WhoAmI {
        pid: std::process::id() as i32,
        uid: nix::unistd::getuid().as_raw(),
        euid: nix::unistd::geteuid().as_raw(),
        gid: nix::unistd::getgid().as_raw(),
        egid: nix::unistd::getegid().as_raw(),
        groups,
        cwd: std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        user: std::env::var("USER").ok(),
        home: std::env::var("HOME").ok(),
    }
}

/// 在 `stream` 上服务直到主进程发 `Shutdown` 或关闭连接。
///
/// 第一帧是 [`FromWorker::Hello`]，让主进程确认身份切换已生效。
pub async fn serve(stream: UnixStream, dispatcher: Arc<Dispatcher>) -> anyhow::Result<()> {
    // 读写共享同一个 `Arc<UnixStream>`：应答可能要附着 PTY 的 fd，而
    // `sendmsg` 需要裸 fd，`OwnedWriteHalf` 给不了。写侧用 `Mutex<()>` 串行化，
    // 保证一帧原子写完。
    let stream = Arc::new(stream);
    let mut reader = FdFrameReader::new(stream.clone());
    let writer = Arc::new(FrameWriter {
        stream: stream.clone(),
        lock: Mutex::new(()),
    });

    {
        let me = whoami();
        writer
            .send(
                &FromWorker::Hello {
                    pid: me.pid,
                    uid: me.uid,
                    gid: me.gid,
                },
                &[],
            )
            .await?;
    }
    tracing::info!(
        pid = std::process::id(),
        uid = nix::unistd::getuid().as_raw(),
        "worker 就绪"
    );

    let mut inflight = tokio::task::JoinSet::new();
    // 订阅是长命的，不能和一次性调用共用 JoinSet：退出时要「取消」而不是「等它跑完」。
    let mut subs: HashMap<u64, Subscription> = HashMap::new();
    loop {
        let msg: Option<ToWorker> = match reader.read().await? {
            Some((payload, fds)) => {
                if !fds.is_empty() {
                    // 主进程从不向 worker 发 fd。真收到说明协议用错了，
                    // 记一笔并关掉，别让它悄悄留在进程里。
                    tracing::warn!(count = fds.len(), "主进程发来了 fd，已关闭");
                }
                Some(strixmaid_types::ipc::decode(&payload)?)
            }
            None => None,
        };
        match msg {
            None => {
                tracing::info!("主进程关闭了通道，worker 退出");
                break;
            }
            Some(ToWorker::Shutdown) => {
                tracing::info!("收到 Shutdown，worker 退出");
                break;
            }
            Some(ToWorker::Call { id, method, params }) => {
                let dispatcher = dispatcher.clone();
                let writer = writer.clone();
                inflight.spawn(async move {
                    let (reply, fds) = match dispatcher.dispatch_with_fds(&method, params).await {
                        Ok((value, fds)) => (FromWorker::Result { id, value }, fds),
                        Err(error) => (FromWorker::Error { id, error }, Vec::new()),
                    };
                    let raw: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
                    if let Err(e) = writer.send(&reply, &raw).await {
                        tracing::warn!(error = %e, "写回 RPC 响应失败");
                    }
                    // fds 在这里 drop：主进程已经拿到了自己的副本。
                });
            }
            Some(ToWorker::Subscribe {
                id,
                channel,
                params,
            }) => {
                // 同 id 重复订阅只可能是主进程的 bug；换掉旧的并留下痕迹，
                // 比静默丢弃或让两个任务抢同一个 id 都好排查。
                if subs.remove(&id).is_some() {
                    tracing::warn!(id, channel, "订阅 id 被复用，已取消旧订阅");
                }
                let (cancel, cancelled) = oneshot::channel();
                let task = tokio::spawn(run_subscription(
                    dispatcher.clone(),
                    writer.clone(),
                    id,
                    channel,
                    params,
                    cancelled,
                ));
                subs.insert(id, Subscription { cancel, task });
            }
            Some(ToWorker::Unsubscribe { id }) => {
                // 丢掉 cancel 端即通知任务收手；任务在「等下一项」的安全点上退出，
                // 不会把一帧写到一半。未知 id 是无操作——主进程可能刚好与 `End` 撞车。
                match subs.remove(&id) {
                    Some(_) => tracing::debug!(id, "订阅已取消"),
                    None => tracing::debug!(id, "收到未知订阅的 Unsubscribe，忽略"),
                }
            }
        }
        // 顺手回收已完成的 task，避免无限增长。
        while inflight.try_join_next().is_some() {}
        subs.retain(|_, sub| !sub.task.is_finished());
    }

    // 让在途调用写完再关。
    while inflight.join_next().await.is_some() {}
    // 订阅则相反：先取消，再等它们走到安全点。对端此时要么已关闭（写立刻报错），
    // 要么还在读（写得出去），两种情况都不会把这里挂住。
    let mut tasks = Vec::with_capacity(subs.len());
    for (_, Subscription { cancel, task }) in subs.drain() {
        drop(cancel);
        tasks.push(task);
    }
    for task in tasks {
        let _ = task.await;
    }
    writer.shutdown_write();
    Ok(())
}

/// 一个在跑的订阅。`cancel` 一旦被 drop，任务就会在下一个安全点退出。
struct Subscription {
    cancel: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

/// 一个订阅任务的全部生命：建流 → 逐项发 `Event` → 发 `End`。
///
/// 三种退出方式：
///
/// | 情形 | 发出的帧 |
/// |---|---|
/// | 建流失败 | `End { error: Some(..) }` |
/// | 流自然走完 | `End { error: None }` |
/// | 被 `Unsubscribe` 取消 | 什么都不发 |
///
/// 取消时不发 `End`：主进程发出 `Unsubscribe` 的那一刻就已经把订阅从等待表里
/// 摘掉了，再回一帧只会变成一条「无人认领的响应」日志。
async fn run_subscription(
    dispatcher: Arc<Dispatcher>,
    writer: SharedWriter,
    id: u64,
    channel: String,
    params: Value,
    mut cancelled: oneshot::Receiver<()>,
) {
    let end = match dispatcher.open_stream(&channel, params).await {
        Err(error) => {
            tracing::debug!(id, channel, error = %error.message, "订阅建立失败");
            Some(error)
        }
        Ok(mut stream) => {
            loop {
                // `biased` 让取消优先于数据：已经退订了就别再多发一帧。
                let item = tokio::select! {
                    biased;
                    _ = &mut cancelled => {
                        tracing::debug!(id, channel, "订阅被取消");
                        return;
                    }
                    item = stream.next() => item,
                };
                let Some(data) = item else { break };
                // 写帧本身不放进 select：写到一半被取消会在 socket 里留下半截帧，
                // 那是整条连接的灾难。这里最多多发一帧，代价可以忽略。
                if let Err(e) = writer.send(&FromWorker::Event { id, data }, &[]).await {
                    // 写不出去 = 连接没了，`End` 同样发不出去，直接收摊。
                    tracing::debug!(id, channel, error = %e, "推送订阅事件失败，结束订阅");
                    return;
                }
            }
            None
        }
    };

    if let Err(e) = writer.send(&FromWorker::End { id, error: end }, &[]).await {
        tracing::debug!(id, channel, error = %e, "写回订阅结束帧失败");
    }
}

/// `strixmaid worker --ipc-fd N` 的入口：接管 fd，跑 [`serve`]。
pub async fn run_from_fd(fd: RawFd, dispatcher: Arc<Dispatcher>) -> anyhow::Result<()> {
    // SAFETY: fd 由 helper dup2 到位、本进程独占；exec 之后没有其它持有者。
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let std_stream = std::os::unix::net::UnixStream::from(owned);
    std_stream
        .set_nonblocking(true)
        .map_err(|e| anyhow::anyhow!("fd {fd} 设置非阻塞失败: {e}"))?;
    let stream = UnixStream::from_std(std_stream)
        .map_err(|e| anyhow::anyhow!("fd {fd} 不是可用的 Unix socket: {e}"))?;
    serve(stream, dispatcher).await
}

/// worker 侧的写端：串行化的、能附着 fd 的帧写入器。
///
/// 多个在途调用与多条订阅共用一条 socket，写必须串行——一帧写到一半被另一帧
/// 插进来，读端看到的就是乱码。`Mutex<()>` 只守写这一个动作，不影响读。
pub struct FrameWriter {
    stream: Arc<UnixStream>,
    lock: Mutex<()>,
}

impl FrameWriter {
    /// 写一帧，可附带 fd。
    pub async fn send<T: serde::Serialize + ?Sized>(
        &self,
        msg: &T,
        fds: &[RawFd],
    ) -> strixmaid_types::ipc::IpcResult<()> {
        let _guard = self.lock.lock().await;
        framing::write_msg_with_fds(&self.stream, msg, fds).await
    }

    /// 半关写方向，让主进程的读端看到 EOF。
    pub fn shutdown_write(&self) {
        let _ = nix::sys::socket::shutdown(
            self.stream.as_raw_fd(),
            nix::sys::socket::Shutdown::Write,
        );
    }
}

/// 「写端」在各处传递时的共享形式。
pub type SharedWriter = Arc<FrameWriter>;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use futures::stream;
    use serde_json::json;

    use super::*;
    use crate::session::worker_handle::WorkerHandle;

    /// drop 时把标志置起来，用来观察「订阅任务真的结束了」——任务结束会 drop 流，
    /// 流 drop 会 drop 它的状态，也就是这个哨兵。
    struct DropFlag(Arc<AtomicBool>);

    impl Drop for DropFlag {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// 每 10 ms 产一帧序号的流；`dropped` 在流被丢弃时置位。
    fn ticker(dropped: Arc<AtomicBool>) -> impl Stream<Item = Value> + Send + 'static {
        stream::unfold((0u64, DropFlag(dropped)), |(n, flag)| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            Some((json!(n), (n + 1, flag)))
        })
    }

    /// 起一个进程内 worker，返回主进程侧的读写两半。
    fn spawn_worker(
        d: Dispatcher,
    ) -> (
        tokio::net::unix::OwnedReadHalf,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let (main_side, worker_side) = UnixStream::pair().unwrap();
        tokio::spawn(serve(worker_side, Arc::new(d)));
        main_side.into_split()
    }

    /// 读一帧，超时即失败。
    async fn next_frame(r: &mut tokio::net::unix::OwnedReadHalf) -> FromWorker {
        tokio::time::timeout(Duration::from_secs(3), framing::read_msg::<_, FromWorker>(r))
            .await
            .expect("3 秒内应收到一帧")
            .unwrap()
            .expect("连接不应关闭")
    }

    #[tokio::test]
    async fn 内置方法与未知方法() {
        let d = Dispatcher::new();
        assert_eq!(
            d.methods(),
            vec![METHOD_PING.to_string(), METHOD_WHOAMI.to_string()]
        );
        assert_eq!(
            d.dispatch(METHOD_PING, Value::Null).await.unwrap(),
            Value::String("pong".into())
        );
        let who: WhoAmI =
            serde_json::from_value(d.dispatch(METHOD_WHOAMI, Value::Null).await.unwrap()).unwrap();
        assert_eq!(who.uid, nix::unistd::getuid().as_raw());
        let err = d.dispatch("nope", Value::Null).await.unwrap_err();
        assert_eq!(err.code, strixmaid_types::ErrorCode::NotFound);
    }

    #[tokio::test]
    async fn 注册的处理器能拿到参数() {
        let mut d = Dispatcher::new();
        d.register_fn("echo", |p| async move { Ok(p) });
        let v = d
            .dispatch("echo", serde_json::json!({"a": 1}))
            .await
            .unwrap();
        assert_eq!(v, serde_json::json!({"a": 1}));
    }

    #[tokio::test]
    async fn serve_先发_hello_再应答调用并响应_shutdown() {
        let (main_side, worker_side) = UnixStream::pair().unwrap();
        let server = tokio::spawn(serve(worker_side, Arc::new(Dispatcher::new())));

        let (mut r, mut w) = main_side.into_split();
        let hello: FromWorker = framing::read_msg(&mut r).await.unwrap().unwrap();
        assert!(matches!(hello, FromWorker::Hello { .. }));

        framing::write_msg(
            &mut w,
            &ToWorker::Call {
                id: 9,
                method: METHOD_PING.into(),
                params: Value::Null,
            },
        )
        .await
        .unwrap();
        let reply: FromWorker = framing::read_msg(&mut r).await.unwrap().unwrap();
        assert_eq!(
            reply,
            FromWorker::Result {
                id: 9,
                value: Value::String("pong".into())
            }
        );

        framing::write_msg(&mut w, &ToWorker::Shutdown)
            .await
            .unwrap();
        server.await.unwrap().unwrap();
        // 对端关闭
        assert!(
            framing::read_msg::<_, FromWorker>(&mut r)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn 订阅推送事件_退订后任务退出且不影响其它调用() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut d = Dispatcher::new();
        let flag = dropped.clone();
        d.register_stream("tick", move |_| {
            let flag = flag.clone();
            async move { Ok(ticker(flag)) }
        });
        assert_eq!(d.stream_channels(), vec!["tick".to_string()]);

        let (mut r, mut w) = spawn_worker(d);
        assert!(matches!(next_frame(&mut r).await, FromWorker::Hello { .. }));

        framing::write_msg(
            &mut w,
            &ToWorker::Subscribe {
                id: 1,
                channel: "tick".into(),
                params: Value::Null,
            },
        )
        .await
        .unwrap();

        for expected in 0..3u64 {
            match next_frame(&mut r).await {
                FromWorker::Event { id, data } => {
                    assert_eq!(id, 1);
                    assert_eq!(data, json!(expected));
                }
                other => panic!("应为 Event，实际 {other:?}"),
            }
        }

        framing::write_msg(&mut w, &ToWorker::Unsubscribe { id: 1 })
            .await
            .unwrap();

        // 退订后立刻发一次普通 RPC：它必须照常应答，证明订阅任务与调用互不牵连。
        // 同时它也是个同步点——收到它时，之前在途的帧都已经读完了。
        framing::write_msg(
            &mut w,
            &ToWorker::Call {
                id: 2,
                method: METHOD_PING.into(),
                params: Value::Null,
            },
        )
        .await
        .unwrap();

        // 退订与在途的 Event 可能擦肩而过，允许再收到若干帧，但必须等到 ping 的应答。
        loop {
            match next_frame(&mut r).await {
                FromWorker::Result { id: 2, value } => {
                    assert_eq!(value, Value::String("pong".into()));
                    break;
                }
                FromWorker::Event { id: 1, .. } => continue,
                other => panic!("退订后不该再有 {other:?}"),
            }
        }

        // 此后彻底安静：不再有 Event，也不该有 End（退订不发 End）。
        let quiet = tokio::time::timeout(
            Duration::from_millis(200),
            framing::read_msg::<_, FromWorker>(&mut r),
        )
        .await;
        assert!(quiet.is_err(), "退订后仍在推送: {quiet:?}");
        assert!(
            dropped.load(Ordering::SeqCst),
            "订阅任务没有退出，流没有被 drop"
        );
    }

    #[tokio::test]
    async fn 流自然结束发_end_建流失败发带错误的_end() {
        let mut d = Dispatcher::new();
        d.register_stream("two", |_| async {
            Ok(stream::iter([json!("a"), json!("b")]))
        });
        d.register_stream("boom", |_| async {
            Err::<stream::Empty<Value>, _>(ApiError::capability_unavailable("journal", "没有日志后端"))
        });

        let (mut r, mut w) = spawn_worker(d);
        assert!(matches!(next_frame(&mut r).await, FromWorker::Hello { .. }));

        framing::write_msg(
            &mut w,
            &ToWorker::Subscribe {
                id: 5,
                channel: "two".into(),
                params: Value::Null,
            },
        )
        .await
        .unwrap();
        assert!(matches!(next_frame(&mut r).await, FromWorker::Event { id: 5, .. }));
        assert!(matches!(next_frame(&mut r).await, FromWorker::Event { id: 5, .. }));
        assert_eq!(
            next_frame(&mut r).await,
            FromWorker::End { id: 5, error: None }
        );

        for (id, channel) in [(6u64, "boom"), (7, "没这个频道")] {
            framing::write_msg(
                &mut w,
                &ToWorker::Subscribe {
                    id,
                    channel: channel.into(),
                    params: Value::Null,
                },
            )
            .await
            .unwrap();
            match next_frame(&mut r).await {
                FromWorker::End {
                    id: got,
                    error: Some(e),
                } => {
                    assert_eq!(got, id);
                    assert!(
                        matches!(
                            e.code,
                            strixmaid_types::ErrorCode::CapabilityUnavailable
                                | strixmaid_types::ErrorCode::NotFound
                        ),
                        "错误码不对: {:?}",
                        e.code
                    );
                }
                other => panic!("应为带错误的 End，实际 {other:?}"),
            }
        }
    }

    /// `WorkerHandle::subscribe` 对进程内 worker 的往返：收帧、drop 接收端即退订。
    #[tokio::test]
    async fn worker_handle_订阅往返且接收端_drop_后自动退订() {
        let dropped = Arc::new(AtomicBool::new(false));
        let mut d = Dispatcher::new();
        let flag = dropped.clone();
        d.register_stream("tick", move |params| {
            let flag = flag.clone();
            // 参数原样可见，证明 Subscribe 的 params 确实到了工厂手里。
            assert_eq!(params, json!({ "from": 0 }));
            async move { Ok(ticker(flag)) }
        });

        let (main_side, worker_side) = std::os::unix::net::UnixStream::pair().unwrap();
        worker_side.set_nonblocking(true).unwrap();
        let worker = tokio::spawn(serve(
            UnixStream::from_std(worker_side).unwrap(),
            Arc::new(d),
        ));
        // pid 传 0：这是进程内 worker，绝不能对它发信号。
        let handle = WorkerHandle::connect(OwnedFd::from(main_side), 0, None)
            .await
            .unwrap();

        let mut rx = handle
            .subscribe("tick", json!({ "from": 0 }))
            .await
            .unwrap();
        for expected in 0..3u64 {
            let v = tokio::time::timeout(Duration::from_secs(3), rx.recv())
                .await
                .expect("3 秒内应收到一帧")
                .expect("流不应结束");
            assert_eq!(v, json!(expected));
        }

        // 普通 RPC 在订阅进行中照常工作。
        assert_eq!(handle.ping().await, Ok(()));

        // drop 接收端 = 退订。worker 侧的流随之被 drop。
        drop(rx);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !dropped.load(Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "worker 没有收到退订");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // 退订之后连接依旧可用。
        assert_eq!(handle.ping().await, Ok(()));
        handle.shutdown().await;
        let _ = worker.await;
    }

    #[tokio::test]
    async fn worker_handle_订阅未知频道时流立刻结束() {
        let (main_side, worker_side) = std::os::unix::net::UnixStream::pair().unwrap();
        worker_side.set_nonblocking(true).unwrap();
        tokio::spawn(serve(
            UnixStream::from_std(worker_side).unwrap(),
            Arc::new(Dispatcher::new()),
        ));
        let handle = WorkerHandle::connect(OwnedFd::from(main_side), 0, None)
            .await
            .unwrap();

        // `subscribe` 本身成功（帧发出去了），失败以「流立刻结束」的形式出现。
        let mut rx = handle.subscribe("没这个频道", Value::Null).await.unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("3 秒内应结束");
        assert_eq!(ended, None);
        handle.shutdown().await;
    }
}
