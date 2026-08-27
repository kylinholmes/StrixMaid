//! 主进程侧的 worker 句柄：在 helper 经 `SCM_RIGHTS` 传回的 socketpair 上做 RPC。
//!
//! 调用可以并发在途——写端用 `Mutex` 串行化，一个后台 task 读回应并按 `id` 唤醒
//! 对应的 `oneshot`。worker 断开时所有在途调用都会收到 [`ErrorCode::Unavailable`]。

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
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use super::SessionError;
use super::framing;

/// 等 worker 第一帧 `Hello` 的上限。exec 一个静态二进制并起 tokio 用不了 1 秒，
/// 这里放宽到 15 秒兜底负载很高的机器。
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// `Shutdown` 之后等 worker 自行退出的时间，超过则 SIGTERM。
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
/// SIGTERM 之后再等这么久，还不退就 SIGKILL。
const TERM_GRACE: Duration = Duration::from_secs(2);

type Pending = HashMap<u64, oneshot::Sender<Result<Value, ApiError>>>;

struct Inner {
    writer: Mutex<OwnedWriteHalf>,
    pending: StdMutex<Pending>,
    next_id: AtomicU64,
    closed: AtomicBool,
    reader: StdMutex<Option<JoinHandle<()>>>,
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
        let (mut reader, writer) = stream.into_split();

        let hello = tokio::time::timeout(
            HELLO_TIMEOUT,
            framing::read_msg::<_, FromWorker>(&mut reader),
        )
        .await
        .map_err(|_| SessionError::Worker("等待 worker Hello 超时".into()))??;
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
            writer: Mutex::new(writer),
            pending: StdMutex::new(HashMap::new()),
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
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, ApiError> {
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
        {
            let mut w = self.inner.writer.lock().await;
            if let Err(e) = framing::write_msg(&mut *w, &msg).await {
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
        }

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(ApiError::new(ErrorCode::Unavailable, "worker 在应答前断开")),
        }
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
            let mut w = self.inner.writer.lock().await;
            let _ = framing::write_msg(&mut *w, &ToWorker::Shutdown).await;
            let _ = tokio::io::AsyncWriteExt::shutdown(&mut *w).await;
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
async fn read_loop(mut reader: tokio::net::unix::OwnedReadHalf, inner: Arc<Inner>) {
    loop {
        match framing::read_msg::<_, FromWorker>(&mut reader).await {
            Ok(Some(FromWorker::Result { id, value })) => deliver(&inner, id, Ok(value)),
            Ok(Some(FromWorker::Error { id, error })) => deliver(&inner, id, Err(error)),
            Ok(Some(FromWorker::Hello { .. })) => {
                tracing::debug!("worker 重复发送 Hello，忽略");
            }
            Ok(None) => {
                tracing::debug!("worker 关闭了连接");
                break;
            }
            Err(e) => {
                tracing::warn!(error = %e, "读取 worker 响应失败");
                break;
            }
        }
    }
    inner.closed.store(true, Ordering::Release);
    fail_all(&inner);
}

fn deliver(inner: &Inner, id: u64, result: Result<Value, ApiError>) {
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
