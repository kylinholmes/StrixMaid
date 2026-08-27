//! 主进程侧的 helper 连接与拉起方式（design.md §10）。
//!
//! helper **每会话一个**，由主进程 `spawn`，通过继承的 socketpair（helper 那边是 fd 3）
//! 通信。拉起方式抽象成 [`HelperLauncher`]：生产用 [`ProcessHelperLauncher`] 真的
//! 起进程；测试用假 helper（线程）走同样的帧协议，见 `session/tests.rs`。

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use strixmaid_types::ipc::{FromHelper, IPC_FD, IpcResult, ToHelper};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use super::SessionError;
use super::framing;

/// `CloseSession` 之后等 helper 退出的上限；超过就 SIGKILL。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// 与一个 helper 进程的连接。
///
/// `Drop` 时若子进程仍在，会被 `kill_on_drop` 终止——这是异常路径的兜底；正常路径
/// 请调用 [`HelperConn::close`] 让它 `pam_close_session` 后自行退出。
pub struct HelperConn {
    stream: UnixStream,
    child: Option<Child>,
}

impl std::fmt::Debug for HelperConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HelperConn")
            .field("pid", &self.pid())
            .finish()
    }
}

impl HelperConn {
    /// 用一条已连接的流（与可选的子进程句柄）构造。
    pub fn new(stream: UnixStream, child: Option<Child>) -> Self {
        HelperConn { stream, child }
    }

    /// helper 进程 pid；假 helper 没有。
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// 发一条消息。
    pub async fn send(&mut self, msg: &ToHelper) -> IpcResult<()> {
        framing::write_msg(&mut self.stream, msg).await
    }

    /// 收一条消息；helper 退出（关闭通道）时返回 `Ok(None)`。
    pub async fn recv(&mut self) -> IpcResult<Option<FromHelper>> {
        framing::read_msg(&mut self.stream).await
    }

    /// 收 `WorkerSpawned` 之后紧跟的 `SCM_RIGHTS` 帧。
    pub async fn recv_fd(&mut self) -> IpcResult<OwnedFd> {
        framing::recv_fd(&self.stream).await
    }

    /// 优雅关闭：`CloseSession` → 等 `SessionClosed` / EOF → 等进程退出，超时则杀。
    pub async fn close(mut self) {
        let _ = self.send(&ToHelper::CloseSession).await;
        let drain = async {
            loop {
                match self.recv().await {
                    Ok(Some(FromHelper::SessionClosed)) | Ok(None) | Err(_) => break,
                    Ok(Some(_)) => continue,
                }
            }
        };
        if tokio::time::timeout(CLOSE_TIMEOUT, drain).await.is_err() {
            tracing::warn!(pid = ?self.pid(), "helper 未在限时内确认 CloseSession");
        }
        if let Some(mut child) = self.child.take() {
            match tokio::time::timeout(CLOSE_TIMEOUT, child.wait()).await {
                Ok(Ok(status)) => tracing::debug!(?status, "helper 已退出"),
                Ok(Err(e)) => tracing::warn!(error = %e, "等待 helper 退出失败"),
                Err(_) => {
                    tracing::warn!(pid = ?child.id(), "helper 未在限时内退出，强制终止");
                    let _ = child.kill().await;
                }
            }
        }
    }
}

/// 拉起一个新 helper 并返回连接。
pub trait HelperLauncher: Send + Sync + 'static {
    /// 拉起。失败应返回 [`SessionError::HelperUnavailable`]。
    fn launch(&self) -> BoxFuture<'_, Result<HelperConn, SessionError>>;
}

/// 生产实现：`spawn` `strixmaid-helper`，socketpair 一端 `dup2` 到子进程的 fd 3。
#[derive(Debug, Clone)]
pub struct ProcessHelperLauncher {
    path: PathBuf,
}

impl ProcessHelperLauncher {
    /// `path` 不含 `/` 时按 `PATH` 查找（与 `Config::helper_path` 语义一致）。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ProcessHelperLauncher { path: path.into() }
    }

    /// helper 二进制路径。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl HelperLauncher for ProcessHelperLauncher {
    fn launch(&self) -> BoxFuture<'_, Result<HelperConn, SessionError>> {
        Box::pin(async move {
            // 两端都 CLOEXEC：主进程之后 spawn 的任何子进程都不会意外继承它们；
            // 子进程里 dup2 到 fd 3 的那份自然不带 CLOEXEC。
            let (ours, theirs) = socketpair(
                AddressFamily::Unix,
                SockType::Stream,
                None,
                SockFlag::SOCK_CLOEXEC,
            )
            .map_err(|e| SessionError::HelperUnavailable(format!("socketpair 失败: {e}")))?;

            let theirs_raw = theirs.as_raw_fd();
            let mut cmd = Command::new(&self.path);
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                // stderr 直接进 journald，helper 只记事件不记内容。
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            // SAFETY: 闭包只调用 dup2 / fcntl，都是 async-signal-safe 的。
            unsafe {
                cmd.pre_exec(move || {
                    if theirs_raw == IPC_FD {
                        // 已经就是 3：只需清掉 CLOEXEC。
                        if libc::fcntl(IPC_FD, libc::F_SETFD, 0) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                    } else if libc::dup2(theirs_raw, IPC_FD) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = cmd.spawn().map_err(|e| {
                SessionError::HelperUnavailable(format!("无法启动 {}: {e}", self.path.display()))
            })?;
            // 子进程已经拿到自己的副本。
            drop(theirs);

            let std_stream = std::os::unix::net::UnixStream::from(ours);
            std_stream
                .set_nonblocking(true)
                .map_err(|e| SessionError::HelperUnavailable(format!("设置非阻塞失败: {e}")))?;
            let stream = UnixStream::from_std(std_stream)
                .map_err(|e| SessionError::HelperUnavailable(format!("注册到 tokio 失败: {e}")))?;
            tracing::debug!(pid = child.id(), path = %self.path.display(), "helper 已启动");
            Ok(HelperConn::new(stream, Some(child)))
        })
    }
}
