//! `SessionManager` 的状态机测试：用**假 helper**（std 线程，走同样的帧协议）替代
//! 真 helper，worker 则直接跑进程内的 [`crate::worker::serve`]——协议的每一帧、
//! `SCM_RIGHTS` 传 fd、`Hello` 握手全部是真的，只有 PAM 与 setuid 被替掉。

use std::io::IoSlice;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use strixmaid_types::auth::{AuthUser, Prompt, PromptStyle};
use strixmaid_types::ipc::{self, FromHelper, IpcPromptResponse, ToHelper};
use zeroize::Zeroizing;

use super::*;
use super::{AUDIT_ACTOR_SYSTEM, AUDIT_DROP_ELEVATION, AUDIT_SESSION_EXPIRE};
use crate::worker::Dispatcher;

const PASSWORD: &str = "correct horse battery staple";
const OTP: &str = "123456";

/// 假 helper 的行为参数与计数器。
#[derive(Clone)]
struct MockLauncher {
    /// 是否假装自己是 root（决定 `as_root` 的 SpawnWorker 是否被拒）。
    am_root: bool,
    /// PAM 对话轮数：1 = 只问密码，2 = 密码 + 验证码。
    rounds: u32,
    /// 拉起过的 helper 数。
    launched: Arc<AtomicUsize>,
    /// 拉起过的 worker 数。
    workers: Arc<AtomicUsize>,
    /// 收到 CloseSession（或通道断开）而退出的 helper 数。
    closed: Arc<AtomicUsize>,
}

