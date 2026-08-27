//! `strixmaid-helper`：动态链接的最小特权进程（design.md §2.1 / §10）。
//!
//! 职责只有三件：PAM 认证（challenge-response 经 IPC 往返主进程）、以认证身份
//! fork + setuid + exec `strixmaid worker`、持有 PAM 会话句柄直到登出。
//!
//! # 生命周期
//!
//! ```text
//! 主进程 spawn helper（fd 3 = socketpair）
//!   → AuthStart → [Prompts ⇄ AuthRespond]* → AuthOk | AuthFail(退出)
//!   → SpawnWorker → WorkerSpawned + SCM_RIGHTS(fd)
//!   → … 空转，持有 PAM 句柄 …
//!   → CloseSession | fd 3 断开 → pam_close_session + pam_end → 退出
//! ```
//!
//! 同步、单线程、不用 tokio：整个进程一次只做一件事，阻塞读 fd 3 就是它的事件循环。
//!
//! # 安全约定
//!
//! stderr 只记事件（见 [`log`]），不记任何消息内容；明文密码只在 `Zeroizing<String>`
//! 与 PAM 的 `malloc` 缓冲里短暂存在（见 [`pam`]）。

mod ipc;
mod log;
mod pam;
mod spawn;

use std::ffi::CString;
use std::path::PathBuf;

use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Group, Pid, Uid, User, getgrouplist};
use strixmaid_types::auth::AuthUser;
use strixmaid_types::ipc::{FromHelper, IPC_FD, ToHelper};

use crate::ipc::Ipc;
use crate::pam::Pam;
use crate::spawn::{WorkerSpec, spawn_worker};

/// 认证通过后记住的身份，供 `SpawnWorker` 使用。
struct Identity {
    user: AuthUser,
    home: PathBuf,
    shell: PathBuf,
}

/// 一个 helper 进程的全部状态。
struct Helper {
    ipc: Ipc,
    pam: Option<Pam>,
    identity: Option<Identity>,
    /// 主二进制路径（`AuthStart` 告知，缺省取 helper 同目录下的 `strixmaid`）。
    worker_exe: PathBuf,
    /// 已 fork 的 worker，退出时回收。
    workers: Vec<Pid>,
}

/// 主循环的退出方式。
enum Exit {
    /// 主进程关闭了通道或发来 `CloseSession`：正常清理后退出。
    Normal,
    /// 认证失败 / 协议错误：清理后以非零码退出。
    Failed,
}

fn main() {
    let ipc = match Ipc::from_inherited_fd(IPC_FD) {
        Ok(ipc) => ipc,
        Err(e) => {
            log::event(&e);
            std::process::exit(2);
        }
    };
    log::event("启动，等待 AuthStart");

    let mut helper = Helper {
        ipc,
        pam: None,
        identity: None,
        worker_exe: default_worker_exe(),
        workers: Vec::new(),
    };

    let exit = helper.run();
    helper.shutdown();
    match exit {
        Exit::Normal => log::event("退出"),
        Exit::Failed => {
            log::event("异常退出");
            std::process::exit(1);
        }
    }
}

/// helper 同目录下的 `strixmaid`。
fn default_worker_exe() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("strixmaid")))
        .unwrap_or_else(|| PathBuf::from("strixmaid"))
}

