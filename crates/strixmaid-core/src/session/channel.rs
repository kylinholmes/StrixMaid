//! 进程间的双向字节通道，以及「随帧附带的内核对象」在两个平台上的形态。
//!
//! ```text
//! 主进程 ──IpcChannel──▶ strixmaid-helper ──IpcChannel──▶ worker
//!                                                └─ 另一端交回主进程（附件）
//! ```
//!
//! # 两个平台的实体
//!
//! | | Unix | Windows |
//! |---|---|---|
//! | 通道 | `socketpair(AF_UNIX, SOCK_STREAM)` 的一端 | 一条命名管道实例 |
//! | 附件 | fd，走 `SCM_RIGHTS` 带外传递 | `HANDLE`，走 `DuplicateHandle` |
//! | 身份证明 | 父子关系（fd 是继承来的） | `GetNamedPipeClientProcessId` 对上子进程 pid |
//!
//! # 附件为什么是「读端去拉」而不是「写端推过来」
//!
//! Unix 的 `SCM_RIGHTS` 是写端推：发送方把 fd 塞进控制消息，内核在接收方
//! 建一个新 fd。Windows 没有这种带外通道，只有 `DuplicateHandle`——而它需要
//! **对另一个进程的 `PROCESS_DUP_HANDLE` 权限**。
//!
//! 谁有这个权限？看一眼本项目的进程拓扑就清楚了：
//!
//! ```text
//! 主进程（服务身份：LocalSystem / 管理员）
//!   └─ helper（同身份，主进程的子进程）
//!        └─ worker（登录用户身份，权限更低）
//! ```
//!
//! **附件永远是朝主进程流动的**（helper 把 worker 通道交给主进程、worker 把
//! 终端通道交给主进程），而主进程恰好是权限最高的那个。所以在 Windows 上
//! 一律由**读端**（主进程）`OpenProcess(PROCESS_DUP_HANDLE)` 打开写端进程、
//! 把句柄拉过来，并用 `DUPLICATE_CLOSE_SOURCE` 顺手关掉源端那一份。
//!
//! 反过来做（让 worker 往主进程里推句柄）需要给 worker 开
//! `PROCESS_DUP_HANDLE` 到一个 SYSTEM 进程上——那等于把提权漏洞写进设计。
//!
//! 这条不对称性是 Windows 侧唯一与 Unix 语义不同的地方，因此
//! [`PeerProcess`] 在 Unix 上是个零大小的空壳，在 Windows 上才真的持有句柄。
//!
//! # 线程模型
//!
//! 两个平台的实现都允许 `&self` 并发读写（tokio 的 `UnixStream` 与
//! `NamedPipeServer` / `NamedPipeClient` 的就绪 API 都取 `&self`），
//! 因此读写两侧共享一个 `Arc<IpcChannel>` 即可，不必 split——这对
//! [`super::framing::FdFrameReader`] 是必需的：它要用 `recvmsg` 收附件，
//! 而那需要裸 fd，`OwnedReadHalf` 给不了。

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

// ===========================================================================
// 附件的两种形态
// ===========================================================================

/// 随帧附带的内核对象：Unix 上是 fd，Windows 上是 `HANDLE`。
///
/// 目前只有终端用得到（`term.open` 要把 PTY 那条通道交给主进程）与
/// helper 交回 worker 通道这两处。
#[cfg(unix)]
pub type Attachment = std::os::fd::OwnedFd;
/// 见上。
#[cfg(windows)]
pub type Attachment = std::os::windows::io::OwnedHandle;

/// 附件在线上的表示。
///
/// Unix 上 fd 号不上线（走 `SCM_RIGHTS` 带外），这个别名只用于
/// `write_msg_with_fds` 的入参；Windows 上它**真的会被写进帧的尾部**，
/// 由读端拿去 `DuplicateHandle`。
///
/// 统一成 `u64` 而不是各用各的（`RawFd` 是 `i32`、`RawHandle` 是裸指针）：
/// 它要跨进程传，必须有确定的线格式；而 `RawHandle` 连 `Send` 都不是。
pub type RawAttachment = u64;

