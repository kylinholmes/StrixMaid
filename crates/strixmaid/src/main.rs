//! StrixMaid 主二进制入口。
//!
//! 见 `docs/design.md` §2（进程模型）、§12（配置与部署）。
//!
//! - 子命令 `serve`（缺省）启动 HTTP 服务；`worker` 是会话 worker（由 helper 认证并
//!   切换身份后拉起）；`service`（仅 Windows）是 SCM 宿主与服务注册命令，见
//!   `service`（`src/service/`）。
//! - 配置按「内置默认 < 配置文件 < 环境变量 < 命令行」四层合并，
//!   合并与校验都在 `strixmaid_core::config`，本文件只负责把 clap 的解析结果交上去。
//! - 日志默认写 **stderr**：Unix 上交由 journald 收集，不自写日志文件（§12）。
//!   Windows 的服务模式是唯一例外——SCM 拉起的进程没有控制台，stderr 无人接收，
//!   那一路改写文件，见 `service::logging`。
//! - 关停分两档，见 [`ShutdownKind`]。
//!
//! # 为什么 `main` 不是 `#[tokio::main]`
//!
//! Windows 的 `service run` 要求把**进程主线程**交给
//! `StartServiceCtrlDispatcherW`：那个调用直到服务停止才返回，tokio 运行时得建在
//! 它回调出来的分派线程上（理由详见 `service::host` 的模块文档）。
//! `#[tokio::main]` 会把主线程变成 `block_on` 的宿主，与之冲突。因此这里手工建
//! 运行时，只在需要异步的那几条路径上 `block_on`。

// 业务逻辑一律在 `strixmaid-node`（`design.md` §11：AgentCore 是唯一的业务逻辑
// 所在地，Server 与 Agent 都只是它的宿主）。本 crate 只剩四样 Server 独有的东西：
// 前端资源、节点目录、Agent 拨进来的那条 WS，以及进程入口。
mod app;
mod cli;
mod embed;
mod agent;
mod routes_nodes;
#[cfg(windows)]
mod winsvc_app;
mod ws_agent;

use std::future::{Future, IntoFuture as _};
use std::io::IsTerminal as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context as _;
use clap::Parser as _;
use strixmaid_core::config::{Config, cli_layer};
use tracing_subscriber::EnvFilter;

use strixmaid_node::state::AppState;
use strixmaid_node::{NoReporter, ShutdownKind, StartupReporter, routes, ws};

use crate::cli::{Cli, Command, ConfigAction, GlobalArgs, WorkerArgs};


fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Windows 服务子命令必须在进程主线程上处理，理由见本文件的模块文档。
    // 这一支自带（或不需要）运行时，绝不能进到下面的 block_on 里。
    #[cfg(windows)]
    if let Some(Command::Service { mode, action }) = &cli.command {
        return strixmaid_node::winsvc::dispatch(action, winsvc_app::app(*mode, &cli.global));
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 tokio 运行时失败")?
        .block_on(run(cli))
}

/// 除 Windows 服务子命令之外的全部入口，跑在手工建起来的运行时上。
async fn run(cli: Cli) -> anyhow::Result<()> {
    // worker 以登录用户身份运行，**不读配置文件**：配置文件对普通用户
    // 可能不可读，而 worker 也不需要里面的任何东西（它连数据库都不碰）。
    if let Some(Command::Worker(args)) = cli.command {
        return run_worker(&cli.global, args).await;
    }

    // `config example` 只打印文本，不加载配置（roadmap/06 §3.4）。
    if let Some(Command::Config { action }) = &cli.command {
        match action {
            // 两种模式的必填项完全不同，所以是两份配置文件、两份示例。
            ConfigAction::Example { agent: false } => {
                print!("{}", Config::example_toml());
                return Ok(());
            }
            ConfigAction::Example { agent: true } => {
                print!("{}", crate::agent::config::AgentConfig::example_toml());
                return Ok(());
            }
        }
    }

    // Agent 模式：读的是另一个配置文件，也不监听端口，与下面 server 那条路径
    // 从第一步就分岔，所以在这里整个截住。
    if let Some(Command::Agent(args)) = &cli.command {
        return crate::agent::run(&cli.global, args).await;
    }

    // 先加载配置再起 tracing —— 日志级别本身来自配置。
    // 这段时间里的错误由 main 的 Err 返回值打到 stderr，不会丢。
    let config = load_config(&cli.global)?;

    // `--check-config`：加载 + 校验（load_config 内完成）即是全部工作。
    if cli.check_config {
        println!("配置有效（{}）", config.listen);
        return Ok(());
    }

    init_tracing(&config, cli.global.log_level.is_some())?;

    serve(config).await
}

