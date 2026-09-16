//! 信号与优先级的 POSIX 实现（Linux 与 macOS 共用）。
//!
//! 这两个是 process provider 里仅有的**写**操作，也是仅有的「内核直接裁决权限」
//! 的地方：`EPERM` 一律映射成 `PermissionDenied` + `can_retry_elevated`，
//! 由上层 `auth::exec::call_escalating_from` 决定要不要换 admin worker 重试
//! （`roadmap/01-worker-execution.md` §4.1）。
//!
//! 内容与平台化改造之前逐字相同——它原本内联在 `providers/process/mod.rs` 里，
//! 为给 Windows 的同名实现让出并列位置才挪进本文件。

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use strixmaid_types::process::SignalName;
use strixmaid_types::{ApiError, ApiResult};

/// `kill(2)`。
///
/// `pid` 只用于错误信息（对外的 u32 形式），实际操作用 `raw_pid`。
pub fn send_signal(pid: u32, raw_pid: i32, signal: SignalName) -> ApiResult<()> {
    let sig = match signal {
        SignalName::Term => Signal::SIGTERM,
        SignalName::Kill => Signal::SIGKILL,
        SignalName::Hup => Signal::SIGHUP,
    };
    kill(Pid::from_raw(raw_pid), sig).map_err(|e| match e {
        Errno::ESRCH => ApiError::not_found(format!("进程 {pid} 不存在")),
        Errno::EPERM => ApiError::permission_denied(format!(
            "内核拒绝向进程 {pid} 发送 {sig}：不是该进程的属主"
        ))
        .with_detail(e.to_string())
        .retry_elevated(),
        other => ApiError::internal(format!("向进程 {pid} 发送 {sig} 失败"))
            .with_detail(other.to_string()),
    })
}

/// `setpriority(2)`。
pub fn set_nice(pid: u32, raw_pid: i32, nice: i32) -> ApiResult<()> {
    // SAFETY: setpriority 只读参数，无内存副作用。
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, raw_pid as libc::id_t, nice) };
    if rc == 0 {
        return Ok(());
    }
    let e = std::io::Error::last_os_error();
    Err(match e.raw_os_error() {
        Some(libc::ESRCH) => ApiError::not_found(format!("进程 {pid} 不存在")),
        Some(libc::EACCES) | Some(libc::EPERM) => ApiError::permission_denied(format!(
            "内核拒绝调整进程 {pid} 的优先级：调低 nice 值（提高优先级）需要 root，且只能操作自己的进程"
        ))
        .with_detail(e.to_string())
        .retry_elevated(),
        _ => ApiError::internal(format!("调整进程 {pid} 优先级失败")).with_detail(e.to_string()),
    })
}
