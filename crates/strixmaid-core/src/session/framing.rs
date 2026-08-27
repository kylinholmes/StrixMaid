//! IPC 帧的 tokio 版读写 + `SCM_RIGHTS` 收 fd。
//!
//! 帧格式与纯编解码在 [`strixmaid_types::ipc`]（types 不依赖 tokio，所以异步壳放这里）。
//! 读入 / 编出的整帧都在 `Zeroizing<Vec<u8>>` 里——帧可能含明文密码。

use std::io::{self, IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use nix::sys::socket::{ControlMessage, ControlMessageOwned, MsgFlags, recvmsg, sendmsg};
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
            // 与「读了一半」，但帧头只有 5 字节，对端只会在帧边界关闭，按干净 EOF 处理。
            return Ok(None);
        }
        Err(e) => return Err(IpcError::Io(e)),
    }
    let (len, fd_count) = ipc::parse_header(header)?;
    // 这个泛型读端读不了带外数据：普通 `read` 会把附着在这些字节上的 fd
    // **静默丢掉**（内核行为，不报错、无痕迹）。需要收 fd 的那一侧用
    // [`FdFrameReader`]；这里只能把它当协议错误报出来。
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

/// 给 fd 打上 `FD_CLOEXEC`。
///
/// Linux 上有 `SOCK_CLOEXEC` / `MSG_CMSG_CLOEXEC` 这类**原子**标志，创建 fd 的同时
/// 就带上 CLOEXEC，不存在窗口。macOS（以及其它 BSD）两个都没有，只能事后补一次
/// `fcntl`——从 fd 产生到这行执行之间存在一个极小的窗口，若恰好有另一个线程在
/// `fork + exec`，这个 fd 会被子进程继承。
///
/// 这是 macOS 内核 API 的固有限制，不是本实现的疏漏。对本项目影响可忽略：
/// macOS 只是开发平台，且主进程在会话建立期间不会并发 spawn 无关子进程。
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

// ===========================================================================
// 带 fd 的帧
// ===========================================================================

/// 附带 fd 的帧读取器。
///
/// # 为什么必须整条连接都用它
///
/// `SOCK_STREAM` 上的 `SCM_RIGHTS` 有个致命性质：**用普通 `read()` 读过附着了 fd
/// 的那些字节，内核会把 fd 直接丢掉**——不报错、无痕迹，只是拿到的终端永远连不上。
/// 因此只要一条连接上**可能**出现带 fd 的帧，它的读端就必须**每一帧**都走 `recvmsg`。
///
/// 主进程读 worker 的方向正是这种情况（`term.open` 会回一个 PTY 的 fd），所以
/// [`WorkerHandle`](super::WorkerHandle) 的读循环用本类型，而不是泛型的 [`read_frame`]。
/// 反方向（worker 读主进程）没有 fd，继续用泛型读端即可。
///
/// # 与帧头的 `fd_count` 对账
///
/// 收到的 fd 个数与帧头声明的不符时报 [`IpcError::Protocol`]。这不是洁癖：
/// 少收了 fd 意味着后面拿着半个终端去调试，早点炸掉比晚点炸掉便宜得多。
pub struct FdFrameReader {
    stream: std::sync::Arc<UnixStream>,
}