/// 把附件随帧发出之后，处置本进程手里那一份。
///
/// # 两个平台在这里是相反的
///
/// Unix：`SCM_RIGHTS` 在 `sendmsg` 那一刻就把 fd 装进了接收方，发送方那一份是
/// 独立的副本，**必须 drop**，否则 PTY 的 master 端永远不关，shell 退出时读端
/// 也等不到 EOF。
///
/// Windows：接收方用 `DUPLICATE_CLOSE_SOURCE` 拉走句柄，**源句柄已经被内核关掉了**。
/// 此时再 `CloseHandle` 一次不只是徒劳：句柄值可能已经被本进程新开的对象复用，
/// 那一下会关掉一个完全无关的内核对象（而且在调试器下会直接触发
/// `STATUS_INVALID_HANDLE`）。所以这里 `forget`，把关闭的责任整个交给接收方。
///
/// 接收方一直没来拉的情况下会漏一个句柄。那只发生在对端进程已经消失时
/// （见 [`super::framing::windows`] 的模块文档），而那条路径上本进程紧接着也会
/// 因为通道断开而退出，句柄随进程一起回收。
pub fn release_sent(sent: Vec<Attachment>) {
    #[cfg(unix)]
    {
        drop(sent);
    }
    #[cfg(windows)]
    for a in sent {
        std::mem::forget(a);
    }
}

/// 取一个附件的线上表示。
pub fn raw_of(a: &Attachment) -> RawAttachment {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd as _;
        a.as_raw_fd() as u64
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle as _;
        a.as_raw_handle() as usize as u64
    }
}

// ===========================================================================
// 对端进程
// ===========================================================================

/// 通道对端的进程身份。
///
/// Unix 上什么都不需要（`SCM_RIGHTS` 由内核完成搬运），这是一个零大小类型；
/// Windows 上它持有一个带 `PROCESS_DUP_HANDLE` 的进程句柄，
/// 读端靠它把附件拉过来。见模块文档。
#[cfg(unix)]
#[derive(Debug, Default, Clone, Copy)]
pub struct PeerProcess;

#[cfg(unix)]
impl PeerProcess {
    /// Unix 上不需要对端进程信息。
    pub fn unknown() -> Self {
        PeerProcess
    }

    /// Unix 上按 pid 打开对端是无操作。
    pub fn open(_pid: u32) -> Self {
        PeerProcess
    }
}

/// 见上。
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct PeerProcess {
    /// `None` 表示「不知道对端是谁」——那种通道上收到附件即为协议错误。
    handle: Option<std::sync::Arc<crate::platform::windows::handle::Owned>>,
}

#[cfg(windows)]
impl PeerProcess {
    /// 未知对端。收到附件时会报协议错误。
    pub fn unknown() -> Self {
        PeerProcess { handle: None }
    }

    /// 按 pid 打开对端进程（只申请 `PROCESS_DUP_HANDLE`）。
    ///
    /// 打不开不是致命错误——只有真的收到附件时才用得上它，
    /// 那时会报一条说得清原因的协议错误。
    pub fn open(pid: u32) -> Self {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_DUP_HANDLE};
        // SAFETY: OpenProcess 只按 pid 查表，失败返回空句柄。
        let h = unsafe { OpenProcess(PROCESS_DUP_HANDLE, 0, pid) };
        // SAFETY: h 刚由 OpenProcess 返回；Owned::new 会挡掉空句柄。
        let owned = unsafe { crate::platform::windows::handle::Owned::new(h) }.ok();
        if owned.is_none() {
            tracing::debug!(pid, "无法打开对端进程用于附件传递");
        }
        PeerProcess {
            handle: owned.map(std::sync::Arc::new),
        }
    }

    /// 把对端进程里的一个句柄拉到本进程，并关掉源端那一份。
    ///
    /// `DUPLICATE_CLOSE_SOURCE` 让「搬运」是原子的：源端不必、也不能再关一次，
    /// 与 Unix 上 `SCM_RIGHTS` 之后发送方 `drop` 自己那份的效果一致。
    pub fn take(&self, raw: RawAttachment) -> io::Result<Attachment> {
        use windows_sys::Win32::Foundation::{
            DUPLICATE_CLOSE_SOURCE, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let Some(peer) = &self.handle else {
            return Err(io::Error::other(
                "这条通道不知道对端是谁，无法接收附件（对端进程句柄未打开）",
            ));
        };
        let mut out: HANDLE = std::ptr::null_mut();
        // SAFETY: peer 是带 PROCESS_DUP_HANDLE 的有效进程句柄；
        // raw 是对端进程里的句柄值，由对端在同一帧里声明；
        // GetCurrentProcess 返回的伪句柄永远有效。
        let ok = unsafe {
            DuplicateHandle(
                peer.raw(),
                raw as usize as HANDLE,
                GetCurrentProcess(),
                &raw mut out,
                0,
                0,
                DUPLICATE_SAME_ACCESS | DUPLICATE_CLOSE_SOURCE,
            )
        };
        if ok == 0 {
            return Err(crate::platform::windows::last_error());
        }
        // SAFETY: DuplicateHandle 成功即 out 是本进程里一个新的、独占的句柄。
        Ok(unsafe {
            <Attachment as std::os::windows::io::FromRawHandle>::from_raw_handle(
                out as std::os::windows::io::RawHandle,
            )
        })
    }
}

