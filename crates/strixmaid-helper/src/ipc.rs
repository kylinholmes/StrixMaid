//! helper 侧的 IPC 通道：fd 3 上的 socketpair，同步读写，外加 `SCM_RIGHTS` 发 fd。

use std::io::IoSlice;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use nix::sys::stat::{SFlag, fstat};
use strixmaid_types::ipc::{self, FromHelper, IpcError, IpcResult, ToHelper};

/// 与主进程的通道。
pub struct Ipc {
    stream: UnixStream,
}

/// `sendmsg` 的标志位：Linux 上用 `MSG_NOSIGNAL` 抑制 `SIGPIPE`，
/// macOS 没有该标志，改由 `SO_NOSIGPIPE` 套接字选项达成同样效果。
#[cfg(target_os = "linux")]
const NO_SIGNAL: MsgFlags = MsgFlags::MSG_NOSIGNAL;
#[cfg(not(target_os = "linux"))]
const NO_SIGNAL: MsgFlags = MsgFlags::empty();

impl Ipc {
    /// 接管继承的 fd（[`ipc::IPC_FD`]）。先 `fstat` 确认它真的是个 socket——
    /// helper 被人从终端里直接敲起来时 fd 3 要么不存在要么是别的东西。
    pub fn from_inherited_fd(fd: i32) -> Result<Ipc, String> {
        // SAFETY: 只借用来 fstat，不接管所有权。
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let st = fstat(borrowed).map_err(|e| format!("fd {fd} 不可用: {e}"))?;
        if SFlag::from_bits_truncate(st.st_mode) & SFlag::S_IFMT != SFlag::S_IFSOCK {
            return Err(format!(
                "fd {fd} 不是 socket；helper 只能由 strixmaid 主进程拉起"
            ));
        }
        // macOS 没有 MSG_NOSIGNAL，它把「写到已关闭的对端不要发信号」做成了
        // 套接字选项。在这里设一次，之后这个 socket 上的所有写（包括普通
        // write，不止 sendmsg）都只返回 EPIPE 而不会打死 helper。
        #[cfg(target_os = "macos")]
        {
            let on: libc::c_int = 1;
            // SAFETY: optval 指向一个 c_int，optlen 如实描述其大小；
            // setsockopt 只读取该缓冲区。
            let rc = unsafe {
                libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    libc::SO_NOSIGPIPE,
                    (&raw const on).cast::<libc::c_void>(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                return Err(format!(
                    "设置 SO_NOSIGPIPE 失败: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }

        // SAFETY: fd 是主进程 dup2 过来的 socketpair 一端，本进程独占。
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Ipc {
            stream: UnixStream::from(owned),
        })
    }

    /// 读一条消息；主进程关闭通道时返回 `Ok(None)`。
    pub fn recv(&mut self) -> IpcResult<Option<ToHelper>> {
        ipc::read_msg(&mut self.stream)
    }

    /// 写一条消息。
    pub fn send(&mut self, msg: &FromHelper) -> IpcResult<()> {
        ipc::write_msg(&mut self.stream, msg)
    }

    /// 发一条消息并阻塞等下一条——conversation 回调用它完成一轮 challenge-response。
    pub fn send_and_wait(&mut self, msg: FromHelper) -> IpcResult<Option<ToHelper>> {
        self.send(&msg)?;
        self.recv()
    }

    /// 经 `SCM_RIGHTS` 把一个 fd 传给主进程。payload 是单字节 `b'F'`——
    /// Linux 要求至少带 1 字节数据才能携带控制消息。
    ///
    /// 对端已消失时不能让内核送 `SIGPIPE` 把 helper 打死：Linux 用
    /// `MSG_NOSIGNAL`，macOS 没有这个标志，改由 [`Ipc::from_inherited_fd`] 里设的
    /// `SO_NOSIGPIPE` 套接字选项达成同样效果。两条路径的结果一样：
    /// 写到已关闭的对端只返回 `EPIPE`，由调用方当普通 I/O 错误处理。
    pub fn send_fd(&mut self, fd: &OwnedFd) -> IpcResult<()> {
        let fds = [fd.as_raw_fd()];
        let iov = [IoSlice::new(b"F")];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        let n = sendmsg::<()>(
            self.stream.as_raw_fd(),
            &iov,
            &cmsg,
            NO_SIGNAL,
            None,
        )
        .map_err(|e| IpcError::Io(std::io::Error::from(e)))?;
        if n != 1 {
            return Err(IpcError::Protocol(format!("sendmsg 只写出了 {n} 字节")));
        }
        Ok(())
    }
}
