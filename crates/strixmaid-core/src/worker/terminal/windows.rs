//! Windows 侧的终端实现：ConPTY + `CreateProcessAsUserW` + 作业对象。
//!
//! 接口形状与 Unix 侧一致，取舍与背景见 [父模块文档](super)。
//!
//! # ConPTY 是什么
//!
//! `CreatePseudoConsole`（Windows 10 1809 起）给出一个伪控制台：我们交给它两根
//! **匿名管道**的一端，它把另一端连到子进程的控制台上。子进程以为自己连着真的
//! 控制台（`GetConsoleMode` 正常、能收窗口大小变化），而我们读到的是带 ANSI
//! 转义序列的字节流——与 Unix 的 PTY 主设备在语义上完全对应。
//!
//! ```text
//!  我们这边                    ConPTY                     shell
//!  ────────                    ──────                     ─────
//!  in_write  ──► in_read  ──►  伪控制台  ──►  子进程的 stdin
//!  out_read ◄──  out_write ◄──          ◄──  子进程的 stdout/stderr
//! ```
//!
//! 建完伪控制台之后，**我们手里的 `in_read` / `out_write` 必须立刻关掉**：
//! `CreatePseudoConsole` 已经复制了自己的一份，我们再留着的话，shell 退出时
//! `out_read` 永远读不到 EOF——与 Unix 侧「从设备 fd 不能留」是同一个道理，
//! 也是同一类难查的 bug。
//!
//! 把 shell **真正接到**这个伪控制台上另有两处与直觉相反、且失败时全程不报错的
//! 细节，见 [`create_process_in_pty`] 的文档。Unix 侧没有对应物：那边
//! `openpty` + `TIOCSCTTY` 之后从设备就是子进程的 0/1/2，不存在「附着了控制台
//! 但标准句柄指向别处」这种中间状态。
//!
//! # 为什么关终端要用作业对象
//!
//! Unix 上 `killpg` 能一次干掉整个进程组。Windows 没有进程组这个概念
//! （`CREATE_NEW_PROCESS_GROUP` 只影响 Ctrl-C 的投递范围，不是一个可以整体
//! 终止的对象），`TerminateProcess` 只杀一个进程——关掉终端后 shell 起的
//! `cargo build` 会继续跑，成为孤儿。
//!
//! 所以每个终端配一个**作业对象**，shell 一创建就被塞进去，它此后起的所有
//! 子孙进程自动继承。`TerminateJobObject` 于是就是 `killpg` 的等价物。
//! 再配上 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`：worker 崩溃时句柄被内核关闭，
//! 整棵树跟着消失，不会留下孤儿——这一条 Unix 侧是靠 `SIGHUP` 实现的。
//!
//! # 关终端为什么先关 ConPTY 再终止作业
//!
//! `ClosePseudoConsole` 会让 shell 的控制台断开，交互式 shell 看到这个会自己
//! 干净退出（相当于 Unix 上的 `SIGHUP`）。给它 [`CLOSE_GRACE`] 的时间，
//! 之后才 `TerminateJobObject` 强杀。顺序反过来就永远拿不到 shell 的正常退出码。
//!
//! # 退出状态里没有 `signal`
//!
//! [`TermExit::signal`] 在 Windows 上恒为 `None`：进程终止只有一个 32 位退出码，
//! 没有「被哪个信号杀死」这一维。被 `TerminateJobObject` 强杀的进程退出码是
//! 我们传进去的那个值（[`KILLED_EXIT_CODE`]），如实报在 `code` 里。
//!
//! # 身份切换
//!
//! admin worker 要以别的用户身份开终端时需要那个用户的**令牌**，而拿令牌需要
//! 凭据（`LogonUserW`）或 `SeTcbPrivilege`。worker 两样都没有——它只是个普通
//! 用户进程。因此本实现**不做跨用户的终端**：`TermOpenParams::user` 与自身
//! 不符时返回 `PermissionDenied`，说清原因。
//!
//! 这不是缺功能：Windows 上「以另一个用户身份开终端」的正确做法是让主进程
//! 走完整的认证链（helper `LogonUserW` → 新 worker），而那条路已经存在——
//! 主进程按 `session.elevated` 派给哪个 worker，派过来的 worker 本身就是
//! 目标身份。与 Unix 上 admin worker 靠 root 特权直接切的区别，只是切换发生
//! 在更早的一步。

use std::collections::HashMap;
use std::ffi::c_void;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use strixmaid_types::rpc::{
    TermCloseParams, TermCloseResult, TermExit, TermOpenParams, TermOpenResult, TermResizeParams,
};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Console::{
    COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON, ResizePseudoConsole,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, INFINITE,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    WaitForSingleObject,
};

use super::{CLOSE_GRACE, PUMP_BUF, REAPED_LINGER, unknown};
use crate::platform::windows::handle::Owned;
use crate::platform::windows::{last_error, token, wide};
use crate::session::channel::{Attachment, IpcChannel};

/// 被 [`TerminateJobObject`] 强杀时报的退出码。
///
/// 选 `1` 而不是某个魔数：`cmd.exe` 与 PowerShell 被外部终止时惯用的就是它，
/// 前端把非零当「异常结束」即可，不必认识一个我们自创的码。
const KILLED_EXIT_CODE: u32 = 1;

/// 解析不出登录 shell 时的兜底。`cmd.exe` 在任何 Windows 上都有。
const FALLBACK_SHELL: &str = r"C:\Windows\System32\cmd.exe";

