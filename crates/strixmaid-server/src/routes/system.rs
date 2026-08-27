//! `/api/v1/system/*` —— 主机信息 / 健康 / 时间 / 主机名 / 时区 / 电源。
//!
//! 本文件只是 `strixmaid_core::providers::system::HostProvider` 的薄壳：
//! 数据采集、写操作与错误映射全部在 core（§1 原则 5「AgentCore 是唯一的业务逻辑所在地」）。
//! 返回的就是 `strixmaid-types` 里的完整 [`SystemInfo`] 等 DTO，不再有 Phase 0 的
//! `PartialSystemInfo` 子集。
//!
//! # 接线
//!
//! ```ignore
//! let system = Arc::new(SystemState::new());
//! OpenApiRouter::new().nest("/api/v1", routes::system::router(system))
//! ```
//!
//! 三个写端点现在**没有权限体系**：认证中间件接线前，任何请求都会以服务进程自身的身份执行，
//! 非 root 时由内核 / 文件权限拒绝并返回 `403 permission_denied`。

// 接线前 `routes/mod.rs` 尚未引用本模块的 `router()`，此处暂时期待 dead_code；
// 接线后这条 expect 会因「未被满足」而告警——届时把它删掉即可。

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use strixmaid_core::providers::system::HostProvider;
use strixmaid_types::ApiError;
use strixmaid_types::system::{
    HealthReport, PowerReq, SetHostnameReq, SetTimezoneReq, SystemInfo, TimeInfo,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiResult;

/// 系统路由的共享状态。
#[derive(Debug, Clone, Default)]
pub struct SystemState {
    pub host: HostProvider,
}

impl SystemState {
    pub fn new() -> Self {
        Self {
            host: HostProvider::new(),
        }
    }
}

/// 构建 `/system/*` 路由（相对 `/api/v1`）。
pub fn router(state: Arc<SystemState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(info))
        .routes(routes!(health))
        .routes(routes!(time))
        .routes(routes!(set_hostname))
        .routes(routes!(set_timezone))
        .routes(routes!(power))
        .with_state(state)
}

/// 主机信息
///
/// 主机名 / 发行版 / 内核 / 架构 / 虚拟化 / DMI / CPU / 内存 / 块设备 / 文件系统 / 开机时长。
/// 全部直读 `/proc`、`/sys`、`/etc`，读不到的字段为 `null`。
#[utoipa::path(
    get,
    path = "/system/info",
    tag = "system",
    responses(
        (status = 200, description = "完整主机信息", body = SystemInfo),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn info(State(state): State<Arc<SystemState>>) -> ApiResult<Json<SystemInfo>> {
    Ok(Json(state.host.system_info().await?))
}

/// 健康聚合
///
/// 需重启 / 文件系统容量与 inode 超阈值 / 负载过高 / 根文件系统只读。
/// failed units 与 SMART 由其它模块补充，`skipped` 里如实标出「未检查」。
#[utoipa::path(
    get,
    path = "/system/health",
    tag = "system",
    responses(
        (status = 200, description = "健康报告；`items` 为空表示一切正常", body = HealthReport),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn health(State(state): State<Arc<SystemState>>) -> ApiResult<Json<HealthReport>> {
    Ok(Json(state.host.health().await?))
}

/// 时间与时区
///
/// 服务端时刻、IANA 时区、UTC 偏移、NTP 服务与同步状态（`adjtimex`）、RTC 模式。不调 `timedatectl`。
#[utoipa::path(
    get,
    path = "/system/time",
    tag = "system",
    responses(
        (status = 200, description = "时间信息", body = TimeInfo),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn time(State(state): State<Arc<SystemState>>) -> ApiResult<Json<TimeInfo>> {
    Ok(Json(state.host.time().await?))
}

/// 修改主机名
///
/// `sethostname(2)` 立即生效并写入 `/etc/hostname`；`pretty_hostname` 写入 `/etc/machine-info`。需要 root。
#[utoipa::path(
    put,
    path = "/system/hostname",
    tag = "system",
    request_body = SetHostnameReq,
    responses(
        (status = 204, description = "已修改"),
        (status = 400, description = "主机名不合法", body = ApiError),
        (status = 403, description = "权限不足（需要 root）", body = ApiError),
        (status = 500, description = "写入失败", body = ApiError),
    ),
)]
pub async fn set_hostname(
    State(state): State<Arc<SystemState>>,
    Json(req): Json<SetHostnameReq>,
) -> ApiResult<StatusCode> {
    state.host.set_hostname(req).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 修改时区
///
/// 原子替换 `/etc/localtime` 软链（并更新 `/etc/timezone`，如存在）。时区名必须存在于 `/usr/share/zoneinfo`。需要 root。
#[utoipa::path(
    put,
    path = "/system/timezone",
    tag = "system",
    request_body = SetTimezoneReq,
    responses(
        (status = 204, description = "已修改"),
        (status = 400, description = "时区名不合法或不存在", body = ApiError),
        (status = 403, description = "权限不足（需要 root）", body = ApiError),
        (status = 500, description = "写入失败", body = ApiError),
    ),
)]
pub async fn set_timezone(
    State(state): State<Arc<SystemState>>,
    Json(req): Json<SetTimezoneReq>,
) -> ApiResult<StatusCode> {
    state.host.set_timezone(req.timezone).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 重启 / 关机
///
/// 调 `systemctl reboot|poweroff`。命令被受理即返回 202，真正的关机是异步的；关机等于远程失联，前端必须二次确认。
#[utoipa::path(
    post,
    path = "/system/power",
    tag = "system",
    request_body = PowerReq,
    responses(
        (status = 202, description = "已受理"),
        (status = 403, description = "权限不足（需要 root）", body = ApiError),
        (status = 501, description = "没有 systemctl", body = ApiError),
        (status = 500, description = "执行失败", body = ApiError),
    ),
)]
pub async fn power(
    State(state): State<Arc<SystemState>>,
    Json(req): Json<PowerReq>,
) -> ApiResult<StatusCode> {
    state.host.power(req.action).await?;
    Ok(StatusCode::ACCEPTED)
}
