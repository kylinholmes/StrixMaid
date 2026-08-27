//! 在一条已经打开的 pty 从设备上 fork + 切换身份 + exec 登录 shell
//! （`roadmap/03-terminal.md` §4.2、§5）。
//!
//! # 与 helper 的 `spawn.rs` 是同一套次序，不是巧合
//!
//! `initgroups` → `setgid` → `setuid`：`setuid` 一旦放弃 root，后两步就再也做不了；
//! 补充组必须在还是 root 时装好。次序写反不会报错，只会**悄悄留下多余的特权**
//! （附加组没换掉），这类问题在测试里几乎看不见，所以两处必须一模一样。
//!
//! # 为什么这里不能直接调 `initgroups`
//!
//! helper 是单线程进程，fork 之后在子进程里走 NSS（`initgroups` 内部要查
//! `/etc/group`、可能连 LDAP / opendirectoryd）是安全的。**worker 不是**：它跑在
//! 多线程的 tokio 运行时上，`fork` 只复制调用线程，其它线程持有的 malloc 锁与
//! NSS 内部锁会以「永远锁着」的状态被复制过来——子进程里再去 malloc 就是死锁，
//! 而且是概率性的、极难复现的那种。
//!
//! 因此这里把顺序拆开：**在 fork 之前**用 `getgrouplist` 把组列表算好（那时还在
//! 正常的多线程环境里，随便分配内存），fork 之后的子进程只调用
//! `setgroups`/`setgid`/`setuid` 这类纯系统调用。同理，argv / envp / 路径全部
//! 在 [`PreparedExec::new`] 里备成 `CString`，子进程一次分配都不做。
//!
//! # 子进程里做的事，一件都不能少
//!
//! | 步骤 | 少了会怎样 |
//! |---|---|
//! | `setsid` | shell 不是会话首领，拿不到控制终端，Ctrl-C 之类全失效 |
//! | `TIOCSCTTY` | 同上；且窗口大小变化不会产生 `SIGWINCH` |
//! | `dup2` 从设备 → 0/1/2 | shell 没有 stdin/stdout |
//! | 关掉 3 号以上的 fd | **shell 会继承 worker 与主进程之间的 IPC socket**（fd 3 是 helper `dup2` 上去的，没有 `CLOEXEC`），用户在终端里就能直接对主进程发 RPC |
//! | 恢复信号处置与信号屏蔽字 | Rust 运行时把 `SIGPIPE` 设成了 `SIG_IGN`，**这个处置会跨 `execve` 保留**，于是 `yes \| head` 这类管道在 shell 里不再正常终止 |

use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::path::Path;

use nix::unistd::{ForkResult, Gid, Pid, Uid, fork};

/// 要切换到的身份。`None` 表示保持 worker 自身身份（user worker 的常态）。
#[derive(Debug, Clone)]
pub struct Identity {
    /// 用户名，`getgrouplist` 用它查补充组。
    pub username: String,
    pub uid: Uid,
    pub gid: Gid,
}

/// 一次 exec 的全部输入。借用形式，真正的所有权在 [`PreparedExec`] 里。
pub struct ExecSpec<'a> {
    /// 要 exec 的可执行文件（shell 的绝对路径）。
    pub program: &'a Path,
    /// `argv[0]`。登录 shell 的约定是在文件名前加 `-`（`login(1)` 就是这么做的），
    /// shell 据此决定要不要读 `/etc/profile` 一类的登录期配置。
    pub argv0: &'a str,
    /// 完整环境（不是增量：子进程只会看到这里列出的变量）。
    pub env: &'a [(String, String)],
    /// 工作目录；不可用时回落到 `/`。
    pub cwd: &'a Path,
    /// 目标身份；`None` 表示不切换。
    pub identity: Option<&'a Identity>,
}

/// 子进程失败时的退出码。父进程从 `waitpid` 的退出码就能分辨死在哪一步——
/// fork 之后没有任何别的通道可以报错。
const EXIT_SETUP_FAILED: i32 = 125;
const EXIT_IDENTITY_DENIED: i32 = 126;
const EXIT_EXEC_FAILED: i32 = 127;