#[cfg(windows)]
impl Clone for PeerProcess {
    fn clone(&self) -> Self {
        PeerProcess {
            handle: self.handle.clone(),
        }
    }
}

// ===========================================================================
// 通道
// ===========================================================================

#[cfg(unix)]
type Inner = tokio::net::UnixStream;

/// Windows 上命名管道的两端是**不同的类型**（服务端还管着实例的生命周期），
/// 但对我们来说它们的行为完全一致，因此用一个枚举抹平。
#[cfg(windows)]
#[derive(Debug)]
enum Inner {
    Server(tokio::net::windows::named_pipe::NamedPipeServer),
    Client(tokio::net::windows::named_pipe::NamedPipeClient),
}

/// 一条与另一个进程之间的双向字节通道。
#[derive(Debug)]
pub struct IpcChannel {
    inner: Inner,
    peer: PeerProcess,
}

impl IpcChannel {
    /// 对端进程，附件传递要用（见模块文档）。
    pub fn peer(&self) -> &PeerProcess {
        &self.peer
    }

    /// 记下对端是哪个进程。
    ///
    /// 在 Unix 上是无操作；Windows 上这一步不做，之后收到附件会报协议错误。
    pub fn set_peer_pid(&mut self, pid: u32) {
        self.peer = PeerProcess::open(pid);
    }

    /// 等可读。
    pub async fn readable(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.readable().await
        }
        #[cfg(windows)]
        match &self.inner {
            Inner::Server(s) => s.readable().await,
            Inner::Client(c) => c.readable().await,
        }
    }

    /// 等可写。
    pub async fn writable(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            self.inner.writable().await
        }
        #[cfg(windows)]
        match &self.inner {
            Inner::Server(s) => s.writable().await,
            Inner::Client(c) => c.writable().await,
        }
    }

    /// 试读一次。就绪只是提示，`WouldBlock` 是正常结果。
    pub fn try_read(&self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            self.inner.try_read(buf)
        }
        #[cfg(windows)]
        match &self.inner {
            Inner::Server(s) => s.try_read(buf),
            Inner::Client(c) => c.try_read(buf),
        }
    }

    /// 试写一次。
    pub fn try_write(&self, buf: &[u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            self.inner.try_write(buf)
        }
        #[cfg(windows)]
        match &self.inner {
            Inner::Server(s) => s.try_write(buf),
            Inner::Client(c) => c.try_write(buf),
        }
    }

    /// 在就绪的前提下做一次自定义 I/O（Unix 侧用它跑 `recvmsg` / `sendmsg`）。
    #[cfg(unix)]
    pub fn try_io<R>(
        &self,
        interest: tokio::io::Interest,
        f: impl FnOnce() -> io::Result<R>,
    ) -> io::Result<R> {
        self.inner.try_io(interest, f)
    }

    /// 裸 fd，供 `sendmsg` / `recvmsg` / `shutdown` 使用。
    #[cfg(unix)]
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.inner.as_raw_fd()
    }

    /// 半关写方向，让对端读到 EOF。
    ///
    /// # Windows 上是无操作
    ///
    /// 命名管道**没有半关**：要么开着，要么整条断开。这一点不影响协议——
    /// 需要「告诉对端我说完了」的地方本来就有显式的帧
    /// （`ToWorker::Shutdown`），半关只是 Unix 上顺手多加的一道保险。
    /// 真正的 EOF 由进程退出时管道关闭产生，那条路径两个平台一致。
    pub fn shutdown_write(&self) {
        #[cfg(unix)]
        {
            let _ = nix::sys::socket::shutdown(self.as_raw_fd(), nix::sys::socket::Shutdown::Write);
        }
    }

    /// 造一对互相连通的通道。
    ///
    /// 用于单元测试与**进程内 worker**（`session::tests` 里那套假 helper）。
    /// 生产路径不走它：那里的两端分别属于两个进程。
    pub fn pair() -> io::Result<(IpcChannel, IpcChannel)> {
        #[cfg(unix)]
        {
            let (a, b) = tokio::net::UnixStream::pair()?;
            Ok((
                IpcChannel {
                    inner: a,
                    peer: PeerProcess::unknown(),
                },
                IpcChannel {
                    inner: b,
                    peer: PeerProcess::unknown(),
                },
            ))
        }
        #[cfg(windows)]
        {
            // Windows 没有 socketpair。等价做法是「自己连自己」：建一条随机名的
            // 命名管道实例，再以客户端身份连上去，两端就配好了。
            // 名字随机 + FIRST_PIPE_INSTANCE，别的进程抢不到这个名字。
            let name = windows_impl::random_pipe_name("pair");
            let server = windows_impl::create_server(&name, None)?;
            let client = tokio::net::windows::named_pipe::ClientOptions::new().open(&name)?;
            // 服务端要把这次连接「收下」。客户端已经连上时返回的是
            // ERROR_PIPE_CONNECTED，tokio 把它当成功处理。
            //
            // 这里不能 await（`pair` 是同步的），但客户端已经连上，
            // `connect()` 会立刻就绪——用 `now_or_never` 取走那个立即完成的 future。
            // 拿不到（理论上不会发生）就当连接尚未建立，交给后续 I/O 报错。
            let connected = futures::FutureExt::now_or_never(server.connect());
            if let Some(Err(e)) = connected {
                return Err(e);
            }
            // 本进程内的两端，对端就是自己。
            let me = std::process::id();
            Ok((
                IpcChannel {
                    inner: Inner::Server(server),
                    peer: PeerProcess::open(me),
                },
                IpcChannel {
                    inner: Inner::Client(client),
                    peer: PeerProcess::open(me),
                },
            ))
        }
    }
}