/// 允许作为 shell 的可执行文件名（小写，不含目录）。
///
/// # 为什么是名字白名单而不是 `/etc/shells`
///
/// Windows 没有 `/etc/shells`。直接放行任意路径等于给「用终端跑任意程序」
/// 开一个口子——虽然那个程序本来就以用户身份运行、内核会管住它，但终端的
/// 语义是「开一个 shell」，把它当成通用的进程启动器会让审计日志失去意义
/// （每条记录都只是「开了个终端」）。
///
/// 列表刻意短：这几个是 Windows 上真正意义的交互式 shell。
/// `pwsh.exe` 是 PowerShell 7+，与系统自带的 `powershell.exe`（5.1）是两个东西，
/// 都列上。
const ALLOWED_SHELLS: &[&str] = &[
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "bash.exe",
    "wsl.exe",
];

// ===========================================================================
// 终端表
// ===========================================================================

/// 一个正在跑的终端。
///
/// # 这里为什么没有 pid、也没有进程句柄
///
/// pid 是本表的**键**，再在值里存一份只会多出一个可能与键不一致的来源。
///
/// shell 的进程句柄则整个交给 [`reap`]：它是唯一需要那个句柄的人
/// （`WaitForSingleObject` + `GetExitCodeProcess`），任务结束时句柄随之关闭。
/// [`close`](TerminalTable::close) 要退出码时是 `await` 那个任务，不是自己再去
/// 查一遍进程——两条取退出码的路迟早会给出不一样的答案。终止进程树也用不到它：
/// 那是作业对象的事。
struct Terminal {
    /// 作业对象：关它 / 终止它就是关掉整棵进程树。
    job: Arc<Owned>,
    /// 伪控制台。`resize` 要用它；关它相当于给 shell 发 `SIGHUP`。
    ///
    /// `Option` 是因为 `close` 会提前把它关掉（那是「请你退出」那一步），
    /// 之后 `Drop` 不该再关一次。
    pcon: Option<PseudoConsole>,
    /// 两个方向的泵。
    pumps: Vec<JoinHandle<()>>,
    /// 收尸任务，结束时给出 shell 的退出状态。
    reaper: Option<JoinHandle<Option<TermExit>>>,
    /// 已经在 `close` 里处理过。
    closed: bool,
    /// 本条目的「代」。pid 是**会被系统复用的**：`close` 先摘表再等收尸，
    /// 这个窗口里新开的终端可能拿到同一个 pid 并插进表里。收尸任务与
    /// 保留期清理只认代号相同的条目，免得把新终端误标 `closed`、
    /// 误关它的伪控制台（那等于无声杀掉一个无辜的终端）。
    serial: u64,
}

/// [`Terminal::serial`] 的来源：进程内单调递增，永不复用。
static NEXT_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for Terminal {
    fn drop(&mut self) {
        for h in &self.pumps {
            h.abort();
        }
        // 先断控制台，给 shell 一个自己退出的机会。
        drop(self.pcon.take());
        if self.closed {
            return;
        }
        // 走到这里说明表被整个丢掉了（worker 退出、dispatcher 析构）。
        // 严格来说作业对象的 `KILL_ON_JOB_CLOSE` 会在最后一个句柄关闭时兜住，
        // 但那要等 `Arc` 真的归零；显式终止一下，语义更清楚也更及时。
        // SAFETY: job 是本进程持有的有效作业对象句柄。
        unsafe { TerminateJobObject(self.job.raw(), KILLED_EXIT_CODE) };
    }
}