impl MockLauncher {
    fn new(am_root: bool, rounds: u32) -> Self {
        MockLauncher {
            am_root,
            rounds,
            launched: Arc::new(AtomicUsize::new(0)),
            workers: Arc::new(AtomicUsize::new(0)),
            closed: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl HelperLauncher for MockLauncher {
    fn launch(&self) -> BoxFuture<'_, Result<HelperConn>> {
        Box::pin(async move {
            self.launched.fetch_add(1, Ordering::SeqCst);
            let (ours, theirs) = StdUnixStream::pair().unwrap();
            let me = self.clone();
            std::thread::spawn(move || me.run(theirs));
            ours.set_nonblocking(true).unwrap();
            let stream = tokio::net::UnixStream::from_std(ours).unwrap();
            Ok(HelperConn::new(stream, None))
        })
    }
}

impl MockLauncher {
    /// 假 helper 主体：与真 helper 的 `main.rs` 同构。
    fn run(self, mut s: StdUnixStream) {
        let username = match ipc::read_msg::<_, ToHelper>(&mut s) {
            Ok(Some(ToHelper::AuthStart { username, .. })) => username,
            _ => return,
        };
        for round in 0..self.rounds {
            let text = if round == 0 {
                "Password: "
            } else {
                "Verification code: "
            };
            ipc::write_msg(
                &mut s,
                &FromHelper::Prompts {
                    prompts: vec![Prompt {
                        id: 0,
                        style: PromptStyle::Prompt,
                        text: text.into(),
                    }],
                },
            )
            .unwrap();
            let responses = match ipc::read_msg::<_, ToHelper>(&mut s) {
                Ok(Some(ToHelper::AuthRespond { responses })) => responses,
                // 主进程在对话中途断开（pending 超时）：真 helper 会在 PAM_CONV_ERR 后退出。
                _ => {
                    self.closed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            };
            let expected = if round == 0 { PASSWORD } else { OTP };
            if responses.first().map(|r| r.value.as_str()) != Some(expected) {
                let _ = ipc::write_msg(
                    &mut s,
                    &FromHelper::AuthFail {
                        reason: "Authentication failure".into(),
                    },
                );
                self.closed.fetch_add(1, Ordering::SeqCst);
                return;
            }
        }
        ipc::write_msg(
            &mut s,
            &FromHelper::AuthOk {
                user: AuthUser {
                    uid: nix::unistd::getuid().as_raw(),
                    gid: nix::unistd::getgid().as_raw(),
                    username: username.clone(),
                    // 主组 + wheel。给 wheel 是为了让默认的 elevate_groups 放行它——
                    // 提权路径的用例要走通，就得是个「有资格提权」的用户。
                    // 「无资格被拒」由 `不在_elevate_groups_的用户提权被提前拒绝` 单独覆盖。
                    groups: vec![username, "wheel".to_owned()],
                },
            },
        )
        .unwrap();

        loop {
            match ipc::read_msg::<_, ToHelper>(&mut s) {
                Ok(Some(ToHelper::SpawnWorker { as_root, .. })) => {
                    if as_root && !self.am_root {
                        ipc::write_msg(
                            &mut s,
                            &FromHelper::Error {
                                message: "helper 不是 root，无法创建 admin worker".into(),
                            },
                        )
                        .unwrap();
                        continue;
                    }
                    let (main_side, worker_side) = StdUnixStream::pair().unwrap();
                    // 进程内的「worker」：真的 serve()，只是没 exec、没 setuid。
                    std::thread::spawn(move || {
                        let rt = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap();
                        rt.block_on(async move {
                            worker_side.set_nonblocking(true).unwrap();
                            let stream = tokio::net::UnixStream::from_std(worker_side).unwrap();
                            let _ = crate::worker::serve(stream, Arc::new(Dispatcher::new())).await;
                        });
                    });
                    self.workers.fetch_add(1, Ordering::SeqCst);
                    ipc::write_msg(
                        &mut s,
                        &FromHelper::WorkerSpawned {
                            // 进程内 worker 没有独立 pid；<= 1 表示未知，主进程不会对它发信号。
                            pid: -1,
                            uid: nix::unistd::getuid().as_raw(),
                            session_opened: false,
                            session_error: Some("mock: 没有 PAM 会话".into()),
                        },
                    )
                    .unwrap();
                    let fds = [main_side.as_raw_fd()];
                    let iov = [IoSlice::new(b"F")];
                    let cmsg = [ControlMessage::ScmRights(&fds)];
                    sendmsg::<()>(s.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None).unwrap();
                    drop(main_side);
                }
                Ok(Some(ToHelper::CloseSession)) => {
                    let _ = ipc::write_msg(&mut s, &FromHelper::SessionClosed);
                    self.closed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => {
                    self.closed.fetch_add(1, Ordering::SeqCst);
                    return;
                }
            }
        }
    }
}

/// 起不来的 helper。
struct BrokenLauncher;

impl HelperLauncher for BrokenLauncher {
    fn launch(&self) -> BoxFuture<'_, Result<HelperConn>> {
        Box::pin(async { Err(SessionError::HelperUnavailable("没有这个二进制".into())) })
    }
}

fn cfg(idle: Duration, elevated: Duration, pending: Duration) -> SessionManagerConfig {
    SessionManagerConfig {
        pam_service: "strixmaid-test".into(),
        worker_exe: Some("/nonexistent/strixmaid".into()),
        open_session: true,
        idle_timeout: idle,
        elevated_idle_timeout: elevated,
        pending_timeout: pending,
        node_id: LOCAL_NODE_ID.into(),
        // mock helper 让测试用户属于 wheel，默认组列表因此放行它
        elevate_groups: strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
    }
}

async fn manager(launcher: Arc<dyn HelperLauncher>, c: SessionManagerConfig) -> SessionManager {
    let store = Store::open_in_memory().await.unwrap();
    SessionManager::new(store, c, launcher).await.unwrap()
}

fn answer(value: &str) -> Vec<IpcPromptResponse> {
    vec![IpcPromptResponse {
        id: 0,
        value: Zeroizing::new(value.to_string()),
    }]
}

async fn wait_for(counter: &AtomicUsize, target: usize) {
    for _ in 0..200 {
        if counter.load(Ordering::SeqCst) >= target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "计数器未达到 {target}，当前 {}",
        counter.load(Ordering::SeqCst)
    );
}

// ===========================================================================

#[test]
fn token_哈希不可逆且稳定() {
    let h = hash_token("smt_abc");
    assert_eq!(h.len(), 64);
    assert_ne!(h, "smt_abc");
    assert!(!h.contains("smt_abc"));
    assert_eq!(h, hash_token("smt_abc"));
    assert_ne!(h, hash_token("smt_abd"));
    // 已知向量：sha256("") 。
    assert_eq!(
        hash_token(""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let t1 = generate_token();
    let t2 = generate_token();
    assert_eq!(t1.len(), 64);
    assert_ne!(t1, t2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 错误密码返回_auth_failed_且不留会话() {
    let mock = MockLauncher::new(false, 1);
    let m = manager(
        Arc::new(mock.clone()),
        cfg(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ),
    )
    .await;

    let (pending, prompts) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].style, PromptStyle::Prompt);

    let err = m
        .login_respond(&pending, answer("wrong"))
        .await
        .unwrap_err();
    match err {
        SessionError::AuthFailed(reason) => assert_eq!(reason, "Authentication failure"),
        other => panic!("应为 AuthFailed，实际 {other:?}"),
    }
    let api: ApiError = SessionError::AuthFailed("x".into()).into();
    assert_eq!(api.code, ErrorCode::Unauthenticated);

    assert_eq!(m.session_count().await, 0);
    assert_eq!(m.pending_count().await, 0);
    assert!(m.store().list_sessions().await.unwrap().is_empty());
    // 同一个 pending 不能再用
    assert!(matches!(
        m.login_respond(&pending, answer(PASSWORD))
            .await
            .unwrap_err(),
        SessionError::PendingNotFound
    ));
    wait_for(&mock.closed, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 完整状态机_登录_提权_超时回收_登出() {
    let mock = MockLauncher::new(true, 1);
    let idle = Duration::from_millis(600);
    let elevated = Duration::from_millis(250);
    let m = manager(
        Arc::new(mock.clone()),
        cfg(idle, elevated, Duration::from_secs(60)),
    )
    .await;

    // ---- 登录 ----
    let meta = ClientMeta {
        user_agent: Some("test-agent".into()),
        remote_addr: Some("127.0.0.1:1234".into()),
    };
    let (pending, _) = m.login_start("alice", meta.clone()).await.unwrap();
    let (token, session) = match m.login_respond(&pending, answer(PASSWORD)).await.unwrap() {
        LoginOutcome::Complete { token, session } => (token, session),
        LoginOutcome::More { .. } => panic!("单轮不该 More"),
    };
    assert_eq!(session.user.username, "alice");
    assert!(!session.elevated);
    assert_eq!(session.meta, meta);
    assert_eq!(session.token_hash, hash_token(&token));
    assert_eq!(mock.workers.load(Ordering::SeqCst), 1);

    // 库表：sessions 只有 hash，node_sessions(local) 一行
    let rows = m.store().list_sessions().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, hash_token(&token));
    assert_ne!(rows[0].id, token);
    assert_eq!(rows[0].user_agent.as_deref(), Some("test-agent"));
    let ns = m
        .store()
        .get_node_session(&session.token_hash, LOCAL_NODE_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ns.username, "alice");
    assert!(!ns.elevated);

    // resolve：明文 token 可解析；hash 本身不是 token
    let resolved = m.resolve(&token).await.unwrap();
    assert_eq!(resolved.token_hash, session.token_hash);
    assert!(m.resolve(&session.token_hash).await.is_none());
    assert!(m.resolve("no-such-token").await.is_none());

    // user worker 真的在应答 RPC
    let worker = m.user_worker(&session.token_hash).await.unwrap();
    worker.ping().await.unwrap();
    let who = worker.whoami().await.unwrap();
    assert_eq!(who.uid, nix::unistd::getuid().as_raw());
    assert!(m.admin_worker(&session.token_hash).await.is_none());

    // ---- 提权 ----
    let (pending, prompts) = m.elevate_start(&session.token_hash, None).await.unwrap();
    assert_eq!(prompts.len(), 1);
    // 错误密码
    assert!(matches!(
        m.elevate_respond(&pending, answer("nope"))
            .await
            .unwrap_err(),
        SessionError::AuthFailed(_)
    ));
    let (pending, _) = m.elevate_start(&session.token_hash, None).await.unwrap();
    let elevated_session = match m.elevate_respond(&pending, answer(PASSWORD)).await.unwrap() {
        ElevateOutcome::Complete(s) => s,
        ElevateOutcome::More { .. } => panic!("单轮不该 More"),
    };
    assert!(elevated_session.elevated);
    assert!(elevated_session.elevated_ts.is_some());
    assert_eq!(mock.workers.load(Ordering::SeqCst), 2);
    let admin = m.admin_worker(&session.token_hash).await.unwrap();
    admin.ping().await.unwrap();
    let ns = m
        .store()
        .get_node_session(&session.token_hash, LOCAL_NODE_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(ns.elevated);
    assert!(ns.elevated_at.is_some());

    // ---- 提权先于会话过期 ----
    tokio::time::sleep(elevated + Duration::from_millis(100)).await;
    // 期间保持会话活跃
    assert!(m.resolve(&token).await.is_some());
    let report = m.sweep().await;
    assert_eq!(report.elevations_expired, 1);
    assert_eq!(report.sessions_expired, 0);
    let after = m.resolve(&token).await.unwrap();
    assert!(!after.elevated);
    assert!(after.elevated_ts.is_none());
    assert!(m.admin_worker(&session.token_hash).await.is_none());
    let ns = m
        .store()
        .get_node_session(&session.token_hash, LOCAL_NODE_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(!ns.elevated);
    // admin helper 收到了 CloseSession
    wait_for(&mock.closed, 2).await; // 1 = 提权失败的那个，2 = 被回收的 admin helper

    // ---- 会话空闲超时 ----
    tokio::time::sleep(idle + Duration::from_millis(100)).await;
    // 过期但尚未 sweep：resolve 也不再认
    assert!(m.resolve(&token).await.is_none());
    let report = m.sweep().await;
    assert_eq!(report.sessions_expired, 1);
    assert_eq!(m.session_count().await, 0);
    assert!(m.store().list_sessions().await.unwrap().is_empty());
    assert!(!worker.is_alive() || worker.ping().await.is_err());
    wait_for(&mock.closed, 3).await;

    // ---- 再登录一次，然后显式登出 ----
    let (pending, _) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    let LoginOutcome::Complete { token, session } =
        m.login_respond(&pending, answer(PASSWORD)).await.unwrap()
    else {
        panic!()
    };
    assert!(m.resolve(&token).await.is_some());
    assert!(m.logout(&session.token_hash).await);
    assert!(!m.logout(&session.token_hash).await);
    assert!(m.resolve(&token).await.is_none());
    assert!(m.store().list_sessions().await.unwrap().is_empty());
    wait_for(&mock.closed, 4).await;
    // 登录 + 提权失败 + 提权成功 + 再登录 = 4 个 helper
    assert_eq!(mock.launched.load(Ordering::SeqCst), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 多轮提示_2fa() {
    let mock = MockLauncher::new(false, 2);
    let m = manager(
        Arc::new(mock),
        cfg(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ),
    )
    .await;
    let (pending, prompts) = m.login_start("bob", ClientMeta::default()).await.unwrap();
    assert_eq!(prompts[0].text, "Password: ");
    let (pending, prompts) = match m.login_respond(&pending, answer(PASSWORD)).await.unwrap() {
        LoginOutcome::More {
            pending_id,
            prompts,
        } => (pending_id, prompts),
        LoginOutcome::Complete { .. } => panic!("第一轮之后应为 More"),
    };
    assert_eq!(prompts[0].text, "Verification code: ");
    match m.login_respond(&pending, answer(OTP)).await.unwrap() {
        LoginOutcome::Complete { session, .. } => assert_eq!(session.user.username, "bob"),
        LoginOutcome::More { .. } => panic!("第二轮之后应完成"),
    }
    m.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_超时被回收() {
    let mock = MockLauncher::new(false, 1);
    let m = manager(
        Arc::new(mock.clone()),
        cfg(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_millis(100),
        ),
    )
    .await;
    let (pending, _) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    assert_eq!(m.pending_count().await, 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    let report = m.sweep().await;
    assert_eq!(report.pending_expired, 1);
    assert_eq!(m.pending_count().await, 0);
    assert!(matches!(
        m.login_respond(&pending, answer(PASSWORD))
            .await
            .unwrap_err(),
        SessionError::PendingNotFound
    ));
    // 假 helper 因通道断开而退出
    wait_for(&mock.closed, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn 非_root_的_helper_拒绝提权() {
    let mock = MockLauncher::new(false, 1);
    let m = manager(
        Arc::new(mock),
        cfg(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ),
    )
    .await;
    let (pending, _) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    let LoginOutcome::Complete { session, .. } =
        m.login_respond(&pending, answer(PASSWORD)).await.unwrap()
    else {
        panic!()
    };
    let (pending, _) = m.elevate_start(&session.token_hash, None).await.unwrap();
    let err = m
        .elevate_respond(&pending, answer(PASSWORD))
        .await
        .unwrap_err();
    assert!(matches!(err, SessionError::ElevationDenied(_)), "{err:?}");
    let api: ApiError = err.into();
    assert_eq!(api.code, ErrorCode::PermissionDenied);
    // 会话本身不受影响
    let s = m.resolve_hash(&session.token_hash).await.unwrap();
    assert!(!s.elevated);
    m.shutdown().await;
}

#[tokio::test]
async fn helper_起不来映射为能力不可用() {
    let m = manager(
        Arc::new(BrokenLauncher),
        cfg(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ),
    )
    .await;
    let err = m
        .login_start("alice", ClientMeta::default())
        .await
        .unwrap_err();
    let api: ApiError = err.into();
    assert_eq!(api.code, ErrorCode::CapabilityUnavailable);
    assert_eq!(api.capability.as_deref(), Some("helper"));
}

#[tokio::test]
async fn 启动时清理残留会话行() {
    let store = Store::open_in_memory().await.unwrap();
    store
        .upsert_node(LOCAL_NODE_ID, "本机", NodeKind::Local, None)
        .await
        .unwrap();
    store
        .create_session("stale-hash", None, None)
        .await
        .unwrap();
    assert_eq!(store.list_sessions().await.unwrap().len(), 1);
    let m = SessionManager::new(
        store.clone(),
        cfg(
            Duration::from_secs(60),
            Duration::from_secs(30),
            Duration::from_secs(60),
        ),
        Arc::new(BrokenLauncher),
    )
    .await
    .unwrap();
    assert!(store.list_sessions().await.unwrap().is_empty());
    assert!(m.resolve("anything").await.is_none());
}

// ===========================================================================
// 真 helper + 真 PAM（手动运行：cargo test -p strixmaid-core -- --ignored 真实）
// ===========================================================================

/// 在 target 目录里找已构建的 `strixmaid-helper`。
fn find_real_helper() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?;
    for _ in 0..3 {
        let candidate = dir.join("strixmaid-helper");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
    None
}

/// 走 `ProcessHelperLauncher`（socketpair + dup2 到 fd 3 + spawn）→ 真 PAM（`sudo` 服务）
/// → 错误密码 → `PAM_AUTH_ERR`。不知道当前用户的密码，所以只能测失败路径，
/// 但 challenge-response 的每一步（prompt 经 IPC 往返、conversation 回调）都真实发生了。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要真实 PAM 与已构建的 target/debug/strixmaid-helper"]
async fn 真实_helper_错误密码路径() {
    let Some(helper) = find_real_helper() else {
        panic!("找不到 strixmaid-helper，先 cargo build -p strixmaid-helper");
    };
    let username = nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name;
    let store = Store::open_in_memory().await.unwrap();
    let mut c = cfg(
        Duration::from_secs(60),
        Duration::from_secs(30),
        Duration::from_secs(60),
    );
    // 本机没有 /etc/pam.d/strixmaid，用现成的 sudo 服务（@include common-auth）。
    c.pam_service = std::env::var("STRIXMAID_TEST_PAM_SERVICE").unwrap_or_else(|_| "sudo".into());
    let m = SessionManager::new(store, c, Arc::new(ProcessHelperLauncher::new(helper)))
        .await
        .unwrap();

    let (pending, prompts) = m
        .login_start(&username, ClientMeta::default())
        .await
        .expect("AuthStart 应得到第一轮 prompts");
    eprintln!("[真实 helper] prompts = {prompts:?}");
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].style, PromptStyle::Prompt);

    let started = std::time::Instant::now();
    let err = m
        .login_respond(&pending, answer("definitely-not-the-password"))
        .await
        .unwrap_err();
    eprintln!("[真实 helper] 结果 = {err:?}，耗时 {:?}", started.elapsed());
    assert!(matches!(err, SessionError::AuthFailed(_)), "{err:?}");
    assert_eq!(m.session_count().await, 0);
    assert_eq!(m.pending_count().await, 0);
    assert!(m.store().list_sessions().await.unwrap().is_empty());
}

/// `roadmap/01-worker-execution.md` §6.5：不在 `elevate_groups` 的用户提权应被
/// **提前**拒绝——返回 403，且**不 spawn 第二个 helper**、不进入 PAM 对话。
///
/// 断言 helper 的 spawn 计数不增加，等价于 roadmap 里写的
/// 「`pgrep -c strixmaid-helper` 不增加」。
#[tokio::test]
async fn 不在_elevate_groups_的用户提权被提前拒绝() {
    let mock = MockLauncher::new(true, 1);
    // 只允许 wheel；mock helper 给用户的组是 [用户名, "wheel"]，
    // 这里把允许列表改成一个它肯定不属于的组。
    let mut c = cfg(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );
    c.elevate_groups = vec!["no-such-group".to_owned()];
    let m = manager(Arc::new(mock.clone()), c).await;

    let (pending, _) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    let (token, session) = match m.login_respond(&pending, answer(PASSWORD)).await.unwrap() {
        LoginOutcome::Complete { token, session } => (token, session),
        LoginOutcome::More { .. } => panic!("单轮不该 More"),
    };
    assert!(!session.elevated);

    let launched_before = mock.launched.load(Ordering::SeqCst);
    let err = m
        .elevate_start(&hash_token(&token), None)
        .await
        .expect_err("没有资格的用户不该进入提权对话");

    let api: ApiError = err.into();
    assert_eq!(api.code, ErrorCode::PermissionDenied);
    let detail = api.detail.unwrap_or_default();
    assert!(detail.contains("no-such-group"), "要说清需要哪个组：{detail}");
    assert!(detail.contains("wheel"), "也要说清用户实际所属：{detail}");
    assert!(
        !api.can_retry_elevated,
        "再提一次权也不会变，这个标志必须是 false"
    );

    assert_eq!(
        mock.launched.load(Ordering::SeqCst),
        launched_before,
        "提前拒绝就不该 spawn 第二个 helper"
    );
    // 会话本身不受影响，仍然可用
    assert!(m.resolve_hash(&hash_token(&token)).await.is_some());
}

/// 有资格的用户照常能提权——上一条不能是靠「把提权整个关掉」实现的。
#[tokio::test]
async fn 在_elevate_groups_的用户提权正常() {
    let mock = MockLauncher::new(true, 1);
    let mut c = cfg(
        Duration::from_secs(60),
        Duration::from_secs(60),
        Duration::from_secs(60),
    );
    c.elevate_groups = vec!["wheel".to_owned()];
    let m = manager(Arc::new(mock.clone()), c).await;

    let (pending, _) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    let (token, _) = match m.login_respond(&pending, answer(PASSWORD)).await.unwrap() {
        LoginOutcome::Complete { token, session } => (token, session),
        LoginOutcome::More { .. } => panic!("单轮不该 More"),
    };

    let launched_before = mock.launched.load(Ordering::SeqCst);
    let (pending, _) = m
        .elevate_start(&hash_token(&token), None)
        .await
        .expect("wheel 用户应能进入提权对话");
    assert!(
        mock.launched.load(Ordering::SeqCst) > launched_before,
        "进入提权对话意味着起了第二个 helper"
    );
    match m.elevate_respond(&pending, answer(PASSWORD)).await.unwrap() {
        ElevateOutcome::Complete(session) => assert!(session.elevated),
        ElevateOutcome::More { .. } => panic!("单轮不该 More"),
    }
}

/// `roadmap/02-audit.md` §4.1：超时回收要留下审计痕迹。
///
/// 回收是**系统**做的决定，没有发起者；但事后追查时要能按用户筛出
/// 「他的会话什么时候被回收过」，所以 `target` 与 `uid` 仍记该用户。
#[tokio::test]
async fn 超时回收写入审计记录() {
    let mock = MockLauncher::new(true, 1);
    let idle = Duration::from_millis(400);
    let elevated = Duration::from_millis(120);
    let m = manager(
        Arc::new(mock.clone()),
        cfg(idle, elevated, Duration::from_secs(60)),
    )
    .await;

    let (pending, _) = m.login_start("alice", ClientMeta::default()).await.unwrap();
    let token = match m.login_respond(&pending, answer(PASSWORD)).await.unwrap() {
        LoginOutcome::Complete { token, .. } => token,
        LoginOutcome::More { .. } => panic!("单轮不该 More"),
    };
    let hash = hash_token(&token);

    // 提权，然后等提权先超时
    let (p, _) = m.elevate_start(&hash, None).await.unwrap();
    match m.elevate_respond(&p, answer(PASSWORD)).await.unwrap() {
        ElevateOutcome::Complete(s) => assert!(s.elevated),
        ElevateOutcome::More { .. } => panic!("单轮不该 More"),
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = m.sweep().await;
    assert_eq!(r.elevations_expired, 1, "提权应先于会话过期");

    let page = m
        .store()
        .audit_query(&strixmaid_core_audit_filter())
        .await
        .unwrap();
    let drop_row = page
        .entries
        .iter()
        .find(|e| e.action == AUDIT_DROP_ELEVATION)
        .expect("应有一条 session.drop_elevation");
    assert_eq!(drop_row.username, AUDIT_ACTOR_SYSTEM, "执行者是系统");
    assert_eq!(drop_row.target.as_deref(), Some("alice"), "目标是被影响的用户");
    assert_eq!(drop_row.result, crate::store::AuditOutcome::Ok);

    // 再等会话超时
    tokio::time::sleep(Duration::from_millis(400)).await;
    let r = m.sweep().await;
    assert_eq!(r.sessions_expired, 1);

    let page = m
        .store()
        .audit_query(&strixmaid_core_audit_filter())
        .await
        .unwrap();
    let expire_row = page
        .entries
        .iter()
        .find(|e| e.action == AUDIT_SESSION_EXPIRE)
        .expect("应有一条 session.expire");
    assert_eq!(expire_row.username, AUDIT_ACTOR_SYSTEM);
    assert_eq!(expire_row.target.as_deref(), Some("alice"));

    // 审计里绝不能出现密码（design.md §5.3）
    for e in &page.entries {
        let blob = format!("{e:?}");
        assert!(
            !blob.contains(PASSWORD),
            "审计记录里出现了密码：{blob}"
        );
    }
}

/// 取全部审计记录的过滤器。
fn strixmaid_core_audit_filter() -> crate::store::AuditFilter {
    crate::store::AuditFilter {
        limit: 100,
        ..Default::default()
    }
}
