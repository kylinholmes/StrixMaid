//! worker 侧的 PTY：`term.open` / `term.resize` / `term.close`
//! （`roadmap/03-terminal.md` §4.1 §4.2 §4.5）。
//!
//! # 为什么 PTY 必须在 worker 里
//!
//! `design.md` §2.2：终端里跑的 shell 就是登录用户本人。worker 是 helper
//! `setuid` 之后 exec 出来的，它 fork 出的 shell 天然继承那个 uid——
//! 内核来裁决这个 shell 能干什么，服务端**一行授权代码都不用写**。
//! 反过来，如果 PTY 开在主进程（root）里，每一次读写文件、每一次发信号都得由
//! 我们自己去判断「该不该」，那正是 `design.md` §5.1 要避免的自建鉴权。
//!
//! # 数据通路：为什么是 fd 而不是 JSON
//!
//! ```text
//! 主进程 ⇄ socketpair ⇄ worker（两个泵）⇄ PTY master ⇄ shell
//! ```
//!
//! 终端是字节流，塞进 RPC 的 JSON 帧要付 base64 与转义的代价，还会和别的
//! RPC 抢同一条 socket 的写锁（`worker::FrameWriter` 是串行的）——一个
//! `cat 大文件` 就能把整个会话的控制面堵住。所以 `term.open` 只用 RPC 回一个
//! **fd**（`Dispatcher::register_fd` + `SCM_RIGHTS`），此后终端字节走它自己那条
//! socketpair，与控制面彻底分开。
//!
//! # 背压
//!
//! 两个泵都是「读一块、写完再读下一块」，中间没有任何无界缓冲。主进程读得慢
//! → socketpair 缓冲满 → 泵卡在写上 → 不再读 PTY master → PTY 缓冲满 →
//! shell 自己被内核挡住。这一路顶回去正是想要的：宁可让 `yes` 慢下来，
//! 也不要在 worker 里堆几百 MB 的终端输出。
//!
//! # 身份：worker 不判断「该不该」
//!
//! `TermOpenParams::user` 只有 admin worker（`getuid() == 0`）会用；user worker
//! **忽略**它——它被内核锁死在自己的 uid 上，`user` 写什么都只能是自己。
//! 「这个会话能不能开别人的终端」由主进程按 `session.elevated` 决定
//! （`roadmap/03-terminal.md` §4.2：未提权 → 403，根本不会派到 admin worker），
//! worker 这里不复核。复核会带来两套判断规则，而两套规则迟早会不一致。

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use nix::sys::signal::{Signal, killpg};
use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use nix::unistd::{Pid, User, getuid};
use portable_pty::{MasterPty, PtySize, native_pty_system};
use serde::de::DeserializeOwned;
use serde_json::Value;
use strixmaid_types::rpc::{
    self, TermCloseParams, TermCloseResult, TermExit, TermOpenParams, TermOpenResult,
    TermResizeParams,
};
use strixmaid_types::{ApiError, ApiResult};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::Dispatcher;
use super::spawn_as::{ExecSpec, Identity, PreparedExec, describe_exit};

/// 允许作为 shell 的白名单文件。Linux 与 macOS 都有。
const SHELLS_FILE: &str = "/etc/shells";

/// 解析不出登录 shell 时的兜底。
const FALLBACK_SHELL: &str = "/bin/sh";

/// 没有从环境继承到 PATH 时的兜底（与 helper 的 `spawn.rs` 一致）。
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// 泵一次搬运的字节上限。
const PUMP_BUF: usize = 16 * 1024;

/// `term.close` 里等 shell 咽气的宽限期，超时就 `SIGKILL`。
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// shell 自行退出后，条目带着退出状态在表里保留多久，等主进程的 `term.close` 来取。
///
/// 主进程在 socketpair 读到 EOF 后总会补一次 `term.close`（`terminal/mod.rs` 的
/// `shutdown`），正常情况下几毫秒内就来取走了；这个上限只兜「主进程一直不来」的
/// 异常路径，防止条目在表里永久滞留。
const REAPED_LINGER: Duration = Duration::from_secs(30);

// ===========================================================================
// 终端表
// ===========================================================================

/// 一个正在跑的终端。
///
/// `master` 必须一直留着：`term.resize` 要拿它做 `TIOCSWINSZ`，而且**它一关，
/// 内核就会给整个会话发 `SIGHUP`**——这既是关终端的手段，也意味着不能随手 drop。
struct Terminal {
    /// shell 的 pid。子进程调过 `setsid`，所以它同时是 pgid。
    pid: Pid,
    master: Box<dyn MasterPty + Send>,
    /// 两个方向的泵。
    pumps: Vec<JoinHandle<()>>,
    /// 收尸任务，结束时给出 shell 的退出状态。`term.close` 会 `take` 走它并等它
    /// 结束——既确认 shell 真的没了，也取回退出状态。
    reaper: Option<JoinHandle<Option<TermExit>>>,
    /// 已经在 `close` 里处理过（信号发过、尸也收过）。
    ///
    /// 它存在只为一件事：**避免对回收后的 pid 再发一次信号**。pid 是会被系统
    /// 重用的，对一个已经不属于我们的进程组发 `SIGHUP` 就是误伤别人。
    closed: bool,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        for h in &self.pumps {
            h.abort();
        }
        if self.closed {
            return;
        }
        // 走到这里说明表被整个丢掉了（worker 退出、dispatcher 析构），
        // 而不是走的 `close`。此时 shell 还活着，且我们即将 drop 掉 master——
        // 关 master 本身就会让内核发 SIGHUP，这里显式补一发是为了覆盖
        // 「shell 把自己从控制终端上摘出去了」的情况（`setsid` 过的后台进程）。
        if let Err(e) = killpg(self.pid, Signal::SIGHUP) {
            tracing::debug!(pid = %self.pid, error = %e, "析构时 SIGHUP 未送达");
        }
    }
}