/// 四层合并 + 校验。
pub(crate) fn load_config(args: &GlobalArgs) -> anyhow::Result<Config> {
    // --config / STRIXMAID_CONFIG 决定读哪个文件；都没给就用默认路径。
    // 文件不存在不是错误（§12：首次启动无需任何安装物即可跑起来）。
    let path = args.config.clone().unwrap_or_else(Config::config_path);
    let overrides = cli_layer(args.overrides()).context("构造命令行配置层失败")?;

    Config::load_from(&path, Some(overrides))
        .with_context(|| format!("加载配置失败（配置文件: {}）", path.display()))
}

/// 组装日志过滤器。
///
/// 过滤器优先级：
/// 1. 命令行上显式给出的 `--log-level`（`log_level_from_cli == true`）；
/// 2. `RUST_LOG` —— 开发者的逃生口，支持完整的 `EnvFilter` 表达式；
/// 3. 配置里的 `log.level`（本身已含内置默认 / TOML / `STRIXMAID_LOG__LEVEL` 三层）。
///
/// 抽出来单放一处是因为 Windows 服务模式要用同一套优先级、换一个输出端
/// （见 `service::logging`），两处若各写一遍迟早会走样。
pub(crate) fn log_filter(config: &Config, log_level_from_cli: bool) -> anyhow::Result<EnvFilter> {
    let level = config.log.level.as_str();
    if log_level_from_cli {
        EnvFilter::try_new(level).with_context(|| format!("非法的日志级别: {level}"))
    } else {
        Ok(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level)))
    }
}

/// 初始化 tracing 订阅者，输出到 stderr。
///
/// 非 tty 时关闭 ANSI 颜色，避免 journald 里存进一堆转义序列。
fn init_tracing(config: &Config, log_level_from_cli: bool) -> anyhow::Result<()> {
    let filter = log_filter(config, log_level_from_cli)?;

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    Ok(())
}

/// 前台运行：关停信号取自当前平台的控制台/信号机制。
async fn serve(config: Config) -> anyhow::Result<()> {
    serve_with(config, Arc::new(NoReporter), shutdown_signal()).await
}

