//! 主进程侧的 helper 连接与拉起方式（design.md §10）。
//!
//! helper **每会话一个**，由主进程 `spawn`。拉起方式抽象成 [`HelperLauncher`]：
//! 生产用 [`ProcessHelperLauncher`] 真的起进程；测试用假 helper（线程）走同样的
//! 帧协议，见 `session/tests.rs`。
//!
//! # 两个平台怎么把通道递给 helper
//!
//! | | Unix | Windows |
//! |---|---|---|
//! | 通道 | `socketpair`，一端 `dup2` 到 fd 3 | 主进程建一条随机名的命名管道，helper 连上来 |
//! | 身份证明 | 父子关系——fd 是继承来的，别的进程拿不到 | `GetNamedPipeClientProcessId` 必须等于刚 spawn 出来的那个 pid |
//!
//! 两者给出的是同一条保证：**这条通道的另一头确实是我刚拉起的那个 helper**。
//! Unix 靠「fd 只能继承」，Windows 靠「名字随机 + 核对 pid」。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use futures::future::BoxFuture;
use strixmaid_types::ipc::{FromHelper, IpcResult, ToHelper};
use tokio::process::{Child, Command};

use super::SessionError;
use super::channel::IpcChannel;
use super::framing;

#[cfg(unix)]
use std::os::fd::OwnedFd;

