//! fork + 切换身份 + exec `strixmaid worker`（design.md §2.2 / §10）。
//!
//! # 身份切换的顺序不能错
//!
//! `initgroups` → `setgid` → `setuid`。`setuid` 一旦放弃 root，后两步就没有权限了；
//! `initgroups` 需要 root 才能设置附加组列表，所以它在最前。
//!
//! # fork 之后、exec 之前只做最少的事
//!
//! helper 是单线程进程，fork 后的子进程里调用 `initgroups`（内部走 NSS）与少量
//! 分配是安全的——这也是 login(1) / sshd 的做法。即便如此，argv / envp / 用户信息
//! 全部在 fork **之前**准备好，子进程只做 dup2 / 切身份 / chdir / execve 四件事，
//! 任何一步失败都 `_exit`，绝不返回到 Rust 的正常控制流里。

use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
use nix::unistd::{ForkResult, Gid, Pid, Uid, fork, geteuid, getuid};
use strixmaid_types::ipc::IPC_FD;

/// exec worker 时的目标身份与环境。
pub struct WorkerSpec {
    /// 用户名（`initgroups` 与 `USER` / `LOGNAME`）。
    pub username: String,
    /// 目标 uid。`as_root` 时忽略。
    pub uid: Uid,
    /// 目标 gid。`as_root` 时忽略。
    pub gid: Gid,
    /// 家目录（`HOME` 与工作目录）。
    pub home: PathBuf,
    /// 登录 shell（`SHELL`）。
    pub shell: PathBuf,
    /// 为 `true` 时不切换身份，以 helper 自己的（root）身份运行。
    pub as_root: bool,
    /// 额外环境变量（来自 `pam_getenvlist`），覆盖同名的基础变量。
    pub extra_env: Vec<(String, String)>,
}

/// 没有 pam_env 时的保底 PATH。
const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// 子进程失败时的退出码，主进程在 `waitpid` 里能据此区分原因。
const EXIT_SETUP_FAILED: i32 = 125;
const EXIT_IDENTITY_DENIED: i32 = 126;
const EXIT_EXEC_FAILED: i32 = 127;

/// 拉起一个 worker。返回 `(pid, 主进程侧的 socketpair fd)`。
pub fn spawn_worker(exe: &Path, spec: &WorkerSpec) -> Result<(Pid, OwnedFd), String> {
    let am_root = geteuid().is_root();

    // ---- 准入检查：在 fork 之前就把不可能成功的情况拒掉 ----
    if spec.as_root && !am_root {
        return Err("helper 不是 root，无法创建 admin worker".into());
    }
    if !spec.as_root && !am_root && spec.uid != getuid() {
        return Err(format!(
            "helper 不是 root，只能以自己（uid {}）的身份拉起 worker，无法切换到 uid {}",
            getuid(),
            spec.uid
        ));
    }

    // ---- 准备 argv / envp（全部在 fork 之前）----
    let c_exe = cstring(exe.as_os_str().as_encoded_bytes(), "worker 路径")?;
    let argv = [
        c_exe.clone(),
        cstring(b"worker", "argv")?,
        cstring(b"--ipc-fd", "argv")?,
        cstring(IPC_FD.to_string().as_bytes(), "argv")?,
    ];

    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), spec.home.to_string_lossy().into_owned()),
        ("USER".into(), spec.username.clone()),
        ("LOGNAME".into(), spec.username.clone()),
        ("SHELL".into(), spec.shell.to_string_lossy().into_owned()),
        ("PATH".into(), DEFAULT_PATH.into()),
    ];
    for (k, v) in &spec.extra_env {
        match env.iter_mut().find(|(ek, _)| ek == k) {
            Some(slot) => slot.1 = v.clone(),
            None => env.push((k.clone(), v.clone())),
        }
    }
    // 让 worker 的 tracing 级别可控：主进程的 RUST_LOG 透传。
    if let Ok(v) = std::env::var("RUST_LOG") {
        env.push(("RUST_LOG".into(), v));
    }
    let envp: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstring(format!("{k}={v}").as_bytes(), "环境变量"))
        .collect::<Result<_, _>>()?;
    let c_home = cstring(spec.home.as_os_str().as_encoded_bytes(), "HOME")?;
    let c_user = cstring(spec.username.as_bytes(), "用户名")?;
    let c_devnull = CString::new("/dev/null").expect("常量");

    // argv / envp 的 NULL 结尾指针数组。
    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let envp_ptrs: Vec<*const libc::c_char> = envp
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    // ---- socketpair：两端都 CLOEXEC，子进程里 dup2 到 fd 3 的那份自然不带 CLOEXEC ----
    let (main_side, worker_side) = socketpair(
        AddressFamily::Unix,
        SockType::Stream,
        None,
        SockFlag::SOCK_CLOEXEC,
    )
    .map_err(|e| format!("socketpair 失败: {e}"))?;

    let switch_identity = !spec.as_root && am_root;
    let uid = spec.uid;
    let gid = spec.gid;

    // SAFETY: 单线程进程；子进程分支只调用 dup2 / open / setuid 族 / chdir / execve / _exit，
    // 且所有参数都已在 fork 前准备完毕。
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // 从这里到 execve 之间任何失败都 _exit，绝不 return。
            // SAFETY: 见上。
            unsafe {
                // 1. worker 的 socketpair 一端 → fd 3（覆盖掉继承自 helper 的主进程通道）。
                if libc::dup2(worker_side.as_raw_fd(), IPC_FD) < 0 {
                    libc::_exit(EXIT_SETUP_FAILED);
                }
                // 2. stdin → /dev/null；stdout / stderr 继承（stderr 进 journald）。
                let devnull = libc::open(c_devnull.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                if devnull < 0 || libc::dup2(devnull, 0) < 0 {
                    libc::_exit(EXIT_SETUP_FAILED);
                }
                // 3. 切换身份：initgroups → setgid → setuid，顺序不能错。
                if switch_identity {
                    if libc::initgroups(c_user.as_ptr(), gid.as_raw()) != 0 {
                        libc::_exit(EXIT_IDENTITY_DENIED);
                    }
                    if libc::setgid(gid.as_raw()) != 0 {
                        libc::_exit(EXIT_IDENTITY_DENIED);
                    }
                    if libc::setuid(uid.as_raw()) != 0 {
                        libc::_exit(EXIT_IDENTITY_DENIED);
                    }
                    // 自检：放弃 root 后必须再也拿不回来。
                    if libc::setuid(0) == 0 {
                        libc::_exit(EXIT_IDENTITY_DENIED);
                    }
                }
                // 4. 工作目录：家目录，不可用则 /。
                if libc::chdir(c_home.as_ptr()) != 0 {
                    let root = c"/";
                    let _ = libc::chdir(root.as_ptr());
                }
                // 5. exec。
                libc::execve(c_exe.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
                libc::_exit(EXIT_EXEC_FAILED);
            }
        }
        Ok(ForkResult::Parent { child }) => {
            // 父进程不需要 worker 那一端。
            drop(worker_side);
            Ok((child, main_side))
        }
        Err(e) => Err(format!("fork 失败: {e}")),
    }
}

