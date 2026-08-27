//! StrixMaid 主二进制入口。
//!
//! 见 `docs/design.md` §2（进程模型）、§12（配置与部署）。
//!
//! - 子命令 `serve`（缺省）启动 HTTP 服务；`worker` 是会话 worker（由 helper setuid 后 exec 拉起）。
//! - 配置按「内置默认 < `/etc/strixmaid/config.toml` < 环境变量 < 命令行」四层合并，
//!   合并与校验都在 `strixmaid_core::config`，本文件只负责把 clap 的解析结果交上去。
//! - 日志一律写 **stderr**，交由 journald 收集，不自写日志文件（§12）。
//! - 收到 SIGTERM / SIGINT 后 axum 停止接受新连接并等待在途请求结束。

mod apidoc;
mod app;
mod assets;
mod auth;
mod cli;
#[cfg(any(debug_assertions, feature = "apidoc"))]
mod debug;
mod embed;
mod error;
mod routes;
mod state;
mod ws;

use std::io::IsTerminal as _;

use anyhow::Context as _;
use clap::Parser as _;
use strixmaid_core::config::{Config, cli_layer};
use tracing_subscriber::EnvFilter;

use crate::cli::{Cli, Command, GlobalArgs, WorkerArgs};
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // worker 以登录用户身份运行，**不读配置文件**：/etc/strixmaid/config.toml 对普通用户
    // 可能不可读，而 worker 也不需要里面的任何东西（它连数据库都不碰）。
    if let Some(Command::Worker(args)) = cli.command {
        return run_worker(&cli.global, args).await;
    }

    // 先加载配置再起 tracing —— 日志级别本身来自配置。
    // 这段时间里的错误由 main 的 Err 返回值打到 stderr，不会丢。
    let config = load_config(&cli.global)?;
    init_tracing(&config, cli.global.log_level.is_some())?;

    serve(config).await
}

/// 四层合并 + 校验。
fn load_config(args: &GlobalArgs) -> anyhow::Result<Config> {
    // --config / STRIXMAID_CONFIG 决定读哪个文件；都没给就用默认路径。
    // 文件不存在不是错误（§12：首次启动无需任何安装物即可跑起来）。
    let path = args.config.clone().unwrap_or_else(Config::config_path);
    let overrides = cli_layer(args.overrides()).context("构造命令行配置层失败")?;

    Config::load_from(&path, Some(overrides))
        .with_context(|| format!("加载配置失败（配置文件: {}）", path.display()))
}

