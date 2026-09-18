//! 一台主机的运行时：起来、提供 API、关停。
//!
//! # 为什么这一层要存在
//!
//! `10-node-layer.md` §3.2 把 API 的单位定成了 `Router`，但光有 Router 还不够——
//! Router 要拿状态，状态背后是一整套活的东西：数据库、会话管理器、指标引擎、
//! provider 注册表、终端注册表，外加五个周期任务。这些东西**起的顺序有讲究**
//! （终端注册表要在会话管理器之后装进去，审计观察者要在终端注册表之后接上），
//! **关的顺序也有讲究**（先落盘再关库，worker 要在库之前收）。
//!
//! 这套编排原先长在 `strixmaid` 的 `serve_with` 里，与「绑 TCP 端口、挂前端资源」
//! 混在同一个函数中。于是 Agent 想装载完整 node 时无从下手：它不监听端口，却要
//! 上面全部东西。[`Node`] 把这条线切开——
//!
//! | 谁的事 | 在哪 |
//! |---|---|
//! | 起这台主机的运行时、给出 Router、按档位关停 | [`Node`]（本模块） |
//! | 把 Router 摆到哪个传输上、前端资源、节点目录 | 宿主 |
//!
//! # 宿主注入的两处
//!
//! node 只认识本机这一个节点，凡是「别的节点」的概念都由宿主注入：
//! [`RemoteSnapshots`] 在 [`Node::start`] 时给，宿主追加的受保护路由在
//! [`Node::router`] 时给。后者必须晚给——`/nodes` 的状态要用 node 自己的
//! [`Node::store`] 与 [`Node::auth`]，它们得先存在。

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use strixmaid_core::capability::{CapabilityRegistry, ProbeReport};
use strixmaid_core::config::Config;
use strixmaid_core::metrics::MetricsEngine;
use strixmaid_core::providers::log::{LogProvider, pick_log_provider};
use strixmaid_core::providers::process::ProcProvider;
use strixmaid_core::providers::service::icon::ServiceIcons;
use strixmaid_core::providers::service::{ServiceProvider, pick_service_provider};
use strixmaid_core::providers::system::HostProvider;
use strixmaid_core::session::SessionManager;
use strixmaid_core::store::Store;
use strixmaid_core::terminal::TerminalRegistry;
use tokio::task::JoinHandle;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::AuthState;
use crate::state::AppState;
use crate::{RemoteSnapshots, ShutdownKind, StartupReporter, URGENT_CLEANUP, routes, ws};

/// 会话清理的周期。
const SESSION_SWEEP: Duration = Duration::from_secs(5);

/// 空闲终端的回收周期。
///
/// 取 30 秒：空闲上限默认 30 分钟，这个粒度足够，又不至于让一个开着 root shell
/// 的终端在超时后还多活很久。
const TERMINAL_SWEEP: Duration = Duration::from_secs(30);

/// `system.health` 的重算周期（roadmap/04 §B.2）。
const HEALTH_INTERVAL: Duration = Duration::from_secs(30);

/// 一台主机的完整运行时。
///
/// 构造即启动（[`Node::start`]），因此不存在「建好了但还没跑起来」的中间态。
/// 关停走 [`Node::shutdown`]；`Drop` 时**不**自动关停——关停要 `await`，
/// 而且档位只有宿主知道。
pub struct Node {
    config: Config,
    store: Store,
    sessions: SessionManager,
    auth: Arc<AuthState>,
    engine: MetricsEngine,
    terminals: Arc<TerminalRegistry>,
    proc: ProcProvider,
    service_icons: ServiceIcons,
    app: AppState,
    hub: Arc<ws::Hub>,
    capabilities: Arc<routes::capabilities::CapabilityState>,
    remotes: Option<Arc<dyn RemoteSnapshots>>,
    /// 周期任务。关停时一律 `abort`——它们只是定时器，没有需要落盘的状态。
    tasks: Vec<JoinHandle<()>>,
}