/// 把子进程退出码翻译成可读原因。
pub fn describe_exit(code: i32) -> &'static str {
    match code {
        EXIT_SETUP_FAILED => "终端 fd 准备失败",
        EXIT_IDENTITY_DENIED => "切换身份被拒绝",
        EXIT_EXEC_FAILED => "exec shell 失败",
        _ => "shell 自行退出",
    }
}

/// fork 之前备好的一切。构造它可以随意分配内存；[`PreparedExec::spawn_on_tty`]
/// 之后的子进程分支则一次分配都不做。
pub struct PreparedExec {
    program: CString,
    /// `argv` 的 `CString` 必须活到 `execve` 之后，所以和指针数组一起存着。
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
    envp_ptrs: Vec<*const libc::c_char>,
    cwd: CString,
    identity: Option<PreparedIdentity>,
    /// 子进程要关到几号 fd 为止（不含）。
    fd_ceiling: RawFd,
}

// `*const c_char` 让编译器保守地认为不能跨线程；这些指针指向的是本结构体自己
// 拥有的 `CString`，随结构体一起移动，因此送进别的线程是安全的。
//
// SAFETY: 指针只指向 `_argv` / `_envp` 里的数据，二者与指针数组同生共死；
// 结构体不提供任何暴露内部可变性的接口。
unsafe impl Send for PreparedExec {}

struct PreparedIdentity {
    uid: libc::uid_t,
    gid: libc::gid_t,
    /// fork **之前**算好的补充组，子进程只需一次 `setgroups`。
    groups: Vec<libc::gid_t>,
}

impl PreparedExec {
    /// 把 `spec` 编译成可以安全地在 fork 之后使用的形式。
    ///
    /// 这里顺手做一次「可执行文件存在且可执行」的检查：exec 失败只能表现为
    /// 「终端刚开就没了」，用户看不出原因；提前查一次就能给出准确的报错。
    pub fn new(spec: &ExecSpec<'_>) -> Result<Self, String> {
        let program = cstring(spec.program.as_os_str().as_encoded_bytes(), "shell 路径")?;
        // SAFETY: program 是有效的 NUL 结尾字符串；access 只读文件系统元数据。
        if unsafe { libc::access(program.as_ptr(), libc::X_OK) } != 0 {
            return Err(format!(
                "{} 不存在或不可执行: {}",
                spec.program.display(),
                std::io::Error::last_os_error()
            ));
        }

        let argv = vec![cstring(spec.argv0.as_bytes(), "argv[0]")?];
        let envp: Vec<CString> = spec
            .env
            .iter()
            .map(|(k, v)| cstring(format!("{k}={v}").as_bytes(), "环境变量"))
            .collect::<Result<_, _>>()?;
        let cwd = cstring(spec.cwd.as_os_str().as_encoded_bytes(), "工作目录")?;

        let identity = match spec.identity {
            None => None,
            Some(id) => Some(PreparedIdentity {
                uid: id.uid.as_raw(),
                gid: id.gid.as_raw(),
                groups: group_list(&id.username, id.gid)?,
            }),
        };

        let argv_ptrs = null_terminated(&argv);
        let envp_ptrs = null_terminated(&envp);
        Ok(PreparedExec {
            program,
            _argv: argv,
            _envp: envp,
            argv_ptrs,
            envp_ptrs,
            cwd,
            identity,
            fd_ceiling: fd_ceiling(),
        })
    }

