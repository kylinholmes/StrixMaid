//! `strixmaid-helper`：动态链接的最小特权进程（design.md §2.1 / §10）。
//!
//! 职责只有三件：认证（challenge-response 经 IPC 往返主进程）、以认证身份拉起
//! `strixmaid worker`、持有系统会话句柄直到登出。三件事在两个平台上是两套完全
//! 不同的系统机制，但**协议状态机是同一个**，所以本文件里除了入口参数之外
//! 没有 `cfg`：
//!
//! | | Unix | Windows |
//! |---|---|---|
//! | 认证与会话（[`auth`]） | PAM | `LogonUserW` + `LoadUserProfileW` |
//! | 拉起 worker（[`spawn`]） | fork + setuid + exec | `CreateProcessAsUserW` |
//! | 与主进程的通道（[`ipc`]） | 继承的 fd 3 上的 socketpair | `--pipe` 指名的命名管道 |
//!
//! # 生命周期
//!
//! ```text
//! 主进程 spawn helper（Unix：fd 3 = socketpair；Windows：--pipe <名字>）
//!   → AuthStart → [Prompts ⇄ AuthRespond]* → AuthOk | AuthFail(退出)
//!   → SpawnWorker → WorkerSpawned（+ Unix 的 SCM_RIGHTS 帧）
//!   → … 空转，持有会话句柄 …
//!   → CloseSession | 通道断开 → 关会话 → 退出
//! ```
//!
//! 同步、单线程、不用 tokio：整个进程一次只做一件事，阻塞读通道就是它的事件循环。
//!
//! # 安全约定
//!
//! stderr 只记事件（见 [`log`]），不记任何消息内容；明文密码只在
//! `Zeroizing` 缓冲里短暂存在（见 [`auth`]）。提权资格的**权威判断**在本进程内做
//! （[`Helper::on_spawn_worker`]）——helper 才是持有特权的组件。

mod auth;
mod ipc;
mod log;
mod spawn;
#[cfg(windows)]
mod win32;

use std::path::PathBuf;

use strixmaid_types::auth::may_elevate;
use strixmaid_types::ipc::{FromHelper, ToHelper};

use crate::auth::{Identity, Session};
use crate::ipc::Ipc;
use crate::spawn::{WorkerProc, WorkerSpec, spawn_worker};

/// 一个 helper 进程的全部状态。
struct Helper {
    ipc: Ipc,
    session: Option<Session>,
    identity: Option<Identity>,
    /// 主二进制路径（`AuthStart` 告知，缺省取 helper 同目录下的 `strixmaid`）。
    worker_exe: PathBuf,
    /// 已拉起的 worker，退出时回收。
    workers: Vec<WorkerProc>,
    /// 允许提权的系统组，由 `AuthStart` 下发（helper 不读配置文件）。
    ///
    /// **提权资格的权威判断在本进程内做**：helper 才是持有特权的组件，
    /// 主进程那边的同款检查只算 UX（`roadmap/01-worker-execution.md` §4.8）。
    elevate_groups: Vec<String>,
}

/// 认证链上某一步失败时写进 stderr 的一行。
///
/// 两个平台的错误类型不同（PAM 返回码 / Win32 错误码），但都有 `func` 与
/// `message` 两个字段，所以格式化只需要一份。`message` 会原样送到浏览器，
/// 因此它必须是各平台**已经脱敏过**的文本——Unix 侧是 `pam_strerror`，
/// Windows 侧是 `auth::windows::logon_failure_message` 那张固定的表。
fn describe_auth_error(e: &auth::AuthError) -> String {
    format!("{} 失败：{}", e.func, e.message)
}

/// 把组名列表渲染成人能读的一行；空列表说清是「空」而不是打印一对空括号。
fn render_groups(groups: &[String]) -> String {
    if groups.is_empty() {
        "（空）".to_owned()
    } else {
        groups.join("、")
    }
}

/// 主循环的退出方式。
enum Exit {
    /// 主进程关闭了通道或发来 `CloseSession`：正常清理后退出。
    Normal,
    /// 认证失败 / 协议错误：清理后以非零码退出。
    Failed,
}

