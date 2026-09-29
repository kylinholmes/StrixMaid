//! 每个附件绑定一个已打开文件；控制 RPC 只传元数据，不承载文件字节。
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};

use strixmaid_types::{ApiError, ApiResult, ErrorCode, rpc::FsOpenStreamResult};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};

use crate::session::channel::{Attachment, IpcChannel};

const MAX_STREAMS: usize = 4;
const BUFFER_SIZE: usize = 64 * 1024;
const STALL: Duration = Duration::from_secs(30);

pub(super) struct Streams {
    slots: Arc<Semaphore>,
    // provider 释放时取消所有泵；每次打开前回收已完成任务，
    // 避免任务句柄或文件描述符随请求累积。
    tasks: Mutex<JoinSet<()>>,
}

impl Default for Streams {
    fn default() -> Self {
        Self {
            slots: Arc::new(Semaphore::new(MAX_STREAMS)),
            tasks: Mutex::new(JoinSet::new()),
        }
    }
}

enum Source {
    File(File, Arc<OwnedSemaphorePermit>),
    Preview(Vec<u8>),
}

impl Streams {
    pub(super) async fn open(
        &self,
        path: PathBuf,
        preview: bool,
    ) -> ApiResult<(FsOpenStreamResult, Attachment)> {
        let permit =
            Arc::new(self.slots.clone().try_acquire_owned().map_err(|_| {
                ApiError::new(ErrorCode::Unavailable, "文件读取繁忙（最多 4 条流）")
            })?);
        // 请求取消也不能提前归还仍在阻塞线程里执行的工作所占名额。
        let (file, stat, permit) = tokio::task::spawn_blocking({
            let path = path.clone();
            move || {
                let (file, stat) = open_regular(&path)?;
                Ok::<_, ApiError>((file, stat, permit))
            }
        })
        .await
        .map_err(|e| ApiError::internal("打开文件任务异常").with_detail(e.to_string()))??;
        let mut meta = FsOpenStreamResult {
            size: stat.len(),
            modified_ms: stat
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .and_then(|d| u64::try_from(d.as_millis()).ok()),
            mime: Some(super::mime_of(&path).to_owned()),
            orientation: 1,
        };
        let (source, permit) = if preview && super::thumb::supported(&path) {
            let (rendered, permit) = super::thumb::decode_job(move || {
                let rendered = super::preview::render(file, &path)?;
                Ok((rendered, permit))
            })
            .await?;
            meta.size = rendered.bytes.len() as u64;
            meta.mime = Some(rendered.mime.to_owned());
            (Source::Preview(rendered.bytes), permit)
        } else {
            (Source::File(file, permit.clone()), permit)
        };
        let (channel, attachment) = transfer_pair()
            .await
            .map_err(|e| ApiError::internal("建立文件通道失败").with_detail(e.to_string()))?;
        let size = meta.size;
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _permit = permit;
            // EOF（含 HEAD）、非法范围、取消和无进展超时统一释放
            // 源文件、通道及名额。
            let _ = serve_source(channel, source, size).await;
        });
        Ok((meta, attachment))
    }
}

/// 预检查避免打开已知设备；O_NONBLOCK 防止检查后路径被换成 FIFO 而阻塞。
/// 返回元数据只信任打开句柄上的 fstat。
pub(super) fn open_regular(path: &Path) -> ApiResult<(File, std::fs::Metadata)> {
    #[cfg(windows)]
    reject_device_path(path)?;
    let pre = std::fs::metadata(path).map_err(|e| super::io_err(path, &e))?;
    if !pre.is_file() {
        return Err(ApiError::invalid_request("只允许读取普通文件"));
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = opts.open(path).map_err(|e| super::io_err(path, &e))?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType};
        // SAFETY: file 持有有效句柄；拒绝管道及字符设备。
        if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK {
            return Err(ApiError::invalid_request("只允许读取磁盘普通文件"));
        }
    }
    let stat = file.metadata().map_err(|e| super::io_err(path, &e))?;
    if !stat.is_file() {
        return Err(ApiError::invalid_request("只允许读取普通文件"));
    }
    Ok((file, stat))
}

