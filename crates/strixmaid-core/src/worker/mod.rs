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

use std::collections::HashMap;
use std::future::Future;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use strixmaid_types::ApiError;
use strixmaid_types::ipc::{FromWorker, METHOD_PING, METHOD_WHOAMI, ToWorker, WhoAmI};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::Mutex;

use crate::session::framing;

/// 一个 RPC 处理器：拿 JSON 参数，返回 JSON 结果或 [`ApiError`]。
pub type Handler =
    Arc<dyn Fn(Value) -> BoxFuture<'static, Result<Value, ApiError>> + Send + Sync + 'static>;

/// 方法名 → 处理器 的注册表。
#[derive(Clone, Default)]
pub struct Dispatcher {
    handlers: HashMap<String, Handler>,
}

impl std::fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&str> = self.handlers.keys().map(String::as_str).collect();
        names.sort_unstable();
        f.debug_struct("Dispatcher")
            .field("methods", &names)
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

    /// 已注册的方法名（排序后），用于探测与调试。
    pub fn methods(&self) -> Vec<String> {
        let mut names: Vec<String> = self.handlers.keys().cloned().collect();
        names.sort_unstable();
        names
    }

    /// 分发一次调用。未知方法 → [`ApiError::not_found`]。
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match self.handlers.get(method) {
            Some(h) => h(params).await,
            None => Err(ApiError::not_found(format!(
                "worker 没有名为 `{method}` 的方法"
            ))),
        }
    }
}

/// 当前进程的身份快照，直接问内核。
pub fn whoami() -> WhoAmI {
    let groups = nix::unistd::getgroups()
        .map(|gs| gs.into_iter().map(|g| g.as_raw()).collect())
        .unwrap_or_default();
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
    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));

    {
        let me = whoami();
        let mut w = writer.lock().await;
        framing::write_msg(
            &mut *w,
            &FromWorker::Hello {
                pid: me.pid,
                uid: me.uid,
                gid: me.gid,
            },
        )
        .await?;
    }
    tracing::info!(
        pid = std::process::id(),
        uid = nix::unistd::getuid().as_raw(),
        "worker 就绪"
    );

    let mut inflight = tokio::task::JoinSet::new();
    loop {
        let msg: Option<ToWorker> = framing::read_msg(&mut reader).await?;
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
                    let reply = match dispatcher.dispatch(&method, params).await {
                        Ok(value) => FromWorker::Result { id, value },
                        Err(error) => FromWorker::Error { id, error },
                    };
                    let mut w = writer.lock().await;
                    if let Err(e) = framing::write_msg(&mut *w, &reply).await {
                        tracing::warn!(error = %e, "写回 RPC 响应失败");
                    }
                });
            }
        }
        // 顺手回收已完成的 task，避免 JoinSet 无限增长。
        while inflight.try_join_next().is_some() {}
    }

    // 让在途调用写完再关。
    while inflight.join_next().await.is_some() {}
    let mut w = writer.lock().await;
    let _ = w.shutdown().await;
    Ok(())
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

/// 占位类型别名：让「写端」类型在文档里有个名字。
pub type SharedWriter = Arc<Mutex<OwnedWriteHalf>>;

#[cfg(test)]
mod tests {
    use super::*;

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
}
