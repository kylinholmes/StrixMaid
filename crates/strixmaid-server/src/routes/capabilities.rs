//! `GET /api/v1/capabilities` —— 两层能力（`docs/design.md` §6）。
//!
//! system 层在启动时由 `strixmaid_core::capability::CapabilityRegistry::probe_all` 探测一次，
//! 结果放进 [`CapabilityState`]；user 层来自请求的 `Extension<UserIdentity>`——
//! 认证中间件接线后会把它塞进来，**没有时 `user` 为 `null`，接口仍返回 200**
//! （登录页要靠 system 层判断 helper 是否可用）。
//!
//! # 接线
//!
//! ```ignore
//! let report = registry.probe_all().await;
//! let caps = Arc::new(CapabilityState::new(report.system));
//! OpenApiRouter::new().nest("/api/v1", routes::capabilities::router(caps))
//! ```

// 接线前 `routes/mod.rs` 尚未引用本模块的 `router()`，此处暂时期待 dead_code；
// 接线后这条 expect 会因「未被满足」而告警——届时把它删掉即可。

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::Json;
use strixmaid_core::capability::UserIdentity;
use strixmaid_types::capability::{Capabilities, SystemCapabilities};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

/// 能力路由的共享状态：启动期探测好的 system 层。
#[derive(Debug, Clone, Copy)]
pub struct CapabilityState {
    pub system: SystemCapabilities,
}

impl CapabilityState {
    pub fn new(system: SystemCapabilities) -> Self {
        Self { system }
    }
}

/// 构建 `/capabilities` 路由（相对 `/api/v1`）。
pub fn router(state: Arc<CapabilityState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(capabilities))
        .with_state(state)
}

/// 能力探测
///
/// `system`：这台机器有没有（启动时探测一次）；`user`：当前登录用户能不能用。
/// **未认证时 `user` 为 `null`，仍返回 200。**
#[utoipa::path(
    get,
    path = "/capabilities",
    tag = "capabilities",
    responses(
        (status = 200, description = "两层能力", body = Capabilities),
    ),
)]
pub async fn capabilities(
    State(state): State<Arc<CapabilityState>>,
    user: Option<Extension<UserIdentity>>,
) -> Json<Capabilities> {
    Json(Capabilities {
        system: state.system,
        user: user.map(|Extension(identity)| identity.capabilities()),
    })
}