/// worker 内所有终端的登记表：`pid → Terminal`。
///
/// # 为什么 `resize` / `close` 必须查这张表
///
/// 参数里的 `pid` 来自主进程，而主进程的 `id → pid` 映射来自更早的一次
/// `term.open`。如果不查表就直接拿 pid 去终止进程，那么**任何一个能发 RPC 的
/// 调用方都能杀掉本机任意进程**——worker 是以登录用户身份跑的，这等于白送一个
/// 「杀死该用户全部进程」的接口。查表把作用域限制成「本 worker 亲手开过的终端」。
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

    /// 开一个终端，返回结果与**主进程侧的通道一端**。
    pub async fn open(&self, params: TermOpenParams) -> ApiResult<(TermOpenResult, Attachment)> {
        if params.cols == 0 || params.rows == 0 {
            return Err(ApiError::invalid_request(format!(
                "终端尺寸必须为正，收到 {}x{}",
                params.cols, params.rows
            )));
        }

        let me = token::current_identity()
            .map_err(|e| ApiError::internal("读不到本进程身份").with_detail(e.to_string()))?;
        check_target(params.user.as_deref(), &me)?;
        let shell = resolve_shell(params.shell.as_deref())?;

        let spawned = tokio::task::spawn_blocking({
            let shell = shell.clone();
            let cols = params.cols;
            let rows = params.rows;
            move || spawn_shell(&shell, cols, rows)
        })
        .await
        .map_err(|e| ApiError::internal("启动 shell 的任务异常").with_detail(e.to_string()))??;

        let (worker_side, main_side) = IpcChannel::pair_for_transfer("term")
            .await
            .map_err(|e| ApiError::internal("建立终端通道失败").with_detail(e.to_string()))?;

        // 解构成局部变量：下面的 async 块要按字段拿走一部分，整体捕获会把
        // `job` / `pcon` 一起拖进任务里。
        let Spawned {
            pid,
            process,
            job,
            pcon,
            out_read,
            in_write,
        } = spawned;

        let sock = Arc::new(worker_side);
        let input = tokio::spawn(pump_socket_to_console(in_write, sock.clone(), pid));
        let input_abort = input.abort_handle();
        let output = tokio::spawn(async move {
            pump_console_to_socket(out_read, sock, pid).await;
            // 输出泵结束 = shell 没了（或主进程侧没了）。命名管道没有半关，
            // 而通道由两个泵共享（`Arc`）：输入泵还停在「等主进程的键盘输入」上，
            // 不停掉它，worker 侧就永远不放手，主进程也就永远读不到断开——
            // 「让主进程读到 EOF」全靠最后一个持有者放手这一件事。
            input_abort.abort();
        });
        let pumps = vec![output, input];

        // 收尸任务持 `Weak`：强引用会让「表」和「任务」互相钉住，
        // dispatcher 析构时表就不会被 drop，`Terminal::drop` 里的清理也就不会跑。
        let weak = Arc::downgrade(&self.inner);
        let serial = NEXT_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let reaper = tokio::spawn(reap(weak, pid, serial, process, self.linger));

        let result = TermOpenResult {
            pid,
            shell: shell.to_string_lossy().into_owned(),
            user: me.username(),
            uid: me.uid,
        };
        self.inner.lock().await.insert(
            pid,
            Terminal {
                job: Arc::new(job),
                pcon: Some(pcon),
                pumps,
                reaper: Some(reaper),
                closed: false,
                serial,
            },
        );
        tracing::info!(
            pid,
            user = %result.user,
            shell = %result.shell,
            cols = params.cols,
            rows = params.rows,
            "终端已开"
        );
        Ok((result, main_side))
    }

    /// 改窗口大小。ConPTY 随即让子进程看到控制台缓冲区尺寸变化。
    pub async fn resize(&self, params: TermResizeParams) -> ApiResult<()> {
        if params.cols == 0 || params.rows == 0 {
            return Err(ApiError::invalid_request(format!(
                "终端尺寸必须为正，收到 {}x{}",
                params.cols, params.rows
            )));
        }
        let table = self.inner.lock().await;
        let term = table.get(&params.pid).ok_or_else(|| unknown(params.pid))?;
        let pcon = term
            .pcon
            .as_ref()
            .ok_or_else(|| ApiError::not_found(format!("终端 {} 正在关闭", params.pid)))?;
        pcon.resize(params.cols, params.rows)
            .map_err(|e| ApiError::internal("改终端尺寸失败").with_detail(e.to_string()))
    }

    /// 关一个终端：断控制台 → 等它咽气 → 超时则终止整个作业。
    ///
    /// shell 已自行退出时（条目被收尸任务标记为 `closed` 后保留在表里），
    /// 不再做任何终止动作——只取走退出状态并清掉条目。
    pub async fn close(&self, params: TermCloseParams) -> ApiResult<TermCloseResult> {
        // **先摘表再等**。反过来（持锁等收尸）必然死锁：收尸任务结束时要拿同一把锁
        // 去标记自己的条目。
        let mut term = {
            let mut table = self.inner.lock().await;
            table
                .remove(&params.pid)
                .ok_or_else(|| unknown(params.pid))?
        };
        let already_reaped = term.closed;
        term.closed = true;

        // 断开控制台 = Unix 上的 SIGHUP：交互式 shell 看到这个会自己退出。
        drop(term.pcon.take());

        let mut exit = None;
        if let Some(mut reaper) = term.reaper.take() {
            match tokio::time::timeout(CLOSE_GRACE, &mut reaper).await {
                Ok(res) => exit = res.ok().flatten(),
                Err(_) => {
                    if !already_reaped {
                        tracing::warn!(pid = params.pid, "断开控制台后仍未退出，终止整个作业");
                        // SAFETY: job 是本进程持有的有效作业对象句柄。
                        unsafe { TerminateJobObject(term.job.raw(), KILLED_EXIT_CODE) };
                    }
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

/// 等 shell 退出并取回退出码。
///
/// 与 Unix 的 `waitpid` 不同，Windows 上进程对象**不需要**被「收尸」——
/// 关掉句柄即可，没有僵尸进程这回事。这个任务存在只为两件事：拿到退出码，
/// 以及在条目上打 `closed` 标记（此后不再对这个 pid 做任何终止动作）。
async fn reap(
    table: Weak<Mutex<HashMap<u32, Terminal>>>,
    pid: u32,
    serial: u64,
    process: Owned,
    linger: Duration,
) -> Option<TermExit> {
    // `WaitForSingleObject(INFINITE)` 是阻塞调用，放进阻塞线程池。
    let code = tokio::task::spawn_blocking(move || {
        // SAFETY: process 是有效的进程句柄，带 SYNCHRONIZE（CreateProcess 给全权限）。
        let waited = unsafe { WaitForSingleObject(process.raw(), INFINITE) };
        if waited != WAIT_OBJECT_0 {
            return Err(last_error());
        }
        let mut code: u32 = 0;
        // SAFETY: 同上；进程已经结束，退出码此刻是确定的。
        let ok = unsafe { GetExitCodeProcess(process.raw(), &raw mut code) };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(code)
    })
    .await;

    let exit = match code {
        Ok(Ok(code)) => {
            tracing::info!(pid, code, "shell 退出");
            Some(TermExit {
                code: Some(code as i32),
                // Windows 没有「被信号杀死」这一维，见模块文档。
                signal: None,
            })
        }
        Ok(Err(e)) => {
            tracing::warn!(pid, error = %e, "取 shell 退出码失败");
            None
        }
        Err(e) => {
            tracing::warn!(pid, error = %e, "收尸任务异常");
            None
        }
    };

    if let Some(table) = table.upgrade() {
        let mut lingering = false;
        let mut pcon = None;
        // 只认代号相同的条目：`close` 摘表后到这里之间，这个 pid 可能已被
        // 系统复用、被一个**新**终端插进表里——动它就是误伤（见 `Terminal::serial`）。
        if let Some(term) = table.lock().await.get_mut(&pid)
            && term.serial == serial
        {
            term.closed = true;
            // shell 已经退出，但 ConPTY **不会**因此关闭输出管道——EOF 只在
            // `ClosePseudoConsole` 之后出现。立刻关掉伪控制台，输出泵才会读到
            // BrokenPipe 收工并放掉通道，主进程才能及时看到 EOF、来取退出码。
            // 不关的话，要等下面的保留期清理经由 `Terminal::drop` 顺带把它关掉：
            // 终端在列表里多装几十秒的活，退出码也因条目先过期而丢失
            // （`roadmap/12-workspace.md` §2.1 实测到的正是这个）。
            pcon = term.pcon.take();
            lingering = true;
        }
        // 在锁外关闭：`ClosePseudoConsole` 要等 conhost 排空输出，可能短暂阻塞。
        drop(pcon);
        if lingering {
            // 条目的正常出口是 `term.close` 来取；这里只兜主进程一直不来的异常
            // 路径。同样只清代号相同的条目，不能把复用了 pid 的新终端误删。
            let weak = Arc::downgrade(&table);
            tokio::spawn(async move {
                tokio::time::sleep(linger).await;
                let Some(table) = weak.upgrade() else { return };
                let mut guard = table.lock().await;
                if guard.get(&pid).is_some_and(|t| t.closed && t.serial == serial) {
                    guard.remove(&pid);
                    tracing::debug!(pid, "退出状态无人来取，条目过期清除");
                }
            });
        }
    }
    exit
}

// ===========================================================================
// 身份与 shell
// ===========================================================================

/// 确认请求的身份就是本进程的身份。
///
/// 见模块文档「身份切换」：worker 在 Windows 上没有切到别人身份的手段，
/// 与其悄悄忽略 `user` 参数（那会让调用方以为切成功了），不如明确拒绝。
fn check_target(requested: Option<&str>, me: &token::TokenIdentity) -> ApiResult<()> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let (_, bare) = token::split_account(requested);
    let matches = me
        .account
        .as_ref()
        .is_some_and(|a| a.name.eq_ignore_ascii_case(&bare) || a.qualified().eq_ignore_ascii_case(requested))
        || me.sid.eq_ignore_ascii_case(requested);
    if matches {
        return Ok(());
    }
    Err(ApiError::permission_denied(format!(
        "本 worker 以 {} 身份运行，无法开 {requested} 的终端",
        me.username()
    ))
    .with_detail(
        "Windows 上切换终端身份需要目标用户的令牌，worker 没有获取它的特权；\
         请由主进程以该身份建立会话后再开终端"
            .to_owned(),
    ))
}

/// 定下用哪个 shell。
///
/// 请求为空时用 `%COMSPEC%`（约定俗成的「默认命令解释器」，几乎总是 `cmd.exe`），
/// 再退回 [`FALLBACK_SHELL`]。请求了具体路径时必须落在 [`ALLOWED_SHELLS`] 里。
fn resolve_shell(requested: Option<&str>) -> ApiResult<PathBuf> {
    let candidate = match requested {
        Some(s) if !s.trim().is_empty() => PathBuf::from(s),
        _ => std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(FALLBACK_SHELL)),
    };

    let name = candidate
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if !ALLOWED_SHELLS.contains(&name.as_str()) {
        return Err(ApiError::invalid_request(format!(
            "{} 不是允许的 shell",
            candidate.display()
        ))
        .with_detail(format!("允许的有：{}", ALLOWED_SHELLS.join("、"))));
    }
    if !candidate.is_absolute() {
        return Err(ApiError::invalid_request(format!(
            "shell 必须是绝对路径，收到 {}",
            candidate.display()
        )));
    }
    if !candidate.is_file() {
        return Err(ApiError::invalid_request(format!(
            "{} 不存在",
            candidate.display()
        )));
    }
    Ok(candidate)
}

// ===========================================================================
// ConPTY
// ===========================================================================

/// 一个伪控制台。`Drop` 时关闭它，相当于给 shell 发 `SIGHUP`。
struct PseudoConsole(HPCON);

// SAFETY: HPCON 是一个内核对象句柄，跨线程使用是安全的；
// ConPTY 的 API（Resize / Close）本身也没有线程亲和性。
unsafe impl Send for PseudoConsole {}
// SAFETY: 同上；`resize` 取 `&self`，而 `ResizePseudoConsole` 是线程安全的。
unsafe impl Sync for PseudoConsole {}

impl PseudoConsole {
    /// 改控制台缓冲区尺寸。
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        // SAFETY: self.0 是有效的伪控制台句柄。
        let hr = unsafe { ResizePseudoConsole(self.0, size) };
        if hr < 0 {
            return Err(io::Error::from_raw_os_error(hr));
        }
        Ok(())
    }
}

impl Drop for PseudoConsole {
    fn drop(&mut self) {
        // SAFETY: self.0 由 CreatePseudoConsole 得到，此处归还，且只归还一次
        // （Drop 只跑一次，且 `close` 是 `take()` 走整个值而非复制句柄）。
        unsafe { ClosePseudoConsole(self.0) };
    }
}

/// [`spawn_shell`] 的产物。
struct Spawned {
    pid: u32,
    process: Owned,
    job: Owned,
    pcon: PseudoConsole,
    /// 读 shell 的输出。
    out_read: Owned,
    /// 写 shell 的输入。
    in_write: Owned,
}

/// 建伪控制台、建作业对象、起 shell。
///
/// 全程同步且有多次 `unsafe`，由调用方放进 `spawn_blocking`。
fn spawn_shell(shell: &std::path::Path, cols: u16, rows: u16) -> ApiResult<Spawned> {
    let (in_read, in_write) = anonymous_pipe()
        .map_err(|e| ApiError::internal("建终端输入管道失败").with_detail(e.to_string()))?;
    let (out_read, out_write) = anonymous_pipe()
        .map_err(|e| ApiError::internal("建终端输出管道失败").with_detail(e.to_string()))?;

    let size = COORD {
        X: cols as i16,
        Y: rows as i16,
    };
    let mut hpcon: HPCON = 0;
    // SAFETY: 两个句柄有效；dwFlags 为 0（不继承游标位置）；phpc 是输出参数。
    let hr = unsafe { CreatePseudoConsole(size, in_read.raw(), out_write.raw(), 0, &raw mut hpcon) };
    if hr < 0 {
        return Err(ApiError::internal("创建伪控制台失败")
            .with_detail(io::Error::from_raw_os_error(hr).to_string()));
    }
    let pcon = PseudoConsole(hpcon);
    // 关键一步：ConPTY 已经复制了自己的一份，我们这两个必须立刻放手，
    // 否则 shell 退出时 out_read 读不到 EOF。见模块文档。
    drop(in_read);
    drop(out_write);

    let job = create_job()
        .map_err(|e| ApiError::internal("创建作业对象失败").with_detail(e.to_string()))?;

    let pi = create_process_in_pty(shell, &pcon)
        .map_err(|e| ApiError::internal("启动 shell 失败").with_detail(e.to_string()))?;

    // SAFETY: 两个句柄都刚由本函数创建，均有效。
    let assigned = unsafe { AssignProcessToJobObject(job.raw(), pi.process.raw()) };
    if assigned == 0 {
        // 进不了作业对象意味着关终端时清不干净整棵树。这不是可以降级继续的事
        // ——留下杀不掉的进程比开不出终端糟得多，所以直接失败。
        let e = last_error();
        return Err(ApiError::internal("无法把 shell 放进作业对象").with_detail(e.to_string()));
    }

    Ok(Spawned {
        pid: pi.pid,
        process: pi.process,
        job,
        pcon,
        out_read,
        in_write,
    })
}

/// 一对匿名管道，两端都**不可继承**。
///
/// 不可继承是刻意的：ConPTY 自己会复制它需要的那一份，子进程的标准句柄由
/// 伪控制台属性接管。让它们可继承只会多一条泄漏路径。
fn anonymous_pipe() -> io::Result<(Owned, Owned)> {
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: 两个输出参数有效；空的 SECURITY_ATTRIBUTES 即默认安全性、不可继承。
    let ok = unsafe { CreatePipe(&raw mut read, &raw mut write, std::ptr::null(), 0) };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: 调用成功即两个句柄有效且本进程独占。
    unsafe { Ok((Owned::new(read)?, Owned::new(write)?)) }
}

/// 建一个「句柄一关就杀光成员」的作业对象。
fn create_job() -> io::Result<Owned> {
    // SAFETY: 两个参数为空即匿名、默认安全性的作业对象。
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    // SAFETY: raw 刚由 CreateJobObjectW 返回；Owned::new 会挡掉空句柄。
    let job = unsafe { Owned::new(raw) }?;

    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
        // SAFETY: 该结构体全是整数与嵌套的整数结构，全零是合法初值。
        unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: job 有效；info 的类型与 JobObjectExtendedLimitInformation 匹配，
    // 长度如实给出。
    let ok = unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&raw const info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(job)
}

/// `CreateProcessW` 的产物里我们要留下的部分。
struct SpawnedProcess {
    pid: u32,
    process: Owned,
}

/// 带着伪控制台属性起 shell。
///
/// # 为什么要 `STARTUPINFOEXW` 和属性列表
///
/// 把子进程连到伪控制台上**只有这一条路**：
/// `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE` 写进扩展启动信息的属性列表。
/// 把 `hStdInput` 之类直接指向管道是不行的——那样子进程拿到的是普通管道，
/// `GetConsoleMode` 会失败，交互式 shell 会退化成非交互模式。
///
/// # 两处与直觉相反、且都不会报错的地方
///
/// 这两条各自都能让终端「开出来了但一个字节也读不到」，而且全程每个 Win32
/// 调用都返回成功，只能靠子进程的表现反推，所以在此写清楚。
///
/// ## 一、`lpValue` 传的是 HPCON **本身**，不是指向它的指针
///
/// `UpdateProcThreadAttribute` 的绝大多数属性都按「`lpValue` 指向值、`cbSize`
/// 是值的长度」理解，唯独伪控制台这一项例外：内核直接把属性表里存的那个指针
/// 当成 `HPCON` 用（`HPCON` 本身就是一个指向内部结构的指针）。
/// 传 `&hpcon` 语法上同样成立、`UpdateProcThreadAttribute` 同样返回成功，
/// 但子进程会在控制台初始化阶段以 `0xC0000142`（`STATUS_DLL_INIT_FAILED`）
/// 直接死掉——那是拿栈上的相邻字节当控制台引用的后果。
/// 微软官方示例（`microsoft/terminal` 的 `samples/ConPTY/EchoCon`）传的也是
/// `hPC` 本身。
///
/// ## 二、必须置 `STARTF_USESTDHANDLES` 且三个标准句柄都留空
///
/// 伪控制台属性只决定子进程**附着到哪个控制台**（`CONIN$` / `CONOUT$`），
/// 不决定它的**标准句柄**。而 `CreateProcessW` 在没有 `STARTF_USESTDHANDLES`
/// 时会把父进程当前的标准句柄复制给子进程——worker 的 stdout 通常是被重定向的
/// （服务宿主、日志管道、测试进程都算），于是 shell 的输出就顺着那条重定向流走了，
/// 伪控制台这边一个字节也收不到，读端既没有数据也不会 EOF。
///
/// 置上 `STARTF_USESTDHANDLES` 而三个句柄都留 `NULL`，等于明确告诉内核
/// 「不要给子进程任何标准句柄」，它于是回落到「用自己所附着的控制台」，
/// 也就是伪控制台。这是 Windows 终端团队给出的做法
/// （`microsoft/terminal` discussion #15814）。父进程恰好没有被重定向时
/// 两种写法表现一样，这正是它难查的原因。
fn create_process_in_pty(
    shell: &std::path::Path,
    pcon: &PseudoConsole,
) -> io::Result<SpawnedProcess> {
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        UpdateProcThreadAttribute,
    };

    // 属性列表要两次调用：第一次问大小（必然失败于 ERROR_INSUFFICIENT_BUFFER），
    // 第二次真的初始化。
    let mut size: usize = 0;
    // SAFETY: 第一次调用按文档传空指针问大小，失败是预期的。
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut size) };
    if size == 0 {
        return Err(last_error());
    }
    let mut attr_buf = vec![0u8; size];
    let attr_list = attr_buf.as_mut_ptr().cast();
    // SAFETY: attr_buf 有上一步问出的容量，size 如实描述它。
    let ok = unsafe { InitializeProcThreadAttributeList(attr_list, 1, 0, &raw mut size) };
    if ok == 0 {
        return Err(last_error());
    }
    // 从这里起，属性列表必须 Delete；用一个守卫保证每条返回路径都走到。
    struct AttrList(*mut c_void);
    impl Drop for AttrList {
        fn drop(&mut self) {
            // SAFETY: 只在 Initialize 成功之后构造，且只删一次。
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _guard = AttrList(attr_list);

    // SAFETY: attr_list 已初始化且容量为 1 项；lpValue 按伪控制台属性的约定传
    // HPCON 本身（见本函数文档「一」），它由调用方持有，在 CreateProcessW
    // 返回前一直有效。
    let ok = unsafe {
        UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            pcon.0 as *const c_void,
            size_of::<HPCON>(),
            std::ptr::null_mut(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }

    let mut si: STARTUPINFOEXW =
        // SAFETY: 该结构体全是整数、指针与句柄，全零是合法初值。
        unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    si.lpAttributeList = attr_list;
    // 三个 hStd* 由 zeroed() 留在 NULL，这里只是打开「按我给的标准句柄来」的开关，
    // 于是子进程一个标准句柄都拿不到，回落到伪控制台。见本函数文档「二」。
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;

    let mut pi: PROCESS_INFORMATION =
        // SAFETY: 全是输出字段。
        unsafe { std::mem::zeroed() };

    // 命令行必须是可写缓冲：CreateProcessW 会就地改它（这是文档明确说明的）。
    let mut cmdline = wide::to_wide(&shell.to_string_lossy());

    // SAFETY: lpApplicationName 为空、lpCommandLine 是以 NUL 结尾的可写缓冲；
    // bInheritHandles = FALSE，子进程只通过伪控制台拿到标准句柄；
    // si 与 pi 的布局与大小如实给出。
    let ok = unsafe {
        CreateProcessW(
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT,
            std::ptr::null(),
            std::ptr::null(),
            (&raw const si).cast(),
            &raw mut pi,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    // 主线程句柄我们用不到，立刻归还；进程句柄留着取退出码。
    // SAFETY: 两个句柄都由 CreateProcessW 填入且有效。
    unsafe { crate::platform::windows::close_handle(pi.hThread) };
    // SAFETY: 同上；所有权转移给 Owned。
    let process = unsafe { Owned::new(pi.hProcess) }?;
    Ok(SpawnedProcess {
        pid: pi.dwProcessId,
        process,
    })
}

// ===========================================================================
// 泵
// ===========================================================================

/// shell 的输出 → 主进程。
///
/// 管道读是阻塞的（`CreatePipe` 给不出重叠 I/O 的管道），所以整个循环跑在
/// `spawn_blocking` 里，读到的块经 `mpsc` 转交给异步侧去写通道。
/// 这多一次拷贝，但换来的是「一个终端占一个阻塞线程」而不是「占一个 runtime
/// 工作线程」——后者会在几个终端之后把整个 worker 饿死。
async fn pump_console_to_socket(out_read: Owned, sink: Arc<IpcChannel>, pid: u32) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let reader = tokio::task::spawn_blocking(move || {
        let mut buf = vec![0u8; PUMP_BUF];
        loop {
            match read_blocking(&out_read, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    // shell 退出后 ConPTY 关掉写端，读到的是 ERROR_BROKEN_PIPE。
                    // 那不是错误，就是这条终端的 EOF。
                    if e.kind() != io::ErrorKind::BrokenPipe {
                        tracing::debug!(pid, error = %e, "读伪控制台失败，泵结束");
                    }
                    break;
                }
            }
        }
    });

    while let Some(chunk) = rx.recv().await {
        let mut sent = 0;
        while sent < chunk.len() {
            if sink.writable().await.is_err() {
                tracing::debug!(pid, "主进程侧已关闭，泵结束");
                reader.abort();
                return;
            }
            match sink.try_write(&chunk[sent..]) {
                Ok(0) => break,
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    tracing::debug!(pid, error = %e, "写终端通道失败，泵结束");
                    reader.abort();
                    return;
                }
            }
        }
    }
    let _ = reader.await;
    // 让主进程读到 EOF——那是它判断「shell 退出了」的唯一信号。命名管道没有
    // 半关，整条管道要等 worker 侧的两个持有者都放手才断开：本泵返回后，
    // `open` 里包着它的任务会把输入泵也停掉（见那里的说明），主进程的读随即
    // 返回 ERROR_BROKEN_PIPE，主进程侧的泵把它等同于干净 EOF。
    tracing::debug!(pid, "终端输出泵结束");
}

/// 主进程的输入（键盘）→ shell。
async fn pump_socket_to_console(in_write: Owned, src: Arc<IpcChannel>, pid: u32) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let writer = tokio::task::spawn_blocking(move || {
        while let Some(chunk) = rx.blocking_recv() {
            let mut written = 0;
            while written < chunk.len() {
                match write_blocking(&in_write, &chunk[written..]) {
                    Ok(0) => return,
                    Ok(n) => written += n,
                    Err(e) => {
                        tracing::debug!(pid, error = %e, "写伪控制台失败，泵结束");
                        return;
                    }
                }
            }
        }
    });

    let mut buf = vec![0u8; PUMP_BUF];
    loop {
        if src.readable().await.is_err() {
            break;
        }
        let n = match src.try_read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break,
            Err(e) => {
                tracing::debug!(pid, error = %e, "读终端通道失败，泵结束");
                break;
            }
        };
        if tx.send(buf[..n].to_vec()).await.is_err() {
            break;
        }
    }
    drop(tx);
    let _ = writer.await;
    // 这里**不关伪控制台、也不杀 shell**：主进程断开只意味着「浏览器没在看」，
    // 终端要继续活着（`roadmap/03-terminal.md` §4.3：刷新页面后能重连）。
    // 真正的关闭只由 `term.close` 触发。
    tracing::debug!(pid, "主进程侧输入通道已关闭，终端继续运行");
}

