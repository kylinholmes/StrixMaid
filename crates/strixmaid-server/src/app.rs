//! 组装 axum 应用：
//! 1. `/api/v1` REST（自动收集 OpenAPI）
//! 2. `/ws` 控制面 WebSocket 与 `/ws/terminal/{id}` 终端流（均受鉴权保护，token 走子协议）
//! 3. debug 构建：`/api/docs`、`/api/v1/openapi.json`、`/debug`，且 `/` 302 到 `/debug`
//! 4. fallback：静态资源与 SPA 回退

use std::sync::Arc;

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::AuthState;
use crate::routes::{self, ApiStates};
use crate::ws::Hub;

pub fn build(
    states: ApiStates,
    hub: Arc<Hub>,
    auth: Arc<AuthState>,
    agent_ws: crate::ws::agent::AgentSocketState,
) -> Router {
    // 终端 WS 要在 `states` 被 `api_v1` 消费掉之前把注册表取出来。
    let terminals = states.terminals.registry.clone();

    let (api_router, openapi) = OpenApiRouter::with_openapi(routes::ApiDoc::openapi())
        .nest("/api/v1", routes::api_v1(states))
        .split_for_parts();

    // 两个会话 WS 端点共用同一套鉴权：token 走子协议，在升级之前完成。
    let ws = crate::ws::router(hub).merge(crate::ws::terminal::router(terminals));
    let ws = crate::auth::middleware::protect(ws, auth);
    // `/ws/agent` 自带 token 鉴权（对 nodes.token_hash，不是 PAM 会话），
    // **不套** require_auth——见 `ws::agent` 模块文档。
    let ws = ws.merge(crate::ws::agent::router(agent_ws));

    let router = crate::apidoc::attach(api_router, openapi).merge(ws);

    #[cfg(any(debug_assertions, feature = "apidoc"))]
    let router = crate::debug::attach(router)
        .route("/", axum::routing::get(crate::debug::index_redirect));

    let router = router.fallback(crate::embed::fallback);

    with_dev_cors(router)
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

#[cfg(debug_assertions)]
fn with_dev_cors(router: Router) -> Router {
    router.layer(tower_http::cors::CorsLayer::very_permissive())
}

#[cfg(not(debug_assertions))]
fn with_dev_cors(router: Router) -> Router {
    router
}