/// worker 内所有终端的登记表：`pid → Terminal`。
///
/// # 为什么 `resize` / `close` 必须查这张表
///
/// 参数里的 `pid` 来自主进程，而主进程的 `id → pid` 映射来自更早的一次
/// `term.open`。如果不查表就直接拿 pid 去 `kill`，那么**任何一个能发 RPC 的
/// 调用方都能对本机任意进程发 SIGHUP**——worker 是以登录用户身份跑的，
/// 这等于白送一个「杀死该用户全部进程」的接口。查表把作用域限制成
/// 「本 worker 亲手开过的终端」。
#[derive(Clone)]
pub struct TerminalTable {
    inner: Arc<Mutex<HashMap<u32, Terminal>>>,
    /// 见 [`REAPED_LINGER`]。做成字段只为让测试能把它缩短到毫秒级。
    linger: Duration,
}

impl Default for TerminalTable {
    fn default() -> Self {
        Self::with_linger(REAPED_LINGER)
    }
}

impl TerminalTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定收尸后的保留时长（仅测试需要缩短它）。
    fn with_linger(linger: Duration) -> Self {
        TerminalTable {
            inner: Arc::default(),
            linger,
        }
    }

    /// 当前登记的终端数。
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }

    /// 表里是不是空的。
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// 开一个 PTY，返回结果与**主进程侧的 socketpair 一端**。
    pub async fn open(&self, params: TermOpenParams) -> ApiResult<(TermOpenResult, OwnedFd)> {
        if params.cols == 0 || params.rows == 0 {
            return Err(ApiError::invalid_request(format!(
                "终端尺寸必须为正，收到 {}x{}",
                params.cols, params.rows
            )));
        }

        let target = resolve_target(params.user.as_deref())?;
        let shell = resolve_shell(params.shell.as_deref(), &target)?;
        // 切身份 = 「我是 root，且目标不是我自己」。user worker 永远走 None 分支。
        let identity = (target.uid != getuid()).then(|| Identity {
            username: target.name.clone(),
            uid: target.uid,
            gid: target.gid,
        });

        let pty = native_pty_system()
            .openpty(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ApiError::internal("打开 PTY 失败").with_detail(e.to_string()))?;

        // 从设备要自己再开一份：`PtyPair::slave` 没有暴露 fd（`SlavePty` 只有
        // `spawn_command`，而它不会 setuid），而我们需要一个能 `dup2` 的裸 fd。
        // 拿到自己的副本后立刻丢掉 portable-pty 那一份——**只要还有任何一个
        // 从设备 fd 开着，shell 退出时主设备就读不到 EOF**，主进程会一直以为
        // 终端还活着。
        let tty_path = pty
            .master
            .tty_name()
            .ok_or_else(|| ApiError::internal("无法取得 PTY 从设备路径"))?;
        let slave = nix::fcntl::open(
            &tty_path,
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|e| {
            ApiError::internal(format!("打开 PTY 从设备 {} 失败", tty_path.display()))
                .with_detail(e.to_string())
        })?;
        drop(pty.slave);

        // 换身份时把 tty 过户给目标用户。openpty 把它设成了 worker 自己的
        // （admin worker 就是 root），不改的话目标用户**打不开 /dev/tty**——
        // sudo、ssh、less 这些要重开控制终端的程序会直接报错。login(1) 也是这么做的。
        if let Some(id) = &identity {
            nix::unistd::chown(&tty_path, Some(id.uid), None).map_err(|e| {
                ApiError::internal(format!("把 {} 过户给 {} 失败", tty_path.display(), id.username))
                    .with_detail(e.to_string())
            })?;
        }

        let env = build_env(&target, &shell, identity.is_some());
        let argv0 = login_argv0(&shell);
        let prepared = PreparedExec::new(&ExecSpec {
            program: &shell,
            argv0: &argv0,
            env: &env,
            cwd: &target.dir,
            identity: identity.as_ref(),
        })
        .map_err(|e| ApiError::invalid_request(format!("无法准备 shell：{e}")))?;

        let (main_side, worker_side) = make_socketpair()?;

        let pid = prepared
            .spawn_on_tty(slave.as_fd())
            .map_err(|e| ApiError::internal("启动 shell 失败").with_detail(e))?;
        // 父进程这边不再需要从设备（见上：留着它 = 永远收不到 EOF）。
        drop(slave);

        let master_fd = pty
            .master
            .as_raw_fd()
            .ok_or_else(|| ApiError::internal("PTY 主设备没有可用的 fd"))?;
        let master_io = async_master(master_fd)
            .map_err(|e| ApiError::internal("PTY 主设备无法异步化").with_detail(e.to_string()))?;

        let sock = to_tokio_stream(worker_side)
            .map_err(|e| ApiError::internal("socketpair 无法异步化").with_detail(e.to_string()))?;
        let (sock_rx, sock_tx) = sock.into_split();

        let master_io = Arc::new(master_io);
        let pumps = vec![
            tokio::spawn(pump_master_to_socket(master_io.clone(), sock_tx, pid)),
            tokio::spawn(pump_socket_to_master(master_io, sock_rx, pid)),
        ];

        // 收尸任务持 `Weak`：强引用会让「表」和「任务」互相钉住，
        // dispatcher 析构时表就不会被 drop，`Terminal::drop` 里的 SIGHUP 也就
        // 永远不会发——worker 退出后 shell 变孤儿。
        let weak = Arc::downgrade(&self.inner);
        let reaper = tokio::spawn(reap(weak, pid, self.linger));

        let result = TermOpenResult {
            pid: pid.as_raw() as u32,
            shell: shell.to_string_lossy().into_owned(),
            user: target.name.clone(),
            uid: target.uid.as_raw(),
        };
        self.inner.lock().await.insert(
            result.pid,
            Terminal {
                pid,
                master: pty.master,
                pumps,
                reaper: Some(reaper),
                closed: false,
            },
        );
        tracing::info!(
            pid = result.pid,
            user = %result.user,
            shell = %result.shell,
            cols = params.cols,
            rows = params.rows,
            "终端已开"
        );
        Ok((result, main_side))
    }

    /// 改窗口大小（`TIOCSWINSZ`），内核随即给前台进程组发 `SIGWINCH`。
    pub async fn resize(&self, params: TermResizeParams) -> ApiResult<()> {
        if params.cols == 0 || params.rows == 0 {
            return Err(ApiError::invalid_request(format!(
                "终端尺寸必须为正，收到 {}x{}",
                params.cols, params.rows
            )));
        }
        let table = self.inner.lock().await;
        let term = table.get(&params.pid).ok_or_else(|| unknown(params.pid))?;
        term.master
            .resize(PtySize {
                rows: params.rows,
                cols: params.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| ApiError::internal("改终端尺寸失败").with_detail(e.to_string()))
    }

    /// 关一个终端：对进程组发 `SIGHUP`，等它咽气，回收 PTY 与两个泵。
    ///
    /// 返回 shell 的退出状态（若能取到）。shell 已自行退出时（条目被收尸任务
    /// 标记为 `closed` 后保留在表里），**不再发任何信号**——pid 可能已被系统
    /// 重用——只取走退出状态并清掉条目。
    pub async fn close(&self, params: TermCloseParams) -> ApiResult<TermCloseResult> {
        // **先摘表再等**。反过来（持锁等收尸）必然死锁：收尸任务结束时要拿同一把锁
        // 去标记自己的条目。
        let mut term = {
            let mut table = self.inner.lock().await;
            table.remove(&params.pid).ok_or_else(|| unknown(params.pid))?
        };
        let already_reaped = term.closed;
        term.closed = true;

        if !already_reaped
            && let Err(e) = killpg(term.pid, Signal::SIGHUP)
        {
            tracing::debug!(pid = params.pid, error = %e, "SIGHUP 未送达，进程可能已退出");
        }
        let mut exit = None;
        if let Some(mut reaper) = term.reaper.take() {
            // 等到「尸也收了」为止，而不只是「信号发出去了」。没收尸的进程仍然
            // 是个僵尸，`kill(pid, 0)` 依旧返回成功，主进程会以为终端还在。
            match tokio::time::timeout(CLOSE_GRACE, &mut reaper).await {
                Ok(res) => exit = res.ok().flatten(),
                Err(_) => {
                    tracing::warn!(pid = params.pid, "SIGHUP 后仍未退出，改用 SIGKILL");
                    let _ = killpg(term.pid, Signal::SIGKILL);
                    if let Ok(res) = tokio::time::timeout(CLOSE_GRACE, &mut reaper).await {
                        exit = res.ok().flatten();
                    }
                }
            }
        }
        tracing::info!(pid = params.pid, exit = ?exit, "终端已关");
        Ok(TermCloseResult { exit })
    }
}

