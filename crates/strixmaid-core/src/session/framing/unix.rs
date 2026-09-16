//! Unix 侧的附件传递：`SCM_RIGHTS` 收发 fd。
//!
//! 内容与平台化改造之前逐字相同——它原本内联在 `session/framing.rs` 里，
//! 为给 Windows 的同名实现让出并列位置才挪进本文件。唯一的机械改动是
//! 把参数类型从 `&tokio::net::UnixStream` 换成 `&IpcChannel`
//! （后者在 Unix 上就是前者的薄封装，`as_raw_fd` / `try_io` 一一对应）。

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
use serde::Serialize;
use strixmaid_types::ipc::{self, FRAME_HEADER_LEN, IpcError, IpcResult};
use tokio::io::Interest;
use zeroize::Zeroizing;

use crate::session::channel::IpcChannel;

/// 给 fd 打上 `FD_CLOEXEC`。
///
/// Linux 上有 `SOCK_CLOEXEC` / `MSG_CMSG_CLOEXEC` 这类**原子**标志，创建 fd 的同时
/// 就带上 CLOEXEC，不存在窗口。macOS（以及其它 BSD）两个都没有，只能事后补一次
/// `fcntl`——从 fd 产生到这行执行之间存在一个极小的窗口，若恰好有另一个线程在
/// `fork + exec`，这个 fd 会被子进程继承。
///
/// 这是 macOS 内核 API 的固有限制，不是本实现的疏漏。对本项目影响可忽略：
/// 主进程在会话建立期间不会并发 spawn 无关子进程。
/// 交付目标 Linux 上走的是原子路径，没有这个窗口。
#[cfg(not(target_os = "linux"))]
pub(crate) fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: 只读改 fd 的标志位，无内存副作用。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 同上。
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `recvmsg` 的标志位：Linux 上原子地给收到的 fd 带上 CLOEXEC，
/// macOS 没有这个标志，改由 [`set_cloexec`] 事后补。
#[cfg(target_os = "linux")]
const RECV_FD_FLAGS: MsgFlags = MsgFlags::MSG_CMSG_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const RECV_FD_FLAGS: MsgFlags = MsgFlags::empty();

/// 从 `stream` 上收一帧带 `SCM_RIGHTS` 的单字节消息，返回其中的 fd。
///
/// 收到的 fd 带 `CLOEXEC`，不会泄漏给之后 spawn 的子进程。
pub async fn recv_fd(stream: &IpcChannel) -> IpcResult<OwnedFd> {
    loop {
        stream.readable().await?;
        let mut byte = [0u8; 1];
        let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);
        let attempt = stream.try_io(Interest::READABLE, || {
            let mut iov = [IoSliceMut::new(&mut byte)];
            let msg = recvmsg::<()>(
                stream.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_buf),
                RECV_FD_FLAGS,
            )
            .map_err(io::Error::from)?;
            let mut fd: Option<RawFd> = None;
            for c in msg.cmsgs().map_err(io::Error::from)? {
                if let ControlMessageOwned::ScmRights(fds) = c {
                    // 只期待一个；多余的立刻关掉，别泄漏。
                    let mut it = fds.into_iter();
                    fd = it.next();
                    for extra in it {
                        // SAFETY: 内核刚交给我们的 fd，尚无其它持有者。
                        drop(unsafe { OwnedFd::from_raw_fd(extra) });
                    }
                }
            }
            Ok((msg.bytes, fd))
        });
        match attempt {
            Ok((0, _)) => {
                return Err(IpcError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "等待 SCM_RIGHTS 时对端关闭",
                )));
            }
            Ok((_, Some(fd))) => {
                // macOS 上 recvmsg 没有 MSG_CMSG_CLOEXEC，补一次；Linux 上已经带了。
                #[cfg(not(target_os = "linux"))]
                set_cloexec(fd)?;
                // SAFETY: 内核刚交给我们的 fd，尚无其它持有者。
                return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
            }
            Ok((_, None)) => {
                return Err(IpcError::Protocol("fd 传递帧里没有 SCM_RIGHTS".into()));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(IpcError::Io(e)),
        }
    }
}

/// 附带 fd 的帧读取器。
///
/// # 为什么必须整条连接都用它
///
/// `SOCK_STREAM` 上的 `SCM_RIGHTS` 有个致命性质：**用普通 `read()` 读过附着了 fd
/// 的那些字节，内核会把 fd 直接丢掉**——不报错、无痕迹，只是拿到的终端永远连不上。
/// 因此只要一条连接上**可能**出现带 fd 的帧，它的读端就必须**每一帧**都走 `recvmsg`。
///
/// 主进程读 worker 的方向正是这种情况（`term.open` 会回一个 PTY 的 fd），所以
/// [`WorkerHandle`](crate::session::WorkerHandle) 的读循环用本类型，而不是泛型的
/// [`read_frame`](super::read_frame)。反方向（worker 读主进程）没有 fd，
/// 继续用泛型读端即可。
///
/// # 与帧头的 `fd_count` 对账
///
/// 收到的 fd 个数与帧头声明的不符时报 [`IpcError::Protocol`]。这不是洁癖：
/// 少收了 fd 意味着后面拿着半个终端去调试，早点炸掉比晚点炸掉便宜得多。
pub struct FdFrameReader {
    stream: std::sync::Arc<IpcChannel>,
}