impl Helper {
    /// 事件循环：读一条、处理一条，直到通道关闭或被要求退出。
    fn run(&mut self) -> Exit {
        loop {
            self.reap_workers();
            let msg = match self.ipc.recv() {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    log::event("主进程关闭了通道");
                    return Exit::Normal;
                }
                Err(e) => {
                    log::event(&format!("读取 IPC 失败: {e}"));
                    return Exit::Failed;
                }
            };
            let outcome = match msg {
                ToHelper::AuthStart {
                    service,
                    username,
                    worker_exe,
                    rhost,
                } => self.on_auth_start(service, username, worker_exe, rhost),
                ToHelper::AuthRespond { .. } => {
                    // 只在 conversation 回调里等它；到这里说明主进程乱序了。
                    self.reply_error("没有进行中的 PAM 提示，AuthRespond 被忽略")
                }
                ToHelper::SpawnWorker {
                    open_session,
                    as_root,
                } => self.on_spawn_worker(open_session, as_root),
                ToHelper::CloseSession => {
                    log::event("收到 CloseSession");
                    let _ = self.ipc.send(&FromHelper::SessionClosed);
                    return Exit::Normal;
                }
            };
            if let Some(exit) = outcome {
                return exit;
            }
        }
    }

    /// 回一条 `Error`；发送失败（主进程没了）就退出。
    fn reply_error(&mut self, message: &str) -> Option<Exit> {
        log::event(message);
        match self.ipc.send(&FromHelper::Error {
            message: message.to_string(),
        }) {
            Ok(()) => None,
            Err(_) => Some(Exit::Failed),
        }
    }

    // ------------------------------------------------------------ AuthStart

    fn on_auth_start(
        &mut self,
        service: String,
        username: String,
        worker_exe: Option<String>,
        rhost: Option<String>,
    ) -> Option<Exit> {
        if self.pam.is_some() {
            return self.reply_error("本 helper 已经完成过一次认证，不能重复 AuthStart");
        }
        if let Some(exe) = worker_exe {
            self.worker_exe = PathBuf::from(exe);
        }
        log::event(&format!("开始 PAM 认证，service={service}"));

        let mut pam = match Pam::start(&service, &username, rhost.as_deref()) {
            Ok(pam) => pam,
            Err(e) => {
                log::event(&format!("pam_start 失败: {e}"));
                let _ = self.ipc.send(&FromHelper::AuthFail {
                    reason: e.message.clone(),
                });
                return Some(Exit::Failed);
            }
        };

        match pam.authenticate(&mut self.ipc) {
            Ok(()) => {}
            Err(e) => {
                if let Some(ipc_err) = pam.take_ipc_error() {
                    // 回调里主进程已经断开或乱序，没有人收 AuthFail 了。
                    log::event(&format!("认证中止（IPC）: {ipc_err}"));
                    self.pam = Some(pam);
                    return Some(Exit::Failed);
                }
                log::event(&format!("认证失败: {}", e.func));
                let _ = self.ipc.send(&FromHelper::AuthFail { reason: e.message });
                self.pam = Some(pam);
                return Some(Exit::Failed);
            }
        }

        // PAM_USER 可能被模块改写（大小写规范化、别名映射），以它为准。
        let final_name = pam.user().unwrap_or(username);
        let identity = match lookup_identity(&final_name) {
            Ok(id) => id,
            Err(reason) => {
                log::event(&format!("认证通过但无法解析用户: {reason}"));
                let _ = self.ipc.send(&FromHelper::AuthFail { reason });
                self.pam = Some(pam);
                return Some(Exit::Failed);
            }
        };
        if pam.stashed_info_count() > 0 {
            log::event(&format!(
                "认证结束时仍有 {} 条未送出的信息消息，丢弃",
                pam.stashed_info_count()
            ));
        }
        log::event(&format!("认证通过，uid={}", identity.user.uid));

        let reply = FromHelper::AuthOk {
            user: identity.user.clone(),
        };
        self.pam = Some(pam);
        self.identity = Some(identity);
        match self.ipc.send(&reply) {
            Ok(()) => None,
            Err(e) => {
                log::event(&format!("发送 AuthOk 失败: {e}"));
                Some(Exit::Failed)
            }
        }
    }

    // ---------------------------------------------------------- SpawnWorker

    fn on_spawn_worker(&mut self, open_session: bool, as_root: bool) -> Option<Exit> {
        let (Some(pam), Some(identity)) = (self.pam.as_mut(), self.identity.as_ref()) else {
            return self.reply_error("尚未认证，不能 SpawnWorker");
        };

        // 开 PAM 会话；失败降级（非 root 下 pam_systemd / pam_loginuid 必然失败）。
        let mut session_error = None;
        if open_session && !pam.session_opened() {
            match pam.open_session(&mut self.ipc) {
                Ok(()) => log::event("pam_open_session 成功"),
                Err(e) => {
                    log::event(&format!("pam_open_session 失败，降级继续: {e}"));
                    session_error = Some(e.message);
                }
            }
        }
        let extra_env = if pam.session_opened() {
            pam.envlist()
        } else {
            Vec::new()
        };

        let spec = WorkerSpec {
            username: identity.user.username.clone(),
            uid: Uid::from_raw(identity.user.uid),
            gid: Gid::from_raw(identity.user.gid),
            home: identity.home.clone(),
            shell: identity.shell.clone(),
            as_root,
            extra_env,
        };
        let (pid, main_side) = match spawn_worker(&self.worker_exe, &spec) {
            Ok(v) => v,
            Err(message) => return self.reply_error(&format!("拉起 worker 失败: {message}")),
        };
        self.workers.push(pid);
        let uid = if as_root { 0 } else { identity.user.uid };
        log::event(&format!(
            "worker 已拉起，pid={pid} uid={uid} as_root={as_root}"
        ));

        let spawned = FromHelper::WorkerSpawned {
            pid: pid.as_raw(),
            uid,
            session_opened: pam.session_opened(),
            session_error,
        };
        if let Err(e) = self.ipc.send(&spawned) {
            log::event(&format!("发送 WorkerSpawned 失败: {e}"));
            return Some(Exit::Failed);
        }
        if let Err(e) = self.ipc.send_fd(&main_side) {
            log::event(&format!("SCM_RIGHTS 传递 fd 失败: {e}"));
            return Some(Exit::Failed);
        }
        // 主进程已拿到它的副本，本进程这份关掉。
        drop(main_side);
        None
    }

    // ------------------------------------------------------------- 清理

    /// 非阻塞回收已退出的 worker。
    fn reap_workers(&mut self) {
        self.workers
            .retain(|pid| match waitpid(*pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => true,
                Ok(WaitStatus::Exited(_, code)) => {
                    log::event(&format!(
                        "worker {pid} 退出，code={code}（{}）",
                        spawn::describe_exit(code)
                    ));
                    false
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    log::event(&format!("worker {pid} 被信号 {sig:?} 终止"));
                    false
                }
                Ok(_) => true,
                // ECHILD 等：已经不是我们的孩子了。
                Err(_) => false,
            });
    }

    /// 关会话、结束 PAM 句柄。主进程应当在此之前终止 worker。
    fn shutdown(&mut self) {
        // 给 worker 一点时间退出，让 waitpid 能回收，避免留下僵尸交给 init。
        for _ in 0..20 {
            self.reap_workers();
            if self.workers.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !self.workers.is_empty() {
            log::event(&format!(
                "{} 个 worker 仍在运行，交由 init 回收",
                self.workers.len()
            ));
        }
        if let Some(pam) = self.pam.take() {
            log::event("关闭 PAM 会话");
            pam.close();
        }
    }
}

