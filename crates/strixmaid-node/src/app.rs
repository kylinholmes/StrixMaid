//! 组装这台主机的 API：
//! 1. `/api/v1` REST（自动收集 OpenAPI）
//! 2. `/ws` 控制面 WebSocket 与 `/ws/terminal/{id}` 终端流（均受鉴权保护，token 走子协议）
//! 3. debug 构建：`/api/docs`、`/api/v1/openapi.json`、`/debug`
//!
//! **不含**前端资源、fallback、压缩与 trace 层、`/` 重定向——那些是「这个进程怎么
//! 对外服务」，不是「这台主机提供什么 API」。宿主自己加，见 `strixmaid-server`
//! 的 `app.rs`。`/ws/agent`（Agent 拨进来的那条连接）同理，它属于 Server。

use std::sync::Arc;

use axum::Router;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::AuthState;
use crate::routes::{self, ApiStates};
use crate::ws::Hub;

pub fn build(states: ApiStates, hub: Arc<Hub>, auth: Arc<AuthState>) -> Router {
    // 终端 WS 要在 `states` 被 `api_v1` 消费掉之前把注册表取出来。
    let terminals = states.terminals.registry.clone();

    let (api_router, openapi) = OpenApiRouter::with_openapi(routes::ApiDoc::openapi())
        .nest("/api/v1", routes::api_v1(states))
        .split_for_parts();

    // 两个会话 WS 端点共用同一套鉴权：token 走子协议，在升级之前完成。
    let ws = crate::ws::router(hub).merge(crate::ws::terminal::router(terminals));
    let ws = crate::auth::middleware::protect(ws, auth);

    let router = crate::apidoc::attach(api_router, openapi).merge(ws);

    #[cfg(any(debug_assertions, feature = "apidoc"))]
    let router = crate::debug::attach(router);

    router
}