// ---------------------------------------------------------------- Unix 构造

#[cfg(unix)]
impl IpcChannel {
    /// 接管一条已连接的 Unix socket。
    pub fn from_unix(stream: tokio::net::UnixStream) -> IpcChannel {
        IpcChannel {
            inner: stream,
            peer: PeerProcess,
        }
    }

    /// 接管一个 socketpair 的一端（helper 经 `SCM_RIGHTS` 交回来的那个）。
    pub fn from_owned_fd(fd: std::os::fd::OwnedFd) -> io::Result<IpcChannel> {
        let std_stream = std::os::unix::net::UnixStream::from(fd);
        std_stream.set_nonblocking(true)?;
        Ok(IpcChannel::from_unix(tokio::net::UnixStream::from_std(
            std_stream,
        )?))
    }
}

// -------------------------------------------------------------- Windows 构造

#[cfg(windows)]
pub use windows_impl::{PIPE_PREFIX, random_pipe_name};

#[cfg(windows)]
impl IpcChannel {
    /// 接管一个**已连接**的命名管道服务端句柄。
    ///
    /// helper 交回来的 worker 通道走这条路：它在 helper 那边已经 `ConnectNamedPipe`
    /// 过了，主进程这边只需把句柄挂到 tokio 的 IOCP 上。
    ///
    /// # Safety
    ///
    /// `handle` 必须是一个本进程拥有的、带 `FILE_FLAG_OVERLAPPED` 的命名管道
    /// 服务端句柄，且尚未被注册到任何 I/O 完成端口。
    pub unsafe fn from_server_handle(
        handle: std::os::windows::io::OwnedHandle,
        peer: PeerProcess,
    ) -> io::Result<IpcChannel> {
        use std::os::windows::io::IntoRawHandle as _;
        // SAFETY: 调用方保证句柄的性质；所有权在此转移给 tokio。
        let server = unsafe {
            tokio::net::windows::named_pipe::NamedPipeServer::from_raw_handle(
                handle.into_raw_handle(),
            )
        }?;
        Ok(IpcChannel {
            inner: Inner::Server(server),
            peer,
        })
    }