/// 用 NSS（glibc `getpwnam` —— helper 是动态链接的，LDAP / SSSD 用户也能解析到）
/// 查出认证用户的 uid / gid / 家目录 / shell / 组列表。
fn lookup_identity(name: &str) -> Result<Identity, String> {
    let user = User::from_name(name)
        .map_err(|e| format!("getpwnam 失败: {e}"))?
        .ok_or_else(|| "PAM 通过了认证但 NSS 里查不到该用户".to_string())?;

    let c_name = CString::new(name).map_err(|_| "用户名含 NUL".to_string())?;
    let gids = getgrouplist(&c_name, user.gid).unwrap_or_else(|_| vec![user.gid]);
    let mut groups: Vec<String> = Vec::with_capacity(gids.len());
    for gid in gids {
        // 查不到名字的 gid 用数字兜底，比丢掉强。
        let gname = Group::from_gid(gid)
            .ok()
            .flatten()
            .map(|g| g.name)
            .unwrap_or_else(|| gid.to_string());
        if !groups.contains(&gname) {
            groups.push(gname);
        }
    }

    Ok(Identity {
        user: AuthUser {
            uid: user.uid.as_raw(),
            gid: user.gid.as_raw(),
            username: user.name.clone(),
            groups,
        },
        home: user.dir,
        shell: user.shell,
    })
}