    /// fork 出子进程，把 `slave` 装成它的控制终端并 exec。返回子进程 pid。
    ///
    /// 子进程调用了 `setsid`，因此它同时是会话首领与进程组首领，**pgid == pid**——
    /// 关终端时对 `-pid` 发 `SIGHUP` 就能带走 shell 拉起的整棵进程树。
    pub fn spawn_on_tty(&self, slave: BorrowedFd<'_>) -> Result<Pid, String> {
        let slave_fd = slave.as_raw_fd();

        // SAFETY: 子进程分支只做系统调用（dup2 / setsid / ioctl / close /
        // signal / setgroups 族 / chdir / execve / _exit），不分配内存、不取锁，
        // 因此多线程进程里 fork 的限制得到满足；所有参数都已在 `new` 里备好。
        match unsafe { fork() } {
            Err(e) => Err(format!("fork 失败: {e}")),
            Ok(ForkResult::Parent { child }) => Ok(child),
            Ok(ForkResult::Child) => {
                // 从这里到 execve 之间任何失败都 _exit，绝不 return——
                // 返回到 Rust 的正常控制流里意味着多出一个「假 worker」。
                // SAFETY: 见上。
                unsafe {
                    // 1. 成为新会话的首领。必须在拿控制终端之前，否则内核会拒绝。
                    if libc::setsid() < 0 {
                        libc::_exit(EXIT_SETUP_FAILED);
                    }
                    // 2. 从设备 → stdin / stdout / stderr。
                    for target in 0..3 {
                        if libc::dup2(slave_fd, target) < 0 {
                            libc::_exit(EXIT_SETUP_FAILED);
                        }
                    }
                    // 3. 认领控制终端。少了它 SIGINT / SIGWINCH 都不会送到 shell。
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                        libc::_exit(EXIT_SETUP_FAILED);
                    }
                    // 4. 关掉 3 号及以上的所有 fd（含刚才那份从设备副本与 IPC fd 3）。
                    //    worker 自己开的 fd 都带 CLOEXEC，这一步防的是**不带**
                    //    CLOEXEC 的那些：helper `dup2` 上来的 fd 3 正是其中之一。
                    let mut fd = 3;
                    while fd < self.fd_ceiling {
                        libc::close(fd);
                        fd += 1;
                    }
                    // 5. 信号处置与屏蔽字回到默认。二者都会跨 execve 保留，
                    //    继承 Rust 运行时的 SIGPIPE=SIG_IGN 会让 shell 里的管道行为变样。
                    for signo in [
                        libc::SIGPIPE,
                        libc::SIGCHLD,
                        libc::SIGHUP,
                        libc::SIGINT,
                        libc::SIGQUIT,
                        libc::SIGTERM,
                        libc::SIGALRM,
                        libc::SIGTSTP,
                        libc::SIGTTIN,
                        libc::SIGTTOU,
                    ] {
                        libc::signal(signo, libc::SIG_DFL);
                    }
                    let empty: libc::sigset_t = std::mem::zeroed();
                    libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());

                    // 6. 切换身份：补充组 → gid → uid，一步都不能提前。
                    if let Some(id) = &self.identity {
                        if set_groups(&id.groups) != 0 {
                            libc::_exit(EXIT_IDENTITY_DENIED);
                        }
                        if libc::setgid(id.gid) != 0 {
                            libc::_exit(EXIT_IDENTITY_DENIED);
                        }
                        if libc::setuid(id.uid) != 0 {
                            libc::_exit(EXIT_IDENTITY_DENIED);
                        }
                        // 自检：放弃 root 之后必须再也拿不回来。拿得回来说明
                        // 上面某一步没有真正生效，此时 exec 出去的是一个
                        // 「看起来是普通用户、实则能变回 root」的 shell。
                        if libc::setuid(0) == 0 {
                            libc::_exit(EXIT_IDENTITY_DENIED);
                        }
                    }

                    // 7. 工作目录：家目录，不可用则 /。
                    if libc::chdir(self.cwd.as_ptr()) != 0 {
                        let root = c"/";
                        let _ = libc::chdir(root.as_ptr());
                    }

                    libc::execve(
                        self.program.as_ptr(),
                        self.argv_ptrs.as_ptr(),
                        self.envp_ptrs.as_ptr(),
                    );
                    libc::_exit(EXIT_EXEC_FAILED);
                }
            }
        }
    }
}

/// `setgroups` 的两平台差异：BSD（含 macOS）第一个参数是 `c_int`，
/// Linux 是 `size_t`。nix 在 Apple 上根本没有这个包装（它认为组关系应当走
/// opendirectoryd），所以只能直接调 libc。
///
/// # Safety
///
/// 在 fork 之后的子进程里调用，只做一次系统调用。
unsafe fn set_groups(groups: &[libc::gid_t]) -> libc::c_int {
    #[cfg(target_os = "linux")]
    let n = groups.len();
    #[cfg(not(target_os = "linux"))]
    let n = groups.len() as libc::c_int;
    // SAFETY: n 如实描述 groups 的长度，指针来自一个仍然存活的切片。
    unsafe { libc::setgroups(n, groups.as_ptr()) }
}

