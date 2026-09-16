//! 以认证到的身份拉起 `strixmaid worker`，并回收退出的 worker。
//!
//! 两个平台的机制毫无共同之处，但对 `main.rs` 暴露同一组函数，
//! 状态机那边因此不需要 `cfg`。
//!
//! # 一环一环的对应关系
//!
//! | 这一步做什么 | Unix（[`unix`]） | Windows（[`windows`]） |
//! |---|---|---|
//! | 建 worker 通道 | `socketpair` | 一条随机名的命名管道（服务端留给主进程、客户端交给 worker） |
//! | 把通道交给 worker | `fork` 后 `dup2` 到 fd 3 | 句柄标成可继承，值写进命令行 `--ipc-handle` |
//! | 只交这一个、不多交 | `CLOEXEC`（默认不继承，指定的才留） | `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`（默认全继承，指定的才留） |
//! | 切换身份 | `initgroups` → `setgid` → `setuid` | 拿着登录令牌 `CreateProcessAsUserW` |
//! | 提权（`as_root`） | 不切换身份，保持 root | UAC 的 linked token（完整管理员令牌） |
//! | 环境 | 手搭一份 + `pam_getenvlist` 覆盖 | `CreateEnvironmentBlock` + 会话环境覆盖 |
//! | 回收 | `waitpid(WNOHANG)` | `WaitForSingleObject(h, 0)` + `GetExitCodeProcess` |
//!
//! 最后一行是唯一渗到类型上的差异：`waitpid` 只要 pid，`GetExitCodeProcess`
//! 要一个**进程句柄**（pid 会被复用，句柄不会）。所以 helper 记的不是裸 pid 而是
//! [`WorkerProc`]，Windows 上它额外持有那个句柄。
//!
//! # `WorkerSpec` 为什么在这里又定义了一遍
//!
//! [`unix`] 里那份是原样搬过来的，字段类型是 `nix` 的 `Uid` / `Gid`。
//! 本模块这份用裸 `u32`，让 `main.rs` 不必引 `nix`。两者之间的转换只在
//! [`spawn_worker`] 的 Unix 分支里发生一次，Unix 那份代码因此一个字都没动
//! ——本机编译不了 Linux 目标，改不能验证的代码是不划算的。

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

use std::path::{Path, PathBuf};

use crate::auth::Session;
use crate::ipc::WorkerChannel;

/// 拉起 worker 时的目标身份与环境。
pub struct WorkerSpec {
    /// 用户名。Unix 用于 `initgroups` 与 `USER` / `LOGNAME`；
    /// Windows 上只用于日志（身份完全由令牌决定）。
    pub username: String,
    /// 目标 uid。`as_root` 时 Unix 侧忽略。
    pub uid: u32,
    /// 目标 gid。`as_root` 时 Unix 侧忽略。
    pub gid: u32,
    /// 家目录：worker 的工作目录。为空表示「不指定」。
    pub home: PathBuf,
    /// 登录 shell（Unix 的 `SHELL` / Windows 的 `%ComSpec%`）。
    pub shell: PathBuf,
    /// 为 `true` 时以最高权限运行 worker：Unix 上是不切换身份（保持 root），
    /// Windows 上是换用 UAC 的完整管理员令牌。
    pub as_root: bool,
    /// 额外环境变量，覆盖同名的基础变量。
    pub extra_env: Vec<(String, String)>,
}

/// 一个已拉起的 worker。
///
/// Unix 上只需要 pid（`waitpid` 认 pid）；Windows 上必须额外攥着进程句柄
/// ——`GetExitCodeProcess` 要句柄，而且只要句柄还开着，内核就不会把这个 pid
/// 分给别人，「等错了进程」这种事在源头上就不可能发生。
pub struct WorkerProc {
    /// 进程 id。进 `WorkerSpawned.pid`，主进程用它在登出时终止 worker。
    pub(crate) pid: i32,
    /// 进程句柄，见结构体文档。
    #[cfg(windows)]
    pub(crate) handle: std::os::windows::io::OwnedHandle,
}

impl WorkerProc {
    /// 进程 id。
    pub fn pid(&self) -> i32 {
        self.pid
    }
}

/// 拉起一个 worker。返回 `(进程, 交给主进程的那半条通道)`。
///
/// `session` 在 Unix 上用不上（身份由 uid / gid 决定，认证句柄与拉进程无关），
/// 在 Windows 上却是**唯一**的身份来源——那边没有 `setuid`，只有
/// 「拿着这张令牌去创建进程」。参数因此出现在两个平台的签名里。
#[cfg(unix)]
pub fn spawn_worker(
    exe: &Path,
    spec: &WorkerSpec,
    _session: &Session,
) -> Result<(WorkerProc, WorkerChannel), String> {
    let native = unix::WorkerSpec {
        username: spec.username.clone(),
        uid: nix::unistd::Uid::from_raw(spec.uid),
        gid: nix::unistd::Gid::from_raw(spec.gid),
        home: spec.home.clone(),
        shell: spec.shell.clone(),
        as_root: spec.as_root,
        extra_env: spec.extra_env.clone(),
    };
    let (pid, channel) = unix::spawn_worker(exe, &native)?;
    Ok((
        WorkerProc {
            pid: pid.as_raw(),
        },
        channel,
    ))
}

/// 见 Unix 版。
#[cfg(windows)]
pub fn spawn_worker(
    exe: &Path,
    spec: &WorkerSpec,
    session: &Session,
) -> Result<(WorkerProc, WorkerChannel), String> {
    windows::spawn_worker(exe, spec, session)
}

/// 非阻塞回收已退出的 worker，见[模块文档](self)的对照表最后一行。
///
/// 只再导出这一个：`describe_exit` 两个平台都有、形状也一样，但它只被各自的
/// `reap_workers` 用到，没有跨模块的调用方。
#[cfg(unix)]
pub use unix::reap_workers;
/// 见 Unix 版。
#[cfg(windows)]
pub use windows::reap_workers;
