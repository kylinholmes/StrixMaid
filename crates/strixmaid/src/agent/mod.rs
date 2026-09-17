//! Agent 模式：本地采集并拨号回上级 Server。
//!
//! # 与 `serve` 是同一个二进制
//!
//! `design.md` §11：「AgentCore 是唯一的业务逻辑所在地，Server 与 Agent 都只是它的
//! 宿主」。两种模式共用同一套采集、环形缓冲、五层聚合与本地存储（全在
//! `strixmaid-core`），区别只有三样：
//!
//! | | `serve` | `agent` |
//! |---|---|---|
//! | 监听端口 | 是 | 否 |
//! | 嵌入前端 | 是 | 是（同一个二进制，只是用不到） |
//! | 拨号回上级 | 否 | 是 |
//! | 管下级节点 | 是 | 否 |
//!
//! 2026-09-17 把原来的 `strixmaid-agent` 二进制并了进来。此前那 4.18 MB 与 10.5 MB
//! 的差距几乎全是 node 层（Router、认证、审计、全部路由），而 `roadmap/11-multi-host.md`
//! 要让 Agent 拿到的正是这一层——那个体积躲不掉，合不合并都一样。合并真正多花的
//! 只有前端资源那 0.83 MB，换来的是产物矩阵减半、推装时推的就是自己这份二进制，
//! 以及「级联」与「就地提升成 Server」从换二进制变成改一个参数。
//!
//! # 关停分两档
//!
//! 与 server 同一套 [`ShutdownKind`]：`Graceful` 做全套，`Urgent` 只做「不做就会丢
//! 数据」的两件事——把未满分钟的指标落盘、正常关闭 SQLite——并带超时。Windows 的
//! `CTRL_CLOSE_EVENT` 与 SCM 的 `SERVICE_CONTROL_SHUTDOWN` 只给几秒，到点直接
//! `TerminateProcess`，那时做全套是徒劳。

pub mod client;
pub mod config;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use strixmaid_core::metrics::MetricsEngine;
use strixmaid_core::store::Store;
use strixmaid_node::{NoReporter, ShutdownKind, StartupReporter};

use crate::cli::{AgentArgs, GlobalArgs};
use config::AgentConfig;

/// `strixmaid agent` 的入口：装配 tracing 之后跑到关停信号为止。
pub async fn run(global: &GlobalArgs, args: &AgentArgs) -> anyhow::Result<()> {
    if global.listen.is_some() {
        anyhow::bail!(
            "`--listen` 只对 `serve` 有意义：Agent 不监听端口，它主动拨号回上级 Server。\
             要指定上级地址请用 `--server-url`。"
        );
    }

    let cfg = load(global, args)?;
    init_tracing(global)?;
    run_with(cfg, Arc::new(NoReporter), crate::shutdown_signal()).await
}

/// 读 Agent 配置。四层优先级与 server 一致（内置默认 < 文件 < 环境变量 < 命令行）。
pub fn load(global: &GlobalArgs, args: &AgentArgs) -> anyhow::Result<AgentConfig> {
    AgentConfig::load_with(global.config.as_deref(), Some(&args.overrides(global)))
}

/// 日志输出到 stderr，与 server 的前台模式同一套口径。
fn init_tracing(global: &GlobalArgs) -> anyhow::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            global.log_level.map_or("info", |l| l.as_str()),
        )
    });
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .try_init()
        .ok();
    Ok(())
}

/// 跑完整个 Agent，到 `shutdown` 产出档位为止。
///
/// 抽成独立函数是为了让 Windows 服务托管能复用它：那边不能用 `#[tokio::main]`，
/// 运行时建在 SCM 的分派线程上，关停档位由控制处理器给（见
/// `strixmaid_node::winsvc`）。`reporter` 让托管方能在启动的每一步上报递增的
/// checkpoint，否则 SCM 会按超时把进程杀掉。
pub async fn run_with(
    cfg: AgentConfig,
    reporter: Arc<dyn StartupReporter>,
    shutdown: impl Future<Output = ShutdownKind> + Send + 'static,
) -> anyhow::Result<()> {
    let node_id = cfg.resolve_node_id()?;
    let token = cfg.resolve_token()?;

    reporter.stage("打开数据库");
    tokio::fs::create_dir_all(&cfg.data_dir)
        .await
        .with_context(|| format!("创建数据目录失败: {}", cfg.data_dir.display()))?;
    let store = Store::open_with(&cfg.db_path(), cfg.metrics.retention)
        .await
        .context("打开本地数据库失败")?;

    // 与 Server 同一套链路：采集 → 环 → 每分钟落盘 → 本地五层聚合。
    reporter.stage("指标引擎");
    let engine = MetricsEngine::start(&cfg.metrics, Some(store.clone()));

    // system 层能力如实探测：Agent 上通常没有 helper，那就是 false。
    reporter.stage("能力探测");
    let caps = strixmaid_core::capability::probe_system(Path::new("strixmaid-helper"));

    let node_name = match &cfg.node_name {
        Some(n) => n.clone(),
        None => host_name().await,
    };
    tracing::info!(
        node = %node_id,
        name = %node_name,
        server = %cfg.server_url,
        db = %cfg.db_path().display(),
        version = env!("CARGO_PKG_VERSION"),
        "strixmaid agent 启动"
    );

    // 关停档位要传给收尾逻辑，所以这里用 watch 而不是 oneshot：
    // `client::run` 只关心「该停了」，收尾才关心「还能做多少」。
    let (kind_tx, kind_rx) = tokio::sync::watch::channel(None::<ShutdownKind>);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let kind = shutdown.await;
        let _ = kind_tx.send(Some(kind));
        let _ = stop_tx.send(true);
    });

    reporter.ready();
    client::run(
        client::AgentRuntime {
            server_url: cfg.server_url.clone(),
            node_id,
            node_name,
            token,
            caps,
            store: store.clone(),
            engine: engine.clone(),
            sync_interval: std::time::Duration::from_secs(cfg.sync_interval_secs),
        },
        stop_rx,
    )
    .await;

    let kind = (*kind_rx.borrow()).unwrap_or(ShutdownKind::Graceful);
    shutdown_store(engine, store, kind).await;
    Ok(())
}

/// 收尾：把未满分钟落盘、关库。
///
/// `Urgent` 档给整段收尾一个上限——系统在 `CTRL_CLOSE_EVENT` /
/// `SERVICE_CONTROL_SHUTDOWN` 之后只给几秒，超时即 `TerminateProcess`。宁可少收
/// 几个后台任务，也要保证 SQLite 有机会正常关闭（关不干净会留下 -wal 文件，
/// 下次启动要做恢复）。
async fn shutdown_store(engine: MetricsEngine, store: Store, kind: ShutdownKind) {
    let finish = async {
        engine.stop().await;
        store.close().await;
    };
    match kind {
        ShutdownKind::Graceful => {
            finish.await;
            tracing::info!("已优雅退出");
        }
        ShutdownKind::Urgent => {
            if tokio::time::timeout(strixmaid_node::URGENT_CLEANUP, finish)
                .await
                .is_err()
            {
                tracing::warn!(
                    budget = ?strixmaid_node::URGENT_CLEANUP,
                    "紧急关停超时，未能干净收尾"
                );
            } else {
                tracing::info!("已紧急退出");
            }
        }
    }
}

/// 主机名，取不到时退回固定串比退回 node_id 直白。
async fn host_name() -> String {
    match strixmaid_core::providers::system::HostProvider::new()
        .system_info()
        .await
    {
        Ok(info) => info.hostname,
        Err(_) => "unknown".to_string(),
    }
}