/// 查 `username` 的补充组列表（含 `gid` 本身）。**必须在 fork 之前调用**。
///
/// `getgrouplist` 的原型两平台不同：macOS 沿用更老的 BSD 版本，组用 `c_int`
/// 表示；Linux 用 `gid_t`。二者都是 32 位，差别只在符号，转换是安全的。
fn group_list(username: &str, gid: Gid) -> Result<Vec<libc::gid_t>, String> {
    let name = cstring(username.as_bytes(), "用户名")?;

    #[cfg(target_os = "linux")]
    type RawGroup = libc::gid_t;
    #[cfg(not(target_os = "linux"))]
    type RawGroup = libc::c_int;

    // 组数未知，按「问一次、不够就翻倍」来。上限 1024 组远超任何真实系统，
    // 到了还不够就说明查询本身有问题，与其无限扩张不如报错。
    let mut capacity = 16;
    loop {
        let mut buf = vec![0 as RawGroup; capacity];
        let mut n = capacity as libc::c_int;
        // SAFETY: name 是有效的 NUL 结尾字符串；buf 有 n 个元素的容量，
        // 内核最多写 n 个。
        let ret = unsafe {
            libc::getgrouplist(
                name.as_ptr(),
                gid.as_raw() as RawGroup,
                buf.as_mut_ptr(),
                &mut n,
            )
        };
        if ret >= 0 {
            // Linux 返回组数，BSD 返回 0；两者都把真实组数写回了 n。
            let len = (n.max(0) as usize).min(buf.len());
            return Ok(buf[..len].iter().map(|g| *g as libc::gid_t).collect());
        }
        if capacity >= 1024 {
            return Err(format!("查询 {username} 的补充组失败：组数超过 1024"));
        }
        capacity *= 2;
    }
}

/// 子进程里 `close` 循环的上界。
///
/// 取 `RLIMIT_NOFILE` 的软限制，但夹在 `[64, 65536]`：系统把它设成
/// `infinity`（systemd 上很常见）时，逐个 close 会跑上几分钟。65536 次
/// `close` 只要几毫秒，而 worker 实际打开的 fd 是两位数——上界更多是保险，
/// 真正的防线是「自己开的 fd 一律 CLOEXEC」。
fn fd_ceiling() -> RawFd {
    // SAFETY: 只读进程自身的 rlimit。
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: lim 是有效的可写内存。
    let cur = if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } == 0 {
        lim.rlim_cur
    } else {
        1024
    };
    cur.clamp(64, 65536) as RawFd
}

fn null_terminated(items: &[CString]) -> Vec<*const libc::c_char> {
    items
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn cstring(bytes: &[u8], what: &str) -> Result<CString, String> {
    CString::new(bytes).map_err(|_| format!("{what}含 NUL 字节"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 不可执行的_program_在_fork_之前就被拒() {
        let env: Vec<(String, String)> = vec![];
        let spec = ExecSpec {
            program: Path::new("/nonexistent/definitely-not-a-shell"),
            argv0: "-sh",
            env: &env,
            cwd: Path::new("/"),
            identity: None,
        };
        // `PreparedExec` 里放着裸指针，没有 `Debug`，所以不能用 `unwrap_err`。
        let Err(err) = PreparedExec::new(&spec) else {
            panic!("不存在的可执行文件必须在 fork 之前就被拒");
        };
        assert!(err.contains("不存在或不可执行"), "{err}");
    }

    /// 组列表至少要包含 gid 本身——这是 `getgrouplist` 的契约，
    /// 拿不到它说明查询整个失败了，而子进程会拿这份列表去 `setgroups`。
    #[test]
    fn 补充组列表包含基础组() {
        let me = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap();
        let groups = group_list(&me.name, me.gid).unwrap();
        assert!(
            groups.contains(&me.gid.as_raw()),
            "{} 的组列表 {:?} 里没有基础组 {}",
            me.name,
            groups,
            me.gid
        );
    }

    #[test]
    fn 退出码有可读的解释() {
        assert!(describe_exit(EXIT_IDENTITY_DENIED).contains("身份"));
        assert!(describe_exit(EXIT_EXEC_FAILED).contains("exec"));
        assert!(describe_exit(0).contains("自行退出"));
    }

    #[test]
    fn fd_上界被夹在合理区间() {
        let c = fd_ceiling();
        assert!((64..=65536).contains(&c), "上界 {c} 越界");
    }
}