/// 启动 HTTP 服务：打开存储 → 会话管理 → 指标引擎 → provider 探测 → 路由。
///
/// `reporter` 在启动的各个阶段被回调（服务模式下折算成 SCM 的
/// `SERVICE_START_PENDING` 上报）；`shutdown` 一旦解析出 [`ShutdownKind`]
/// 就进入关停，档位决定后面还等多久。
pub(crate) async fn serve_with(
    config: Config,
    reporter: Arc<dyn StartupReporter>,
    shutdown: impl Future<Output = ShutdownKind> + Send + 'static,
) -> anyhow::Result<()> {
    use strixmaid_core::capability::CapabilityRegistry;
    use strixmaid_core::metrics::MetricsEngine;
    use strixmaid_core::providers::log::pick_log_provider;
    use strixmaid_core::providers::process::ProcProvider;
    use strixmaid_core::providers::service::icon::ServiceIcons;
    use strixmaid_core::providers::service::pick_service_provider;
    use strixmaid_core::providers::system::HostProvider;
    use strixmaid_core::session::SessionManager;
    use strixmaid_core::terminal::TerminalRegistry;
    use strixmaid_core::store::Store;

    let listen = config.listen_addr().context("监听地址不合法")?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %config.data_dir.display(),
        db = %config.db_path().display(),
        "StrixMaid 启动中"
    );

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
    let sweeper = sessions.spawn_sweeper(std::time::Duration::from_secs(5));
    let auth = strixmaid_node::auth::AuthState::new(sessions.clone(), config.trusted_proxies.clone());

    // ---- 终端注册表（roadmap/03 §4.3）----
    //
    // 装进 SessionManager：登出与空闲超时都要连带关掉该会话的终端，否则会留下
    // 一个没有主人的登录 shell。装在这里而不是构造时传入，是因为两者互不依赖。
    let terminals = TerminalRegistry::new(config.terminal.clone());
    sessions.set_terminal_registry(terminals.clone());
    // 非 REST 的关闭（空闲、shell 自退、登出）经观察者写审计（roadmap/03 §7）。
    terminals.set_observer(Arc::new(strixmaid_node::auth::audit::TerminalAudit::new(
        store.clone(),
        strixmaid_core::session::LOCAL_NODE_ID,
    )));
    // 空闲终端回收。周期取 30 秒：空闲上限默认 30 分钟，这个粒度足够，
    // 又不至于让一个开着 root shell 的终端在超时后还多活很久。
    let terminal_sweeper = {
        let reg = terminals.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                ticker.tick().await;
                let n = reg.sweep_idle().await;
                if n > 0 {
                    tracing::info!(count = n, "回收空闲终端");
                }
            }
        })
    };

    // ---- 审计保留期清理（roadmap/02 §4.4）----
    let audit_pruner =
        routes::audit::spawn_prune_task(store.clone(), config.audit.retention_secs());

    // ---- 指标引擎（常驻采集，与登录无关，§2.2）----
    reporter.stage("指标引擎");
    let engine = MetricsEngine::start(&config.metrics, Some(store.clone()));

    // ---- provider 选择与能力探测 ----
    //
    // 请求**不再**经过这里的 provider（`roadmap/01` §4.3：一律走 worker）。
    // 主进程仍然构造它们，只为四件与登录用户无关的事：启动期的 system 层能力探测、
    // `services.changed` 的事件源、后续 `system.health` 频道的定时检查，
    // 以及进程图标的缓存与预热（见下面的 `icon_warmer`）。
    reporter.stage("能力探测");
    let svc = pick_service_provider().await;
    let log = pick_log_provider().await;
    let proc = ProcProvider::new();
    let mut registry = CapabilityRegistry::from_config(&config);
    registry
        .register(Box::new(HostProvider::new()))
        .register(Box::new(proc.clone()));
    if let Some(p) = &svc {
        registry.register(Box::new(Arc::clone(p)));
    }
    if let Some(p) = &log {
        registry.register(Box::new(Arc::clone(p)));
    }
    let report = registry.probe_all().await;
    for probe in &report.providers {
        tracing::info!(provider = probe.id, probe = ?probe.probe, "能力探测");
    }
    tracing::info!(caps = ?report.system, "system 能力");

    // ---- Agent 汇聚（roadmap/05）----
    let agents = crate::ws_agent::AgentRegistry::new();

    // ---- WS 控制面 ----
    let hub = Arc::new(ws::Hub::new());
    hub.register(Arc::new(
        ws::channels::MetricsLive::new(engine.clone()).with_agents(agents.clone()),
    ));
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
    let (system_health, health_task) = ws::channels::SystemHealth::start(
        HostProvider::new(),
        svc.clone(),
        std::time::Duration::from_secs(30),
    );
    hub.register(Arc::new(system_health));
    // `processes.live`：按会话投递到 user worker——CPU% 的差分基线在那里，
    // 与 REST 的 `GET /processes` 共享（roadmap/04 §B.3）。
    hub.register(Arc::new(ws::channels::ProcessesLive::new(auth.clone())));

    // 未认证可见的主机身份（登录页展示 + 发行版主题色）。取不到不算致命：
    // 登录页仍能工作，只是身份区退化为空、主题色回落中性灰。
    let host_identity = match HostProvider::new().system_info().await {
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
    };

    // ---- 进程图标预热（见 `strixmaid_core::providers::process::icon`）----
    //
    // **在后台跑，绝不阻塞启动**：`spawn` 之后立刻往下走，服务照常开始接受请求。
    // 第一轮预热在任务内部完成，之后每 2 分半钟补热一轮（TTL 的一半）。
    // 本平台不提供图标时（Linux / macOS 的空实现）返回 `None`，一个任务也不起。
    let icon_warmer = proc.spawn_icon_warmer();

    // ---- 路由 ----
    reporter.stage("装配路由");
    let states = routes::ApiStates {
        app: AppState::new(),
        auth: auth.clone(),
        proc,
        service_icons: ServiceIcons::new(),
        capabilities: Arc::new(routes::capabilities::CapabilityState::new(
            report.system,
            host_identity,
            config.session.elevate_groups.clone(),
            auth.clone(),
        )),
        metrics: Arc::new(
            routes::metrics::MetricsState::new(engine.clone()).with_agents(agents.clone()),
        ),
        audit: Arc::new(routes::audit::AuditState::new(store.clone())),
        terminals: routes::terminals::TerminalState::new(
            terminals.clone(),
            auth.clone(),
            store.clone(),
        ),
        files: routes::files::FilesState::new(auth.clone(), &config.files.allowed_roots),
        // `/nodes` 是 Server 独有的：node 只认识本机这一个节点。经 `extra_protected`
        // 挂进去，与其余受保护路由同批套鉴权（见 `routes::ApiStates` 的字段说明）。
        extra_protected: Some(crate::routes_nodes::router(
            crate::routes_nodes::NodesState::new(store.clone(), agents.clone(), auth.clone()),
        )),
    };
    let router = app::build(
        states,
        hub,
        auth,
        crate::ws_agent::AgentSocketState {
            store: store.clone(),
            registry: agents.clone(),
        },
    );

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("无法监听 {listen}"))?;
    let local_addr = listener.local_addr().context("获取监听地址失败")?;
    tracing::info!(%local_addr, "开始接受请求");
    reporter.ready();

    // 关停档位要跨两处读：交给 axum 的 graceful-shutdown future 在那里写入，
    // axum 返回之后的收尾段在这里读出。用 `AtomicBool` 而不是把档位从 future
    // 里「返回」出来，是因为 `with_graceful_shutdown` 只接受
    // `Future<Output = ()>`，拿不到返回值。配套的 oneshot 只负责叫醒下面那个
    // `select!`——不用 `Notify`，是因为 `notify_waiters` 只叫醒**此刻已登记**的
    // 等待者，而信号完全可能在 `select!` 第一次轮询 server 时就已经触发。
    let urgent = Arc::new(AtomicBool::new(false));
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();

    // `into_make_service_with_connect_info`：让审计与会话记录拿得到客户端地址。
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown({
        let urgent = Arc::clone(&urgent);
        async move {
            let kind = shutdown.await;
            urgent.store(kind == ShutdownKind::Urgent, Ordering::SeqCst);
            tracing::info!(?kind, "收到关停请求");
            let _ = signalled_tx.send(());
        }
    })
    // axum 0.8 的 `WithGracefulShutdown` 只实现 `IntoFuture`，不实现 `Future`：
    // 直接 `.await` 时由 `.await` 隐式转换，而这里要把它按引用轮询两次
    // （先等信号，再限时排空），必须显式取出 future。
    .into_future();
    tokio::pin!(server);

    // 先等到「信号到了」或「服务自己结束了」，此时才知道该给多少排空时间。
    let mut finished = false;
    tokio::select! {
        res = &mut server => {
            finished = true;
            res.context("HTTP 服务异常退出")?;
        }
        _ = signalled_rx => {}
    }

    let kind = if urgent.load(Ordering::SeqCst) {
        ShutdownKind::Urgent
    } else {
        ShutdownKind::Graceful
    };

    // 排空在途连接，但不无限等：见 [`GRACEFUL_DRAIN`] 的说明。
    if !finished {
        let budget = kind.drain_budget();
        match tokio::time::timeout(budget, &mut server).await {
            Ok(res) => res.context("HTTP 服务异常退出")?,
            Err(_) => tracing::warn!(?budget, "仍有连接未结束（多为长连的 WebSocket），不再等待"),
        }
    }

    // ---- 收尾 ----
    //
    // 周期任务一律直接 abort：它们只是定时器，没有需要落盘的状态。
    sweeper.abort();
    terminal_sweeper.abort();
    health_task.abort();
    audit_pruner.abort();
    if let Some(warmer) = icon_warmer {
        warmer.abort();
    }

    match kind {
        ShutdownKind::Graceful => {
            // 落盘未满分钟、逐个关 worker/helper、关库。
            engine.stop().await;
            sessions.shutdown().await;
            store.close().await;
            tracing::info!("已优雅退出");
        }
        ShutdownKind::Urgent => {
            // 只保数据：`sessions.shutdown()` 要逐个等 worker 进程退出，
            // 在只有几秒的时限里既做不完也没必要——进程一退，IPC 端点关闭，
            // worker 读到 EOF 自行结束。
            let done = tokio::time::timeout(strixmaid_node::URGENT_CLEANUP, async {
                engine.stop().await;
                store.close().await;
            })
            .await;
            if done.is_err() {
                tracing::error!(
                    budget = ?strixmaid_node::URGENT_CLEANUP,
                    "紧急关停：落盘与关库未在时限内完成，可能丢失最后一分钟的指标"
                );
            } else {
                tracing::info!("已紧急退出（未等待 worker 收尾）");
            }
        }
    }
    Ok(())
}

