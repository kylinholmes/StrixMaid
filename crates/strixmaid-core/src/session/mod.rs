//! 会话与 worker 生命周期、提权状态（design.md §2.2 / §5 / §8 / §12）。
//!
//! # 一个会话里有什么
//!
//! ```text
//! 浏览器 token ──sha256──▶ sessions.id
//!   ├─ node_sessions(local)：uid / username / elevated
//!   ├─ user helper（持有 PAM 句柄）+ user worker（uid = 登录用户）
//!   └─ [提权后] admin helper（第二次 PAM 对话）+ admin worker（uid = 0）
//! ```
//!
//! 登录与提权走**同一套** challenge-response：`*_start` 拉起一个 helper、发 `AuthStart`、
//! 把第一轮 prompts 交给调用方并留下一个 pending 记录；`*_respond` 把答案转给 helper，
//! 得到更多 prompts 或最终结果。成功后向同一个 helper 要 worker。
//!
//! # 凭据处理（§5.3）
//!
//! - 本模块只见到 token 的 **hash**（[`hash_token`]）；明文 token 生成后立即交给调用方，
//!   不存、不记日志；
//! - 密码在 [`IpcPromptResponse`] 的 `Zeroizing<String>` 里，只经 [`HelperConn::send`]
//!   进入 socketpair，本模块不复制、不打印。
//!
//! # 超时（§12）
//!
//! - `idle_timeout`：会话空闲超时，超过即登出；
//! - `elevated_idle_timeout`：提权状态**独立的、更短的**空闲超时，超过只回收 admin
//!   worker、`elevated` 降回 false，会话本身继续；实际生效值取
//!   `min(elevated_idle_timeout, idle_timeout)`，保证提权先于会话过期；
//! - `pending_timeout`：进行中的 PAM 对话超时（默认 60 s），超过即终止 helper。
//!
//! 后台清理由 [`SessionManager::spawn_sweeper`] 定期跑 [`SessionManager::sweep`]。
//! 进程重启后 DB 里残留的会话行没有对应的 worker，[`SessionManager::new`] 会全部清掉。

pub mod framing;
pub mod helper;
pub mod worker_handle;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng as _;
use sha2::{Digest, Sha256};
use strixmaid_types::auth::{AuthUser, Prompt, SessionInfo};
use strixmaid_types::ipc::{FromHelper, IpcError, IpcPromptResponse, ToHelper};
use strixmaid_types::{ApiError, ErrorCode};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::config::Config;
use crate::store::{NodeKind, Store, StoreError, now_unix};

pub use helper::{HelperConn, HelperLauncher, ProcessHelperLauncher};
pub use worker_handle::WorkerHandle;

// ===========================================================================
// 常量
// ===========================================================================

/// 本地节点 id（§8：MVP 中 `node_sessions` 永远只有这一行）。
pub const LOCAL_NODE_ID: &str = "local";
/// 进行中的 PAM 对话超时。
pub const DEFAULT_PENDING_TIMEOUT: Duration = Duration::from_secs(60);
/// 登录 token 的随机字节数（hex 后 64 字符）。
const TOKEN_BYTES: usize = 32;
/// pending id 的随机字节数（hex 后 16 字符）。
const PENDING_ID_BYTES: usize = 8;
/// `resolve` 触发 DB `touch` 的最小间隔——每个请求都写库没有意义。
const DB_TOUCH_INTERVAL: Duration = Duration::from_secs(10);

// ===========================================================================
// 错误
// ===========================================================================