#[cfg(windows)]
fn reject_device_path(path: &Path) -> ApiResult<()> {
    use std::path::{Component, Prefix};
    // 普通盘符下的 DOS 设备名也能打开字符设备；在 metadata/open 前拒绝，
    // 避免串口等设备的打开操作阻塞。normalize 已拒绝显式设备命名空间。
    for component in path.components() {
        let reserved = match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy().to_ascii_uppercase();
                let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
                matches!(
                    stem,
                    "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
                ) || ["COM", "LPT"].iter().any(|prefix| {
                    stem.strip_prefix(prefix).is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                        )
                    })
                })
            }
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::UNC(_, share) | Prefix::VerbatimUNC(_, share) => {
                    share.to_string_lossy().eq_ignore_ascii_case("pipe")
                }
                Prefix::DeviceNS(_) | Prefix::Verbatim(_) => true,
                _ => false,
            },
            _ => false,
        };
        if reserved {
            return Err(ApiError::invalid_request("不允许读取设备或命名管道"));
        }
    }
    Ok(())
}

async fn transfer_pair() -> io::Result<(IpcChannel, Attachment)> {
    #[cfg(unix)]
    {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        Ok((IpcChannel::from_owned_fd(ours.into())?, theirs.into()))
    }
    #[cfg(windows)]
    {
        // 客户端不能注册进当前运行时的 IOCP；node 接收 OwnedHandle
        // 后只调用一次 from_client_handle 接管。
        IpcChannel::pair_for_transfer("file").await
    }
}

async fn progress<T>(future: impl std::future::Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(STALL, future)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "file stream stalled"))?
}

async fn serve_source(mut channel: IpcChannel, source: Source, size: u64) -> io::Result<()> {
    // 按实际 read 进度重置超时，与输出泵相同；read_exact 无法区分
    // 完全停滞与握手字节仍在缓慢到达。
    let mut handshake = [0; 16];
    let mut filled = 0;
    while filled < handshake.len() {
        let n = progress(channel.read(&mut handshake[filled..])).await?;
        if n == 0 {
            return Ok(());
        }
        filled += n;
    }
    let offset = u64::from_be_bytes(handshake[..8].try_into().unwrap());
    let length = u64::from_be_bytes(handshake[8..].try_into().unwrap());
    if offset > size || length > size - offset {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "range outside opened file",
        ));
    }
    if length == 0 {
        return Ok(());
    }
    let (mut peer, mut sink) = tokio::io::split(channel);
    let mut extra = [0];
    tokio::select! {
        // 握手后不再接受输入；EOF 同时取消待完成的磁盘读取
        // 或因背压等待的通道写入。
        _ = peer.read(&mut extra) => Ok(()),
        result = async {
            match source {
                Source::File(file, permit) => pump_file(file, &mut sink, offset, length, permit).await,
                Source::Preview(bytes) => pump(io::Cursor::new(bytes), &mut sink, offset, length).await,
            }
        } => result,
    }
}

/// 一条流只占一个持续的阻塞读取任务，避免每 64 KiB 重新调度磁盘读取，
/// 在线程池扩张时留下大量分配器 arena（Linux THP 会进一步放大其驻留内存）。
/// 队列与当前块均有界；取消时 receiver 释放，blocking_send 随即退出。
/// 若文件系统读取仍未返回，任务持有的 permit 保证不能提前复用名额。
async fn pump_file<W: AsyncWrite + Unpin>(
    mut file: File,
    sink: &mut W,
    offset: u64,
    mut remaining: u64,
    permit: Arc<OwnedSemaphorePermit>,
) -> io::Result<()> {
    use std::io::{Read, Seek};
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let reader = tokio::task::spawn_blocking(move || -> io::Result<()> {
        let _permit = permit;
        file.seek(io::SeekFrom::Start(offset))?;
        while remaining > 0 && !tx.is_closed() {
            let mut buf = vec![0; remaining.min(BUFFER_SIZE as u64) as usize];
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file shortened during read",
                ));
            }
            remaining -= n as u64;
            buf.truncate(n);
            if tx.blocking_send(buf).is_err() {
                return Ok(());
            }
        }
        Ok(())
    });
    while let Some(buf) = progress(async { Ok(rx.recv().await) }).await? {
        let mut sent = 0;
        while sent < buf.len() {
            let n = progress(sink.write(&buf[sent..])).await?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            sent += n;
        }
    }
    reader.await.map_err(io::Error::other)??;
    progress(sink.shutdown()).await
}