    /// 接管一个**已连接**的命名管道客户端句柄。
    ///
    /// worker 启动时走这条路：helper 把管道的一端作为可继承句柄传了进来，
    /// 句柄值经命令行告知（对应 Unix 上 `dup2` 到 fd 3 的那一步）。
    ///
    /// # Safety
    ///
    /// 同 [`IpcChannel::from_server_handle`]，但要求是客户端一侧的句柄。
    pub unsafe fn from_client_handle(
        handle: std::os::windows::io::OwnedHandle,
    ) -> io::Result<IpcChannel> {
        use std::os::windows::io::IntoRawHandle as _;
        // SAFETY: 调用方保证句柄的性质；所有权在此转移给 tokio。
        let client = unsafe {
            tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(
                handle.into_raw_handle(),
            )
        }?;
        Ok(IpcChannel {
            inner: Inner::Client(client),
            peer: PeerProcess::unknown(),
        })
    }

    /// 用一个命名管道服务端建通道（尚未连接，需 `accept` 等对端连上）。
    pub fn server(name: &str, sddl: Option<&str>) -> io::Result<IpcChannel> {
        Ok(IpcChannel {
            inner: Inner::Server(windows_impl::create_server(name, sddl)?),
            peer: PeerProcess::unknown(),
        })
    }

    /// 等对端连上来，并核对它确实是 `expect_pid` 那个进程。
    ///
    /// # 这一步就是 Windows 上的身份证明
    ///
    /// Unix 上「fd 是从父进程继承来的」本身就是身份证明；命名管道是有名字的，
    /// 任何能访问该名字的进程都可以来连。因此必须核对客户端的 pid ——
    /// 名字是 128 位随机的，再加上这道核对，冒名顶替就不成立了。
    pub async fn accept_from(&mut self, expect_pid: u32) -> io::Result<()> {
        let Inner::Server(server) = &self.inner else {
            return Err(io::Error::other("只有服务端一侧才能等待连接"));
        };
        server.connect().await?;
        let actual = windows_impl::client_process_id(server)?;
        if actual != expect_pid {
            return Err(io::Error::other(format!(
                "命名管道的客户端是 pid {actual}，与预期的 {expect_pid} 不符——拒绝这条连接"
            )));
        }
        self.peer = PeerProcess::open(expect_pid);
        Ok(())
    }

    /// 造一对管道：本进程留下服务端，另一端是准备**交给别的进程**的裸句柄。
    ///
    /// `term.open` 用它：worker 开完终端要把通道的一端连同结果一起交给主进程。
    ///
    /// # 为什么不能直接用 [`IpcChannel::pair`] 再把一端拆出来
    ///
    /// `pair` 的两端都已经注册到**本进程**的 I/O 完成端口上了。把这样一个句柄
    /// 交出去（还带着 `DUPLICATE_CLOSE_SOURCE`）等于在 tokio 背后把它用的句柄
    /// 关掉，之后 tokio 对它的每一次操作都是未定义行为。
    ///
    /// 所以交出去的那一端从一开始就不注册：用 `CreateFileW` 直接开，
    /// 拿到的是裸 `OwnedHandle`，由接收方在它自己的运行时里注册
    /// （[`IpcChannel::from_client_handle`]）。
    pub async fn pair_for_transfer(
        kind: &str,
    ) -> io::Result<(IpcChannel, std::os::windows::io::OwnedHandle)> {
        use windows_sys::Win32::Foundation::GENERIC_READ;
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
        };

        let name = windows_impl::random_pipe_name(kind);
        let server = windows_impl::create_server(&name, None)?;
        let wide = crate::platform::windows::wide::to_wide(&name);
        // SAFETY: wide 以 NUL 结尾；其余参数为常量；失败返回 INVALID_HANDLE_VALUE，
        // 由 Owned::new 挡下。FILE_FLAG_OVERLAPPED 是接收方注册到 IOCP 的前提。
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: raw 刚由 CreateFileW 返回；Owned::new 会挡掉空句柄与
        // INVALID_HANDLE_VALUE 两种失败形态。
        let client = unsafe { crate::platform::windows::handle::Owned::new(raw) }?;

        // 客户端已经连上，`connect()` 会立刻以 ERROR_PIPE_CONNECTED 成功返回。
        server.connect().await?;