impl Node {
    /// 起这台主机：开库 → 会话管理 → 终端注册表 → 指标引擎 → 能力探测 → WS 频道。
    ///
    /// `reporter` 在每一步之前被回调，服务模式下折算成 SCM 的
    /// `SERVICE_START_PENDING` 上报。**这里不调 [`StartupReporter::ready`]**：
    /// 「就绪」的判据是宿主的传输已经能收请求了，而传输是宿主的事。
    ///
    /// `remotes` 是别的节点的快照来源，`None` 时所有 `?node=X` 查询的行为与
    /// 「该节点没连过」一致（见 [`RemoteSnapshots`]）。
    pub async fn start(
        config: Config,
        reporter: &dyn StartupReporter,
        remotes: Option<Arc<dyn RemoteSnapshots>>,
    ) -> anyhow::Result<Self> {
        // ---- 存储 ----
        reporter.stage("打开数据库");
        tokio::fs::create_dir_all(&config.data_dir)
            .await
            .with_context(|| format!("创建数据目录失败: {}", config.data_dir.display()))?;
        let store = Store::open_with(&config.db_path(), config.metrics.retention)
            .await
            .context("打开数据库失败")?;

        // ---- 会话（PAM helper）----
        reporter.stage("会话管理");
        let sessions = SessionManager::with_process_helper(store.clone(), &config)
            .await
            .context("初始化会话管理失败")?;
        let mut tasks = vec![sessions.spawn_sweeper(SESSION_SWEEP)];
        let auth = AuthState::new(sessions.clone(), config.trusted_proxies.clone());

        // ---- 终端注册表（roadmap/03 §4.3）----
        //
        // 装进 SessionManager：登出与空闲超时都要连带关掉该会话的终端，否则会留下
        // 一个没有主人的登录 shell。装在这里而不是构造时传入，是因为两者互不依赖。
        let terminals = TerminalRegistry::new(config.terminal.clone());
        sessions.set_terminal_registry(terminals.clone());
        // 非 REST 的关闭（空闲、shell 自退、登出）经观察者写审计（roadmap/03 §7）。
        terminals.set_observer(Arc::new(crate::auth::audit::TerminalAudit::new(
            store.clone(),
            strixmaid_core::session::LOCAL_NODE_ID,
        )));
        // 空闲终端回收。
        tasks.push({
            let reg = terminals.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(TERMINAL_SWEEP);
                loop {
                    ticker.tick().await;
                    let n = reg.sweep_idle().await;
                    if n > 0 {
                        tracing::info!(count = n, "回收空闲终端");
                    }
                }
            })
        });

        // ---- 审计保留期清理（roadmap/02 §4.4）----
        tasks.push(routes::audit::spawn_prune_task(
            store.clone(),
            config.audit.retention_secs(),
        ));

        // ---- 指标引擎（常驻采集，与登录无关，§2.2）----
        reporter.stage("指标引擎");
        let engine = MetricsEngine::start(&config.metrics, Some(store.clone()));

        // ---- provider 选择与能力探测 ----
        //
        // 请求**不再**经过这里的 provider（`roadmap/01` §4.3：一律走 worker）。
        // 主进程仍然构造它们，只为四件与登录用户无关的事：启动期的 system 层能力探测、
        // `services.changed` 的事件源、`system.health` 频道的定时检查，
        // 以及进程图标的缓存与预热（见下面的图标预热）。
        reporter.stage("能力探测");
        let svc = pick_service_provider().await;
        let log = pick_log_provider().await;
        let proc = ProcProvider::new();
        let report = probe(&config, &proc, svc.as_ref(), log.as_ref()).await;

        // ---- WS 控制面 ----
        let hub = Arc::new(ws::Hub::new());
        hub.register(Arc::new(with_remotes(
            ws::channels::MetricsLive::new(engine.clone()),
            &remotes,
            ws::channels::MetricsLive::with_agents,
        )));
        if let Some(p) = &svc {
            hub.register(Arc::new(ws::channels::ServicesChanged::new(Arc::clone(p))));
        }
        // logs.follow 按会话取 worker，不用主进程的 log provider——**日志的可见范围
        // 必须随用户**（roadmap/01 §4.4）。这里刻意不加 `if let Some(log)`：
        // 频道可不可用取决于**那个用户的 worker** 里有没有日志后端，
        // 主进程自己的探测结果对它没有决定权。
        hub.register(Arc::new(ws::channels::LogsFollow::new(auth.clone())));
        // `system.health`：健康是全局事实，主进程每 30 秒重算一次并把 failed units
        // 并入（roadmap/04 §B.2）。变更才广播，任务随进程关停一起收。
        let (system_health, health_task) =
            ws::channels::SystemHealth::start(HostProvider::new(), svc.clone(), HEALTH_INTERVAL);
        hub.register(Arc::new(system_health));
        tasks.push(health_task);
        // `processes.live`：按会话投递到 user worker——CPU% 的差分基线在那里，
        // 与 REST 的 `GET /processes` 共享（roadmap/04 §B.3）。
        hub.register(Arc::new(ws::channels::ProcessesLive::new(auth.clone())));

        // ---- 进程图标预热（见 `strixmaid_core::providers::process::icon`）----
        //
        // **在后台跑，绝不阻塞启动**：`spawn` 之后立刻往下走，服务照常开始接受请求。
        // 第一轮预热在任务内部完成，之后每 2 分半钟补热一轮（TTL 的一半）。
        // 本平台不提供图标时（Linux / macOS 的空实现）返回 `None`，一个任务也不起。
        tasks.extend(proc.spawn_icon_warmer());

        let capabilities = Arc::new(routes::capabilities::CapabilityState::new(
            report.system,
            host_identity().await,
            config.session.elevate_groups.clone(),
            auth.clone(),
        ));

        Ok(Self {
            config,
            store,
            sessions,
            auth,
            engine,
            terminals,
            proc,
            service_icons: ServiceIcons::new(),
            app: AppState::new(),
            hub,
            capabilities,
            remotes,
            tasks,
        })
    }

    /// 这台主机的 `/api/v1` 与 `/ws`，不含前端资源与节点前缀。
    ///
    /// `extra_protected` 是宿主追加的受保护路由，与其余受保护路由**同批**套鉴权
    /// （见 [`routes::ApiStates::extra_protected`]）。Server 用它挂 `/nodes`，
    /// Agent 传 `None`。
    ///
    /// 取 `&self` 而不是 `self`：Router 里的状态全是句柄的克隆，同一个 Node 完全
    /// 可以给出多份 Router（`11-multi-host.md` 的 `/` 与 `/nodes/local` 两条路径
    /// 就需要这个）。
    pub fn router(&self, extra_protected: Option<OpenApiRouter<()>>) -> axum::Router {
        let states = routes::ApiStates {
            app: self.app.clone(),
            auth: self.auth.clone(),
            proc: self.proc.clone(),
            service_icons: self.service_icons.clone(),
            capabilities: self.capabilities.clone(),
            metrics: Arc::new(with_remotes(
                routes::metrics::MetricsState::new(self.engine.clone()),
                &self.remotes,
                routes::metrics::MetricsState::with_agents,
            )),
            audit: Arc::new(routes::audit::AuditState::new(self.store.clone())),
            terminals: routes::terminals::TerminalState::new(
                self.terminals.clone(),
                self.auth.clone(),
                self.store.clone(),
            ),
            files: routes::files::FilesState::new(
                self.auth.clone(),
                &self.config.files.allowed_roots,
            ),
            extra_protected,
        };
        crate::router(states, self.hub.clone(), self.auth.clone())
    }

    /// 这台主机的存储。宿主用它建自己的状态（`/nodes` 的节点目录、Agent 拨进来的 WS）。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 鉴权状态。宿主追加的受保护路由要用同一份。
    pub fn auth(&self) -> &Arc<AuthState> {
        &self.auth
    }

    /// 生效中的配置。
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 按档位关停。
    ///
    /// `Graceful` 做全套：落盘未满分钟的指标、逐个关 worker 与 helper、正常关库。
    /// `Urgent` 只做「不做就会丢数据」的两件事并带超时——`sessions.shutdown()` 要
    /// 逐个等 worker 进程退出，在只有几秒的时限里既做不完也没必要：进程一退，
    /// IPC 端点关闭，worker 读到 EOF 自行结束。
    pub async fn shutdown(&self, kind: ShutdownKind) {
        // 周期任务一律直接 abort：它们只是定时器，没有需要落盘的状态。
        for task in &self.tasks {
            task.abort();
        }

        match kind {
            ShutdownKind::Graceful => {
                self.engine.stop().await;
                self.sessions.shutdown().await;
                self.store.close().await;
                tracing::info!("已优雅退出");
            }
            ShutdownKind::Urgent => {
                let done = tokio::time::timeout(URGENT_CLEANUP, async {
                    self.engine.stop().await;
                    self.store.close().await;
                })
                .await;
                if done.is_err() {
                    tracing::error!(
                        budget = ?URGENT_CLEANUP,
                        "紧急关停：落盘与关库未在时限内完成，可能丢失最后一分钟的指标"
                    );
                } else {
                    tracing::info!("已紧急退出（未等待 worker 收尾）");
                }
            }
        }
    }
}

