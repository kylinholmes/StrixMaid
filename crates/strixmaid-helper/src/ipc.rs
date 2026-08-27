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
    pub fn send_fd(&mut self, fd: &OwnedFd) -> IpcResult<()> {
        let fds = [fd.as_raw_fd()];
        let iov = [IoSlice::new(b"F")];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        let n = sendmsg::<()>(
            self.stream.as_raw_fd(),
            &iov,
            &cmsg,
            MsgFlags::MSG_NOSIGNAL,
            None,
        )
        .map_err(|e| IpcError::Io(std::io::Error::from(e)))?;
        if n != 1 {
            return Err(IpcError::Protocol(format!("sendmsg 只写出了 {n} 字节")));
        }
        Ok(())
    }
}