/// 把子进程退出码翻译成可读原因（供 `waitpid` 后记日志）。
pub fn describe_exit(code: i32) -> &'static str {
    match code {
        EXIT_SETUP_FAILED => "fd 准备失败",
        EXIT_IDENTITY_DENIED => "切换身份被拒绝",
        EXIT_EXEC_FAILED => "exec 失败（路径不存在或不可执行）",
        _ => "worker 自行退出",
    }
}

fn cstring(bytes: &[u8], what: &str) -> Result<CString, String> {
    CString::new(bytes).map_err(|_| format!("{what}含 NUL 字节"))
}

#[cfg(test)]
mod tests {
    //! 以自己的身份（非 root 下 setuid(自己) 是合法空操作）真的 fork + exec
    //! `strixmaid worker`，走 Hello → whoami → Shutdown 一整圈。
    //! 需要 `target/debug/strixmaid` 已构建；找不到就跳过（打印说明）。

    use super::*;
    use std::os::unix::net::UnixStream;
    use strixmaid_types::ipc::{self, FromWorker, METHOD_WHOAMI, ToWorker, WhoAmI};

    fn find_strixmaid() -> Option<PathBuf> {
        // 测试二进制在 target/<profile>/deps/ 下，strixmaid 在 target/<profile>/ 下。
        let exe = std::env::current_exe().ok()?;
        let mut dir = exe.parent()?;
        for _ in 0..3 {
            let candidate = dir.join("strixmaid");
            if candidate.is_file() {
                return Some(candidate);
            }
            dir = dir.parent()?;
        }
        None
    }

    #[test]
    fn spawn_真实_worker_并完成_whoami() {
        let Some(exe) = find_strixmaid() else {
            eprintln!("跳过：找不到 strixmaid 二进制（先 cargo build -p strixmaid-server）");
            return;
        };
        let me = nix::unistd::User::from_uid(getuid()).unwrap().unwrap();
        let spec = WorkerSpec {
            username: me.name.clone(),
            uid: me.uid,
            gid: me.gid,
            home: me.dir.clone(),
            shell: me.shell.clone(),
            as_root: false,
            extra_env: vec![("STRIXMAID_TEST_MARK".into(), "1".into())],
        };
        let (pid, fd) = spawn_worker(&exe, &spec).expect("spawn_worker");
        assert!(pid.as_raw() > 1);

        let mut s = UnixStream::from(fd);
        let hello: FromWorker = ipc::read_msg(&mut s).unwrap().expect("Hello");
        match hello {
            FromWorker::Hello { pid: hp, uid, gid } => {
                assert_eq!(hp, pid.as_raw());
                assert_eq!(uid, me.uid.as_raw());
                assert_eq!(gid, me.gid.as_raw());
            }
            other => panic!("第一帧应为 Hello，实际 {other:?}"),
        }
        ipc::write_msg(
            &mut s,
            &ToWorker::Call {
                id: 1,
                method: METHOD_WHOAMI.into(),
                params: serde_json::Value::Null,
            },
        )
        .unwrap();
        let reply: FromWorker = ipc::read_msg(&mut s).unwrap().unwrap();
        let who: WhoAmI = match reply {
            FromWorker::Result { id: 1, value } => serde_json::from_value(value).unwrap(),
            other => panic!("whoami 应答异常: {other:?}"),
        };
        assert_eq!(who.pid, pid.as_raw());
        assert_eq!(who.uid, me.uid.as_raw());
        assert_eq!(who.euid, me.uid.as_raw());
        assert_eq!(who.user.as_deref(), Some(me.name.as_str()));
        assert_eq!(who.home.as_deref(), Some(me.dir.to_string_lossy().as_ref()));
        // 工作目录 = HOME（家目录存在时）
        if me.dir.is_dir() {
            assert_eq!(std::path::Path::new(&who.cwd), me.dir.as_path());
        }

        ipc::write_msg(&mut s, &ToWorker::Shutdown).unwrap();
        // worker 关闭连接
        assert!(ipc::read_msg::<_, FromWorker>(&mut s).unwrap().is_none());
        let status = nix::sys::wait::waitpid(pid, None).unwrap();
        assert!(
            matches!(status, nix::sys::wait::WaitStatus::Exited(_, 0)),
            "worker 退出状态: {status:?}"
        );
    }
}
