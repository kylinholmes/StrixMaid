//! helper 与主进程之间的通道。同步读写——helper 是单线程的，阻塞读就是它的事件循环。
//!
//! # 两个平台怎么拿到这条通道
//!
//! | | Unix（[`unix`]） | Windows（[`windows`]） |
//! |---|---|---|
//! | 实体 | `socketpair` 的一端，主进程 `dup2` 到 fd 3 | 一条命名管道，名字由 `--pipe` 给出 |
//! | 「确实是主进程给的」 | fd 3 是继承来的，且 `fstat` 确认是 socket | 名字是 128 位随机的，主进程还会核对本进程 pid |
//! | 交出 worker 通道 | `WorkerSpawned` 之后**再发一帧** `SCM_RIGHTS` | 句柄值写在 `WorkerSpawned` 里，主进程自己来取 |
//!
//! 第三行的差异渗到了调用次序上，所以交接拆成两步：
//! [`Ipc::prepare_handover`]（在发 `WorkerSpawned` **之前**，拿到要写进帧里的值）
//! 与 [`Ipc::finish_handover`]（在**之后**，补那一帧 `SCM_RIGHTS`）。
//! 两个平台各只用到其中一步，另一步是无操作——把两步都写出来，
//! 调用点就不需要 `cfg` 了。

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::{Ipc, WorkerChannel};
#[cfg(windows)]
pub use windows::{Ipc, WorkerChannel};