fn unknown(pid: u32) -> ApiError {
    ApiError::not_found(format!("本 worker 没有 pid 为 {pid} 的终端"))
}

/// 等 shell 退出并回收它，返回退出状态。
///
/// 必须有人 `waitpid`：worker 是长命进程，不收尸就会攒一堆僵尸，而且
/// 僵尸占着 pid，`kill(pid, 0)` 仍然成功，主进程判断不出终端已经没了。
///
/// 收尸后条目**不立即出表**：标记 `closed` 后保留至多 `linger`，等主进程随
/// EOF 补发的 `term.close` 来取退出状态（`roadmap/03-terminal.md` §6.3）。
/// 保留期内 `closed = true` 保证不会再有任何信号打到这个 pid 上。
async fn reap(
    table: Weak<Mutex<HashMap<u32, Terminal>>>,
    pid: Pid,
    linger: Duration,
) -> Option<TermExit> {
    // `waitpid` 是阻塞调用，放进阻塞线程池，别占住 runtime 的工作线程。
    let status = tokio::task::spawn_blocking(move || {
        loop {
            match nix::sys::wait::waitpid(pid, None) {
                // 信号打断只是被打断，不是结果，继续等。
                Err(nix::errno::Errno::EINTR) => continue,
                other => return other,
            }
        }
    })
    .await;

    let exit = match status {
        Ok(Ok(st)) => {
            if let nix::sys::wait::WaitStatus::Exited(_, code) = st {
                tracing::info!(pid = %pid, code, reason = describe_exit(code), "shell 退出");
            } else {
                tracing::info!(pid = %pid, status = ?st, "shell 退出");
            }
            term_exit_of(st)
        }
        Ok(Err(e)) => {
            tracing::warn!(pid = %pid, error = %e, "回收 shell 失败");
            None
        }
        Err(e) => {
            tracing::warn!(pid = %pid, error = %e, "收尸任务异常");
            None
        }
    };

    let pid_key = pid.as_raw() as u32;
    if let Some(table) = table.upgrade() {
        let mut lingering = false;
        if let Some(term) = table.lock().await.get_mut(&pid_key) {
            // 进程已经回收，pid 随时可能被系统重用——绝不能再往它身上发信号。
            term.closed = true;
            lingering = true;
        }
        if lingering {
            // 条目的正常出口是 `term.close` 来取；这里只兜主进程一直不来的异常
            // 路径。只清 `closed` 的条目：万一这个 pid 已被一个新终端占用
            // （新条目 `closed = false`），不能把人家误删。
            let weak = Arc::downgrade(&table);
            tokio::spawn(async move {
                tokio::time::sleep(linger).await;
                let Some(table) = weak.upgrade() else { return };
                let mut guard = table.lock().await;
                if guard.get(&pid_key).is_some_and(|t| t.closed) {
                    guard.remove(&pid_key);
                    tracing::debug!(pid = pid_key, "退出状态无人来取，条目过期清除");
                }
            });
        }
    }
    exit
}

