//! `strixmaid-agent`：远程节点的常驻采集器（roadmap/05）。
//!
//! 与 Server 完全相同的采集、环形缓冲与五层聚合（复用 `strixmaid-core`），
//! 外加一条到 Server 的推送连接（`client`）。**只读**：不接受管理操作，
//! `agent.request` 只答 `host.info` 与 `caps.probe`。

mod client;
mod config;

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use strixmaid_core::metrics::MetricsEngine;
use strixmaid_core::store::Store;

use crate::config::AgentConfig;

#[derive(Debug, Parser)]
#[command(name = "strixmaid-agent", version, about = "StrixMaid 远程采集 Agent")]
struct Cli {
    /// 配置文件路径；缺省 /etc/strixmaid/agent.toml（不存在则全用默认值 + 环境变量）。
    #[arg(long, env = "STRIXMAID_AGENT_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = AgentConfig::load(cli.config.as_deref())?;
    let node_id = cfg.resolve_node_id()?;
    let token = cfg.resolve_token()?;

    tokio::fs::create_dir_all(&cfg.data_dir)
        .await
        .with_context(|| format!("创建数据目录失败: {}", cfg.data_dir.display()))?;
    let store = Store::open_with(&cfg.db_path(), cfg.metrics.retention)
        .await
        .context("打开本地数据库失败")?;

    // 与 Server 同一套链路：采集 → 环 → 每分钟落盘 → 本地五层聚合。
    let engine = MetricsEngine::start(&cfg.metrics, Some(store.clone()));

    // system 层能力如实探测：Agent 上通常没有 helper，那就是 false。
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
        "strixmaid-agent 启动"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

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
        shutdown_rx,
    )
    .await;

    // 收尾：把未满分钟落盘、关库。
    engine.stop().await;
    store.close().await;
    tracing::info!("已优雅退出");
    Ok(())
}

/// 主机名，取不到时退回 node_id 不如退回固定串直白。
async fn host_name() -> String {
    match strixmaid_core::providers::system::HostProvider::new()
        .system_info()
        .await
    {
        Ok(info) => info.hostname,
        Err(_) => "unknown".to_string(),
    }
}

/// SIGINT / SIGTERM 任一到达即返回。
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("注册 SIGTERM 失败");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
