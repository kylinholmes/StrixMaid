//! IPC 帧的 tokio 版读写 + `SCM_RIGHTS` 收 fd。
//!
//! 帧格式与纯编解码在 [`strixmaid_types::ipc`]（types 不依赖 tokio，所以异步壳放这里）。
//! 读入 / 编出的整帧都在 `Zeroizing<Vec<u8>>` 里——帧可能含明文密码。

use std::io::{self, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use serde::Serialize;
use serde::de::DeserializeOwned;
use strixmaid_types::ipc::{self, FRAME_HEADER_LEN, IpcError, IpcResult};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest};
use tokio::net::UnixStream;
use zeroize::Zeroizing;

/// 异步读一帧的 JSON 部分；对端在帧边界上关闭 → `Ok(None)`。
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
) -> IpcResult<Option<Zeroizing<Vec<u8>>>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // read_exact 在读到 0 字节时报 UnexpectedEof；分不清「一个字节都没读到」
            // 与「读了一半」，但帧头只有 4 字节，对端只会在帧边界关闭，按干净 EOF 处理。
            return Ok(None);
        }
        Err(e) => return Err(IpcError::Io(e)),
    }
    let len = ipc::parse_header(header)?;
    let mut payload = Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

/// 异步读一条消息。
pub async fn read_msg<R: AsyncRead + Unpin + ?Sized, T: DeserializeOwned>(
    r: &mut R,
) -> IpcResult<Option<T>> {
    match read_frame(r).await? {
        None => Ok(None),
        Some(payload) => ipc::decode(&payload).map(Some),
    }
}

/// 异步写一条消息。编码缓冲写完即擦除。
pub async fn write_msg<W: AsyncWrite + Unpin + ?Sized, T: Serialize + ?Sized>(
    w: &mut W,
    msg: &T,
) -> IpcResult<()> {
    let frame = ipc::encode(msg)?;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// 从 `stream` 上收一帧带 `SCM_RIGHTS` 的单字节消息，返回其中的 fd。
///
/// 收到的 fd 带 `CLOEXEC`（`MSG_CMSG_CLOEXEC`），不会泄漏给之后 spawn 的子进程。
pub async fn recv_fd(stream: &UnixStream) -> IpcResult<OwnedFd> {
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
                MsgFlags::MSG_CMSG_CLOEXEC,
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
            // SAFETY: 内核刚交给我们的 fd，尚无其它持有者。
            Ok((_, Some(fd))) => return Ok(unsafe { OwnedFd::from_raw_fd(fd) }),
            Ok((_, None)) => {
                return Err(IpcError::Protocol("fd 传递帧里没有 SCM_RIGHTS".into()));
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(IpcError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::ipc::{FromHelper, ToHelper};

    #[tokio::test]
    async fn 异步读写往返与干净_eof() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_msg(&mut a, &ToHelper::CloseSession).await.unwrap();
        write_msg(&mut a, &FromHelper::SessionClosed).await.unwrap();
        drop(a);
        let m: ToHelper = read_msg(&mut b).await.unwrap().unwrap();
        assert!(matches!(m, ToHelper::CloseSession));
        let m: FromHelper = read_msg(&mut b).await.unwrap().unwrap();
        assert_eq!(m, FromHelper::SessionClosed);
        assert!(read_msg::<_, FromHelper>(&mut b).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn 异步侧同样拒绝超长帧() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let bad = (ipc::MAX_FRAME_LEN as u32 + 1).to_be_bytes();
        a.write_all(&bad).await.unwrap();
        match read_frame(&mut b).await {
            Err(IpcError::TooLarge { .. }) => {}
            other => panic!("应为 TooLarge，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn scm_rights_收_fd() {
        use nix::sys::socket::{ControlMessage, sendmsg};
        use std::io::{IoSlice, Read, Write};

        let (a, b) = UnixStream::pair().unwrap();
        // 要传的 fd：一对 std socketpair，把一端传过去，另一端写字节验证连通。
        let (mut keep, give) = std::os::unix::net::UnixStream::pair().unwrap();
        let give_fd = give.as_raw_fd();
        let sender = tokio::task::spawn_blocking(move || {
            let fds = [give_fd];
            let iov = [IoSlice::new(b"F")];
            let cmsg = [ControlMessage::ScmRights(&fds)];
            sendmsg::<()>(a.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None).unwrap();
            // a 与 give 在这里 drop。
            drop(give);
            drop(a);
        });
        let fd = recv_fd(&b).await.unwrap();
        sender.await.unwrap();

        let mut received = std::os::unix::net::UnixStream::from(fd);
        keep.write_all(b"hello").unwrap();
        let mut buf = [0u8; 5];
        received.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }
}