/// 会话层错误。[`From<SessionError> for ApiError`] 给出对外的错误码映射。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// PAM 拒绝（密码错误、账户锁定、过期……）。`reason` 来自 `pam_strerror`，不含凭据。
    #[error("认证失败: {0}")]
    AuthFailed(String),
    /// pending id 不存在、已完成或已超时。
    #[error("认证会话不存在或已过期")]
    PendingNotFound,
    /// token 对应的会话不存在或已过期。
    #[error("会话不存在或已过期")]
    SessionNotFound,
    /// helper 起不来 / 提前退出——登录根本不可能成功，对应能力 `helper`。
    #[error("helper 不可用: {0}")]
    HelperUnavailable(String),
    /// 无法创建 admin worker（典型：helper 不是 root）。
    #[error("无法提权: {0}")]
    ElevationDenied(String),
    /// helper 发来了当前阶段不该出现的消息。
    #[error("helper 协议错误: {0}")]
    Protocol(String),
    /// worker 连接 / 握手失败。
    #[error("worker 错误: {0}")]
    Worker(String),
    /// IPC 通道错误。
    #[error(transparent)]
    Ipc(#[from] IpcError),
    /// 数据库错误。
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<SessionError> for ApiError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::AuthFailed(reason) => {
                ApiError::unauthenticated("认证失败").with_detail(reason)
            }
            SessionError::PendingNotFound => ApiError::not_found("认证会话不存在或已过期"),
            SessionError::SessionNotFound => ApiError::unauthenticated("会话不存在或已过期"),
            SessionError::HelperUnavailable(msg) => {
                ApiError::capability_unavailable("helper", "PAM helper 不可用，无法登录")
                    .with_detail(msg)
            }
            SessionError::ElevationDenied(msg) => {
                ApiError::permission_denied("无法启用管理访问").with_detail(msg)
            }
            SessionError::Protocol(msg) => ApiError::internal("helper 协议错误").with_detail(msg),
            SessionError::Worker(msg) => ApiError::internal("worker 启动失败").with_detail(msg),
            SessionError::Ipc(e) => ApiError::new(ErrorCode::Unavailable, "与 helper 的通信中断")
                .with_detail(e.to_string()),
            SessionError::Store(e) => ApiError::internal("会话存储失败").with_detail(e.to_string()),
        }
    }
}

/// 本模块的 Result 别名。
pub type Result<T, E = SessionError> = std::result::Result<T, E>;

// ===========================================================================
// 配置与公开类型
// ===========================================================================

/// 登录时记录的客户端信息（`sessions.user_agent` / `remote_addr`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientMeta {
    /// User-Agent。
    pub user_agent: Option<String>,
    /// 来源地址；经反向代理时可能是代理地址。也作为 `PAM_RHOST` 交给 PAM 模块。
    pub remote_addr: Option<String>,
}

/// [`SessionManager`] 的运行参数。生产用 [`SessionManagerConfig::from_config`]，
/// 测试可以直接构造短超时。
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// PAM 服务名（`/etc/pam.d/<名字>`）。
    pub pam_service: String,
    /// `strixmaid` 主二进制路径，helper 用它 exec `worker`；`None` 取当前可执行文件。
    pub worker_exe: Option<PathBuf>,
    /// 登录时是否 `pam_open_session`（§5.4）。
    pub open_session: bool,
    /// 会话空闲超时。
    pub idle_timeout: Duration,
    /// 提权状态空闲超时。
    pub elevated_idle_timeout: Duration,
    /// 进行中的 PAM 对话超时。
    pub pending_timeout: Duration,
    /// 本节点 id。
    pub node_id: String,
}

impl SessionManagerConfig {
    /// 从全局配置派生。
    pub fn from_config(cfg: &Config) -> Self {
        SessionManagerConfig {
            pam_service: cfg.pam_service.clone(),
            worker_exe: None,
            open_session: true,
            idle_timeout: cfg.session.idle_timeout(),
            elevated_idle_timeout: cfg.session.elevated_idle_timeout(),
            pending_timeout: DEFAULT_PENDING_TIMEOUT,
            node_id: LOCAL_NODE_ID.to_string(),
        }
    }

    /// 实际生效的提权超时：提权必须比会话更早过期（§12）。
    pub fn effective_elevated_timeout(&self) -> Duration {
        self.elevated_idle_timeout.min(self.idle_timeout)
    }
}

/// 一个已认证会话的快照。挂在 axum 的 `Extension` 里给处理器用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// token 的 hash（`sessions.id`）。**不是**明文 token。
    pub token_hash: String,
    /// 节点 id。
    pub node: String,
    /// 认证到的系统身份。
    pub user: AuthUser,
    /// 是否已提权（admin worker 就绪）。
    pub elevated: bool,
    /// 提权时刻。
    pub elevated_ts: Option<i64>,
    /// 认证完成时刻。
    pub authed_ts: i64,
    /// 会话创建时刻。
    pub created_ts: i64,
    /// 最近活跃时刻。
    pub last_active_ts: i64,
    /// 登录时的客户端信息。
    pub meta: ClientMeta,
    /// `pam_open_session` 是否成功（决定用户级 unit 是否可用）。
    pub session_opened: bool,
}