        let handle = client.into_std();
        Ok((
            IpcChannel {
                inner: Inner::Server(server),
                peer: PeerProcess::unknown(),
            },
            handle,
        ))
    }

    /// 以客户端身份连上一条命名管道。
    pub fn connect(name: &str) -> io::Result<IpcChannel> {
        let client = tokio::net::windows::named_pipe::ClientOptions::new().open(name)?;
        Ok(IpcChannel {
            inner: Inner::Client(client),
            peer: PeerProcess::unknown(),
        })
    }
}

// ===========================================================================
// AsyncRead / AsyncWrite
// ===========================================================================
//
// 泛型的 `framing::{read_msg, write_msg}` 要求 `AsyncRead + Unpin` /
// `AsyncWrite + Unpin`，这里按内部类型转发。两个平台的内部类型都是 `Unpin` 的，
// 所以 `Pin::new` 是安全的。

impl AsyncRead for IpcChannel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        #[cfg(unix)]
        {
            Pin::new(&mut this.inner).poll_read(cx, buf)
        }
        #[cfg(windows)]
        match &mut this.inner {
            Inner::Server(s) => Pin::new(s).poll_read(cx, buf),
            Inner::Client(c) => Pin::new(c).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for IpcChannel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        #[cfg(unix)]
        {
            Pin::new(&mut this.inner).poll_write(cx, buf)
        }
        #[cfg(windows)]
        match &mut this.inner {
            Inner::Server(s) => Pin::new(s).poll_write(cx, buf),
            Inner::Client(c) => Pin::new(c).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        #[cfg(unix)]
        {
            Pin::new(&mut this.inner).poll_flush(cx)
        }
        #[cfg(windows)]
        match &mut this.inner {
            Inner::Server(s) => Pin::new(s).poll_flush(cx),
            Inner::Client(c) => Pin::new(c).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        #[cfg(unix)]
        {
            Pin::new(&mut this.inner).poll_shutdown(cx)
        }
        #[cfg(windows)]
        match &mut this.inner {
            Inner::Server(s) => Pin::new(s).poll_shutdown(cx),
            Inner::Client(c) => Pin::new(c).poll_shutdown(cx),
        }
    }
}

// ===========================================================================
// Windows 侧的管道细节
// ===========================================================================

#[cfg(windows)]
mod windows_impl {
    use std::io;

    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    use crate::platform::windows::last_error;
    use crate::platform::windows::wide::to_wide;

    /// 本项目全部命名管道的名字前缀。
    ///
    /// `\\.\pipe\` 之后的部分随便起，但要能一眼认出是谁开的——排查时
    /// `Get-ChildItem \\.\pipe\` 列出来的东西太多了。
    pub const PIPE_PREFIX: &str = r"\\.\pipe\strixmaid-";

    /// 造一个带 128 位随机量的管道名。
    ///
    /// 随机量是安全性的一部分：名字猜不到，再加上连接时核对 pid
    /// （见 `IpcChannel::accept_from`），冒名顶替就不成立。
    pub fn random_pipe_name(kind: &str) -> String {
        use rand::Rng as _;
        let mut buf = [0u8; 16];
        rand::rng().fill_bytes(&mut buf);
        format!("{PIPE_PREFIX}{kind}-{}", hex::encode(buf))
    }

    /// 建一条命名管道实例。
    ///
    /// `sddl` 给出安全描述符（SDDL 串）。为 `None` 时用默认安全性——
    /// 那等于「只有创建者与系统能连」，适合主进程↔helper（同一身份）；
    /// 而 helper↔worker 跨了身份，**必须**显式给出包含目标用户 SID 的 SDDL，
    /// 否则 worker 连不上自己的通道。
    pub fn create_server(name: &str, sddl: Option<&str>) -> io::Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(true)
            // 本项目的管道一律是「一对一」的：一条通道对应一个子进程。
            .max_instances(1)
            // 拒绝远程客户端：这些管道只服务本机进程，开着网络可达面没有任何好处。
            .reject_remote_clients(true)
            .in_buffer_size(64 * 1024)
            .out_buffer_size(64 * 1024);

        let Some(sddl) = sddl else {
            return options.create(name);
        };

        let mut descriptor: *mut core::ffi::c_void = std::ptr::null_mut();
        let w = to_wide(sddl);
        // SAFETY: w 以 NUL 结尾；SDDL_REVISION_1 = 1；descriptor 是输出参数，
        // 成功时指向一块 LocalAlloc 的内存，由下面的 LocalFree 归还。
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        let mut attrs = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        // SAFETY: attrs 指向本栈帧上一个填好的 SECURITY_ATTRIBUTES，
        // 其 lpSecurityDescriptor 在本调用期间有效。
        let result = unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&raw mut attrs).cast::<core::ffi::c_void>(),
            )
        };
        // SAFETY: descriptor 由 ConvertStringSecurityDescriptor… 用 LocalAlloc 分配。
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(descriptor);
        }
        result
    }

    /// 已连接的客户端是哪个进程。
    pub fn client_process_id(server: &NamedPipeServer) -> io::Result<u32> {
        use std::os::windows::io::AsRawHandle as _;
        let mut pid: u32 = 0;
        // SAFETY: server 持有的是有效的命名管道服务端句柄；pid 是输出参数。
        let ok = unsafe {
            GetNamedPipeClientProcessId(server.as_raw_handle() as HANDLE, &raw mut pid)
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[tokio::test]
    async fn 成对通道可双向传字节() {
        let (mut a, mut b) = IpcChannel::pair().expect("建一对通道");
        a.write_all(b"ping").await.unwrap();
        a.flush().await.unwrap();
        let mut buf = [0u8; 4];
        b.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");

        b.write_all(b"pong").await.unwrap();
        b.flush().await.unwrap();
        let mut buf = [0u8; 4];
        a.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    /// 一端关闭后另一端读到干净的 EOF——协议里「对端在帧边界上关闭」全靠这个。
    #[tokio::test]
    async fn 一端关闭后另一端读到_eof() {
        let (a, mut b) = IpcChannel::pair().unwrap();
        drop(a);
        let mut buf = [0u8; 8];
        let n = b.read(&mut buf).await;
        match n {
            Ok(0) => {}
            // 命名管道的对端消失时，Windows 先报 ERROR_BROKEN_PIPE 而不是 0 字节。
            // 两者在协议层是同一件事：没有更多数据了。
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
            other => panic!("应当读到 EOF，实际 {other:?}"),
        }
    }

    /// 就绪 API 走 `&self`，读写两侧因此可以共享同一个 `Arc`。
    /// 这是 `FdFrameReader` 与附着方并发写能成立的前提。
    #[tokio::test]
    async fn 同一个通道可以并发读写() {
        let (a, b) = IpcChannel::pair().unwrap();
        let a = std::sync::Arc::new(a);
        let b = std::sync::Arc::new(b);

        let writer = {
            let a = a.clone();
            tokio::spawn(async move {
                let mut sent = 0;
                while sent < 16 {
                    a.writable().await.unwrap();
                    // 就绪只是提示：Windows 上一次重叠写尚未完成时，下一次
                    // `try_write` 照样报 `WouldBlock`。把它当成功计数会让读端
                    // 永远等不到第 16 个字节，所以这里必须重试而不是丢弃。
                    match a.try_write(b"x") {
                        Ok(n) => sent += n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                        Err(e) => panic!("写失败: {e}"),
                    }
                }
            })
        };
        let reader = tokio::spawn(async move {
            let mut got = 0;
            while got < 16 {
                b.readable().await.unwrap();
                let mut buf = [0u8; 16];
                match b.try_read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => panic!("读失败: {e}"),
                }
            }
            got
        });
        writer.await.unwrap();
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), reader)
            .await
            .expect("读者不该挂住")
            .unwrap();
        assert_eq!(got, 16);
        drop(a);
    }

    #[cfg(windows)]
    #[test]
    fn 管道名带前缀且互不重复() {
        let a = random_pipe_name("worker");
        let b = random_pipe_name("worker");
        assert!(a.starts_with(PIPE_PREFIX), "{a}");
        assert!(a.contains("worker"));
        assert_ne!(a, b, "两次生成的名字不该相同");
        // 128 位随机量 = 32 个十六进制字符
        assert_eq!(a.len(), b.len());
        assert!(a.len() > PIPE_PREFIX.len() + 32);
    }

    /// Windows 上「不知道对端是谁」的通道收到附件必须报错，
    /// 而不是拿一个随便的句柄值去 `DuplicateHandle`。
    #[cfg(windows)]
    #[test]
    fn 未知对端时取附件报错() {
        let peer = PeerProcess::unknown();
        let err = peer.take(0x1234).unwrap_err();
        assert!(
            err.to_string().contains("不知道对端"),
            "错误信息要说清原因：{err}"
        );
    }
}
