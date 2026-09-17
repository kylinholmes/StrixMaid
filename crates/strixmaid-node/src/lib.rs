//! `strixmaid-node`：一台主机的完整 API。
//!
//! # 这一层是什么
//!
//! `design.md` 开篇第 5 条：「**AgentCore 是唯一的业务逻辑所在地。Server 与 Agent
//! 都只是它的宿主**」。本 crate 就是那个 AgentCore 的**对外面**——把 `strixmaid-core`
//! 的能力做成一个 `axum::Router`。
//!
//! | crate | 角色 | 知不知道 HTTP |
//! |---|---|---|
//! | `strixmaid-core` | 能力库：providers、worker、session、metrics、store、platform | 不知道 |
//! | `strixmaid-node` | 把能力做成 API：Router、认证与会话、审计、WS 频道、服务托管 | 知道 |
//! | `strixmaid`（server） | node + 前端 + 节点目录 + 转发 | —— |
//! | `strixmaid-agent` | node + 向 Server 拨号 | —— |
//!
//! 两个宿主装载**同一份** node，因此它们提供逐字节相同的 API。这条边界由依赖表
//! 守着：core 的 `Cargo.toml` 里没有 axum，想 `use` 也 `use` 不到。
//!
//! # API 的单位是 `Router`，不是「一堆 handler」
//!
//! [`router`] 返回完整的 `/api/v1` 与 `/ws`，不带节点前缀、不含前端资源。
//! 于是同一份 API 可以被摆在三种地方：
//!
//! | 摆在哪 | 谁这么用 |
//! |---|---|
//! | `TcpListener` 上 | server 的本机 API（今天的行为） |
//! | 一条多路复用流上 | agent——`hyper` 能在任意 `AsyncRead + AsyncWrite` 上 `serve_connection` |
//! | 进程内直调 | server 的 `/nodes/local` |
//!
//! 详见 `docs/roadmap/10-node-layer.md`。
//!
//! # 宿主要提供的两处东西
//!
//! node 只认识**本机这一个节点**。凡是「别的节点」的概念都由宿主注入：
//!
//! - [`RemoteSnapshots`]：别的节点的实时快照来源。server 用它的 agent 注册表实现，
//!   agent 传 `None`。
//! - [`ApiStates::extra_protected`]：宿主追加的受保护路由。server 用它挂 `/nodes`。
//!
//! 这是依赖倒置：node 定义缝，宿主填。node 的依赖表里因此没有任何「多节点」的东西。

pub mod apidoc;
pub mod cli;
pub mod assets;
pub mod auth;
pub mod error;
pub mod routes;
pub mod state;
pub mod ws;

#[cfg(any(debug_assertions, feature = "apidoc"))]
pub mod debug;

/// Windows 服务（SCM）托管。两个宿主共用，各自给一份 [`winsvc::ServiceIdentity`]。
#[cfg(windows)]
pub mod winsvc;

use std::sync::Arc;

use anyhow::Context as _;
use strixmaid_core::config::Config;
use tracing_subscriber::EnvFilter;

pub use lifecycle::{NoReporter, ShutdownKind, StartupReporter, URGENT_CLEANUP};
pub use remote::RemoteSnapshots;

mod lifecycle;
mod remote;

/// `/api/v1` 与 `/ws` 的全部路由，不含前端资源与节点前缀。
///
/// 层（压缩、trace、CORS）与 fallback 由宿主加——它们是「这个进程怎么对外服务」
/// 的事，不是「这台主机提供什么 API」的事。
pub fn router(states: routes::ApiStates, hub: Arc<ws::Hub>, auth: Arc<auth::AuthState>) -> axum::Router {
    app::build(states, hub, auth)
}

mod app;

/// 日志过滤器。前台与服务模式共用同一套口径，只换输出端。
///
/// 命令行显式给了 `--log-level` 时以它为准；否则 `RUST_LOG` 优先，都没有才用配置里的值。
pub fn log_filter(config: &Config, log_level_from_cli: bool) -> anyhow::Result<EnvFilter> {
    let level = config.log.level.as_str();
    if log_level_from_cli {
        EnvFilter::try_new(level).with_context(|| format!("非法的日志级别: {level}"))
    } else {
        Ok(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level)))
    }
}