async fn pump<R: AsyncRead + AsyncSeek + Unpin, W: AsyncWrite + Unpin>(
    mut source: R,
    sink: &mut W,
    offset: u64,
    mut remaining: u64,
) -> io::Result<()> {
    progress(source.seek(io::SeekFrom::Start(offset))).await?;
    let mut buf = vec![0; BUFFER_SIZE];
    while remaining != 0 {
        let limit = remaining.min(BUFFER_SIZE as u64) as usize;
        let n = progress(source.read(&mut buf[..limit])).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file shortened during read",
            ));
        }
        let mut sent = 0;
        while sent < n {
            let wrote = progress(sink.write(&buf[sent..n])).await?;
            if wrote == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            sent += wrote;
        }
        remaining -= n as u64;
    }
    progress(sink.shutdown()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::WorkerHandle;
    use std::sync::atomic::{AtomicU64, Ordering};
    use strixmaid_types::rpc::{self, FsOpenStreamParams};

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "strixmaid-stream-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn params(&self, name: &str, preview: bool) -> serde_json::Value {
            serde_json::to_value(FsOpenStreamParams {
                path: self.0.join(name).to_string_lossy().into_owned(),
                allowed_roots: vec![self.0.to_string_lossy().into_owned()],
                preview,
            })
            .unwrap()
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn worker() -> (WorkerHandle, tokio::task::JoinHandle<()>) {
        let (main, worker) = IpcChannel::pair().unwrap();
        let dispatcher = crate::worker::providers::default_dispatcher().await;
        let task = tokio::spawn(async move {
            crate::worker::serve(worker, Arc::new(dispatcher))
                .await
                .unwrap();
        });
        (WorkerHandle::connect(main, -1, None).await.unwrap(), task)
    }
    fn receive(attachment: Attachment) -> IpcChannel {
        #[cfg(unix)]
        {
            IpcChannel::from_owned_fd(attachment).unwrap()
        }
        #[cfg(windows)]
        // SAFETY: 真 worker 交来的未注册 IOCP 的客户端句柄，接收后只接管一次。
        unsafe {
            IpcChannel::from_client_handle(attachment).unwrap()
        }
    }
    async fn open(
        handle: &WorkerHandle,
        params: serde_json::Value,
    ) -> ApiResult<(FsOpenStreamResult, IpcChannel)> {
        let (value, mut attachments) = handle.call_with_fds(rpc::FS_OPEN_STREAM, params).await?;
        assert_eq!(attachments.len(), 1);
        Ok((
            serde_json::from_value(value).unwrap(),
            receive(attachments.pop().unwrap()),
        ))
    }
    async fn read(mut channel: IpcChannel, offset: u64, length: u64) -> Vec<u8> {
        channel.write_u64(offset).await.unwrap();
        channel.write_u64(length).await.unwrap();
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), channel.read_to_end(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        bytes
    }
    async fn finish(handle: WorkerHandle, task: tokio::task::JoinHandle<()>) {
        handle.shutdown().await;
        task.await.unwrap();
    }

    #[cfg(unix)]
    fn source_fds(path: &Path) -> usize {
        use std::os::unix::fs::MetadataExt;
        let target = path.metadata().unwrap();
        let dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
                    return false;
                };
                let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
                // SAFETY: fstat 只借用 fd 并写入有效缓冲；并发关闭时会报 EBADF。
                if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
                    return false;
                }
                // SAFETY: 上面 fstat 已成功初始化结构体。
                let stat = unsafe { stat.assume_init() };
                #[cfg(target_os = "macos")]
                let dev = stat.st_dev as u64;
                #[cfg(not(target_os = "macos"))]
                let dev = stat.st_dev;
                dev == target.dev() && stat.st_ino == target.ino()
            })
            .count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真ipc_超过帧上限的文件完整一致_非图片preview仍走原始流() {
        let temp = Temp::new();
        let bytes: Vec<u8> = (0..3 * 1024 * 1024 + 17).map(|i| (i % 251) as u8).collect();
        std::fs::write(temp.0.join("data.bin"), &bytes).unwrap();
        let (handle, task) = worker().await;
        let (meta, channel) = open(&handle, temp.params("data.bin", true)).await.unwrap();
        assert_eq!(meta.size, bytes.len() as u64);
        assert!(meta.modified_ms.is_some());
        assert_eq!(read(channel, 0, meta.size).await, bytes);
        finish(handle, task).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真ipc_稀疏文件偏移超过4gib_空文件和非法范围() {
        use std::io::{Seek, Write};
        let temp = Temp::new();
        let offset = (1u64 << 32) + 12345;
        let mut file = File::create(temp.0.join("sparse.bin")).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::{IO::DeviceIoControl, Ioctl::FSCTL_SET_SPARSE};
            let mut returned = 0;
            // SAFETY: 有效文件句柄，SET_SPARSE 不需要输入输出缓冲。
            assert_ne!(
                unsafe {
                    DeviceIoControl(
                        file.as_raw_handle(),
                        FSCTL_SET_SPARSE,
                        std::ptr::null(),
                        0,
                        std::ptr::null_mut(),
                        0,
                        &mut returned,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
        }
        file.seek(io::SeekFrom::Start(offset)).unwrap();
        file.write_all(b"range-over-four-gib").unwrap();
        drop(file);
        std::fs::write(temp.0.join("empty"), []).unwrap();
        let (handle, task) = worker().await;
        let (meta, channel) = open(&handle, temp.params("sparse.bin", false))
            .await
            .unwrap();
        assert_eq!(
            read(channel, offset, meta.size - offset).await,
            b"range-over-four-gib"
        );
        let (_, channel) = open(&handle, temp.params("sparse.bin", false))
            .await
            .unwrap();
        assert!(read(channel, u64::MAX, 2).await.is_empty());
        let (meta, channel) = open(&handle, temp.params("empty", false)).await.unwrap();
        assert_eq!(meta.size, 0);
        assert!(read(channel, 0, 0).await.is_empty());
        finish(handle, task).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真ipc_四流背压不堵控制请求_取消及head归还名额() {
        let temp = Temp::new();
        File::create(temp.0.join("big"))
            .unwrap()
            .set_len(128 * 1024 * 1024)
            .unwrap();
        let (handle, task) = worker().await;
        let mut streams = Vec::new();
        for _ in 0..4 {
            let (meta, mut channel) = open(&handle, temp.params("big", false)).await.unwrap();
            channel.write_u64(0).await.unwrap();
            channel.write_u64(meta.size).await.unwrap();
            streams.push(channel);
        }
        #[cfg(unix)]
        assert_eq!(source_fds(&temp.0.join("big")), 4);
        assert_eq!(
            open(&handle, temp.params("big", false))
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::Unavailable
        );
        let start = std::time::Instant::now();
        tokio::time::timeout(
            Duration::from_secs(1),
            handle.call(rpc::FS_LIST, temp.params(".", false)),
        )
        .await
        .unwrap()
        .unwrap();
        eprintln!("四流背压时控制请求：{:?}", start.elapsed());
        streams.clear();
        #[cfg(unix)]
        {
            tokio::time::timeout(Duration::from_secs(2), async {
                while source_fds(&temp.0.join("big")) != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            eprintln!("真 IPC 取消后源文件 fd：4 → 0");
        }
        // 名额必须在取消后及时归还，不能等 30 秒兜底超时。
        for _ in 0..8 {
            let (_, channel) = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    match open(&handle, temp.params("big", false)).await {
                        Ok(v) => break v,
                        Err(e) if e.code == ErrorCode::Unavailable => {
                            tokio::task::yield_now().await
                        }
                        Err(e) => panic!("{e:?}"),
                    }
                }
            })
            .await
            .unwrap();
            // HEAD：不写握手，直接丢弃附件。
            drop(channel);
        }
        finish(handle, task).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真ipc_拒绝fifo设备socket目录且不阻塞() {
        let temp = Temp::new();
        nix::unistd::mkfifo(&temp.0.join("fifo"), nix::sys::stat::Mode::S_IRUSR).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(temp.0.join("socket")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", temp.0.join("device")).unwrap();
        let (handle, task) = worker().await;
        for name in ["fifo", "socket", "device", "."] {
            let error = tokio::time::timeout(
                Duration::from_secs(1),
                open(&handle, temp.params(name, false)),
            )
            .await
            .unwrap()
            .err()
            .unwrap();
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{name}: {error:?}");
        }
        finish(handle, task).await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真ipc_路径替换仍读已打开句柄_缩短文件显式短读() {
        let temp = Temp::new();
        let path = temp.0.join("data");
        std::fs::write(&path, b"original").unwrap();
        let (handle, task) = worker().await;
        let (meta, channel) = open(&handle, temp.params("data", false)).await.unwrap();
        std::fs::rename(&path, temp.0.join("old")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert_eq!(read(channel, 0, meta.size).await, b"original");
        let (meta, channel) = open(&handle, temp.params("data", false)).await.unwrap();
        File::create(&path).unwrap();
        assert!(meta.size > 0);
        assert!(read(channel, 0, meta.size).await.is_empty());
        finish(handle, task).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn 真ipc_图片preview转为1600档而下载保留原始字节() {
        let temp = Temp::new();
        let img = image::RgbImage::from_pixel(2000, 1000, image::Rgb([50, 110, 190]));
        img.save(temp.0.join("picture.png")).unwrap();
        let (handle, task) = worker().await;
        let (meta, channel) = open(&handle, temp.params("picture.png", true))
            .await
            .unwrap();
        assert_eq!(meta.orientation, 1);
        assert_eq!(meta.mime.as_deref(), Some("image/jpeg"));
        let output = read(channel, 0, meta.size).await;
        let decoded = image::load_from_memory(&output).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (1600, 800));
        let (meta, channel) = open(&handle, temp.params("picture.png", false))
            .await
            .unwrap();
        assert_eq!(
            read(channel, 0, meta.size).await,
            std::fs::read(temp.0.join("picture.png")).unwrap()
        );
        finish(handle, task).await;
    }

    #[tokio::test(start_paused = true)]
    async fn 等待握手与输出背压均有30秒无进展超时() {
        let (ours, theirs) = IpcChannel::pair().unwrap();
        let task = tokio::spawn(serve_source(ours, Source::Preview(vec![1]), 1));
        tokio::task::yield_now().await;
        tokio::time::advance(STALL + Duration::from_secs(1)).await;
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        drop(theirs);

        let (ours, mut theirs) = IpcChannel::pair().unwrap();
        let task = tokio::spawn(serve_source(
            ours,
            Source::Preview(vec![1; 16 * 1024 * 1024]),
            16 * 1024 * 1024,
        ));
        theirs.write_u64(0).await.unwrap();
        theirs.write_u64(16 * 1024 * 1024).await.unwrap();
        // 先等第一个字节，证明握手完成、数据泵已经启动。
        theirs.read_u8().await.unwrap();
        assert_eq!(
            task.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn 持续进展可以超过30秒总时长() {
        let (mut tx, mut rx) = tokio::io::duplex(1);
        let task =
            tokio::spawn(async move { pump(io::Cursor::new(vec![7; 8]), &mut tx, 0, 8).await });
        for _ in 0..8 {
            assert_eq!(rx.read_u8().await.unwrap(), 7);
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn provider释放取消所有附件任务() {
        let temp = Temp::new();
        std::fs::write(temp.0.join("data"), b"hello").unwrap();
        let provider = super::super::FsProvider::new();
        let (_, attachment) = provider
            .open_stream(serde_json::from_value(temp.params("data", false)).unwrap())
            .await
            .unwrap();
        let slots = provider.streams.slots.clone();
        assert_eq!(slots.available_permits(), 3);
        let mut channel = receive(attachment);
        drop(provider);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), channel.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert_eq!(slots.available_permits(), 4);
    }

    #[cfg(windows)]
    #[test]
    fn windows设备路径在打开前拒绝() {
        for path in [
            r"C:\NUL",
            r"C:\CON.txt",
            r"C:\folder\COM1",
            r"C:\LPT².log",
            r"\\server\pipe\blocked",
        ] {
            assert!(reject_device_path(Path::new(path)).is_err(), "{path}");
        }
        assert!(reject_device_path(Path::new(r"C:\folder\report.txt")).is_ok());
    }
}
