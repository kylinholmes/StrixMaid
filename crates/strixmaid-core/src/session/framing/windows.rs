//! Windows 侧的附件传递：帧尾带句柄值，由**读端**去对端进程里取。
//!
//! # 线格式
//!
//! 帧头与 JSON 与 Unix 完全一致（`u32 长度 + u8 附件数 + JSON`）。
//! 附件数大于 0 时，JSON **之后**紧跟 `附件数 × 8` 字节的小端 `u64`：
//!
//! ```text
//! ┌──────────┬──────────┬──────────────┬───────────────────────────┐
//! │ u32 长度 │ u8 个数  │ JSON（长度）  │ 个数 × u64（发送方的句柄）  │
//! └──────────┴──────────┴──────────────┴───────────────────────────┘
//!                                       └─ 只有 Windows 有这一段
//! ```
//!
//! 这一段是 **Windows 独有的线格式扩展**。不会造成跨平台不一致：一条通道的
//! 两端永远是同一台机器上的两个进程（主进程 / helper / worker），
//! 三者同版本、同平台发布。
//!
//! # 为什么句柄值要由读端去「取」
//!
//! 因为 `DuplicateHandle` 要的是**对源进程的 `PROCESS_DUP_HANDLE` 权限**，
//! 而附件永远是朝主进程流动的、主进程又永远是权限最高的那个。完整论证见
//! [`crate::session::channel`] 的模块文档。
//!
//! 写端只是把「我这边的句柄值是多少」写进帧尾；真正的搬运由读端的
//! [`PeerProcess::take`](crate::session::channel::PeerProcess::take) 完成，
//! 它带 `DUPLICATE_CLOSE_SOURCE`，所以写端**不要**再关自己那一份——
//! 与 Unix 上 `SCM_RIGHTS` 之后发送方 drop 掉自己那份是同一个节奏。
//!
//! # 一个 Unix 上没有的失败模式
//!
//! `SCM_RIGHTS` 是内核在 `sendmsg` 的那一刻就把 fd 装进了接收方；而这里
//! 「写帧」与「取句柄」是两步，中间隔着一次进程间往返。如果写端在这两步之间
//! **退出**了，句柄随进程一起消失，读端的 `DuplicateHandle` 会失败。
//! 这不是可以修掉的竞态，而是机制本身的性质；表现为一次 `Protocol` 错误，
//! 调用方按「这次附件没拿到」处理（与 Unix 上 fd 丢失时的处理一致）。

use std::io;
use std::os::windows::io::OwnedHandle;

use serde::Serialize;
use strixmaid_types::ipc::{self, FRAME_HEADER_LEN, IpcError, IpcResult};
use zeroize::Zeroizing;

use crate::session::channel::{IpcChannel, RawAttachment};

/// 帧尾每个附件占的字节数。
const ATTACHMENT_SIZE: usize = std::mem::size_of::<u64>();

/// 附带附件的帧读取器。
///
/// 与 Unix 版同名同形：调用方（`worker_handle`、`worker::serve`、`terminal`）
/// 不需要写 `cfg`。
pub struct FdFrameReader {
    stream: std::sync::Arc<IpcChannel>,
}

impl FdFrameReader {
    /// 接管一条连接的读方向。
    pub fn new(stream: std::sync::Arc<IpcChannel>) -> Self {
        FdFrameReader { stream }
    }

    /// 读一帧，连同它附带的句柄。对端在帧边界上关闭 → `Ok(None)`。
    pub async fn read(&mut self) -> IpcResult<Option<(Zeroizing<Vec<u8>>, Vec<OwnedHandle>)>> {
        let mut header = [0u8; FRAME_HEADER_LEN];
        if !self.read_exact(&mut header).await? {
            return Ok(None);
        }
        let (len, fd_count) = ipc::parse_header(header)?;

        let mut payload = Zeroizing::new(vec![0u8; len]);
        if len > 0 && !self.read_exact(&mut payload).await? {
            return Err(IpcError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "IPC 帧 payload 未读完对端即关闭",
            )));
        }

        if fd_count == 0 {
            return Ok(Some((payload, Vec::new())));
        }

        let mut trailer = vec![0u8; fd_count as usize * ATTACHMENT_SIZE];
        if !self.read_exact(&mut trailer).await? {
            return Err(IpcError::Io(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "帧尾的附件表未读完对端即关闭",
            )));
        }

        let mut handles = Vec::with_capacity(fd_count as usize);
        for i in 0..fd_count as usize {
            let at = i * ATTACHMENT_SIZE;
            let raw = RawAttachment::from_le_bytes(
                trailer[at..at + ATTACHMENT_SIZE]
                    .try_into()
                    .expect("切片长度恰为 8"),
            );
            match self.stream.peer().take(raw) {
                Ok(h) => handles.push(h),
                Err(e) => {
                    // 已经拿到的那些随 `handles` 一起 drop，不泄漏。
                    return Err(IpcError::Protocol(format!(
                        "无法从对端进程取回第 {} 个附件（句柄 {raw:#x}）：{e}",
                        i + 1
                    )));
                }
            }
        }
        Ok(Some((payload, handles)))
    }

    /// 读满 `buf`。返回 `Ok(false)` 表示「一个字节都没读到就 EOF」。
    async fn read_exact(&self, buf: &mut [u8]) -> IpcResult<bool> {
        let mut filled = 0;
        while filled < buf.len() {
            self.stream.readable().await.map_err(IpcError::Io)?;
            match self.stream.try_read(&mut buf[filled..]) {
                Ok(0) => return eof_or_error(filled),
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                // 对端进程消失：与读到 0 字节在协议层是同一件事。
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return eof_or_error(filled),
                Err(e) => return Err(IpcError::Io(e)),
            }
        }
        Ok(true)
    }
}

/// 读到一半就没了：帧边界上是干净 EOF，帧中间是协议错误。
fn eof_or_error(filled: usize) -> IpcResult<bool> {
    if filled == 0 {
        return Ok(false);
    }
    Err(IpcError::Io(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "IPC 帧未读完对端即关闭",
    )))
}

/// 写一条**附带附件**的消息。
///
/// `fds` 是**本进程**里的句柄值（用 [`crate::session::channel::raw_of`] 取），
/// 由读端负责搬走。见模块文档。
pub async fn write_msg_with_fds<T: Serialize + ?Sized>(
    stream: &IpcChannel,
    msg: &T,
    fds: &[RawAttachment],
) -> IpcResult<()> {
    let count = u8::try_from(fds.len()).map_err(|_| IpcError::TooManyFds { count: u8::MAX })?;
    let frame = ipc::encode_with_fds(msg, count)?;

    // 帧与帧尾必须**一次性**写出去，中间不能让别的写者插进来：
    // 写侧的串行化由调用方的 Mutex 保证（`worker::FrameWriter` / `WorkerHandle`），
    // 这里只保证自己这一份是连续的一段字节。
    let mut out = Zeroizing::new(Vec::with_capacity(frame.len() + fds.len() * ATTACHMENT_SIZE));
    out.extend_from_slice(&frame);
    for raw in fds {
        out.extend_from_slice(&raw.to_le_bytes());
    }

    let mut sent = 0;
    while sent < out.len() {
        stream.writable().await.map_err(IpcError::Io)?;
        match stream.try_write(&out[sent..]) {
            Ok(0) => {
                return Err(IpcError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "命名管道写出 0 字节",
                )));
            }
            Ok(n) => sent += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(IpcError::Io(e)),
        }
    }
    Ok(())
}