impl Session {
    /// 转成对外 DTO。
    pub fn info(&self) -> SessionInfo {
        SessionInfo {
            node: self.node.clone(),
            uid: self.user.uid,
            username: self.user.username.clone(),
            groups: self.user.groups.clone(),
            elevated: self.elevated,
            elevated_ts: self.elevated_ts,
            authed_ts: self.authed_ts,
            created_ts: self.created_ts,
            last_active_ts: self.last_active_ts,
            user_agent: self.meta.user_agent.clone(),
            remote_addr: self.meta.remote_addr.clone(),
        }
    }
}

/// `login_respond` 的结果。
#[derive(Debug)]
pub enum LoginOutcome {
    /// 登录完成。`token` 是**明文**，只在这里出现一次，调用方交给浏览器后即丢弃。
    Complete {
        /// 明文 Bearer token。
        token: String,
        /// 新会话。
        session: Session,
    },
    /// PAM 还要追问。
    More {
        /// 继续使用的 pending id。
        pending_id: String,
        /// 新一轮提示。
        prompts: Vec<Prompt>,
    },
}

/// `elevate_respond` 的结果。
#[derive(Debug)]
pub enum ElevateOutcome {
    /// 提权完成，会话快照里 `elevated == true`。
    Complete(Session),
    /// PAM 还要追问。
    More {
        /// 继续使用的 pending id。
        pending_id: String,
        /// 新一轮提示。
        prompts: Vec<Prompt>,
    },
}

/// 一次 [`SessionManager::sweep`] 回收了什么。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// 超时登出的会话数。
    pub sessions_expired: usize,
    /// 超时降权的会话数。
    pub elevations_expired: usize,
    /// 超时终止的进行中认证数。
    pub pending_expired: usize,
}

// ===========================================================================
// token
// ===========================================================================

/// token 的存储形式：`sha256(token)` 的小写 hex。不可逆；`sessions.id` 存的就是它。
pub fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

fn generate_token() -> String {
    random_hex(TOKEN_BYTES)
}

fn generate_pending_id() -> String {
    random_hex(PENDING_ID_BYTES)
}

// ===========================================================================
// 内部状态
// ===========================================================================

/// 一次进行中的 PAM 对话。
struct Pending {
    kind: PendingKind,
    helper: HelperConn,
    meta: ClientMeta,
    created: Instant,
    /// PAM 无需交互时 `AuthStart` 直接返回 `AuthOk`；结果先存这里，等调用方 respond 时完成。
    authed: Option<AuthUser>,
}

enum PendingKind {
    Login,
    Elevate { token_hash: String },
}

/// 活跃度：会话与提权各自计时。
struct Activity {
    last_active: Instant,
    last_active_ts: i64,
    last_db_touch: Instant,
}

/// 提权后的 admin helper + admin worker。
struct Admin {
    helper: HelperConn,
    worker: Arc<WorkerHandle>,
    last_active: Instant,
    elevated_ts: i64,
}

/// 一个活着的会话。
struct Live {
    token_hash: String,
    node: String,
    user: AuthUser,
    created_ts: i64,
    authed_ts: i64,
    meta: ClientMeta,
    session_opened: bool,
    helper: Mutex<Option<HelperConn>>,
    worker: Arc<WorkerHandle>,
    activity: Mutex<Activity>,
    admin: Mutex<Option<Admin>>,
}

impl Live {
    async fn snapshot(&self) -> Session {
        let activity = self.activity.lock().await;
        let admin = self.admin.lock().await;
        Session {
            token_hash: self.token_hash.clone(),
            node: self.node.clone(),
            user: self.user.clone(),
            elevated: admin.is_some(),
            elevated_ts: admin.as_ref().map(|a| a.elevated_ts),
            authed_ts: self.authed_ts,
            created_ts: self.created_ts,
            last_active_ts: activity.last_active_ts,
            meta: self.meta.clone(),
            session_opened: self.session_opened,
        }
    }
}

