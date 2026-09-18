//! 认证层：共享状态、Bearer 提取、鉴权中间件（design.md §5 / §9.1）。
//!
//! # 接线方式（由 `app.rs` / `routes/mod.rs` 的维护者统一处理）
//!
//! ```ignore
//! // 1. 建状态（serve() 里，Store 打开之后）
//! let sessions = strixmaid_core::session::SessionManager::with_process_helper(store.clone(), &config).await?;
//! let _sweeper = sessions.spawn_sweeper(std::time::Duration::from_secs(5));
//! let auth_state = crate::auth::AuthState::new(sessions, config.trusted_proxies.clone());
//!
//! // 2. auth 路由自带状态，直接 merge 进 /api/v1
//! OpenApiRouter::new().merge(crate::auth::routes::router(auth_state.clone()))
//!
//! // 3. 需要登录的路由树套上中间件（只对已匹配的路由生效，未匹配仍是 404）
//! crate::auth::protect(protected_router, auth_state.clone())
//!
//! // 4. OpenAPI：声明 bearer 安全方案 + auth 标签
//! #[openapi(modifiers(&crate::auth::SecurityAddon), tags((name = "auth", description = "认证与提权")))]
//! ```
//!
//! `routes/auth.rs` 目前通过 `#[path]` 挂在本模块下（`crate::auth::routes`），
//! 接线时把它改成 `routes/mod.rs` 里的 `pub mod auth;` 即可，文件不用动。

// 接线之前本模块（含 routes/auth.rs）没有被 app.rs 引用，会触发 dead_code；
// app.rs 接上 `routes::router` / `protect` / `SecurityAddon` 之后删掉下面这行。

pub mod audit;
pub mod exec;
pub mod extract;
pub mod middleware;

use std::sync::Arc;

use strixmaid_core::session::SessionManager;
use utoipa::Modify;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};

pub use extract::CurrentSession;

/// OpenAPI 里 Bearer 安全方案的名字，`#[utoipa::path(security(("bearer" = [])))]` 引用它。
pub const SECURITY_SCHEME: &str = "bearer";

/// 认证相关路由与中间件共享的状态。
///
/// 独立于 `AppState`：auth 路由自带它（`router()` 返回无状态 router），
/// 中间件通过 `from_fn_with_state` 拿它，两者都不要求 `AppState` 实现 `FromRef`。
#[derive(Clone)]
pub struct AuthState {
    /// 会话管理器（core）。
    pub sessions: SessionManager,
    /// 可信反向代理的直连地址（`config.trusted_proxies`，默认空）。
    ///
    /// 只有直连地址在这个列表里时才采信 `X-Forwarded-For`（见 [`audit::remote_addr`]）。
    /// 放在 `AuthState` 而不是每次从 `Config` 里现取：审计写入点在
    /// [`exec`] 与认证路由里，它们手上只有 `AuthState`，为了一个字符串列表
    /// 再把 `Config` 传一遍会让每个处理器都多背一个参数。
    pub trusted_proxies: Vec<String>,
}

impl std::fmt::Debug for AuthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthState")
            .field("sessions", &self.sessions)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish()
    }
}

impl AuthState {
    /// 包成 `Arc`，路由与中间件共用同一份。
    ///
    /// `trusted_proxies` 取自 `Config::trusted_proxies`。
    pub fn new(sessions: SessionManager, trusted_proxies: Vec<String>) -> Arc<Self> {
        Arc::new(AuthState {
            sessions,
            trusted_proxies,
        })
    }
}

/// 往 OpenAPI 文档里加 `bearer` 安全方案（`Authorization: Bearer <token>`）。
///
/// 用法：`#[openapi(modifiers(&SecurityAddon))]`。
#[derive(Debug, Clone, Copy)]
pub struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            SECURITY_SCHEME,
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("opaque")
                    .description(Some(
                        "POST /api/v1/auth/respond 返回的 token；服务端只存它的 hash。",
                    ))
                    .build(),
            ),
        );
    }
}