/// 把 `WaitStatus` 折成跨进程可传的退出状态。
fn term_exit_of(st: nix::sys::wait::WaitStatus) -> Option<TermExit> {
    match st {
        nix::sys::wait::WaitStatus::Exited(_, code) => Some(TermExit {
            code: Some(code),
            signal: None,
        }),
        nix::sys::wait::WaitStatus::Signaled(_, sig, _) => Some(TermExit {
            code: None,
            signal: Some(sig as i32),
        }),
        _ => None,
    }
}

// ===========================================================================
// 身份、shell 与环境
// ===========================================================================

/// 定下这个终端跑在谁名下。
///
/// user worker（非 root）**无视** `user` 参数：内核不给它别的选择。
/// admin worker（root）按参数切；参数为空时就是 root 自己。
/// 这里不判断「该不该」——那是主进程按 `session.elevated` 决定的
/// （`design.md` §5.1：不自建鉴权）。
fn resolve_target(requested: Option<&str>) -> ApiResult<User> {
    let me = getuid();
    if !me.is_root() {
        if let Some(name) = requested {
            tracing::debug!(requested = name, "user worker 忽略 user 参数");
        }
        return User::from_uid(me)
            .ok()
            .flatten()
            .ok_or_else(|| ApiError::internal(format!("passwd 里没有 uid {me}")));
    }
    match requested {
        None => User::from_uid(me)
            .ok()
            .flatten()
            .ok_or_else(|| ApiError::internal("passwd 里没有 root")),
        Some(name) => User::from_name(name)
            .ok()
            .flatten()
            .ok_or_else(|| ApiError::not_found(format!("没有名为 {name} 的用户"))),
    }
}

/// 定下要跑哪个 shell。
///
/// - 没指定 → 目标用户 passwd 里的登录 shell，空则 `/bin/sh`。**不校验白名单**：
///   那一项是管理员写进 passwd 的，管理员的决定不需要我们复核。
/// - 指定了 → **必须在 `/etc/shells` 里**。这是唯一挡住「把 shell 参数当成
///   任意命令执行接口」的东西：少了它，`shell = "/usr/bin/python"`（甚至任何
///   可执行文件）就成了一条绕过所有审计的执行通道。
fn resolve_shell(requested: Option<&str>, target: &User) -> ApiResult<PathBuf> {
    let Some(req) = requested else {
        let login = &target.shell;
        return Ok(if login.as_os_str().is_empty() {
            PathBuf::from(FALLBACK_SHELL)
        } else {
            login.clone()
        });
    };

    let listed = listed_shells();
    // 读不到 /etc/shells 时**全部拒绝**（fail closed）。放行等于在文件缺失的
    // 机器上悄悄取消这道检查，而那恰恰是最该谨慎的时候。
    if !listed.iter().any(|s| s == req) {
        return Err(ApiError::invalid_request(format!(
            "{req} 不在 {SHELLS_FILE} 的登录 shell 列表里"
        ))
        .with_detail(if listed.is_empty() {
            format!("{SHELLS_FILE} 不存在或没有任何可用条目")
        } else {
            format!("可用：{}", listed.join(", "))
        }));
    }
    Ok(PathBuf::from(req))
}

/// `/etc/shells` 里列出的 shell。注释与空行跳过。
///
/// **不做路径规范化**：`/etc/shells` 写的是什么就比什么。规范化（解符号链接、
/// 折叠 `..`）会把 `/bin/../bin/zsh` 也放进来，凭空扩大匹配面；而拒绝它的代价
/// 只是调用方要写规范路径。
fn listed_shells() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(SHELLS_FILE) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// 登录 shell 的 `argv[0]`：文件名前加 `-`。
///
/// 这是 `login(1)` / `sshd` 的约定，shell 靠它决定要不要读 `/etc/profile`、
/// `~/.zprofile` 这些登录期配置。不加的话用户会发现「网页终端里 PATH 和
/// SSH 进来不一样」，而且很难猜到原因。
fn login_argv0(shell: &Path) -> String {
    let name = shell
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| FALLBACK_SHELL.into());
    format!("-{name}")
}

/// shell 的环境：worker 自己的环境 + 身份相关的覆盖。
///
/// 继承 worker 的环境是有意的（`roadmap/03-terminal.md` §4.2）：`pam_open_session`
/// 产生的 `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` 就在里面，缺了它们
/// `systemctl --user` 在终端里会失灵。
///
/// 但**换了身份就不能继承这几个**：它们指向的是 worker 自己那个用户的运行时
/// 目录与 session bus，目标用户既没有权限也不该看见——留着只会让 `systemctl --user`
/// 报出一堆莫名其妙的权限错误。
fn build_env(target: &User, shell: &Path, switched: bool) -> Vec<(String, String)> {
    // 非 UTF-8 的环境变量直接跳过：它们要变成 `CString` 送进 execve，
    // 而 worker 自己的环境是 helper 构造的，不会有这种东西。
    let mut env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();

    if switched {
        const OWNER_BOUND: [&str; 4] = [
            "XDG_RUNTIME_DIR",
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_SESSION_ID",
            "XAUTHORITY",
        ];
        env.retain(|(k, _)| !OWNER_BOUND.contains(&k.as_str()));
    }

    let mut set = |k: &str, v: String| match env.iter_mut().find(|(ek, _)| ek == k) {
        Some(slot) => slot.1 = v,
        None => env.push((k.to_owned(), v)),
    };
    // xterm-256color：xterm.js 支持 256 色，报低了会让 shell 主题变单调。
    set("TERM", "xterm-256color".into());
    set("HOME", target.dir.to_string_lossy().into_owned());
    set("USER", target.name.clone());
    set("LOGNAME", target.name.clone());
    set("SHELL", shell.to_string_lossy().into_owned());
    if !env.iter().any(|(k, _)| k == "PATH") {
        env.push(("PATH".into(), DEFAULT_PATH.into()));
    }
    env
}