struct Inner {
    store: Store,
    cfg: SessionManagerConfig,
    launcher: Arc<dyn HelperLauncher>,
    pending: Mutex<HashMap<String, Pending>>,
    sessions: RwLock<HashMap<String, Arc<Live>>>,
}

// ===========================================================================
// SessionManager
// ===========================================================================

/// 会话管理器。`Clone` 代价 = `Arc` 自增。
#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for SessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionManager")
            .field("node", &self.inner.cfg.node_id)
            .field("pam_service", &self.inner.cfg.pam_service)
            .finish()
    }
}

impl SessionManager {
    /// 创建管理器：确保本节点在 `nodes` 表里，并清掉上次进程留下的、已无 worker 的会话行。
    pub async fn new(
        store: Store,
        cfg: SessionManagerConfig,
        launcher: Arc<dyn HelperLauncher>,
    ) -> Result<Self> {
        store
            .upsert_node(&cfg.node_id, "本机", NodeKind::Local, None)
            .await?;
        let stale = store.prune_sessions(i64::MAX).await?;
        if stale > 0 {
            tracing::info!(stale, "清理了上次运行残留的会话");
        }
        Ok(SessionManager {
            inner: Arc::new(Inner {
                store,
                cfg,
                launcher,
                pending: Mutex::new(HashMap::new()),
                sessions: RwLock::new(HashMap::new()),
            }),
        })
    }

    /// 生产入口：用 `Config` 里的 `helper_path` / `pam_service` / 超时构造。
    pub async fn with_process_helper(store: Store, config: &Config) -> Result<Self> {
        let launcher = Arc::new(ProcessHelperLauncher::new(config.helper_path.clone()));
        SessionManager::new(store, SessionManagerConfig::from_config(config), launcher).await
    }

    /// 运行参数。
    pub fn config(&self) -> &SessionManagerConfig {
        &self.inner.cfg
    }

    /// 存储句柄。
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    // ------------------------------------------------------------ 登录

    /// 开始登录：拉起 helper、发 `AuthStart`，返回 `(pending_id, 第一轮 prompts)`。
    pub async fn login_start(
        &self,
        username: &str,
        meta: ClientMeta,
    ) -> Result<(String, Vec<Prompt>)> {
        self.start_auth(username, meta, PendingKind::Login).await
    }

    /// 回应一轮 prompts。成功时建会话、起 user worker。
    pub async fn login_respond(
        &self,
        pending_id: &str,
        responses: Vec<IpcPromptResponse>,
    ) -> Result<LoginOutcome> {
        let (mut pending, user) = match self.exchange(pending_id, responses).await? {
            Exchange::More { prompts } => {
                return Ok(LoginOutcome::More {
                    pending_id: pending_id.to_string(),
                    prompts,
                });
            }
            Exchange::Authed { pending, user } => (*pending, user),
        };
        if !matches!(pending.kind, PendingKind::Login) {
            // 调用方把 elevate 的 pending 拿来 login_respond；不至于崩，但要拒。
            pending.helper.close().await;
            return Err(SessionError::PendingNotFound);
        }
        tracing::info!(username = %user.username, uid = user.uid, "PAM 认证通过");

        // 1. token 与库表。明文 token 只在本函数与返回值里出现。
        let token = generate_token();
        let token_hash = hash_token(&token);
        let store = &self.inner.store;
        store
            .create_session(
                &token_hash,
                pending.meta.user_agent.as_deref(),
                pending.meta.remote_addr.as_deref(),
            )
            .await?;
        let node_session = match store
            .upsert_node_session(
                &token_hash,
                &self.inner.cfg.node_id,
                i64::from(user.uid),
                &user.username,
            )
            .await
        {
            Ok(ns) => ns,
            Err(e) => {
                let _ = store.delete_session(&token_hash).await;
                pending.helper.close().await;
                return Err(e.into());
            }
        };

        // 2. user worker。
        let (worker, session_opened) = match spawn_worker(
            &mut pending.helper,
            self.inner.cfg.open_session,
            false,
            Some(user.uid),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let _ = store.delete_session(&token_hash).await;
                pending.helper.close().await;
                return Err(e);
            }
        };