/// 有远端快照来源就装上，没有就原样返回。
///
/// `MetricsLive` 与 `MetricsState` 的 `with_agents` 形状相同（`self` 进、`Self` 出），
/// 两处各写一遍 `match` 只是重复。
fn with_remotes<T>(
    value: T,
    remotes: &Option<Arc<dyn RemoteSnapshots>>,
    attach: fn(T, Arc<dyn RemoteSnapshots>) -> T,
) -> T {
    match remotes {
        Some(r) => attach(value, Arc::clone(r)),
        None => value,
    }
}

/// 注册全部 provider 并探测一遍，把结果记进日志。
async fn probe(
    config: &Config,
    proc: &ProcProvider,
    svc: Option<&Arc<dyn ServiceProvider>>,
    log: Option<&Arc<dyn LogProvider>>,
) -> ProbeReport {
    let mut registry = CapabilityRegistry::from_config(config);
    registry
        .register(Box::new(HostProvider::new()))
        .register(Box::new(proc.clone()));
    if let Some(p) = svc {
        registry.register(Box::new(Arc::clone(p)));
    }
    if let Some(p) = log {
        registry.register(Box::new(Arc::clone(p)));
    }
    let report = registry.probe_all().await;
    for probe in &report.providers {
        tracing::info!(provider = probe.id, probe = ?probe.probe, "能力探测");
    }
    tracing::info!(caps = ?report.system, "system 能力");
    report
}

