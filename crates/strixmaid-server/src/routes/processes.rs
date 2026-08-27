//! `/api/v1/processes/*` —— 进程列表 / 详情 / 信号 / renice。
//!
//! # 执行路径
//!
//! 本模块不再持有 provider。每个处理器转成一次 worker RPC，经 [`crate::auth::exec`]
//! 派给该会话的 worker（`roadmap/01-worker-execution.md` §4.3）：
//!
//! ```text
//! HTTP 请求 → exec::call(方法名, 参数) → WorkerHandle → worker 进程内的 ProcProvider
//! ```
//!
//! 由此得到两个不是「顺手」而是必需的性质：
//!
//! - **详情里的 `cwd` / `exe` / `environ` / `fds` 反映的是登录用户真实能看到的东西。**
//!   这些字段的可读性由 `/proc/<pid>` 的属主决定；留在主进程（root）里读，
//!   任何用户都能看到全部内容，等于绕过了内核。
//! - **CPU% 的差分基线随会话而非随服务进程。** provider 实例活在 worker 里，
//!   每个会话一个，因此每个新会话的首次请求 CPU% 为 0（`roadmap` §8 未决问题 3）。
//!
//! # 为什么信号与 renice 不用 `Privilege::Admin`
//!
//! 见 `roadmap/01-worker-execution.md` §4.1 带 `*` 的说明：**向自己的进程发信号是
//! 普通用户的正当操作**，把它划成「写操作 → 必须提权」会让用户杀掉自己刚起的进程
//! 都要重输一次密码，是明显的倒退。所以这两个端点走 [`exec::call_escalating`]：
//! 先以登录用户的身份试，只有内核真的回了 `EPERM` 才考虑用管理身份重试。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::process::{
    ProcessDetail, ProcessListQuery, ProcessSummary, ReniceReq, SignalReq,
};
use strixmaid_types::rpc;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege, RequestOrigin};
use crate::error::ApiResult;

/// 构建 `/processes/*` 路由（相对 `/api/v1`）。
///
/// 状态只有 [`AuthState`]：处理器需要它来找到本会话的 worker。
pub fn router(auth: Arc<AuthState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(detail))
        .routes(routes!(signal))
        .routes(routes!(renice))
        .with_state(auth)
}

/// 进程列表
///
/// 平铺数组，树由前端按 `ppid` 拼。`tree=true` 时命中项的全部祖先一并返回并按深度优先排序。
/// CPU% 为两次请求之间的差分；差分基线在本会话的 worker 内，因此**每个会话的首次请求为 0**。
#[utoipa::path(
    get,
    path = "/processes",
    tag = "processes",
    security(("bearer" = [])),
    params(ProcessListQuery),
    responses(
        (status = 200, description = "进程列表", body = Vec<ProcessSummary>),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 500, description = "采集任务异常", body = ApiError),
    ),
)]
pub async fn list(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Query(query): Query<ProcessListQuery>,
) -> ApiResult<Json<Vec<ProcessSummary>>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::PROC_LIST, query).await?,
    ))
}

/// 进程详情
///
/// cmdline / cwd / exe / 环境变量 / fd / IO / cgroup 与所属 systemd unit。
/// `cwd` / `exe` / `environ` / `fds` 只有同 uid 或 root 能读，否则为 `null`——
/// 判断的依据是**登录用户**，因为读取动作发生在该用户的 worker 里。
#[utoipa::path(
    get,
    path = "/processes/{pid}",
    tag = "processes",
    security(("bearer" = [])),
    params(("pid" = u32, Path, description = "进程 id")),
    responses(
        (status = 200, description = "进程详情", body = ProcessDetail),
        (status = 400, description = "pid 不合法", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 404, description = "进程不存在", body = ApiError),
    ),
)]
pub async fn detail(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(pid): Path<u32>,
) -> ApiResult<Json<ProcessDetail>> {
    Ok(Json(
        exec::call(
            &auth,
            &session,
            Privilege::User,
            rpc::PROC_DETAIL,
            rpc::PidParams { pid },
        )
        .await?,
    ))
}

/// 发送信号
///
/// 只开放 `term` / `kill` / `hup`。**权限完全由内核裁决**，服务端不做任何判断。
///
/// 先以登录用户的身份发（自己的进程本就该发得动）；内核回 `EPERM` 且会话已提权时
/// 自动改用管理身份重试一次。未提权时返回内核给出的原始 403，并带
/// `can_retry_elevated = true` —— 前端据此提供「启用管理访问后重试」，
/// 而不是把它显示成一条死路。
#[utoipa::path(
    post,
    path = "/processes/{pid}/signal",
    tag = "processes",
    security(("bearer" = [])),
    params(("pid" = u32, Path, description = "进程 id")),
    request_body = SignalReq,
    responses(
        (status = 204, description = "已发送"),
        (status = 400, description = "pid 不合法或为 1", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "内核拒绝（不是属主）且会话未提权，`can_retry_elevated = true`；\
                                      启用管理访问后重试可成功", body = ApiError),
        (status = 404, description = "进程不存在", body = ApiError),
    ),
)]
pub async fn signal(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(pid): Path<u32>,
    origin: RequestOrigin,
    Json(req): Json<SignalReq>,
) -> ApiResult<StatusCode> {
    exec::call_escalating_from::<_, ()>(
        &auth,
        &session,
        &origin,
        rpc::PROC_SIGNAL,
        rpc::SignalParams {
            pid,
            signal: req.signal,
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// 调整 nice 值
///
/// `setpriority(2)`。调低（提高优先级）需要 root，非特权用户只能调高自己的进程。
///
/// 与信号同一套规则：先以登录用户的身份试，被内核拒绝且已提权时改用管理身份重试；
/// 未提权则返回 403 并带 `can_retry_elevated = true`。「把自己的进程调低优先级」
/// 是无需提权的日常操作，不该被一刀切成管理操作。
#[utoipa::path(
    post,
    path = "/processes/{pid}/renice",
    tag = "processes",
    security(("bearer" = [])),
    params(("pid" = u32, Path, description = "进程 id")),
    request_body = ReniceReq,
    responses(
        (status = 204, description = "已调整"),
        (status = 400, description = "pid 或 nice 值不合法", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "内核拒绝（不是属主，或调低优先级需要 root）且会话未提权，\
                                      `can_retry_elevated = true`；启用管理访问后重试可成功", body = ApiError),
        (status = 404, description = "进程不存在", body = ApiError),
    ),
)]
pub async fn renice(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(pid): Path<u32>,
    origin: RequestOrigin,
    Json(req): Json<ReniceReq>,
) -> ApiResult<StatusCode> {
    exec::call_escalating_from::<_, ()>(
        &auth,
        &session,
        &origin,
        rpc::PROC_RENICE,
        rpc::ReniceParams {
            pid,
            nice: req.nice,
        },
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