// ===========================================================================
// fd 与泵
// ===========================================================================

/// 建一对 socketpair，两端都带 `CLOEXEC`。
///
/// 一端交给主进程，另一端留在 worker 里泵字节。带 `CLOEXEC` 是因为 worker
/// 随时可能 fork+exec（下一个终端、`journalctl -f`……），漏一个就等于把
/// 别人的终端通道送给了一个子进程。
fn make_socketpair() -> ApiResult<(OwnedFd, OwnedFd)> {
    // Linux 有原子的 SOCK_CLOEXEC；macOS 没有，只能事后补 fcntl
    // （窗口与 `session::framing::set_cloexec` 里说的是同一个，理由也相同）。
    #[cfg(target_os = "linux")]
    let flags = SockFlag::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = SockFlag::empty();

    let (main_side, worker_side) = socketpair(AddressFamily::Unix, SockType::Stream, None, flags)
        .map_err(|e| ApiError::internal("建立终端 socketpair 失败").with_detail(e.to_string()))?;

    #[cfg(not(target_os = "linux"))]
    for fd in [main_side.as_raw_fd(), worker_side.as_raw_fd()] {
        crate::session::framing::set_cloexec(fd)
            .map_err(|e| ApiError::internal("设置 CLOEXEC 失败").with_detail(e.to_string()))?;
    }
    Ok((main_side, worker_side))
}

/// 把 worker 侧的 socketpair 一端变成 tokio 的 `UnixStream`。
fn to_tokio_stream(fd: OwnedFd) -> io::Result<UnixStream> {
    let std_stream = std::os::unix::net::UnixStream::from(fd);
    std_stream.set_nonblocking(true)?;
    UnixStream::from_std(std_stream)
}