        let now = Instant::now();
        let live = Arc::new(Live {
            token_hash: token_hash.clone(),
            node: self.inner.cfg.node_id.clone(),
            user,
            created_ts: node_session.authed_at,
            authed_ts: node_session.authed_at,
            meta: pending.meta,
            session_opened,
            helper: Mutex::new(Some(pending.helper)),
            worker,
            activity: Mutex::new(Activity {
                last_active: now,
                last_active_ts: node_session.last_active,
                last_db_touch: now,
            }),
            admin: Mutex::new(None),
        });
        let session = live.snapshot().await;
        self.inner.sessions.write().await.insert(token_hash, live);
        Ok(LoginOutcome::Complete { token, session })
    }

    // ------------------------------------------------------------ 提权

    /// 开始提权：对当前会话再走一次 PAM 对话。`username` 为 `None` 时用会话自己的用户
    /// （sudo 语义）；给别的用户名则是 su 语义。
    pub async fn elevate_start(
        &self,
        token_hash: &str,
        username: Option<&str>,
    ) -> Result<(String, Vec<Prompt>)> {
        let live = self
            .live(token_hash)
            .await
            .ok_or(SessionError::SessionNotFound)?;
        let username = username.unwrap_or(&live.user.username).to_string();
        self.start_auth(
            &username,
            live.meta.clone(),
            PendingKind::Elevate {
                token_hash: token_hash.to_string(),
            },
        )
        .await
    }

    /// 回应提权 prompts。成功时起 admin worker、`elevated = true`。
    pub async fn elevate_respond(
        &self,
        pending_id: &str,
        responses: Vec<IpcPromptResponse>,
    ) -> Result<ElevateOutcome> {
        let (mut pending, user) = match self.exchange(pending_id, responses).await? {
            Exchange::More { prompts } => {
                return Ok(ElevateOutcome::More {
                    pending_id: pending_id.to_string(),
                    prompts,
                });
            }
            Exchange::Authed { pending, user } => (*pending, user),
        };
        let token_hash = match &pending.kind {
            PendingKind::Elevate { token_hash } => token_hash.clone(),
            PendingKind::Login => {
                pending.helper.close().await;
                return Err(SessionError::PendingNotFound);
            }
        };
        let Some(live) = self.live(&token_hash).await else {
            pending.helper.close().await;
            return Err(SessionError::SessionNotFound);
        };
        tracing::info!(
            session_user = %live.user.username,
            elevate_as = %user.username,
            "提权认证通过"
        );

        // admin worker 由 root helper 不切换身份直接 fork，uid 由 helper 报告、不再二次核对。
        let (worker, _) = match spawn_worker(&mut pending.helper, false, true, None).await {
            Ok(v) => v,
            Err(e) => {
                pending.helper.close().await;
                return Err(e);
            }
        };

        let now_ts = now_unix();
        self.inner
            .store
            .set_elevated(&token_hash, &self.inner.cfg.node_id, true, now_ts)
            .await?;
        let old = live.admin.lock().await.replace(Admin {
            helper: pending.helper,
            worker,
            last_active: Instant::now(),
            elevated_ts: now_ts,
        });
        if let Some(old) = old {
            // 重复提权：换掉旧的 admin worker。
            teardown_admin(old).await;
        }
        Ok(ElevateOutcome::Complete(live.snapshot().await))
    }

    /// 主动放弃管理访问：回收 admin worker、`elevated = false`。返回之前是否处于提权状态。
    pub async fn drop_elevation(&self, token_hash: &str) -> Result<bool> {
        let Some(live) = self.live(token_hash).await else {
            return Err(SessionError::SessionNotFound);
        };
        let admin = live.admin.lock().await.take();
        match admin {
            Some(admin) => {
                teardown_admin(admin).await;
                self.inner
                    .store
                    .set_elevated(token_hash, &self.inner.cfg.node_id, false, now_unix())
                    .await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    // ------------------------------------------------------------ 查询

    /// 按**明文** token 查会话并刷新活跃时间。不存在 / 已过期 → `None`。
    pub async fn resolve(&self, token: &str) -> Option<Session> {
        self.resolve_hash(&hash_token(token)).await
    }

    /// 同 [`resolve`](Self::resolve)，但入参已是 hash。
    pub async fn resolve_hash(&self, token_hash: &str) -> Option<Session> {
        let live = self.live(token_hash).await?;
        let now = Instant::now();
        let should_touch_db = {
            let mut activity = live.activity.lock().await;
            if now.duration_since(activity.last_active) > self.inner.cfg.idle_timeout {
                // 已过期但 sweeper 还没跑到：当作不存在。
                return None;
            }
            activity.last_active = now;
            activity.last_active_ts = now_unix();
            if now.duration_since(activity.last_db_touch) >= DB_TOUCH_INTERVAL {
                activity.last_db_touch = now;
                true
            } else {
                false
            }
        };
        if should_touch_db {
            let ts = now_unix();
            let store = &self.inner.store;
            if let Err(e) = store.touch_session(token_hash, ts).await {
                tracing::warn!(error = %e, "刷新 sessions.last_active 失败");
            }
            if let Err(e) = store
                .touch_node_session(token_hash, &self.inner.cfg.node_id, ts)
                .await
            {
                tracing::warn!(error = %e, "刷新 node_sessions.last_active 失败");
            }
        }
        Some(live.snapshot().await)
    }

    /// 会话的 user worker。
    pub async fn user_worker(&self, token_hash: &str) -> Option<Arc<WorkerHandle>> {
        self.live(token_hash).await.map(|l| l.worker.clone())
    }

    /// 会话的 admin worker（未提权 → `None`）。取用即视为一次管理操作，刷新提权计时。
    pub async fn admin_worker(&self, token_hash: &str) -> Option<Arc<WorkerHandle>> {
        let live = self.live(token_hash).await?;
        let mut admin = live.admin.lock().await;
        let admin = admin.as_mut()?;
        admin.last_active = Instant::now();
        Some(admin.worker.clone())
    }

    /// 当前活跃会话数。
    pub async fn session_count(&self) -> usize {
        self.inner.sessions.read().await.len()
    }

    /// 当前进行中的认证数。
    pub async fn pending_count(&self) -> usize {
        self.inner.pending.lock().await.len()
    }

    // ------------------------------------------------------------ 登出与回收

    /// 登出：终止 worker、关 helper（`pam_close_session`）、删库表。返回会话是否存在。
    pub async fn logout(&self, token_hash: &str) -> bool {
        let live = self.inner.sessions.write().await.remove(token_hash);
        match live {
            Some(live) => {
                self.teardown(live).await;
                true
            }
            None => false,
        }
    }

    /// 跑一轮超时回收：先提权、后会话、再 pending。
    pub async fn sweep(&self) -> SweepReport {
        let now = Instant::now();
        let cfg = &self.inner.cfg;
        let mut report = SweepReport::default();

        // --- 进行中的认证 ---
        let expired_pending: Vec<Pending> = {
            let mut pending = self.inner.pending.lock().await;
            let expired: Vec<String> = pending
                .iter()
                .filter(|(_, p)| now.duration_since(p.created) > cfg.pending_timeout)
                .map(|(id, _)| id.clone())
                .collect();
            expired
                .into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect()
        };
        for p in expired_pending {
            tracing::info!("进行中的认证超时，终止 helper");
            // 不发 CloseSession：对话可能正卡在 PAM 回调里，直接断开让它退出。
            drop(p.helper);
            report.pending_expired += 1;
        }

        // --- 会话 ---
        let all: Vec<Arc<Live>> = self.inner.sessions.read().await.values().cloned().collect();
        let elevated_timeout = cfg.effective_elevated_timeout();
        for live in all {
            // 提权先于会话过期。
            let expired_admin = {
                let mut admin = live.admin.lock().await;
                match admin.as_ref() {
                    Some(a) if now.duration_since(a.last_active) > elevated_timeout => admin.take(),
                    _ => None,
                }
            };
            if let Some(admin) = expired_admin {
                tracing::info!(username = %live.user.username, "提权空闲超时，回收 admin worker");
                teardown_admin(admin).await;
                if let Err(e) = self
                    .inner
                    .store
                    .set_elevated(&live.token_hash, &cfg.node_id, false, now_unix())
                    .await
                {
                    tracing::warn!(error = %e, "写回 elevated=0 失败");
                }
                report.elevations_expired += 1;
            }

            let session_expired = {
                let activity = live.activity.lock().await;
                now.duration_since(activity.last_active) > cfg.idle_timeout
            };
            if session_expired {
                tracing::info!(username = %live.user.username, "会话空闲超时，登出");
                if self.logout(&live.token_hash).await {
                    report.sessions_expired += 1;
                }
            }
        }
        report
    }

    /// 起一个后台 task 每 `interval` 跑一次 [`sweep`](Self::sweep)。
    pub fn spawn_sweeper(&self, interval: Duration) -> JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let report = manager.sweep().await;
                if report != SweepReport::default() {
                    tracing::debug!(?report, "会话回收");
                }
            }
        })
    }

    /// 关闭全部会话与进行中的认证（进程退出前调用）。
    pub async fn shutdown(&self) {
        let pending: Vec<Pending> = self
            .inner
            .pending
            .lock()
            .await
            .drain()
            .map(|(_, p)| p)
            .collect();
        for p in pending {
            p.helper.close().await;
        }
        let all: Vec<Arc<Live>> = self
            .inner
            .sessions
            .write()
            .await
            .drain()
            .map(|(_, l)| l)
            .collect();
        for live in all {
            self.teardown(live).await;
        }
    }

    // ------------------------------------------------------------ 内部

    async fn live(&self, token_hash: &str) -> Option<Arc<Live>> {
        self.inner.sessions.read().await.get(token_hash).cloned()
    }

    /// 拉起 helper、发 `AuthStart`、收第一轮。
    async fn start_auth(
        &self,
        username: &str,
        meta: ClientMeta,
        kind: PendingKind,
    ) -> Result<(String, Vec<Prompt>)> {
        let mut helper = self.inner.launcher.launch().await?;
        let worker_exe = match &self.inner.cfg.worker_exe {
            Some(p) => Some(p.clone()),
            None => std::env::current_exe().ok(),
        };
        helper
            .send(&ToHelper::AuthStart {
                service: self.inner.cfg.pam_service.clone(),
                username: username.to_string(),
                worker_exe: worker_exe.map(|p| p.to_string_lossy().into_owned()),
                rhost: meta.remote_addr.clone(),
            })
            .await?;

        let (prompts, authed) = match helper.recv().await? {
            Some(FromHelper::Prompts { prompts }) => (prompts, None),
            Some(FromHelper::AuthOk { user }) => (Vec::new(), Some(user)),
            Some(FromHelper::AuthFail { reason }) => return Err(SessionError::AuthFailed(reason)),
            Some(FromHelper::Error { message }) => return Err(SessionError::Protocol(message)),
            Some(other) => {
                return Err(SessionError::Protocol(format!(
                    "AuthStart 之后收到 {other:?}"
                )));
            }
            None => {
                return Err(SessionError::HelperUnavailable(
                    "helper 在认证开始前退出（PAM 服务配置缺失？）".into(),
                ));
            }
        };

        let pending_id = generate_pending_id();
        self.inner.pending.lock().await.insert(
            pending_id.clone(),
            Pending {
                kind,
                helper,
                meta,
                created: Instant::now(),
                authed,
            },
        );
        Ok((pending_id, prompts))
    }

    /// 把一轮答案交给 helper。`More` 时把 pending 放回去；认证通过时把 pending 交出来。
    async fn exchange(
        &self,
        pending_id: &str,
        responses: Vec<IpcPromptResponse>,
    ) -> Result<Exchange> {
        // 取出来再做 IO：不在持锁期间 await，也让并发的重复 respond 得到 PendingNotFound。
        let mut pending = self
            .inner
            .pending
            .lock()
            .await
            .remove(pending_id)
            .ok_or(SessionError::PendingNotFound)?;
        if pending.created.elapsed() > self.inner.cfg.pending_timeout {
            drop(pending.helper);
            return Err(SessionError::PendingNotFound);
        }
        if let Some(user) = pending.authed.take() {
            return Ok(Exchange::Authed {
                pending: Box::new(pending),
                user,
            });
        }

        pending
            .helper
            .send(&ToHelper::AuthRespond { responses })
            .await?;
        match pending.helper.recv().await {
            Ok(Some(FromHelper::Prompts { prompts })) => {
                self.inner
                    .pending
                    .lock()
                    .await
                    .insert(pending_id.to_string(), pending);
                Ok(Exchange::More { prompts })
            }
            Ok(Some(FromHelper::AuthOk { user })) => Ok(Exchange::Authed {
                pending: Box::new(pending),
                user,
            }),
            Ok(Some(FromHelper::AuthFail { reason })) => {
                // helper 自己会退出；等它退干净。
                pending.helper.close().await;
                Err(SessionError::AuthFailed(reason))
            }
            Ok(Some(FromHelper::Error { message })) => {
                pending.helper.close().await;
                Err(SessionError::Protocol(message))
            }
            Ok(Some(other)) => {
                pending.helper.close().await;
                Err(SessionError::Protocol(format!(
                    "AuthRespond 之后收到 {other:?}"
                )))
            }
            Ok(None) => Err(SessionError::HelperUnavailable(
                "helper 在认证中途退出".into(),
            )),
            Err(e) => Err(e.into()),
        }
    }

    async fn teardown(&self, live: Arc<Live>) {
        if let Some(admin) = live.admin.lock().await.take() {
            teardown_admin(admin).await;
        }
        live.worker.shutdown().await;
        if let Some(helper) = live.helper.lock().await.take() {
            helper.close().await;
        }
        if let Err(e) = self.inner.store.delete_session(&live.token_hash).await {
            tracing::warn!(error = %e, "删除会话行失败");
        }
    }
}

