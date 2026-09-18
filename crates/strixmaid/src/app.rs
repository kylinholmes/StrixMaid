//! 把 node 的 API 装进这个进程：
//! 1. node 的 `/api/v1` 与 `/ws`（`strixmaid_node::Node::router`，由调用方传进来）
//! 2. `/ws/agent`：Agent 拨进来的那条连接。**自带 token 鉴权**（对 `nodes.token_hash`，
//!    不是 PAM 会话），因此不套 `require_auth`——见 `ws_agent` 模块文档
//! 3. debug 构建：`/` 302 到 `/debug`
//! 4. fallback：静态资源与 SPA 回退
//! 5. 压缩与 trace 层
//!
//! 2–5 是 Server 独有的：一个 Agent 没有前端、没有下级节点，它把同一份 node router
//! serve 在别的传输上。这条分界见 `docs/roadmap/10-node-layer.md` §3.5。

use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

pub fn build(node: Router, agent_ws: crate::ws_agent::AgentSocketState) -> Router {
    let router = node.merge(crate::ws_agent::router(agent_ws));

    #[cfg(any(debug_assertions, feature = "apidoc"))]
    let router = router.route(
        "/",
        axum::routing::get(strixmaid_node::debug::index_redirect),
    );

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
