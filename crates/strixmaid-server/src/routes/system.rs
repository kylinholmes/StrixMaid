//! `/api/v1/system/*` —— 主机信息 / 健康 / 时间 / 主机名 / 时区 / 电源。
//!
//! # 执行路径
//!
//! 本模块不再持有任何 provider。每个处理器把请求转成一次 worker RPC，经
//! [`crate::auth::exec`] 派给该会话的 worker 执行（`roadmap/01-worker-execution.md` §4.3）：
//!
//! ```text
//! HTTP 请求 → exec::call(方法名, 参数) → WorkerHandle → worker 进程内的 HostProvider
//! ```
//!
//! 这样做的意义在 `design.md` §2.2 与 §5.1：worker 的 uid 就是登录用户，
//! 因此 polkit、文件权限、内核裁决的对象是**真实的人**，而不是服务进程。
//! 服务端自身不含任何授权判断。
//!
//! # 读与写分派到不同的 worker
//!
//! - `info` / `health` / `time` 是读，走 user worker（[`Privilege::User`]）；
//! - `set_hostname` / `set_timezone` / `power` 改的是整机状态，任何单个登录用户都无权，
//!   走 admin worker（[`Privilege::Admin`]）。会话未提权时没有这个 worker，
//!   [`exec::call`] 直接返回 403 `elevation_required`，前端据此弹提权对话框。
//!
//! # 接线
//!
//! ```ignore
//! OpenApiRouter::new().nest("/api/v1", routes::system::router(auth_state.clone()))
//! ```

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::{Extension, Json};
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::rpc;
use strixmaid_types::system::{
    HealthReport, PowerReq, SetHostnameReq, SetTimezoneReq, SystemInfo, TimeInfo,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege, RequestOrigin};
use crate::error::ApiResult;

/// 构建 `/system/*` 路由（相对 `/api/v1`）。
///
/// 状态只有 [`AuthState`]：处理器需要它来找到本会话的 worker，除此之外不需要任何东西。
pub fn router(auth: Arc<AuthState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(info))
        .routes(routes!(health))
        .routes(routes!(time))
        .routes(routes!(set_hostname))
        .routes(routes!(set_timezone))
        .routes(routes!(power))
        .with_state(auth)
}

/// 主机信息
///
/// 主机名 / 发行版 / 内核 / 架构 / 虚拟化 / DMI / CPU / 内存 / 块设备 / 文件系统 / 开机时长。
/// 全部直读 `/proc`、`/sys`、`/etc`，读不到的字段为 `null`。
#[utoipa::path(
    get,
    path = "/system/info",
    tag = "system",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "完整主机信息", body = SystemInfo),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn info(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
) -> ApiResult<Json<SystemInfo>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::HOST_INFO, ()).await?,
    ))
}

/// 健康聚合
///
/// 需重启 / 文件系统容量与 inode 超阈值 / 负载过高 / 根文件系统只读。
/// failed units 与 SMART 由其它模块补充，`skipped` 里如实标出「未检查」。
///
/// 在 user worker 内执行，因此「读不到」与「无权读」的结果对登录用户是真实的。
#[utoipa::path(
    get,
    path = "/system/health",
    tag = "system",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "健康报告；`items` 为空表示一切正常", body = HealthReport),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn health(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
) -> ApiResult<Json<HealthReport>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::HOST_HEALTH, ()).await?,
    ))
}

/// 时间与时区
///
/// 服务端时刻、IANA 时区、UTC 偏移、NTP 服务与同步状态（`adjtimex`）、RTC 模式。不调 `timedatectl`。
#[utoipa::path(
    get,
    path = "/system/time",
    tag = "system",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "时间信息", body = TimeInfo),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn time(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
) -> ApiResult<Json<TimeInfo>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::HOST_TIME, ()).await?,
    ))
}

/// 修改主机名
///
/// `sethostname(2)` 立即生效并写入 `/etc/hostname`；`pretty_hostname` 写入 `/etc/machine-info`。
///
/// **需要管理访问**：改的是整机标识，在 admin worker（uid = 0）内执行。
/// 会话未提权时不做任何事，直接 403 `elevation_required`。
#[utoipa::path(
    put,
    path = "/system/hostname",
    tag = "system",
    security(("bearer" = [])),
    request_body = SetHostnameReq,
    responses(
        (status = 204, description = "已修改"),
        (status = 400, description = "主机名不合法", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "需要管理访问：会话尚未提权，`code = elevation_required`、\
                                      `can_retry_elevated = true`，提权后重试即可", body = ApiError),
        (status = 500, description = "写入失败", body = ApiError),
    ),
)]
pub async fn set_hostname(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Json(req): Json<SetHostnameReq>,
) -> ApiResult<StatusCode> {
    exec::call_from::<_, ()>(
        &auth,
        &session,
        &origin,
        Privilege::Admin,
        rpc::HOST_SET_HOSTNAME,
        req,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 修改时区
///
/// 原子替换 `/etc/localtime` 软链（并更新 `/etc/timezone`，如存在）。时区名必须存在于 `/usr/share/zoneinfo`。
///
/// **需要管理访问**：在 admin worker（uid = 0）内执行，未提权返回 403 `elevation_required`。
#[utoipa::path(
    put,
    path = "/system/timezone",
    tag = "system",
    security(("bearer" = [])),
    request_body = SetTimezoneReq,
    responses(
        (status = 204, description = "已修改"),
        (status = 400, description = "时区名不合法或不存在", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "需要管理访问：会话尚未提权，`code = elevation_required`、\
                                      `can_retry_elevated = true`，提权后重试即可", body = ApiError),
        (status = 500, description = "写入失败", body = ApiError),
    ),
)]
pub async fn set_timezone(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Json(req): Json<SetTimezoneReq>,
) -> ApiResult<StatusCode> {
    exec::call_from::<_, ()>(
        &auth,
        &session,
        &origin,
        Privilege::Admin,
        rpc::HOST_SET_TIMEZONE,
        req,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 重启 / 关机
///
/// 调 `systemctl reboot|poweroff`。命令被受理即返回 202，真正的关机是异步的；关机等于远程失联，前端必须二次确认。
///
/// **需要管理访问**：在 admin worker（uid = 0）内执行，未提权返回 403 `elevation_required`。
#[utoipa::path(
    post,
    path = "/system/power",
    tag = "system",
    security(("bearer" = [])),
    request_body = PowerReq,
    responses(
        (status = 202, description = "已受理"),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "需要管理访问：会话尚未提权，`code = elevation_required`、\
                                      `can_retry_elevated = true`，提权后重试即可", body = ApiError),
        (status = 501, description = "没有 systemctl", body = ApiError),
        (status = 500, description = "执行失败", body = ApiError),
    ),
)]
pub async fn power(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Json(req): Json<PowerReq>,
) -> ApiResult<StatusCode> {
    exec::call_from::<_, ()>(&auth, &session, &origin, Privilege::Admin, rpc::HOST_POWER, req).await?;
    Ok(StatusCode::ACCEPTED)
}