/// 初始化 tracing 订阅者，输出到 stderr。
///
/// 过滤器优先级：
/// 1. 命令行上显式给出的 `--log-level`（`log_level_from_cli == true`）；
/// 2. `RUST_LOG` —— 开发者的逃生口，支持完整的 `EnvFilter` 表达式；
/// 3. 配置里的 `log.level`（本身已含内置默认 / TOML / `STRIXMAID_LOG__LEVEL` 三层）。
///
/// 非 tty 时关闭 ANSI 颜色，避免 journald 里存进一堆转义序列。
fn init_tracing(config: &Config, log_level_from_cli: bool) -> anyhow::Result<()> {
    let level = config.log.level.as_str();
    let filter = if log_level_from_cli {
        EnvFilter::try_new(level).with_context(|| format!("非法的日志级别: {level}"))?
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    Ok(())
}

/// 启动 HTTP 服务：打开存储 → 会话管理 → 指标引擎 → provider 探测 → 路由。
async fn serve(config: Config) -> anyhow::Result<()> {
    use std::sync::Arc;

    use strixmaid_core::capability::CapabilityRegistry;
    use strixmaid_core::metrics::MetricsEngine;
    use strixmaid_core::providers::log::pick_log_provider;
    use strixmaid_core::providers::process::ProcProvider;
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
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("创建数据目录失败: {}", config.data_dir.display()))?;
    let store = Store::open_with(&config.db_path(), config.metrics.retention)
        .await
        .context("打开数据库失败")?;

    // ---- 会话（PAM helper）----
    let sessions = SessionManager::with_process_helper(store.clone(), &config)
        .await
        .context("初始化会话管理失败")?;
    let sweeper = sessions.spawn_sweeper(std::time::Duration::from_secs(5));
    let auth = auth::AuthState::new(sessions.clone(), config.trusted_proxies.clone());

    // ---- 终端注册表（roadmap/03 §4.3）----
    //
    // 装进 SessionManager：登出与空闲超时都要连带关掉该会话的终端，否则会留下
    // 一个没有主人的登录 shell。装在这里而不是构造时传入，是因为两者互不依赖。
    let terminals = TerminalRegistry::new(config.terminal.clone());
    sessions.set_terminal_registry(terminals.clone());
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
    let engine = MetricsEngine::start(&config.metrics, Some(store.clone()));

    // ---- provider 选择与能力探测 ----
    //
    // 请求**不再**经过这里的 provider（`roadmap/01` §4.3：一律走 worker）。
    // 主进程仍然构造它们，只为三件与登录用户无关的事：启动期的 system 层能力探测、
    // `services.changed` 的事件源、以及后续 `system.health` 频道的定时检查。
    let svc = pick_service_provider().await;
    let log = pick_log_provider().await;
    let mut registry = CapabilityRegistry::from_config(&config);
    registry
        .register(Box::new(HostProvider::new()))
        .register(Box::new(ProcProvider::new()));
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

    // ---- WS 控制面 ----
    let hub = Arc::new(ws::Hub::new());
    hub.register(Arc::new(ws::channels::MetricsLive::new(engine.clone())));
    if let Some(p) = &svc {
        hub.register(Arc::new(ws::channels::ServicesChanged::new(Arc::clone(p))));
    }
    // logs.follow 按会话取 worker，不用主进程的 log provider——**日志的可见范围
    // 必须随用户**（roadmap/01 §4.4）。这里刻意不加 `if let Some(log)`：
    // 频道可不可用取决于**那个用户的 worker** 里有没有日志后端，
    // 主进程自己的探测结果对它没有决定权。
    hub.register(Arc::new(ws::channels::LogsFollow::new(auth.clone())));

    // ---- 路由 ----
    let states = routes::ApiStates {
        app: AppState::new(),
        auth: auth.clone(),
        capabilities: Arc::new(routes::capabilities::CapabilityState::new(
            report.system,
            config.session.elevate_groups.clone(),
            auth.clone(),
        )),
        metrics: Arc::new(routes::metrics::MetricsState::new(engine.clone())),
        audit: Arc::new(routes::audit::AuditState::new(store.clone())),
        terminals: routes::terminals::TerminalState::new(
            terminals.clone(),
            auth.clone(),
            store.clone(),
        ),
    };
    let router = app::build(states, hub, auth);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("无法监听 {listen}"))?;
    let local_addr = listener.local_addr().context("获取监听地址失败")?;
    tracing::info!(%local_addr, "开始接受请求");

    // `into_make_service_with_connect_info`：让审计与会话记录拿得到客户端地址。
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP 服务异常退出")?;

    // ---- 收尾：落盘未满分钟、关 worker/helper、关库 ----
    engine.stop().await;
    sweeper.abort();
    terminal_sweeper.abort();
    audit_pruner.abort();
    sessions.shutdown().await;
    store.close().await;
    tracing::info!("已优雅退出");
    Ok(())
}

/// `worker` 子命令：由 helper 完成 PAM 认证与 setuid 后 exec 拉起，
/// 通过 `--ipc-fd`（缺省 3）传入的 socketpair 与主进程通信（§10）。
///
/// 日志级别只看 `--log-level` 与 `RUST_LOG`（helper 会把主进程的 `RUST_LOG` 透传过来），
/// 缺省 `info`。RPC 分发表是 `strixmaid_core::worker::Dispatcher`，provider 后续在这里注册。
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

    let fd = args.ipc_fd.unwrap_or(strixmaid_types::ipc::IPC_FD);
    // provider 在 worker 内构造：它们因此天然是登录用户的身份
    // （roadmap/01 §4.2）。
    let dispatcher =
        std::sync::Arc::new(strixmaid_core::worker::providers::default_dispatcher().await);
    strixmaid_core::worker::run_from_fd(fd, dispatcher)
        .await
        .context("worker 异常退出")
}

/// 等待 SIGTERM 或 SIGINT。
///
/// SIGTERM 是 systemd `stop` 的默认信号，SIGINT 是终端里的 Ctrl-C，两者都要能优雅退出。
async fn shutdown_signal() {
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
}