/// 复制一份 PTY 主设备 fd，设成非阻塞并交给 tokio 的事件循环。
///
/// 复制而不是直接用：`portable_pty::MasterPty` 仍然拥有原来那个 fd，
/// 我们要的是一个生命周期独立、能被 `AsyncFd` 持有的副本。两份共享同一个
/// 文件描述（`O_NONBLOCK` 因此对两者都生效），但 `MasterPty` 只用它做 ioctl，
/// 不受影响。
fn async_master(fd: RawFd) -> io::Result<AsyncFd<OwnedFd>> {
    // SAFETY: F_DUPFD_CLOEXEC 只复制 fd，不写内存。
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 刚由内核分配、还没有别的持有者。
    let owned = unsafe { OwnedFd::from_raw_fd(dup) };
    // SAFETY: 只读改 fd 的状态标志。
    let flags = unsafe { libc::fcntl(dup, libc::F_GETFL) };
    // SAFETY: 同上。
    if flags < 0 || unsafe { libc::fcntl(dup, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    AsyncFd::new(owned)
}

/// 在 `AsyncFd` 上读一次。
///
/// PTY 主设备不是 socket 也不是普通文件，tokio 没有现成的封装，只能自己
/// 「等可读 → 试一次 read」。
async fn read_master(master: &AsyncFd<OwnedFd>, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let mut guard = master.readable().await?;
        let attempt = guard.try_io(|inner| {
            // SAFETY: buf 有效且长度如实；read 至多写入 buf.len() 字节。
            let n = unsafe {
                libc::read(
                    inner.get_ref().as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match attempt {
            Ok(r) => return r,
            // try_io 报 WouldBlock：就绪状态是假的，清掉重等。
            Err(_) => continue,
        }
    }
}

/// 在 `AsyncFd` 上写一次。
async fn write_master(master: &AsyncFd<OwnedFd>, buf: &[u8]) -> io::Result<usize> {
    loop {
        let mut guard = master.writable().await?;
        let attempt = guard.try_io(|inner| {
            // SAFETY: buf 有效且长度如实；write 只读它。
            let n = unsafe {
                libc::write(
                    inner.get_ref().as_raw_fd(),
                    buf.as_ptr().cast(),
                    buf.len(),
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        });
        match attempt {
            Ok(r) => return r,
            Err(_) => continue,
        }
    }
}

/// shell 的输出 → 主进程。
async fn pump_master_to_socket(
    master: Arc<AsyncFd<OwnedFd>>,
    mut sink: tokio::net::unix::OwnedWriteHalf,
    pid: Pid,
) {
    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        let n = match read_master(&master, &mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            // shell 退出后，Linux 的主设备读返回 EIO 而不是 0——它不是错误，
            // 就是这条 PTY 的 EOF。当成错误记日志会把每一次正常关终端都变成
            // 一条 warn。
            Err(e) if e.raw_os_error() == Some(libc::EIO) => break,
            Err(e) => {
                tracing::debug!(pid = %pid, error = %e, "读 PTY 主设备失败，泵结束");
                break;
            }
        };
        if let Err(e) = sink.write_all(&buf[..n]).await {
            tracing::debug!(pid = %pid, error = %e, "主进程侧已关闭，泵结束");
            return;
        }
    }
    // 半关写方向，让主进程读到 EOF——那是它判断「shell 退出了」的唯一信号。
    if let Err(e) = sink.shutdown().await {
        tracing::debug!(pid = %pid, error = %e, "关闭终端 socket 写方向失败");
    }
}

/// 主进程的输入（键盘）→ shell。
async fn pump_socket_to_master(
    master: Arc<AsyncFd<OwnedFd>>,
    mut src: tokio::net::unix::OwnedReadHalf,
    pid: Pid,
) {
    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        let n = match src.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::debug!(pid = %pid, error = %e, "读终端 socket 失败，泵结束");
                break;
            }
        };
        let mut written = 0;
        while written < n {
            match write_master(&master, &buf[written..n]).await {
                Ok(0) => return,
                Ok(w) => written += w,
                Err(e) => {
                    tracing::debug!(pid = %pid, error = %e, "写 PTY 主设备失败，泵结束");
                    return;
                }
            }
        }
    }
    // 这里**不关 PTY、也不杀 shell**：主进程断开只意味着「浏览器没在看」，
    // 终端要继续活着（`roadmap/03-terminal.md` §4.3：刷新页面后能重连）。
    // 真正的关闭只由 `term.close` 触发。
    tracing::debug!(pid = %pid, "主进程侧输入通道已关闭，终端继续运行");
}

// ===========================================================================
// 注册
// ===========================================================================

/// 把 JSON 参数解成 `P`。
///
/// 解不出来是**主进程构造错了调用**，不是用户输入有问题，所以报 `internal`；
/// 理由与 `worker::providers::params` 相同，那边是私有的，这里重写一份。
fn decode<P: DeserializeOwned>(method: &'static str, v: Value) -> ApiResult<P> {
    serde_json::from_value(v).map_err(|e| {
        ApiError::internal(format!("worker 无法解析 {method} 的参数")).with_detail(e.to_string())
    })
}

fn encode<R: serde::Serialize>(r: R) -> ApiResult<Value> {
    serde_json::to_value(r)
        .map_err(|e| ApiError::internal("worker 无法序列化结果").with_detail(e.to_string()))
}

/// 把 `term.*` 三个方法注册进分发表，共享同一张终端表。
///
/// `term.open` 走 [`Dispatcher::register_fd`]：它要交出的 socketpair fd 必须和
/// 结果**在同一帧**里发出去（`SCM_RIGHTS`），普通处理器的签名表达不了这件事。
pub fn register(d: &mut Dispatcher) -> TerminalTable {
    let table = TerminalTable::new();

    let t = table.clone();
    d.register_fd(
        rpc::TERM_OPEN,
        Arc::new(move |v: Value| {
            let t = t.clone();
            Box::pin(async move {
                let params: TermOpenParams = decode(rpc::TERM_OPEN, v)?;
                let (result, fd) = t.open(params).await?;
                Ok((encode(result)?, vec![fd]))
            })
        }),
    );

    let t = table.clone();
    d.register_fn(rpc::TERM_RESIZE, move |v| {
        let t = t.clone();
        async move {
            t.resize(decode(rpc::TERM_RESIZE, v)?).await?;
            encode(())
        }
    });

    let t = table.clone();
    d.register_fn(rpc::TERM_CLOSE, move |v| {
        let t = t.clone();
        async move {
            let result = t.close(decode(rpc::TERM_CLOSE, v)?).await?;
            encode(result)
        }
    });

    table
}

#[cfg(test)]
mod tests {
    use std::io::ErrorKind;
    use std::os::unix::net::UnixStream as StdUnixStream;

    use super::*;

    /// 测试里固定用 `/bin/sh`：它一定在 `/etc/shells` 里，行为也可预测。
    /// 用登录用户自己的 shell 会把测试和开发者的 rc 文件绑在一起（比如某些
    /// 配置会在启动时问问题），那种失败与被测代码无关。
    const TEST_SHELL: &str = "/bin/sh";

    fn open_params(cols: u16, rows: u16) -> TermOpenParams {
        TermOpenParams {
            shell: Some(TEST_SHELL.into()),
            user: None,
            cols,
            rows,
        }
    }

    /// 把主进程侧的 fd 变成异步流。
    fn attach(fd: OwnedFd) -> UnixStream {
        let s = StdUnixStream::from(fd);
        s.set_nonblocking(true).unwrap();
        UnixStream::from_std(s).unwrap()
    }

    /// 一直读到累计输出里出现 `needle` 为止；超时即失败并打印已收到的内容。
    async fn read_until(stream: &mut UnixStream, needle: &str) -> String {
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            let mut buf = [0u8; 4096];
            let n = tokio::time::timeout_at(deadline, stream.read(&mut buf))
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "15 秒内没等到 {needle:?}，已收到：{:?}",
                        String::from_utf8_lossy(&seen)
                    )
                });
            match n {
                Ok(0) => panic!(
                    "终端在出现 {needle:?} 之前就关闭了，已收到：{:?}",
                    String::from_utf8_lossy(&seen)
                ),
                Ok(n) => seen.extend_from_slice(&buf[..n]),
                Err(e) => panic!("读终端失败: {e}"),
            }
            if String::from_utf8_lossy(&seen).contains(needle) {
                return String::from_utf8_lossy(&seen).into_owned();
            }
        }
    }

    /// 进程还在不在。已回收（ESRCH）返回 false。
    fn alive(pid: u32) -> bool {
        match nix::sys::signal::kill(Pid::from_raw(pid as i32), None) {
            Ok(()) => true,
            Err(nix::errno::Errno::ESRCH) => false,
            // EPERM 说明进程存在但不归我们管——终端里的 shell 不可能是这种情况。
            Err(e) => panic!("kill(pid, 0) 返回意外错误: {e}"),
        }
    }

    async fn wait_gone(pid: u32) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "10 秒后 pid {pid} 仍然存在（僵尸也算存在：没人收尸）"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn 不在_etc_shells_里的_shell_被拒绝() {
        let table = TerminalTable::new();
        let err = table
            .open(TermOpenParams {
                // 一定存在、一定可执行、也一定不是登录 shell。
                shell: Some("/bin/ls".into()),
                user: None,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap_err();
        assert_eq!(
            err.code,
            strixmaid_types::ErrorCode::InvalidRequest,
            "白名单外的 shell 必须报参数错误：{err:?}"
        );
        assert!(err.message.contains(SHELLS_FILE), "{}", err.message);
        assert!(table.is_empty().await, "被拒的请求不该留下终端");
    }

    #[tokio::test]
    async fn 尺寸为零被拒绝() {
        let table = TerminalTable::new();
        for (cols, rows) in [(0, 24), (80, 0)] {
            let err = table.open(open_params(cols, rows)).await.unwrap_err();
            assert_eq!(err.code, strixmaid_types::ErrorCode::InvalidRequest);
        }
        assert!(table.is_empty().await);
    }

    #[tokio::test]
    async fn 未指定_shell_时用_passwd_里的登录_shell() {
        let me = User::from_uid(getuid()).unwrap().unwrap();
        let resolved = resolve_shell(None, &me).unwrap();
        assert_eq!(resolved, me.shell, "应当直接采用 passwd 里的登录 shell");
    }

    /// 真的开一个终端，从返回的 fd 里读到 shell 执行命令后的输出。
    ///
    /// 命令写成 `STRIX""MAID_OK`：PTY 会把输入回显回来，如果标记在回显里和
    /// 输出里长得一样，那这个断言其实只证明了「我们自己写进去的字节又回来了」，
    /// 什么也没测到。加一对引号后，回显是 `STRIX""MAID_OK`，只有 shell 真正
    /// 执行了 `echo` 才会出现 `STRIXMAID_OK`。
    #[tokio::test]
    async fn 开终端后能读到_shell_的真实输出() {
        let table = TerminalTable::new();
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let me = User::from_uid(getuid()).unwrap().unwrap();
        assert_eq!(info.uid, me.uid.as_raw(), "user worker 只能以自己的身份开");
        assert_eq!(info.user, me.name);
        assert_eq!(info.shell, TEST_SHELL);
        assert!(alive(info.pid), "shell 应当活着");
        assert_eq!(table.len().await, 1);

        let mut stream = attach(fd);
        stream
            .write_all(b"echo STRIX\"\"MAID_OK\n")
            .await
            .unwrap();
        let out = read_until(&mut stream, "STRIXMAID_OK").await;
        assert!(
            out.contains("STRIX\"\"MAID_OK"),
            "应当先看到 PTY 的回显：{out:?}"
        );

        // 顺带确认环境变量真的进了 shell。
        stream.write_all(b"echo \"[$TERM]\"\n").await.unwrap();
        read_until(&mut stream, "[xterm-256color]").await;

        table
            .close(TermCloseParams { pid: info.pid })
            .await
            .unwrap();
    }

    /// resize 之后 `stty size` 必须报出新尺寸——这证明 `TIOCSWINSZ` 落到了
    /// 从设备上。只断言「resize 没报错」测不到任何东西：ioctl 打在错的 fd 上
    /// 一样会返回成功。
    #[tokio::test]
    async fn resize_后_stty_看到新尺寸() {
        let table = TerminalTable::new();
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let mut stream = attach(fd);

        // 先确认初始尺寸，否则「看到 30 100」也可能是碰巧的默认值。
        stream.write_all(b"stty size\n").await.unwrap();
        read_until(&mut stream, "24 80").await;

        table
            .resize(TermResizeParams {
                pid: info.pid,
                cols: 100,
                rows: 30,
            })
            .await
            .unwrap();

        stream.write_all(b"stty size\n").await.unwrap();
        read_until(&mut stream, "30 100").await;

        table
            .close(TermCloseParams { pid: info.pid })
            .await
            .unwrap();
    }

    /// close 之后：shell 进程消失（`kill(pid,0)` → ESRCH，僵尸也不算），
    /// 表清空，主进程侧的 fd 读到 EOF。
    #[tokio::test]
    async fn close_之后_shell_消失且_socket_收到_eof() {
        let table = TerminalTable::new();
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let mut stream = attach(fd);
        stream.write_all(b"echo STRIX\"\"MAID_UP\n").await.unwrap();
        read_until(&mut stream, "STRIXMAID_UP").await;

        table
            .close(TermCloseParams { pid: info.pid })
            .await
            .unwrap();

        assert!(!alive(info.pid), "close 返回时 shell 必须已经被回收");
        assert!(table.is_empty().await, "close 之后表里不该还有条目");

        // 泵关掉写方向后，主进程侧读到 EOF；shell 退出前可能还有尾巴要读完。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut buf = [0u8; 4096];
            match tokio::time::timeout_at(deadline, stream.read(&mut buf)).await {
                Err(_) => panic!("10 秒内没有读到 EOF"),
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => continue,
                // 对端进程没了，某些平台上先报 ECONNRESET 再报 EOF。
                Ok(Err(e)) if e.kind() == ErrorKind::ConnectionReset => break,
                Ok(Err(e)) => panic!("读终端失败: {e}"),
            }
        }

        // 关第二次必须是 not_found，而不是对一个可能被重用的 pid 再发一次信号。
        let err = table
            .close(TermCloseParams { pid: info.pid })
            .await
            .unwrap_err();
        assert_eq!(err.code, strixmaid_types::ErrorCode::NotFound);
    }

    /// shell 自己退出（`exit`）后：进程不留僵尸；条目带着退出状态保留一段时间
    /// 等 `term.close` 来取，无人来取则在保留期后自动出表。
    #[tokio::test]
    async fn shell_自行退出后条目在保留期后被自动清理() {
        let table = TerminalTable::with_linger(Duration::from_millis(50));
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let mut stream = attach(fd);
        stream.write_all(b"echo STRIX\"\"MAID_UP\n").await.unwrap();
        read_until(&mut stream, "STRIXMAID_UP").await;

        stream.write_all(b"exit\n").await.unwrap();
        wait_gone(info.pid).await;

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !table.is_empty().await {
            assert!(
                std::time::Instant::now() < deadline,
                "保留期过后条目没有从表里清掉"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// `exit 7` 之后 `term.close` 必须取回退出码 7（`roadmap/03` §6.3 的地基）。
    /// shell 已死、条目处于保留期，这条路径同时验证「不对已回收的 pid 发信号」
    /// ——发了的话这里只会拿到 ESRCH 而不是状态。
    #[tokio::test]
    async fn 自行退出后_close_取回真实退出码() {
        let table = TerminalTable::new();
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let mut stream = attach(fd);
        stream.write_all(b"echo STRIX\"\"MAID_UP\n").await.unwrap();
        read_until(&mut stream, "STRIXMAID_UP").await;

        stream.write_all(b"exit 7\n").await.unwrap();
        wait_gone(info.pid).await;

        let result = table.close(TermCloseParams { pid: info.pid }).await.unwrap();
        assert_eq!(
            result.exit,
            Some(TermExit {
                code: Some(7),
                signal: None
            }),
            "退出码必须是 shell 真实的 7，不能是编造的 0"
        );
        assert!(table.is_empty().await);
    }

    /// 被信号杀死的 shell，`close` 报告的是信号而不是一个假的退出码。
    #[tokio::test]
    async fn 被信号终止时_close_报告信号() {
        let table = TerminalTable::new();
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let mut stream = attach(fd);
        stream.write_all(b"echo STRIX\"\"MAID_UP\n").await.unwrap();
        read_until(&mut stream, "STRIXMAID_UP").await;

        killpg(Pid::from_raw(info.pid as i32), Signal::SIGKILL).unwrap();
        wait_gone(info.pid).await;

        let result = table.close(TermCloseParams { pid: info.pid }).await.unwrap();
        assert_eq!(
            result.exit,
            Some(TermExit {
                code: None,
                signal: Some(libc::SIGKILL)
            })
        );
    }

    /// shell 不能继承 worker 里那些**不带 CLOEXEC** 的 fd。
    ///
    /// 这不是洁癖：worker 与主进程之间的 IPC socket 就在 fd 3 上，而且是
    /// helper 用 `dup2` 放上去的——`dup2` 出来的 fd 天生没有 CLOEXEC。它一旦漏进
    /// shell，终端里的用户就能直接往主进程的控制通道里写 RPC 帧。这里用一个
    /// 同样不带 CLOEXEC 的 fd 复现那个条件，靠 `/dev/fd/N` 在 shell 内部验证。
    #[tokio::test]
    async fn shell_不继承_worker_不带_cloexec_的_fd() {
        let (keep, _other) =
            socketpair(AddressFamily::Unix, SockType::Stream, None, SockFlag::empty()).unwrap();
        // F_DUPFD（而不是 F_DUPFD_CLOEXEC）：拿一个编号 ≥ 20 的、不带 CLOEXEC 的副本。
        // SAFETY: 只复制 fd，不写内存。
        let leaky = unsafe { libc::fcntl(keep.as_raw_fd(), libc::F_DUPFD, 20) };
        assert!(leaky >= 20, "复制 fd 失败: {}", io::Error::last_os_error());
        // SAFETY: 刚由内核分配，没有别的持有者。
        let _leaky_owned = unsafe { OwnedFd::from_raw_fd(leaky) };

        let table = TerminalTable::new();
        let (info, fd) = table.open(open_params(80, 24)).await.unwrap();
        let mut stream = attach(fd);
        // 两个标记都写成拼接形式，免得 PTY 的回显本身就把断言喂饱。
        stream
            .write_all(
                format!("if [ -e /dev/fd/{leaky} ]; then echo LE\"\"AK; else echo CLE\"\"AN; fi\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let out = read_until(&mut stream, "CLEAN").await;
        assert!(!out.contains("LEAK"), "fd {leaky} 漏进了 shell：{out:?}");

        table
            .close(TermCloseParams { pid: info.pid })
            .await
            .unwrap();
    }

    /// 表被整个丢掉（worker 退出、dispatcher 析构）时，shell 不能变成孤儿。
    #[tokio::test]
    async fn 表被丢弃时终端被回收() {
        let table = TerminalTable::new();
        let (info, _fd) = table.open(open_params(80, 24)).await.unwrap();
        assert!(alive(info.pid));

        drop(table);
        wait_gone(info.pid).await;
    }

    /// 不是本 worker 开的 pid 一律 not_found。放行意味着任何调用方都能拿
    /// 这两个方法去操作本机的任意进程。
    #[tokio::test]
    async fn 未知_pid_的_resize_与_close_报_not_found() {
        let table = TerminalTable::new();
        // 自己的 pid：一定存在，因此「报错」只可能来自查表而不是进程不存在。
        let me = std::process::id();
        let err = table
            .resize(TermResizeParams {
                pid: me,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap_err();
        assert_eq!(err.code, strixmaid_types::ErrorCode::NotFound);

        let err = table.close(TermCloseParams { pid: me }).await.unwrap_err();
        assert_eq!(err.code, strixmaid_types::ErrorCode::NotFound);
        // 真被执行了的话，测试进程自己就先没了。
        assert!(alive(me), "未知 pid 绝不能被信号波及");
    }

    /// 经分发表走一遍：`term.open` 必须以「结果 + 1 个 fd」的形式返回。
    #[tokio::test]
    async fn 经_dispatcher_调用时_fd_随结果一起返回() {
        let mut d = Dispatcher::new();
        let table = register(&mut d);
        assert!(d.methods().iter().any(|m| m == rpc::TERM_OPEN));

        let (value, fds) = d
            .dispatch_with_fds(
                rpc::TERM_OPEN,
                serde_json::to_value(open_params(80, 24)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fds.len(), 1, "term.open 必须交出一个 fd");
        let info: TermOpenResult = serde_json::from_value(value).unwrap();

        let mut stream = attach(fds.into_iter().next().unwrap());
        stream.write_all(b"echo STRIX\"\"MAID_RPC\n").await.unwrap();
        read_until(&mut stream, "STRIXMAID_RPC").await;

        d.dispatch(
            rpc::TERM_CLOSE,
            serde_json::json!({ "pid": info.pid }),
        )
        .await
        .unwrap();
        assert!(!alive(info.pid));
        assert!(table.is_empty().await);
    }
}
