//! `GET /api/v1/health` —— 探活端点。

use axum::Json;
use axum::extract::State;
use serde::Serialize;
use utoipa::ToSchema;

use crate::state::AppState;

/// 健康检查响应。
#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    /// 固定为 `ok`；服务起不来时根本不会有响应，所以这里没有别的取值。
    #[schema(example = "ok")]
    pub status: &'static str,
    /// 二进制版本（`CARGO_PKG_VERSION`）。
    #[schema(example = "0.1.0")]
    pub version: &'static str,
    /// 进程已运行秒数。
    #[schema(example = 42)]
    pub uptime_secs: u64,
}

/// 健康检查
///
/// 无需认证。用于反向代理探活、部署校验，以及前端确认后端版本。
#[utoipa::path(
    get,
    path = "/health",
    tag = "system",
    responses(
        (status = 200, description = "服务存活", body = HealthResponse),
    ),
)]
pub async fn health(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: state.uptime_secs(),
    })
}