/// 同步读一个管道句柄。
fn read_blocking(h: &Owned, buf: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;

    let mut got: u32 = 0;
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: h 有效；buf 有 len 字节可写；lpOverlapped 为空即同步读。
    let ok = unsafe { ReadFile(h.raw(), buf.as_mut_ptr(), len, &raw mut got, std::ptr::null_mut()) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(got as usize)
}

/// 同步写一个管道句柄。
fn write_blocking(h: &Owned, buf: &[u8]) -> io::Result<usize> {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;

    let mut put: u32 = 0;
    let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
    // SAFETY: h 有效；buf 有 len 字节可读；lpOverlapped 为空即同步写。
    let ok = unsafe { WriteFile(h.raw(), buf.as_ptr(), len, &raw mut put, std::ptr::null_mut()) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(put as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认_shell_可解析且在白名单里() {
        let shell = resolve_shell(None).expect("应能解析出默认 shell");
        assert!(shell.is_absolute());
        let name = shell
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_ascii_lowercase();
        assert!(ALLOWED_SHELLS.contains(&name.as_str()), "{name}");
    }

    #[test]
    fn 白名单之外的程序被拒() {
        let err = resolve_shell(Some(r"C:\Windows\System32\notepad.exe")).unwrap_err();
        assert!(err.message.contains("不是允许的 shell"), "{}", err.message);
    }

    #[test]
    fn 相对路径被拒() {
        // 名字在白名单里，但不是绝对路径。
        let err = resolve_shell(Some("cmd.exe")).unwrap_err();
        assert!(err.message.contains("绝对路径"), "{}", err.message);
    }

    #[test]
    fn 尺寸为零被拒() {
        let table = TerminalTable::new();
        let params = TermOpenParams {
            shell: None,
            user: None,
            cols: 0,
            rows: 24,
        };
        let err = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(table.open(params))
            .unwrap_err();
        assert!(err.message.contains("必须为正"), "{}", err.message);
    }

    /// 请求一个不是自己的用户必须被明确拒绝，而不是悄悄忽略参数。
    #[test]
    fn 请求他人身份被拒() {
        let me = token::TokenIdentity {
            sid: "S-1-5-21-1-2-3-1001".to_owned(),
            account: Some(token::AccountName {
                name: "alice".to_owned(),
                domain: "WS".to_owned(),
            }),
            uid: 1001,
            gid: 513,
            groups: Vec::new(),
            group_rids: Vec::new(),
        };
        assert!(check_target(None, &me).is_ok());
        assert!(check_target(Some("alice"), &me).is_ok());
        assert!(check_target(Some("WS\\alice"), &me).is_ok());
        assert!(check_target(Some("ALICE"), &me).is_ok(), "比较应忽略大小写");
        let err = check_target(Some("bob"), &me).unwrap_err();
        assert!(err.message.contains("无法开 bob 的终端"), "{}", err.message);
    }

    /// 真机端到端：开一个 cmd.exe，收到它的输出，再关掉。
    #[tokio::test]
    async fn 开关一个真实终端() {
        let table = TerminalTable::with_linger(Duration::from_millis(200));
        let params = TermOpenParams {
            shell: None,
            user: None,
            cols: 80,
            rows: 24,
        };
        let (result, main_side) = match table.open(params).await {
            Ok(v) => v,
            // ConPTY 需要 Windows 10 1809+。更老的系统上跳过而不是失败。
            Err(e) => {
                eprintln!("跳过：本机开不出伪控制台（{}）", e.message);
                return;
            }
        };
        assert!(result.pid > 0);
        assert_eq!(table.len().await, 1);

        // shell 启动后总会打印点什么（版本横幅 / 提示符）。
        let main = unsafe { IpcChannel::from_client_handle(main_side) }
            .expect("主进程侧应能注册到 tokio");
        let mut buf = vec![0u8; 256];
        let got = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                main.readable().await.ok()?;
                match main.try_read(&mut buf) {
                    Ok(0) => return None,
                    Ok(n) => return Some(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => return None,
                }
            }
        })
        .await;
        assert!(
            matches!(got, Ok(Some(n)) if n > 0),
            "应能从终端读到输出，实际 {got:?}"
        );

        let closed = table
            .close(TermCloseParams { pid: result.pid })
            .await
            .expect("关终端应成功");
        assert!(closed.exit.is_some(), "应能取到退出码");
        assert!(table.is_empty().await);
    }

    /// 表里没有的 pid 一律 not_found —— 这是「不能拿 RPC 杀任意进程」的那道闸。
    #[tokio::test]
    async fn 未登记的_pid_无法_resize_或_close() {
        let table = TerminalTable::new();
        let err = table
            .resize(TermResizeParams {
                pid: 4,
                cols: 80,
                rows: 24,
            })
            .await
            .unwrap_err();
        assert!(err.message.contains("没有 pid 为 4 的终端"), "{}", err.message);

        let err = table.close(TermCloseParams { pid: 4 }).await.unwrap_err();
        assert!(err.message.contains("没有 pid 为 4 的终端"), "{}", err.message);
    }

    /// 剥掉 ANSI 转义序列，只留渲染出来的可见文本。
    ///
    /// ConPTY 的输出流里混着大量**自带数字**的控制序列（光标定位、擦除 N 个字符），
    /// 不剥掉它们，「输出里出现过 132」就什么都证明不了——那个 132 极可能来自
    /// `ESC [ 132 X`，而不是 shell 真的看到了 132 列。
    ///
    /// 非 ASCII 字节按原值塞进字符串（够用即可：只拿它找十进制数字）。
    fn strip_ansi(raw: &[u8]) -> String {
        let mut out = String::new();
        let mut it = raw.iter().copied();
        while let Some(b) = it.next() {
            if b != 0x1b {
                out.push(b as char);
                continue;
            }
            match it.next() {
                // CSI：参数与中间字节之后，终止于 0x40..=0x7e。
                Some(b'[') => {
                    for b in it.by_ref() {
                        if (0x40..=0x7e).contains(&b) {
                            break;
                        }
                    }
                }
                // OSC：到 BEL 或 ESC \ 为止。
                Some(b']') => {
                    while let Some(b) = it.next() {
                        if b == 0x07 {
                            break;
                        }
                        if b == 0x1b {
                            it.next();
                            break;
                        }
                    }
                }
                // 其余两字节转义序列，第二个字节已经吃掉了。
                _ => {}
            }
        }
        out
    }

    /// 文本里所有连续的十进制数字段。
    fn numbers(text: &str) -> Vec<&str> {
        text.split(|c: char| !c.is_ascii_digit())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// 把一整块字节写进终端通道。
    async fn feed(main: &IpcChannel, data: &[u8]) {
        let mut sent = 0;
        while sent < data.len() {
            main.writable().await.expect("终端通道应可写");
            match main.try_write(&data[sent..]) {
                Ok(0) => panic!("终端通道写入 0 字节"),
                Ok(n) => sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => panic!("写终端通道失败：{e}"),
            }
        }
    }

    /// `resize` 之后 shell 必须真的看到新尺寸。
    ///
    /// `ResizePseudoConsole` 返回成功只说明伪控制台自己的缓冲区改了，
    /// 而真正要的是**子进程那一侧**也跟着变——两者之间还隔着 ConPTY 把尺寸
    /// 变化投递给已附着客户端的这一步，它没生效的话前端画的窗口和 shell 的
    /// 换行位置就此永久错位，而且从我们这边看一切正常。
    ///
    /// 所以这里不看返回值，改让 cmd.exe 自己用 `mode con` 去问控制台 API，
    /// 以它报出来的数字作准。`mode con` 的标签是本地化的，数字不是，所以只比数字。
    #[tokio::test]
    async fn 改尺寸后_shell_看到新尺寸() {
        // 刻意挑两个不常见的值：80/24/25 这类默认值就算蒙对也说明不了问题。
        const COLS: u16 = 132;
        const ROWS: u16 = 47;

        let table = TerminalTable::with_linger(Duration::from_millis(200));
        // 这条测试依赖 cmd.exe 的语法，所以不用 `%COMSPEC%` 而是点名它。
        let params = TermOpenParams {
            shell: Some(FALLBACK_SHELL.to_owned()),
            user: None,
            cols: 80,
            rows: 24,
        };
        let (result, main_side) = match table.open(params).await {
            Ok(v) => v,
            // ConPTY 需要 Windows 10 1809+；没有 cmd.exe 的系统也走这条。
            Err(e) => {
                eprintln!("跳过：本机开不出 cmd.exe 的伪控制台（{}）", e.message);
                return;
            }
        };
        let main = unsafe { IpcChannel::from_client_handle(main_side) }
            .expect("主进程侧应能注册到 tokio");

        // 先等 shell 真的起来：尺寸变化只对**已经附着上来**的客户端有意义，
        // 在 cmd.exe 连上伪控制台之前改尺寸，测的就不是投递那一步了。
        let mut buf = vec![0u8; PUMP_BUF];
        let started = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                main.readable().await.ok()?;
                match main.try_read(&mut buf) {
                    Ok(0) => return None,
                    Ok(n) => return Some(n),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(_) => return None,
                }
            }
        })
        .await;
        assert!(
            matches!(started, Ok(Some(n)) if n > 0),
            "shell 应先打印点什么，实际 {started:?}"
        );

        table
            .resize(TermResizeParams {
                pid: result.pid,
                cols: COLS,
                rows: ROWS,
            })
            .await
            .expect("改尺寸应成功");

        feed(&main, b"mode con\r\n").await;

        // 只看发出命令之后收到的字节：之前的横幅与提示符里也可能带数字。
        let outcome = tokio::time::timeout(Duration::from_secs(15), async {
            let mut seen: Vec<u8> = Vec::new();
            let mut buf = vec![0u8; PUMP_BUF];
            loop {
                if main.readable().await.is_err() {
                    return Err(strip_ansi(&seen));
                }
                match main.try_read(&mut buf) {
                    Ok(0) => return Err(strip_ansi(&seen)),
                    Ok(n) => {
                        seen.extend_from_slice(&buf[..n]);
                        let text = strip_ansi(&seen);
                        let nums = numbers(&text);
                        if nums.contains(&"132") && nums.contains(&"47") {
                            return Ok(());
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(format!("读终端通道失败：{e}\n{}", strip_ansi(&seen))),
                }
            }
        })
        .await;

        // 先把终端收掉，免得断言失败时留下进程。
        let _ = table.close(TermCloseParams { pid: result.pid }).await;

        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(text)) => panic!("终端提前结束，没等到 {COLS}x{ROWS}；已收到：\n{text}"),
            Err(_) => panic!("15 秒内 shell 没报出 {COLS}x{ROWS}，resize 没有传到子进程"),
        }
    }
}
