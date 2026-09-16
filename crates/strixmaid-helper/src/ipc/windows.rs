//! Windows 侧：主进程建的命名管道，同步读写。
//!
//! 接口形状与 Unix 侧一致，背景见 [父模块文档](super)。
//!
//! # 「确实是主进程给的」怎么保证
//!
//! Unix 上 fd 3 是继承来的，别的进程拿不到；命名管道**有名字**，任何能访问该
//! 名字的进程都可以来连。这里靠两件事把冒名顶替堵死：
//!
//! 1. 名字是 128 位随机的，只经命令行传给本进程；
//! 2. 主进程在 `accept` 之后会核对客户端 pid 必须等于它刚 spawn 的那个
//!    （`GetNamedPipeClientProcessId`，见 `strixmaid_core::session::channel`）。
//!
//! 第 2 条是关键：即使名字泄漏，冒充者的 pid 也对不上，连接会被主进程拒绝。
//! 本进程这一侧不需要再验证什么——管道是主进程建的，能连上就说明对端是它。
//!
//! # 同步 I/O
//!
//! 管道用 `CreateFileW` 开，**不带** `FILE_FLAG_OVERLAPPED`：helper 是单线程
//! 同步程序，重叠 I/O 只会凭空多一套完成端口的管理。主进程那一端是
//! `FILE_FLAG_OVERLAPPED` 的（它跑在 tokio 上），两端的模式互不影响——
//! 重叠与否是**打开句柄时**的属性，不是管道本身的。

use std::io::Write;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use strixmaid_types::ipc::{self, FromHelper, IpcError, IpcResult, ToHelper};
use windows_sys::Win32::Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_GENERIC_WRITE, FILE_SHARE_NONE, OPEN_EXISTING,
};

/// 交给主进程的那半条 worker 通道。
///
/// 与 Unix 的 `OwnedFd` 对应。注意所有权语义不同：这个句柄**不由本进程关闭**，
/// 主进程会用 `DUPLICATE_CLOSE_SOURCE` 把它连人带所有权一起搬走，
/// 见 [`Ipc::finish_handover`]。
pub type WorkerChannel = OwnedHandle;

/// 与主进程的通道。
pub struct Ipc {
    stream: std::fs::File,
}

impl Ipc {
    /// 连上主进程建好的命名管道。
    ///
    /// 用 `std::fs::File` 承载句柄：`Read` / `Write` 直接可用，而
    /// `strixmaid_types::ipc` 的同步读写正是泛型于这两个 trait 的。
    /// 命名管道在 Win32 里本来就是文件对象，这不是取巧。
    pub fn connect(name: &str) -> Result<Ipc, String> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: wide 以 NUL 结尾；其余参数为常量。失败返回
        // INVALID_HANDLE_VALUE，下面立即检查。
        let raw = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | FILE_GENERIC_WRITE,
                FILE_SHARE_NONE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if raw == INVALID_HANDLE_VALUE || raw.is_null() {
            return Err(format!(
                "连接 IPC 管道 {name} 失败：{}；helper 只能由 strixmaid 主进程拉起",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: raw 刚由 CreateFileW 成功返回，本进程独占。
        let handle = unsafe { OwnedHandle::from_raw_handle(raw.cast()) };
        Ok(Ipc {
            stream: std::fs::File::from(handle),
        })
    }

    /// 读一条消息；主进程关闭通道时返回 `Ok(None)`。
    pub fn recv(&mut self) -> IpcResult<Option<ToHelper>> {
        match ipc::read_msg(&mut self.stream) {
            // 主进程退出时，命名管道给的是 ERROR_BROKEN_PIPE 而不是「读到 0 字节」。
            // 在协议层这与干净 EOF 是同一件事：没有更多消息了。
            Err(IpcError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(None),
            other => other,
        }
    }

    /// 写一条消息。
    pub fn send(&mut self, msg: &FromHelper) -> IpcResult<()> {
        ipc::write_msg(&mut self.stream, msg)?;
        self.stream.flush().map_err(IpcError::Io)
    }

    /// 发一条消息并阻塞等下一条——认证的一轮 challenge-response 用它。
    pub fn send_and_wait(&mut self, msg: FromHelper) -> IpcResult<Option<ToHelper>> {
        self.send(&msg)?;
        self.recv()
    }

    /// 交接的第一步：给出要写进 `WorkerSpawned.worker_handle` 的句柄值。
    ///
    /// 只是读出句柄的数值，此刻什么都没发生——真正的搬运由主进程发起。
    pub fn prepare_handover(ch: &WorkerChannel) -> Option<u64> {
        use std::os::windows::io::AsRawHandle as _;
        Some(ch.as_raw_handle() as usize as u64)
    }

    /// 交接的第二步。Windows 上无需再发什么，但**必须放弃这个句柄的所有权**。
    ///
    /// 主进程用 `DUPLICATE_CLOSE_SOURCE` 取走它时，内核已经把本进程这一份关掉了。
    /// 这里若正常 drop，`CloseHandle` 会作用在一个可能已被复用的句柄值上——
    /// 那会关掉一个完全无关的内核对象。所以 `forget`。
    ///
    /// 主进程一直不来取时会漏一个句柄。那只发生在主进程已经消失的情况下，
    /// 而那时 helper 自己也会因为通道断开而退出，句柄随进程一起回收。
    pub fn finish_handover(&mut self, ch: WorkerChannel) -> IpcResult<()> {
        std::mem::forget(ch);
        Ok(())
    }
}
