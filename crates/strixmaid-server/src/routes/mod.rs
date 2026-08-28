//! REST 路由树与 OpenAPI 元信息。
//!
//! 路径由 `utoipa-axum` 的 [`OpenApiRouter`] 从各处理器的 `#[utoipa::path]` 自动收集，
//! **不手写 `paths(...)` 清单**（§12.1）——加端点只需在对应模块的 `router()` 里加一行。
//!
//! 鉴权分三档（§5 / §6）：
//! - **公开**：`/health`、`/auth/start`、`/auth/respond`；
//! - **软鉴权**：`/capabilities` —— 带有效 token 就填 `user` 层，没带也返回 `system` 层；
//! - **受保护**：其余全部，缺少或无效 token 一律 401。
//!   auth 模块内的 `elevate/*`、`logout`、`session` 通过 `CurrentSession` 提取器自行强制。

pub mod audit;
pub mod auth;
pub mod capabilities;
pub mod files;
pub mod health;
pub mod logs;
pub mod metrics;
pub mod nodes;
pub mod processes;
pub mod services;
pub mod system;
pub mod terminals;

use std::sync::Arc;

use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{AuthState, SecurityAddon};
use crate::auth::middleware::{protect_openapi, protect_optional};
use crate::state::AppState;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "StrixMaid API",
        description = "轻量、通用、现代化的服务器观测与管理平台。\n\n\
                       所有路径以 /api/v1 为前缀；写操作一律走 REST，实时流走 WebSocket `/ws`。\n\n\
                       除 /health、/auth/start、/auth/respond、/capabilities 外，其余端点需要 \
                       `Authorization: Bearer <token>`；WebSocket 握手用子协议携带：\
                       `Sec-WebSocket-Protocol: bearer, <token>`。",
    ),
    modifiers(&SecurityAddon),
    tags(
        (name = "auth", description = "认证与提权（PAM challenge-response）"),
        (name = "capabilities", description = "两层能力探测：system / user"),
        (name = "audit", description = "审计日志查询（需管理访问）"),
        (name = "system", description = "主机信息、健康状态与时间"),
        (name = "services", description = "systemd unit 列表、详情与操作"),
        (name = "logs", description = "journald 日志查询与 boot 列表"),
        (name = "processes", description = "进程列表、详情、信号与 renice"),
        (name = "metrics", description = "指标：可用序列、自动选层查询与实时快照"),
        (name = "files", description = "只读文件浏览（在登录用户的 worker 内执行）"),
        (name = "nodes", description = "多节点：登记、列表与在线状态（写操作需管理访问）"),
    ),
)]
pub struct ApiDoc;

/// 各路由模块自带的状态。全部在 `main::serve` 里构造一次，这里只做拼装。
///
/// `system` / `processes` / `services` / `logs` 四个模块**不再有自己的状态**：
/// 它们原先的状态唯一的作用是持有 provider，而请求现在一律经 worker 执行
/// （`roadmap/01-worker-execution.md` §4.3），需要的只是 [`AuthState`]
/// ——从中按会话取 worker。
pub struct ApiStates {
    pub app: AppState,
    pub auth: Arc<AuthState>,
    pub capabilities: Arc<capabilities::CapabilityState>,
    pub audit: Arc<audit::AuditState>,
    pub metrics: Arc<metrics::MetricsState>,
    pub terminals: terminals::TerminalState,
    pub files: files::FilesState,
    pub nodes: nodes::NodesState,
}

/// `/api/v1` 下的全部路由。各子 router 自带状态，因此返回无状态的 `OpenApiRouter<()>`。
pub fn api_v1(s: ApiStates) -> OpenApiRouter<()> {
    let public = OpenApiRouter::new()
        .routes(routes!(health::health))
        .with_state(s.app)
        .merge(auth::router(s.auth.clone()));

    let soft = protect_optional(capabilities::router(s.capabilities), s.auth.clone());

    let protected = OpenApiRouter::new()
        .merge(system::router(s.auth.clone()))
        .merge(processes::router(s.auth.clone()))
        .merge(services::router(s.auth.clone()))
        .merge(logs::router(s.auth.clone()))
        .merge(metrics::router(s.metrics))
        .merge(audit::router(s.audit))
        .merge(terminals::router(s.terminals))
        .merge(files::router(s.files))
        .merge(nodes::router(s.nodes));
    let protected = protect_openapi(protected, s.auth);

    public.merge(soft).merge(protected)
}