impl FdFrameReader {
    /// 接管一条连接的读方向。
    ///
    /// 用 `Arc<UnixStream>` 而不是 `OwnedReadHalf`：`recvmsg` 需要裸 fd，而
    /// tokio 的 `UnixStream` 允许 `&self` 并发读写（`readable()` / `writable()`
    /// 都取 `&self`），因此读写两侧共享同一个 `Arc` 即可，不必 split。
    pub fn new(stream: std::sync::Arc<UnixStream>) -> Self {
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
    stream: &UnixStream,
    msg: &T,
    fds: &[RawFd],
) -> IpcResult<()> {
    let count = u8::try_from(fds.len()).map_err(|_| ipc::IpcError::TooManyFds { count: u8::MAX })?;
    let frame = ipc::encode_with_fds(msg, count)?;

    let mut sent = 0;
    while sent < frame.len() {
        stream.writable().await.map_err(IpcError::Io)?;
        let first = sent == 0;
        let attempt = stream.try_io(Interest::WRITABLE, || {
            let iov = [IoSlice::new(&frame[sent..])];
            // 控制消息只随第一次写发出；后续续写不再带，否则读端会收到重复的 fd。
            let cmsgs: Vec<ControlMessage<'_>> = if first && !fds.is_empty() {
                vec![ControlMessage::ScmRights(fds)]
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
        let mut bad = (ipc::MAX_FRAME_LEN as u32 + 1).to_be_bytes().to_vec();
        bad.push(0); // fd_count：帧头是 5 字节，少写一个字节会让读端一直等下去
        a.write_all(&bad).await.unwrap();
        match read_frame(&mut b).await {
            Err(IpcError::TooLarge { .. }) => {}
            other => panic!("应为 TooLarge，实际 {other:?}"),
        }
    }

    /// 造一对可验证连通性的 fd：返回 (留在本地的一端, 要传出去的一端)。
    fn pipe_pair() -> (std::os::unix::net::UnixStream, std::os::unix::net::UnixStream) {
        std::os::unix::net::UnixStream::pair().unwrap()
    }

    #[tokio::test]
    async fn 带_fd_的帧往返_并且_fd_真的能用() {
        use std::io::{Read, Write};
        use std::sync::Arc;

        let (a, b) = UnixStream::pair().unwrap();
        let (mut keep, give) = pipe_pair();

        write_msg_with_fds(&a, &FromHelper::SessionClosed, &[give.as_raw_fd()])
            .await
            .unwrap();

        let mut reader = FdFrameReader::new(Arc::new(b));
        let (payload, fds) = reader.read().await.unwrap().unwrap();
        assert_eq!(ipc::decode::<FromHelper>(&payload).unwrap(), FromHelper::SessionClosed);
        assert_eq!(fds.len(), 1, "帧头声明了 1 个 fd，就必须收到 1 个");

        // 收到的 fd 必须是**能用的**那一端，而不是随便一个数字：
        // 从它写字节，本地留着的一端要能读到。
        let mut got = std::os::unix::net::UnixStream::from(fds.into_iter().next().unwrap());
        got.write_all(b"ping").unwrap();
        let mut buf = [0u8; 4];
        keep.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"ping");
        drop(give);
    }

    #[tokio::test]
    async fn 不带_fd_的帧走同一个读端也正常() {
        use std::sync::Arc;
        let (mut a, b) = UnixStream::pair().unwrap();
        write_msg_with_fds(&a, &ToHelper::CloseSession, &[]).await.unwrap();
        // 普通 write_msg 写的帧，FdFrameReader 也要读得了——读端是整条连接共用的。
        write_msg(&mut a, &FromHelper::SessionClosed).await.unwrap();
        drop(a);

        let mut reader = FdFrameReader::new(Arc::new(b));
        let (p1, f1) = reader.read().await.unwrap().unwrap();
        assert!(f1.is_empty());
        assert!(matches!(ipc::decode::<ToHelper>(&p1).unwrap(), ToHelper::CloseSession));
        let (p2, f2) = reader.read().await.unwrap().unwrap();
        assert!(f2.is_empty());
        assert_eq!(ipc::decode::<FromHelper>(&p2).unwrap(), FromHelper::SessionClosed);
        assert!(reader.read().await.unwrap().is_none(), "帧边界上的关闭是干净 EOF");
    }

    #[tokio::test]
    async fn 多个_fd_一次传完() {
        use std::sync::Arc;
        let (a, b) = UnixStream::pair().unwrap();
        let pairs: Vec<_> = (0..ipc::MAX_FRAME_FDS).map(|_| pipe_pair()).collect();
        let raw: Vec<RawFd> = pairs.iter().map(|(_, g)| g.as_raw_fd()).collect();

        write_msg_with_fds(&a, &FromHelper::SessionClosed, &raw).await.unwrap();
        let mut reader = FdFrameReader::new(Arc::new(b));
        let (_, fds) = reader.read().await.unwrap().unwrap();
        assert_eq!(fds.len(), ipc::MAX_FRAME_FDS);
    }

    #[tokio::test]
    async fn 超过上限的_fd_在发送前就被拒() {
        let (a, _b) = UnixStream::pair().unwrap();
        let pairs: Vec<_> = (0..ipc::MAX_FRAME_FDS + 1).map(|_| pipe_pair()).collect();
        let raw: Vec<RawFd> = pairs.iter().map(|(_, g)| g.as_raw_fd()).collect();
        match write_msg_with_fds(&a, &FromHelper::SessionClosed, &raw).await {
            Err(IpcError::TooManyFds { .. }) => {}
            other => panic!("应为 TooManyFds，实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn 声称带_fd_却没真的附上就报协议错() {
        use std::sync::Arc;
        // 这是最要紧的一条：fd 丢失必须炸得响。若放过去，调用方会拿着
        // 一个没有终端的「终端」继续往下走，故障点离病因隔着好几层。
        let (mut a, b) = UnixStream::pair().unwrap();
        let frame = ipc::encode_with_fds(&FromHelper::SessionClosed, 1).unwrap();
        a.write_all(&frame).await.unwrap();
        drop(a);

        let mut reader = FdFrameReader::new(Arc::new(b));
        match reader.read().await {
            Err(IpcError::Protocol(m)) => assert!(m.contains("实际收到 0"), "错误信息要说清差在哪：{m}"),
            other => panic!("应为 Protocol，实际 {other:?}"),
        }
    }

    /// 多个写者并发往同一条连接上写带 fd 的帧，读端必须一帧不少、一个 fd 不丢。
    ///
    /// 这是终端的真实形态：一个 worker 上同时挂着多个终端与订阅，写侧靠一把锁
    /// 串行化。要防的是两类错误——
    ///
    /// 1. **交错**：两个写者的字节混在一起，读端解出乱码；
    /// 2. **fd 错配**：帧头说 1 个，实际收到 0 个或 2 个（`sendmsg` 只保证写出
    ///    至少一个字节，续写时若把控制消息再带一遍，读端就会多收）。
    ///
    /// 用 `timeout` 包住：这条路径出问题的典型表现是**挂住**而不是断言失败，
    /// 没有超时的话它会把整个测试进程拖死，看起来像「测试跑不完」而不是「有 bug」。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn 并发写带_fd_的帧不交错也不丢_fd() {
        use std::sync::Arc;
        use tokio::sync::Mutex as TokioMutex;

        const WRITERS: usize = 4;
        const PER_WRITER: usize = 25;

        let (a, b) = UnixStream::pair().unwrap();
        let a = Arc::new(a);
        let lock = Arc::new(TokioMutex::new(()));

        let mut writers = Vec::new();
        for w in 0..WRITERS {
            let a = a.clone();
            let lock = lock.clone();
            writers.push(tokio::spawn(async move {
                for i in 0..PER_WRITER {
                    // 一半带 fd、一半不带，混着来才能测出「不带 fd 的帧被算进了
                    // 上一帧的控制消息」这类错误。
                    let (keep, give) = pipe_pair();
                    let with_fd = (w + i) % 2 == 0;
                    let fds: Vec<RawFd> = if with_fd {
                        vec![give.as_raw_fd()]
                    } else {
                        Vec::new()
                    };
                    {
                        let _g = lock.lock().await;
                        write_msg_with_fds(&a, &FromHelper::SessionClosed, &fds)
                            .await
                            .unwrap();
                    }
                    drop(keep);
                    drop(give);
                }
            }));
        }

        let total = WRITERS * PER_WRITER;
        let reader = tokio::spawn(async move {
            let mut reader = FdFrameReader::new(Arc::new(b));
            let mut frames = 0;
            let mut fds = 0;
            while frames < total {
                let (payload, got) = reader.read().await.unwrap().expect("对端不该提前关闭");
                // 解得出来就说明这一帧没被别的写者插进来。
                assert_eq!(
                    ipc::decode::<FromHelper>(&payload).unwrap(),
                    FromHelper::SessionClosed,
                    "第 {frames} 帧解析失败，说明字节交错了"
                );
                fds += got.len();
                frames += 1;
            }
            fds
        });

        for w in writers {
            tokio::time::timeout(std::time::Duration::from_secs(20), w)
                .await
                .expect("写者挂住了")
                .unwrap();
        }
        let fds = tokio::time::timeout(std::time::Duration::from_secs(20), reader)
            .await
            .expect("读者挂住了：帧数没凑齐说明有帧或 fd 丢了")
            .unwrap();

        // 每个写者里 (w+i) 偶数的那些带 fd。
        let expected: usize = (0..WRITERS)
            .map(|w| (0..PER_WRITER).filter(|i| (w + i) % 2 == 0).count())
            .sum();
        assert_eq!(fds, expected, "收到的 fd 总数与发出的不符");
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