/// 未认证可见的主机身份（登录页展示 + 发行版主题色）。
///
/// 取不到不算致命：登录页仍能工作，只是身份区退化为空、主题色回落中性灰。
async fn host_identity() -> strixmaid_types::capability::HostIdentity {
    match HostProvider::new().system_info().await {
        Ok(info) => strixmaid_types::capability::HostIdentity {
            hostname: info.hostname,
            os_id: info.os.id,
            os_name: info.os.pretty_name,
            kernel: info.kernel,
        },
        Err(e) => {
            tracing::warn!(error = %e.message, "主机身份获取失败，登录页身份区将为空");
            strixmaid_types::capability::HostIdentity::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    /// 记下 reporter 被回调了什么、什么时候。
    #[derive(Default)]
    struct Recorder {
        stages: Mutex<Vec<String>>,
        ready: Mutex<bool>,
    }

    impl StartupReporter for Recorder {
        fn stage(&self, name: &str) {
            self.stages.lock().unwrap().push(name.to_owned());
        }
        fn ready(&self) {
            *self.ready.lock().unwrap() = true;
        }
    }

    /// 本用例专属的空目录。同进程内按 tag 区分，跨进程按 pid。
    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "strixmaid-node-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config_in(dir: &std::path::Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            ..Config::default()
        }
    }

    async fn get(router: axum::Router, uri: &str) -> StatusCode {
        router
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    /// 起一台主机、要两份 Router、再关掉。
    ///
    /// 这条路径原先埋在 `strixmaid` 的 `serve_with` 里，没有任何测试覆盖——
    /// 它只在进程真的起停时才跑到，而那正是出了错最难看见的地方。
    #[tokio::test]
    async fn 起一台主机_给出路由_再关掉() {
        let dir = scratch("lifecycle");
        let rec = Recorder::default();
        let node = Node::start(config_in(&dir), &rec, None)
            .await
            .expect("Node 应当起得来");

        // 启动阶段要逐个上报：SCM 靠这些回调递增 checkpoint，
        // 少报一步就可能在 `dwWaitHint` 到期时被判启动失败并杀掉进程。
        assert_eq!(
            *rec.stages.lock().unwrap(),
            ["打开数据库", "会话管理", "指标引擎", "能力探测"]
        );
        // `ready` 归宿主：传输还没起来，node 自己报就是撒谎。
        assert!(!*rec.ready.lock().unwrap(), "Node::start 不该报 ready");

        // 同一个 Node 要能给出多份 Router（`11-multi-host.md` 的 `/` 与
        // `/nodes/local` 是两份），所以这里连要两次。
        assert_eq!(get(node.router(None), "/api/v1/health").await, StatusCode::OK);
        assert_eq!(
            get(node.router(None), "/api/v1/services").await,
            StatusCode::UNAUTHORIZED,
            "受保护路由没带 token 就该 401"
        );

        assert!(dir.join("strixmaid.db").exists(), "库应当落在 data_dir 下");
        node.shutdown(ShutdownKind::Graceful).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 宿主追加的路由必须落在**受保护那一批**里。
    ///
    /// Server 用 `extra_protected` 挂 `/nodes`。要是哪天它被 merge 到了鉴权层外面，
    /// 节点目录就变成未认证可读可写——OpenAPI diff 看不出这种事（路径与 schema
    /// 都没变），只有实打实发一个不带 token 的请求才看得见。
    #[tokio::test]
    async fn 宿主追加的路由同批套鉴权() {
        let dir = scratch("extra");
        let rec = Recorder::default();
        let node = Node::start(config_in(&dir), &rec, None)
            .await
            .expect("Node 应当起得来");

        let extra = OpenApiRouter::new().route(
            "/extra-probe",
            axum::routing::get(|| async { "不该看到这个" }),
        );
        assert_eq!(
            get(node.router(Some(extra)), "/api/v1/extra-probe").await,
            StatusCode::UNAUTHORIZED,
        );

        node.shutdown(ShutdownKind::Graceful).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 紧急关停要在时限内返回。
    ///
    /// `Urgent` 只做落盘与关库并带 [`URGENT_CLEANUP`] 的超时——系统在
    /// `CTRL_CLOSE_EVENT` / `SERVICE_CONTROL_SHUTDOWN` 之后只给几秒，
    /// 这条路径要是会卡住，等来的就是 `TerminateProcess`。
    #[tokio::test]
    async fn 紧急关停不超时() {
        let dir = scratch("urgent");
        let rec = Recorder::default();
        let node = Node::start(config_in(&dir), &rec, None)
            .await
            .expect("Node 应当起得来");

        let t = std::time::Instant::now();
        node.shutdown(ShutdownKind::Urgent).await;
        // 留一倍余量：断言的是「没卡住」，不是精确计时。
        assert!(
            t.elapsed() < URGENT_CLEANUP * 2,
            "紧急关停用了 {:?}，超过 {:?} 的两倍",
            t.elapsed(),
            URGENT_CLEANUP
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