enum Exchange {
    More {
        prompts: Vec<Prompt>,
    },
    // Pending 里带着 HelperConn（含 tokio Child），比另一个变体大一个数量级，装箱。
    Authed {
        pending: Box<Pending>,
        user: AuthUser,
    },
}

/// 向已认证的 helper 要一个 worker：`SpawnWorker` → `WorkerSpawned` → `SCM_RIGHTS` → `Hello`。
///
/// `expected_uid` 为 `Some` 时，helper 声明的 uid 与 worker `Hello` 报告的 uid 都必须等于它
/// ——不等说明身份切换没有发生，这种 worker 绝不能用。
async fn spawn_worker(
    helper: &mut HelperConn,
    open_session: bool,
    as_root: bool,
    expected_uid: Option<u32>,
) -> Result<(Arc<WorkerHandle>, bool)> {
    helper
        .send(&ToHelper::SpawnWorker {
            open_session,
            as_root,
        })
        .await?;
    let (pid, uid, session_opened) = match helper.recv().await? {
        Some(FromHelper::WorkerSpawned {
            pid,
            uid,
            session_opened,
            session_error,
        }) => {
            if let Some(reason) = session_error {
                tracing::warn!(%reason, "pam_open_session 失败，用户级 unit 不可用");
            }
            (pid, uid, session_opened)
        }
        Some(FromHelper::Error { message }) => {
            return Err(if as_root {
                SessionError::ElevationDenied(message)
            } else {
                SessionError::Worker(message)
            });
        }
        Some(other) => {
            return Err(SessionError::Protocol(format!(
                "SpawnWorker 之后收到 {other:?}"
            )));
        }
        None => {
            return Err(SessionError::HelperUnavailable(
                "helper 在拉起 worker 时退出".into(),
            ));
        }
    };
    if let Some(expected) = expected_uid
        && uid != expected
    {
        return Err(SessionError::Worker(format!(
            "helper 声明的 worker uid={uid} 与预期 {expected} 不符"
        )));
    }
    let fd = helper.recv_fd().await?;
    let worker = WorkerHandle::connect(fd, pid, expected_uid).await?;
    Ok((Arc::new(worker), session_opened))
}

async fn teardown_admin(admin: Admin) {
    admin.worker.shutdown().await;
    admin.helper.close().await;
}