/// `worker` 子命令：由 helper 完成认证与身份切换后拉起，通过命令行上指定的
/// IPC 端点与主进程通信（§10）。
///
/// 日志级别只看 `--log-level` 与 `RUST_LOG`（helper 会把主进程的 `RUST_LOG` 透传过来），
/// 缺省 `info`。RPC 分发表是 `strixmaid_core::worker::Dispatcher`，provider 后续在这里注册。
///
/// # 两个平台的端点不是同一种东西
///
/// Unix：helper `fork` 后把 socketpair 的一端 `dup2` 到约定的 fd（`IPC_FD`，3），
/// 再 `exec` 自己；fd 号是**约定**的，`--ipc-fd` 只是给非标准场景留的口子。
/// fd 号折算成 `strixmaid_core::session::channel::RawAttachment`（`u64`）后交给
/// `run_from_ipc`。
///
/// Windows：没有 fd 继承那一套，helper 建的是命名管道，而且**交出去的东西随
/// 拉起方式而变**：
///
/// | helper 用的 API | 能交出句柄 | worker 收到 | core 入口 |
/// |---|---|---|---|
/// | `CreateProcessAsUserW` / `CreateProcessW` | 能（标成可继承） | `--ipc-handle <十进制>` | `run_from_ipc` |
/// | `CreateProcessWithTokenW` | **不能** | `--ipc-pipe <名字>` | `run_from_pipe` |
///
/// 第二行是本质限制而非实现取舍：那一级的进程由 seclogon 服务代建，函数签名里
/// 连 `bInheritHandles` 都没有，helper 句柄表里的东西到不了 worker，只能把名字
/// 告诉它、由它自己连一次。选哪一条由 [`crate::cli::WorkerArgs::endpoint`] 判定。
async fn run_worker(global: &GlobalArgs, args: WorkerArgs) -> anyhow::Result<()> {
    let filter = match global.log_level {
        Some(level) => EnvFilter::new(level.as_str()),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    // 端点先定下来：参数给错时不该先花几百毫秒把 provider 探测一遍。
    #[cfg(unix)]
    let raw = {
        let fd = args.ipc_fd.unwrap_or(strixmaid_types::ipc::IPC_FD);
        u64::try_from(fd).with_context(|| format!("--ipc-fd 必须非负，收到 {fd}"))?
    };
    #[cfg(windows)]
    let endpoint = args.endpoint()?;

    // provider 在 worker 内构造：它们因此天然是登录用户的身份
    // （roadmap/01 §4.2）。
    let dispatcher =
        std::sync::Arc::new(strixmaid_core::worker::providers::default_dispatcher().await);

    #[cfg(unix)]
    {
        strixmaid_core::worker::run_from_ipc(raw, dispatcher)
            .await
            .context("worker 异常退出")
    }

    #[cfg(windows)]
    match endpoint {
        crate::cli::WorkerEndpoint::Handle(raw) => {
            strixmaid_core::worker::run_from_ipc(raw, dispatcher)
                .await
                .context("worker 异常退出")
        }
        crate::cli::WorkerEndpoint::Pipe(name) => {
            strixmaid_core::worker::run_from_pipe(&name, dispatcher)
                .await
                .context("worker 异常退出")
        }
    }
}

/// 等待 SIGTERM 或 SIGINT。
///
/// SIGTERM 是 systemd `stop` 的默认信号，SIGINT 是终端里的 Ctrl-C，两者都要能优雅退出。
/// Unix 上没有「时限很短的关停信号」这回事，因此恒定返回
/// [`ShutdownKind::Graceful`]。
#[cfg(unix)]
async fn shutdown_signal() -> ShutdownKind {
    use tokio::signal::unix::{SignalKind, signal};

    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "注册 SIGINT 处理器失败");
            // 注册失败就永久挂起，让另一路信号负责退出。
            std::future::pending::<()>().await;
        }
    };

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "注册 SIGTERM 处理器失败");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = ctrl_c => tracing::info!("收到 SIGINT，开始优雅退出"),
        _ = terminate => tracing::info!("收到 SIGTERM，开始优雅退出"),
    }
    ShutdownKind::Graceful
}