fn main() {
    let ipc = match open_ipc() {
        Ok(ipc) => ipc,
        Err(e) => {
            log::event(&e);
            std::process::exit(2);
        }
    };
    log::event("启动，等待 AuthStart");

    let mut helper = Helper {
        ipc,
        session: None,
        identity: None,
        worker_exe: default_worker_exe(),
        workers: Vec::new(),
        elevate_groups: Vec::new(),
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

/// Unix：接管主进程 `dup2` 到 [`strixmaid_types::ipc::IPC_FD`] 的 socketpair。
///
/// 这里没有参数要解析——fd 号是协议常量，helper 只认那一个。
#[cfg(unix)]
fn open_ipc() -> Result<Ipc, String> {
    Ipc::from_inherited_fd(strixmaid_types::ipc::IPC_FD)
}

/// Windows：连主进程建好的命名管道，名字由 `--pipe <名字>` 给出。
///
/// Windows 上没有「继承的 fd 号」这种约定，通道必须有个名字才找得到
/// （`strixmaid_core::session::helper` 的 `launch_impl` 就是这么传的）。
/// 手写 argv 解析而不是引 clap：helper 一共只认这一个参数，
/// 为它多一棵依赖树不划算，而依赖少正是这个进程的设计目标之一。
#[cfg(windows)]
fn open_ipc() -> Result<Ipc, String> {
    let name = parse_pipe_arg(std::env::args().skip(1))?;
    Ipc::connect(&name)
}

/// 从命令行参数里取出 `--pipe` 的值。
///
/// 认 `--pipe <名字>` 与 `--pipe=<名字>` 两种写法：前者是主进程实际用的，
/// 后者是人手敲命令行时的习惯，认下来不花什么代价。
/// 多余的参数一律报错而不是忽略：helper 的命令行完全由主进程构造，
/// 出现别的东西说明有人在手动运行它，那种情况下报错比「看起来跑起来了」有用。
#[cfg(windows)]
fn parse_pipe_arg(args: impl Iterator<Item = String>) -> Result<String, String> {
    let mut args = args;
    let Some(first) = args.next() else {
        return Err("缺少 --pipe 参数；helper 只能由 strixmaid 主进程拉起".to_owned());
    };
    let value = if let Some(v) = first.strip_prefix("--pipe=") {
        v.to_owned()
    } else if first == "--pipe" {
        args.next().unwrap_or_default()
    } else {
        return Err(format!(
            "无法识别的参数 {first}；helper 只接受 --pipe <管道名>，\
             且只能由 strixmaid 主进程拉起"
        ));
    };
    if value.is_empty() {
        return Err("--pipe 后面缺少管道名".to_owned());
    }
    if let Some(extra) = args.next() {
        return Err(format!("--pipe 之后还有多余的参数 {extra}"));
    }
    Ok(value)
}

/// helper 同目录下的 `strixmaid`（Windows 上是 `strixmaid.exe`）。
fn default_worker_exe() -> PathBuf {
    let name = format!("strixmaid{}", std::env::consts::EXE_SUFFIX);
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

impl Helper {
    /// 事件循环：读一条、处理一条，直到通道关闭或被要求退出。
    fn run(&mut self) -> Exit {
        loop {
            spawn::reap_workers(&mut self.workers);
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
                    elevate_groups,
                } => self.on_auth_start(service, username, worker_exe, rhost, elevate_groups),
                ToHelper::AuthRespond { .. } => {
                    // 只在索要凭据的那一轮里等它；到这里说明主进程乱序了。
                    self.reply_error("没有进行中的认证提示，AuthRespond 被忽略")
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
        elevate_groups: Vec<String>,
    ) -> Option<Exit> {
        if self.session.is_some() {
            return self.reply_error("本 helper 已经完成过一次认证，不能重复 AuthStart");
        }
        self.elevate_groups = elevate_groups;
        if let Some(exe) = worker_exe {
            self.worker_exe = PathBuf::from(exe);
        }
        log::event(&format!("开始认证，service={service}"));

        let mut session = match Session::start(&service, &username, rhost.as_deref()) {
            Ok(session) => session,
            Err(e) => {
                log::event(&describe_auth_error(&e));
                let _ = self.ipc.send(&FromHelper::AuthFail {
                    reason: e.message.clone(),
                });
                return Some(Exit::Failed);
            }
        };

        match session.authenticate(&mut self.ipc) {
            Ok(()) => {}
            Err(e) => {
                if let Some(ipc_err) = session.take_ipc_error() {
                    // 索要凭据时主进程已经断开或乱序，没有人收 AuthFail 了。
                    log::event(&format!("认证中止（IPC）: {ipc_err}"));
                    self.session = Some(session);
                    return Some(Exit::Failed);
                }
                log::event(&describe_auth_error(&e));
                let _ = self.ipc.send(&FromHelper::AuthFail { reason: e.message });
                self.session = Some(session);
                return Some(Exit::Failed);
            }
        }

        // 系统可能改写用户名（PAM 模块的大小写规范化 / 别名映射，或 Windows 上
        // 从令牌 SID 反查回来的规范名），以它为准。
        let final_name = session.user().unwrap_or(username);
        let identity = match session.lookup_identity(&final_name) {
            Ok(id) => id,
            Err(reason) => {
                log::event(&format!("认证通过但无法解析用户: {reason}"));
                let _ = self.ipc.send(&FromHelper::AuthFail { reason });
                self.session = Some(session);
                return Some(Exit::Failed);
            }
        };
        if session.stashed_info_count() > 0 {
            log::event(&format!(
                "认证结束时仍有 {} 条未送出的信息消息，丢弃",
                session.stashed_info_count()
            ));
        }
        log::event(&format!("认证通过，uid={}", identity.user.uid));

        let reply = FromHelper::AuthOk {
            user: identity.user.clone(),
        };
        self.session = Some(session);
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
        let (Some(session), Some(identity)) = (self.session.as_mut(), self.identity.as_ref()) else {
            return self.reply_error("尚未认证，不能 SpawnWorker");
        };

        // ---- 提权资格：权威检查，在创建进程之前 ----
        //
        // 用的是 `AuthOk` 阶段解析出来的组（Unix 走 NSS，Windows 读令牌），
        // 不是主进程报过来的——主进程若被攻破，它说什么都不该左右这里的判断。
        // 主进程侧的同款检查只是提前拒绝、省一轮对话。
        if as_root
            && !may_elevate(
                identity.user.uid,
                &identity.user.groups,
                &self.elevate_groups,
            )
        {
            let msg = format!(
                "用户 {} (uid {}) 没有提权资格：需属于 {} 之一，实际所属 {}",
                identity.user.username,
                identity.user.uid,
                render_groups(&self.elevate_groups),
                render_groups(&identity.user.groups),
            );
            log::event(&format!("拒绝提权：{msg}"));
            return self.reply_error(&msg);
        }

        // 开系统会话；失败降级（Unix 上非 root 时 pam_systemd / pam_loginuid 必然
        // 失败，Windows 上缺 SeRestorePrivilege 时 LoadUserProfileW 必然失败）。
        let mut session_error = None;
        if open_session && !session.session_opened() {
            match session.open_session(&mut self.ipc) {
                Ok(()) => log::event("用户会话已建立"),
                Err(e) => {
                    log::event(&format!("建立用户会话失败，降级继续: {e}"));
                    session_error = Some(e.message);
                }
            }
        }
        let extra_env = if session.session_opened() {
            session.envlist()
        } else {
            Vec::new()
        };

        let spec = WorkerSpec {
            username: identity.user.username.clone(),
            uid: identity.user.uid,
            gid: identity.user.gid,
            home: identity.home.clone(),
            shell: identity.shell.clone(),
            as_root,
            extra_env,
        };
        let (proc, main_side) = match spawn_worker(&self.worker_exe, &spec, session) {
            Ok(v) => v,
            Err(message) => return self.reply_error(&format!("拉起 worker 失败: {message}")),
        };
        let pid = proc.pid();
        // `as_root` 时报什么 uid 两个平台不同：Unix 上 worker 真的以 uid 0 运行；
        // Windows 上提权不换账户、只换令牌，用户 SID 没变，如实报原 uid
        // （理由见 `spawn::windows` 的模块文档）。主进程对 admin worker
        // 不做 uid 比对，两种报法都不会破坏检查。
        let uid = if as_root && cfg!(unix) {
            0
        } else {
            identity.user.uid
        };
        log::event(&format!(
            "worker 已拉起，pid={pid} uid={uid} as_root={as_root}"
        ));

        // 交接分两步，次序不能换：句柄值要先取出来写进帧里（Windows），
        // 补发 SCM_RIGHTS 那一帧则必须在 `WorkerSpawned` 之后（Unix）。
        // 两个平台各只有一步是实质动作，见 `ipc` 的模块文档。
        let worker_handle = Ipc::prepare_handover(&main_side);
        let spawned = FromHelper::WorkerSpawned {
            pid,
            uid,
            session_opened: session.session_opened(),
            session_error,
            worker_handle,
        };
        if let Err(e) = self.ipc.send(&spawned) {
            log::event(&format!("发送 WorkerSpawned 失败: {e}"));
            return Some(Exit::Failed);
        }
        if let Err(e) = self.ipc.finish_handover(main_side) {
            log::event(&format!("交接 worker 通道失败: {e}"));
            return Some(Exit::Failed);
        }
        self.workers.push(proc);
        None
    }

    // ------------------------------------------------------------- 清理

    /// 关会话、结束认证句柄。主进程应当在此之前终止 worker。
    fn shutdown(&mut self) {
        // 给 worker 一点时间退出，让回收能完成（Unix 上避免留下僵尸交给 init）。
        for _ in 0..20 {
            spawn::reap_workers(&mut self.workers);
            if self.workers.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if !self.workers.is_empty() {
            log::event(&format!(
                "{} 个 worker 仍在运行，不再等待",
                self.workers.len()
            ));
        }
        if let Some(session) = self.session.take() {
            log::event("关闭用户会话");
            session.close();
        }
    }
}

#[cfg(test)]
mod tests {
    //! helper 侧的提权资格检查（`roadmap/01-worker-execution.md` §6.4）
    //! 与入口参数解析。
    //!
    //! 提权这里只测判定本身。「不满足就不创建进程」由 `on_spawn_worker` 的结构
    //! 保证：检查在 `spawn_worker` 调用**之前**，走 `reply_error` 直接返回。

    use strixmaid_types::auth::{DEFAULT_ELEVATE_GROUPS, may_elevate};

    fn g(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    /// helper 用的是 `AuthOk` 阶段解析出来的组，与主进程报什么无关。
    #[test]
    fn 提权资格按组判定() {
        let allow = g(DEFAULT_ELEVATE_GROUPS);
        // 本平台的默认组必须真的能放行本平台的管理员。
        #[cfg(windows)]
        {
            assert!(may_elevate(1001, &g(&["Users", "Administrators"]), &allow));
            assert!(!may_elevate(1001, &g(&["Users"]), &allow));
        }
        #[cfg(not(windows))]
        {
            assert!(may_elevate(1000, &g(&["alice", "sudo"]), &allow));
            assert!(!may_elevate(1000, &g(&["alice", "users"]), &allow));
        }
        // uid 0 无条件放行
        assert!(may_elevate(0, &[], &allow));
        // 空的允许列表 = 禁止任何人提权（uid 0 除外）
        assert!(!may_elevate(1000, &g(&["sudo", "Administrators"]), &[]));
    }

    #[test]
    fn 组名渲染便于排错() {
        assert_eq!(super::render_groups(&g(&["sudo", "wheel"])), "sudo、wheel");
        assert_eq!(super::render_groups(&[]), "（空）");
    }

    /// 默认的 worker 路径必须带本平台的可执行后缀，否则 Windows 上
    /// `CreateProcessW` 会去找一个不存在的无后缀文件。
    #[test]
    fn 默认_worker_路径带平台后缀() {
        let exe = super::default_worker_exe();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(name, format!("strixmaid{}", std::env::consts::EXE_SUFFIX));
    }

    #[cfg(windows)]
    #[test]
    fn 解析_pipe_参数() {
        use super::parse_pipe_arg;

        let one = |args: &[&str]| {
            parse_pipe_arg(args.iter().map(|s| (*s).to_owned()))
        };
        assert_eq!(
            one(&["--pipe", r"\\.\pipe\strixmaid-helper-abc"]).unwrap(),
            r"\\.\pipe\strixmaid-helper-abc"
        );
        assert_eq!(one(&["--pipe=abc"]).unwrap(), "abc");
        // 缺参数 / 空值 / 多余参数都要报错，而不是拿一个空名字去连。
        assert!(one(&[]).is_err());
        assert!(one(&["--pipe"]).is_err());
        assert!(one(&["--pipe="]).is_err());
        assert!(one(&["--pipe", ""]).is_err());
        assert!(one(&["worker"]).is_err());
        // 错误信息要说清 helper 该由谁拉起。
        assert!(one(&[]).unwrap_err().contains("主进程"));
    }
}
