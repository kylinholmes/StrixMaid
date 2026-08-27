//! 鉴权中间件：`Authorization: Bearer <token>` → `SessionManager::resolve` → `Extension<Session>`。
//!
//! 失败返回 401 + [`ApiError`]（`code = unauthenticated`）。成功时把
//! [`Session`] 放进 request extensions，处理器用 `Extension<Session>`（或
//! [`super::CurrentSession`]）取。`resolve` 顺带刷新了会话活跃时间。

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use strixmaid_core::capability::UserIdentity;
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use utoipa_axum::router::OpenApiRouter;

use super::AuthState;
use super::extract::bearer_from_headers;
use crate::error::ApiErr;

/// 中间件本体。配合 `axum::middleware::from_fn_with_state(auth_state, require_auth)` 使用，
/// 或直接用 [`protect`] / [`protect_openapi`]。
pub async fn require_auth(
    State(auth): State<Arc<AuthState>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer_from_headers(req.headers()) else {
        return ApiErr(ApiError::unauthenticated(
            "缺少 Authorization: Bearer token",
        ))
        .into_response();
    };
    match auth.sessions.resolve(&token).await {
        Some(session) => {
            attach_session(&mut req, session);
            next.run(req).await
        }
        None => ApiErr(ApiError::unauthenticated("会话不存在或已过期")).into_response(),
    }
}

/// 软鉴权：带了有效 token 就注入身份，没带或无效也放行。
/// 只给 `GET /capabilities` 这类「未登录也要能看 system 层」的端点用（§6）。
pub async fn optional_auth(
    State(auth): State<Arc<AuthState>>,
    mut req: Request,
    next: Next,
) -> Response {
    if let Some(token) = bearer_from_headers(req.headers())
        && let Some(session) = auth.sessions.resolve(&token).await
    {
        attach_session(&mut req, session);
    }
    next.run(req).await
}

/// 把会话同时以两种形态放进 extensions：
/// - `Session`：给需要 worker / token_hash 的处理器；
/// - `UserIdentity`：给 capability 层推导 user 能力（capability 模块不依赖 session）。
fn attach_session(req: &mut Request, session: Session) {
    req.extensions_mut().insert(UserIdentity {
        uid: session.user.uid,
        username: session.user.username.clone(),
        groups: session.user.groups.clone(),
        elevated: session.elevated,
    });
    req.extensions_mut().insert(session);
}

pub fn protect_optional<S>(router: OpenApiRouter<S>, auth: Arc<AuthState>) -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.route_layer(from_fn_with_state(auth, optional_auth))
}

/// 给一棵 `Router` 套上鉴权。用 `route_layer`：只对已匹配的路由生效，
/// 未匹配的路径仍返回 404 而不是 401。
pub fn protect<S>(router: Router<S>, auth: Arc<AuthState>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.route_layer(from_fn_with_state(auth, require_auth))
}

/// 同 [`protect`]，作用于 `OpenApiRouter`（OpenAPI 收集不受影响）。
pub fn protect_openapi<S>(router: OpenApiRouter<S>, auth: Arc<AuthState>) -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.route_layer(from_fn_with_state(auth, require_auth))
}