/// 等待控制台关停事件（Windows 前台运行）。
///
/// Windows 没有信号，也没有 `SIGTERM`。控制台程序收到的是四种控制事件，
/// 它们的语义与**系统给的时限**并不一样：
///
/// | 事件 | 触发方式 | 折算档位 | 系统给的时限 |
/// |---|---|---|---|
/// | `CTRL_C_EVENT` | Ctrl-C | [`ShutdownKind::Graceful`] | 不限 |
/// | `CTRL_BREAK_EVENT` | Ctrl-Break | [`ShutdownKind::Graceful`] | 不限 |
/// | `CTRL_CLOSE_EVENT` | 关掉控制台窗口 | [`ShutdownKind::Urgent`] | 约 5 秒 |
/// | `CTRL_SHUTDOWN_EVENT` | 关机 / 重启 | [`ShutdownKind::Urgent`] | 约 5 秒 |
///
/// 前两个是 Unix 上 `SIGINT` / `SIGTERM` 的真正对等物：处理函数想跑多久跑多久，
/// 进程自己决定何时退出。
///
/// **后两个不是。** 它们的处理函数跑在系统创建的线程上，超过
/// `HKLM\SYSTEM\CurrentControlSet\Control\WaitToKillServiceTimeout`
/// （现代 Windows 默认 5000 毫秒，关机场景还可能更短）系统就直接
/// `TerminateProcess`，且**从处理函数返回也视为「我处理完了」**——返回
/// `TRUE` 并不会换来更多时间。因此这两种事件下能做的收尾极其有限：
/// 来不及排空在途的 HTTP 请求，更来不及逐个等 worker 子进程退出。
/// 这正是 [`ShutdownKind::Urgent`] 存在的理由。
///
/// 注销事件（`CTRL_LOGOFF_EVENT`）刻意不订阅：本进程的定位是机器级服务，
/// 某个交互用户注销与它无关，订阅了反而会在多用户机器上被误关。
#[cfg(windows)]
async fn shutdown_signal() -> ShutdownKind {
    use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};

    /// 注册失败时让这一路永久挂起，把退出交给其余几路，而不是整个放弃关停能力。
    macro_rules! wait {
        ($reg:expr, $name:expr) => {
            async {
                match $reg {
                    Ok(mut sig) => {
                        sig.recv().await;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, event = $name, "注册控制台事件处理器失败");
                        std::future::pending::<()>().await;
                    }
                }
            }
        };
    }

    tokio::select! {
        _ = wait!(ctrl_c(), "CTRL_C") => {
            tracing::info!("收到 Ctrl-C，开始优雅退出");
            ShutdownKind::Graceful
        }
        _ = wait!(ctrl_break(), "CTRL_BREAK") => {
            tracing::info!("收到 Ctrl-Break，开始优雅退出");
            ShutdownKind::Graceful
        }
        _ = wait!(ctrl_close(), "CTRL_CLOSE") => {
            tracing::warn!("控制台被关闭，系统只给几秒，走紧急关停");
            ShutdownKind::Urgent
        }
        _ = wait!(ctrl_shutdown(), "CTRL_SHUTDOWN") => {
            tracing::warn!("系统正在关机，只给几秒，走紧急关停");
            ShutdownKind::Urgent
        }
    }
}