impl FdFrameReader {
    /// 接管一条连接的读方向。
    ///
    /// 用 `Arc<IpcChannel>` 而不是 `OwnedReadHalf`：`recvmsg` 需要裸 fd，而
    /// tokio 的 `UnixStream` 允许 `&self` 并发读写（`readable()` / `writable()`
    /// 都取 `&self`），因此读写两侧共享同一个 `Arc` 即可，不必 split。
    pub fn new(stream: std::sync::Arc<IpcChannel>) -> Self {
        FdFrameReader { stream }
    }

    /// 读一帧，连同它附带的 fd。对端在帧边界上关闭 → `Ok(None)`。
    pub async fn read(&mut self) -> IpcResult<Option<(Zeroizing<Vec<u8>>, Vec<OwnedFd>)>> {
        let mut header = [0u8; FRAME_HEADER_LEN];
        let mut fds = Vec::new();

        // 帧头与 payload 都要走 recvmsg：fd 附着在**哪一段字节**上由发送端的
        // 一次 sendmsg 决定，读端无法预知，只能两段都备好控制缓冲。
        match self.recv_exact(&mut header, &mut fds).await {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(e) => return Err(e),
        }
        let (len, fd_count) = ipc::parse_header(header)?;

        let mut payload = Zeroizing::new(vec![0u8; len]);
        if len > 0 && !self.recv_exact(&mut payload, &mut fds).await? {
            return Err(IpcError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "IPC 帧 payload 未读完对端即关闭",
            )));
        }

        if fds.len() != fd_count as usize {
            return Err(IpcError::Protocol(format!(
                "帧头声称附带 {fd_count} 个 fd，实际收到 {}",
                fds.len()
            )));
        }
        Ok(Some((payload, fds)))
    }

    /// 用 `recvmsg` 读满 `buf`，把途中收到的 fd 追加进 `fds`。
    ///
    /// 返回 `Ok(false)` 表示「一个字节都没读到就 EOF」，即对端在帧边界干净关闭。
    async fn recv_exact(&self, buf: &mut [u8], fds: &mut Vec<OwnedFd>) -> IpcResult<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            self.stream.readable().await.map_err(IpcError::Io)?;
            let attempt = self.stream.try_io(Interest::READABLE, || {
                let mut cmsg = nix::cmsg_space!([RawFd; ipc::MAX_FRAME_FDS]);
                let mut iov = [IoSliceMut::new(&mut buf[filled..])];
                let msg = recvmsg::<()>(
                    self.stream.as_raw_fd(),
                    &mut iov,
                    Some(&mut cmsg),
                    RECV_FD_FLAGS,
                )
                .map_err(io::Error::from)?;

                let mut got = Vec::new();
                for c in msg.cmsgs().map_err(io::Error::from)? {
                    if let ControlMessageOwned::ScmRights(raw) = c {
                        for fd in raw {
                            // macOS 上 recvmsg 没有 MSG_CMSG_CLOEXEC，补一次。
                            #[cfg(not(target_os = "linux"))]
                            set_cloexec(fd)?;
                            // SAFETY: 内核刚交给我们的 fd，尚无其它持有者。
                            got.push(unsafe { OwnedFd::from_raw_fd(fd) });
                        }
                    }
                }
                Ok((msg.bytes, got))
            });

            match attempt {
                Ok((0, got)) => {
                    // 对端关闭。已经收到的 fd 随 `got` 一起 drop，不泄漏。
                    drop(got);
                    if filled == 0 {
                        return Ok(false);
                    }
                    return Err(IpcError::Io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "IPC 帧未读完对端即关闭",
                    )));
                }
                Ok((n, got)) => {
                    filled += n;
                    fds.extend(got);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(IpcError::Io(e)),
            }
        }
        Ok(true)
    }
}

/// 写一条**附带 fd** 的消息。
///
/// fd 与帧的第一个字节一起经 `SCM_RIGHTS` 发出；帧头里的 `fd_count` 让读端知道
/// 要收几个。`SOCK_STREAM` 保证字节序，因此读端按帧组装时 fd 必然落在这一帧内。
///
/// 注意 `sendmsg` 只保证**至少写出一个字节**，剩余部分要继续写；控制消息只在
/// 第一次调用时带上，重复带会让读端收到多份 fd。
pub async fn write_msg_with_fds<T: Serialize + ?Sized>(
    stream: &IpcChannel,
    msg: &T,
    fds: &[crate::session::channel::RawAttachment],
) -> IpcResult<()> {
    let count = u8::try_from(fds.len()).map_err(|_| ipc::IpcError::TooManyFds { count: u8::MAX })?;
    let frame = ipc::encode_with_fds(msg, count)?;
    // 线上传的是 u64（跨平台统一），Unix 这边要还原成 RawFd 交给 SCM_RIGHTS。
    let raw: Vec<RawFd> = fds.iter().map(|f| *f as RawFd).collect();

    let mut sent = 0;
    while sent < frame.len() {
        stream.writable().await.map_err(IpcError::Io)?;
        let first = sent == 0;
        let attempt = stream.try_io(Interest::WRITABLE, || {
            let iov = [IoSlice::new(&frame[sent..])];
            // 控制消息只随第一次写发出；后续续写不再带，否则读端会收到重复的 fd。
            let cmsgs: Vec<ControlMessage<'_>> = if first && !raw.is_empty() {
                vec![ControlMessage::ScmRights(&raw)]
            } else {
                Vec::new()
            };
            let n = sendmsg::<()>(stream.as_raw_fd(), &iov, &cmsgs, MsgFlags::empty(), None)
                .map_err(io::Error::from)?;
            Ok(n)
        });
        match attempt {
            Ok(0) => {
                return Err(IpcError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "sendmsg 写出 0 字节",
                )));
            }
            Ok(n) => sent += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(IpcError::Io(e)),
        }
    }
    Ok(())
}
