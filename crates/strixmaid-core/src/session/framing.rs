//! IPC 帧的 tokio 版读写 + 附件（fd / HANDLE）传递。
//!
//! 帧格式与纯编解码在 [`strixmaid_types::ipc`]（types 不依赖 tokio，所以异步壳放这里）。
//! 读入 / 编出的整帧都在 `Zeroizing<Vec<u8>>` 里——帧可能含明文密码。
//!
//! # 分层
//!
//! | 这里 | 内容 |
//! |---|---|
//! | 本文件 | 与平台无关的部分：[`read_frame`] / [`read_msg`] / [`write_msg`]，泛型于 `AsyncRead` / `AsyncWrite` |
//! | [`unix`] | `SCM_RIGHTS` 收发 fd、`set_cloexec` |
//! | [`windows`] | 帧尾附带句柄值、由读端 `DuplicateHandle` 取走 |
//!
//! 两个平台侧暴露**同名同形**的 [`FdFrameReader`] 与 [`write_msg_with_fds`]，
//! 调用方（`worker_handle`、`worker::serve`、`terminal`）因此不必写 `cfg`。
//!
//! # 「附件」这个词
//!
//! 类型与函数名里保留了 `fd` 这个字眼（`FdFrameReader`、`write_msg_with_fds`、
//! 帧头的 `fd_count`）——那是 Unix 的说法，改名要动三个平台的调用点与线格式文档，
//! 不值得。在 Windows 上把它读成「附件」即可：同一个位置装的是 `HANDLE`，
//! 语义完全对应，搬运机制不同（见 [`super::channel`] 的模块文档）。

use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use strixmaid_types::ipc::{self, FRAME_HEADER_LEN, IpcError, IpcResult};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::{FdFrameReader, recv_fd, write_msg_with_fds};
#[cfg(windows)]
pub use windows::{FdFrameReader, write_msg_with_fds};

/// 给 fd 打上 `FD_CLOEXEC`（Unix 专属，见 [`unix::set_cloexec`]）。
///
/// `pub(crate)` 而不是 `pub`：被重导出的那一项本身就是 `pub(crate)`，用 `pub use`
/// 转出去是 E0364。两个调用点（`session::helper`、`worker::terminal::unix`）都在
/// 本 crate 内，从来不需要 `pub`。
///
/// 这一行的 cfg 只在 macOS 上成立（Linux 有原子的 `SOCK_CLOEXEC`，Windows 没有
/// `unix` 模块），所以 Linux 与 Windows 两条 CI 都编不到它——macOS 进 CI 的第一次
/// 就把它挡下了。
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) use unix::set_cloexec;

/// 异步读一帧的 JSON 部分；对端在帧边界上关闭 → `Ok(None)`。
pub async fn read_frame<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
) -> IpcResult<Option<Zeroizing<Vec<u8>>>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            // read_exact 在读到 0 字节时报 UnexpectedEof；分不清「一个字节都没读到」
            // 与「读了一半」，但帧头只有 5 字节，对端只会在帧边界关闭，按干净 EOF 处理。
            return Ok(None);
        }
        // 命名管道的对端进程消失时，Windows 报的是 ERROR_BROKEN_PIPE 而不是
        // 「读到 0 字节」。在协议层这与干净 EOF 是同一件事：没有更多帧了。
        // 不把它当错误，否则每次 worker 正常退出都会在日志里留一条 warn。
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
        Err(e) => return Err(IpcError::Io(e)),
    }
    let (len, fd_count) = ipc::parse_header(header)?;
    // 这个泛型读端读不了附件：Unix 上普通 `read` 会把附着在这些字节上的 fd
    // **静默丢掉**（内核行为，不报错、无痕迹），Windows 上则会把帧尾的句柄值
    // 当成下一帧的开头。需要收附件的那一侧用 [`FdFrameReader`]；
    // 这里只能把它当协议错误报出来。
    if fd_count > 0 {
        return Err(IpcError::Protocol(format!(
            "该读端不具备接收能力，却收到声称附带 {fd_count} 个 fd 的帧"
        )));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::channel::IpcChannel;
    use strixmaid_types::ipc::{FromHelper, ToHelper};

    #[tokio::test]
    async fn 异步读写往返与干净_eof() {
        let (mut a, mut b) = IpcChannel::pair().unwrap();
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
        let (mut a, mut b) = IpcChannel::pair().unwrap();
        let mut bad = (ipc::MAX_FRAME_LEN as u32 + 1).to_be_bytes().to_vec();
        bad.push(0); // fd_count：帧头是 5 字节，少写一个字节会让读端一直等下去
        a.write_all(&bad).await.unwrap();
        a.flush().await.unwrap();
        match read_frame(&mut b).await {
            Err(IpcError::TooLarge { .. }) => {}
            other => panic!("应为 TooLarge，实际 {other:?}"),
        }
    }

    /// 泛型读端**不具备**接收附件的能力，收到声称带附件的帧必须报协议错误，
    /// 而不是把附件悄悄丢掉（Unix）或把帧尾当成下一帧（Windows）。
    #[tokio::test]
    async fn 泛型读端拒绝带附件的帧() {
        let (mut a, mut b) = IpcChannel::pair().unwrap();
        let frame = ipc::encode_with_fds(&FromHelper::SessionClosed, 1).unwrap();
        a.write_all(&frame).await.unwrap();
        a.flush().await.unwrap();
        match read_frame(&mut b).await {
            Err(IpcError::Protocol(m)) => assert!(m.contains("fd"), "{m}"),
            other => panic!("应为 Protocol，实际 {other:?}"),
        }
    }
}