/// `CloseSession` 之后等 helper 退出的上限；超过就 SIGKILL。
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// 与一个 helper 进程的连接。
///
/// `Drop` 时若子进程仍在，会被 `kill_on_drop` 终止——这是异常路径的兜底；正常路径
/// 请调用 [`HelperConn::close`] 让它 `pam_close_session` 后自行退出。
pub struct HelperConn {
    stream: IpcChannel,
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
    pub fn new(stream: IpcChannel, child: Option<Child>) -> Self {
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

    /// 收 `WorkerSpawned` 之后紧跟的 `SCM_RIGHTS` 帧（Unix 专属）。
    ///
    /// Windows 上没有这一步：worker 通道的句柄**写在 `WorkerSpawned` 帧里**
    /// （`worker_handle` 字段），由 `channel::PeerProcess::take` 从 helper 进程里取，
    /// 见 [`crate::session::framing::windows`] 的模块文档。
    #[cfg(unix)]
    pub async fn recv_fd(&mut self) -> IpcResult<OwnedFd> {
        framing::recv_fd(&self.stream).await
    }

    /// helper 进程里的一个句柄值 → 本进程的通道（Windows 专属）。
    ///
    /// 对应 Unix 的 [`HelperConn::recv_fd`]：都是「把 helper 手里那半条 worker 通道
    /// 搬到主进程来」，只是搬运机制不同。
    #[cfg(windows)]
    pub fn take_worker_channel(&self, raw: u64) -> Result<IpcChannel, SessionError> {
        let handle = self.stream.peer().take(raw).map_err(|e| {
            SessionError::Protocol(format!("无法从 helper 取回 worker 通道句柄: {e}"))
        })?;
        // SAFETY: 这个句柄是 helper 用 `CreateNamedPipeW(FILE_FLAG_OVERLAPPED)` 建的
        // 服务端实例，且已在 helper 侧 `ConnectNamedPipe` 完成；刚由
        // DuplicateHandle 搬进本进程，尚未注册到任何完成端口。
        unsafe { IpcChannel::from_server_handle(handle, super::channel::PeerProcess::unknown()) }
            .map_err(|e| SessionError::Worker(format!("worker 通道注册到 tokio 失败: {e}")))
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

/// 生产实现：`spawn` `strixmaid-helper`，按平台把通道递过去（见模块文档的表）。
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
        Box::pin(async move { self.launch_impl().await })
    }
}

impl ProcessHelperLauncher {
    /// 公共的命令构造：三个平台一致。
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.path);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            // stderr 直接进 journald / 事件日志，helper 只记事件不记内容。
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        cmd
    }

    /// Unix：socketpair + `dup2` 到 fd 3。
    #[cfg(unix)]
    async fn launch_impl(&self) -> Result<HelperConn, SessionError> {
        use std::io;
        use std::os::fd::AsRawFd as _;

        use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
        use strixmaid_types::ipc::IPC_FD;

        // 两端都 CLOEXEC：主进程之后 spawn 的任何子进程都不会意外继承它们；
        // 子进程里 dup2 到 fd 3 的那份自然不带 CLOEXEC。
        //
        // Linux 用 SOCK_CLOEXEC 一步到位；macOS 没有这个标志，
        // 只能创建后补 fcntl，代价与固有的竞态窗口见 `framing::set_cloexec`。
        #[cfg(target_os = "linux")]
        let sock_flags = SockFlag::SOCK_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let sock_flags = SockFlag::empty();

        let (ours, theirs) = socketpair(AddressFamily::Unix, SockType::Stream, None, sock_flags)
            .map_err(|e| SessionError::HelperUnavailable(format!("socketpair 失败: {e}")))?;

        #[cfg(not(target_os = "linux"))]
        for fd in [ours.as_raw_fd(), theirs.as_raw_fd()] {
            framing::set_cloexec(fd)
                .map_err(|e| SessionError::HelperUnavailable(format!("设置 CLOEXEC 失败: {e}")))?;
        }

        let theirs_raw = theirs.as_raw_fd();
        let mut cmd = self.base_command();
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

        let stream = IpcChannel::from_owned_fd(ours)
            .map_err(|e| SessionError::HelperUnavailable(format!("注册到 tokio 失败: {e}")))?;
        tracing::debug!(pid = child.id(), path = %self.path.display(), "helper 已启动");
        Ok(HelperConn::new(stream, Some(child)))
    }

    /// Windows：主进程建命名管道、helper 连回来、核对 pid。
    ///
    /// # 为什么不用「继承句柄」那条路
    ///
    /// 那更贴近 Unix 的 `dup2`，但 Rust 的 `std::process::Command` 在 Windows 上
    /// 是 `bInheritHandles = TRUE` + 继承**全部**可继承句柄——同一时刻别的线程
    /// 在 spawn 子进程时，我们的通道会漏给它。要精确控制就得自己
    /// `CreateProcessW` 加 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`，
    /// 那等于把 `Command` 重写一遍。
    ///
    /// 命名管道这条路没有这个问题：名字是 128 位随机的，且连上来的客户端
    /// **必须**是我们刚 spawn 的那个 pid（见 `IpcChannel::accept_from`）。
    #[cfg(windows)]
    async fn launch_impl(&self) -> Result<HelperConn, SessionError> {
        /// 等 helper 连上来的上限。它启动后第一件事就是连管道，正常在毫秒级；
        /// 这个上限只兜「helper 起来了但卡住了」的异常路径。
        const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

        let name = super::channel::random_pipe_name("helper");
        // 不给 SDDL：helper 与主进程同身份，默认安全性（创建者 + 系统）正合适。
        let mut server = IpcChannel::server(&name, None)
            .map_err(|e| SessionError::HelperUnavailable(format!("建立命名管道失败: {e}")))?;

        let mut cmd = self.base_command();
        cmd.arg("--pipe").arg(&name);
        let child = cmd.spawn().map_err(|e| {
            SessionError::HelperUnavailable(format!("无法启动 {}: {e}", self.path.display()))
        })?;
        let pid = child.id().ok_or_else(|| {
            SessionError::HelperUnavailable("helper 刚启动就没有 pid 了".to_owned())
        })?;

        match tokio::time::timeout(CONNECT_TIMEOUT, server.accept_from(pid)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                return Err(SessionError::HelperUnavailable(format!(
                    "helper 连接命名管道失败: {e}"
                )));
            }
            Err(_) => {
                return Err(SessionError::HelperUnavailable(format!(
                    "helper（pid {pid}）在 {} 秒内没有连上 IPC 管道",
                    CONNECT_TIMEOUT.as_secs()
                )));
            }
        }

        tracing::debug!(pid, path = %self.path.display(), "helper 已启动");
        Ok(HelperConn::new(server, Some(child)))
    }
}
