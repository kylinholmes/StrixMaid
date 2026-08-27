//! 组装 axum 应用：
//! 1. `/api/v1` REST（自动收集 OpenAPI）
//! 2. `/ws` 控制面 WebSocket（受鉴权保护，token 走子协议）
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

pub fn build(states: ApiStates, hub: Arc<Hub>, auth: Arc<AuthState>) -> Router {
    let (api_router, openapi) = OpenApiRouter::with_openapi(routes::ApiDoc::openapi())
        .nest("/api/v1", routes::api_v1(states))
        .split_for_parts();

    let ws = crate::auth::middleware::protect(crate::ws::router(hub), auth);

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
