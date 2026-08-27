//! `/api/v1/processes/*` —— 进程列表 / 详情 / 信号 / renice。
//!
//! 薄壳：全部逻辑在 `strixmaid_core::providers::process::ProcProvider`。
//! provider 内部保存 CPU 差分快照，因此**必须整个进程共用一个实例**（放在 [`ProcessState`] 里），
//! 否则每次请求都是「首次调用」、CPU% 永远为 0。
//!
//! # 接线
//!
//! ```ignore
//! let processes = Arc::new(ProcessState::new());
//! OpenApiRouter::new().nest("/api/v1", routes::processes::router(processes))
//! ```
//!
//! 信号与 renice 现在没有权限体系：以服务进程自身身份执行，由内核裁决（`EPERM` → 403）。

// 接线前 `routes/mod.rs` 尚未引用本模块的 `router()`，此处暂时期待 dead_code；
// 接线后这条 expect 会因「未被满足」而告警——届时把它删掉即可。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use strixmaid_core::providers::process::ProcProvider;
use strixmaid_types::ApiError;
use strixmaid_types::process::{
    ProcessDetail, ProcessListQuery, ProcessSummary, ReniceReq, SignalReq,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiResult;

/// 进程路由的共享状态。
#[derive(Clone, Default)]
pub struct ProcessState {
    pub proc: ProcProvider,
}

impl ProcessState {
    pub fn new() -> Self {
        Self {
            proc: ProcProvider::new(),
        }
    }
}

/// 构建 `/processes/*` 路由（相对 `/api/v1`）。
pub fn router(state: Arc<ProcessState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(detail))
        .routes(routes!(signal))
        .routes(routes!(renice))
        .with_state(state)
}

/// 进程列表
///
/// 平铺数组，树由前端按 `ppid` 拼。`tree=true` 时命中项的全部祖先一并返回并按深度优先排序。
/// CPU% 为两次请求之间的差分，**首次请求为 0**。
#[utoipa::path(
    get,
    path = "/processes",
    tag = "processes",
    params(ProcessListQuery),
    responses(
        (status = 200, description = "进程列表", body = Vec<ProcessSummary>),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn list(
    State(state): State<Arc<ProcessState>>,
    Query(query): Query<ProcessListQuery>,
) -> ApiResult<Json<Vec<ProcessSummary>>> {
    Ok(Json(state.proc.list(query).await?))
}

/// 进程详情
///
/// cmdline / cwd / exe / 环境变量 / fd / IO / cgroup 与所属 systemd unit。
/// `cwd` / `exe` / `environ` / `fds` 只有同 uid 或 root 能读，否则为 `null`。
#[utoipa::path(
    get,
    path = "/processes/{pid}",
    tag = "processes",
    params(("pid" = u32, Path, description = "进程 id")),
    responses(
        (status = 200, description = "进程详情", body = ProcessDetail),
        (status = 400, description = "pid 不合法", body = ApiError),
        (status = 404, description = "进程不存在", body = ApiError),
    ),
)]
pub async fn detail(
    State(state): State<Arc<ProcessState>>,
    Path(pid): Path<u32>,
) -> ApiResult<Json<ProcessDetail>> {
    Ok(Json(state.proc.detail(pid).await?))
}

/// 发送信号
///
/// 只开放 `term` / `kill` / `hup`。由内核裁决权限。
#[utoipa::path(
    post,
    path = "/processes/{pid}/signal",
    tag = "processes",
    params(("pid" = u32, Path, description = "进程 id")),
    request_body = SignalReq,
    responses(
        (status = 204, description = "已发送"),
        (status = 400, description = "pid 不合法或为 1", body = ApiError),
        (status = 403, description = "内核拒绝（不是属主）", body = ApiError),
        (status = 404, description = "进程不存在", body = ApiError),
    ),
)]
pub async fn signal(
    State(state): State<Arc<ProcessState>>,
    Path(pid): Path<u32>,
    Json(req): Json<SignalReq>,
) -> ApiResult<StatusCode> {
    state.proc.signal(pid, req.signal)?;
    Ok(StatusCode::NO_CONTENT)
}

/// 调整 nice 值
///
/// `setpriority(2)`。调低（提高优先级）需要 root，非特权用户只能调高自己的进程。
#[utoipa::path(
    post,
    path = "/processes/{pid}/renice",
    tag = "processes",
    params(("pid" = u32, Path, description = "进程 id")),
    request_body = ReniceReq,
    responses(
        (status = 204, description = "已调整"),
        (status = 400, description = "pid 或 nice 值不合法", body = ApiError),
        (status = 403, description = "内核拒绝", body = ApiError),
        (status = 404, description = "进程不存在", body = ApiError),
    ),
)]
pub async fn renice(
    State(state): State<Arc<ProcessState>>,
    Path(pid): Path<u32>,
    Json(req): Json<ReniceReq>,
) -> ApiResult<StatusCode> {
    state.proc.renice(pid, req.nice)?;
    Ok(StatusCode::NO_CONTENT)
}
