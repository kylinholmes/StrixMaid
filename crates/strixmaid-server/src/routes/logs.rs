//! `/api/v1/logs/*` —— journald 日志（`docs/design.md` §9.1「日志」组）。
//!
//! # 为什么这三个端点必须经 worker
//!
//! 日志的可见范围**不是**一个可以在应用层判断的东西：journald 用 ACL 决定谁能读到哪些条目
//! （macOS 上则是统一日志自己的权限模型）。同一条查询，root 看到全机、`adm` 组成员看到全机、
//! 普通用户只看到自己的条目——而且**不报错**，只是结果更少。
//!
//! 因此裁决只能发生在「以登录用户身份运行的进程」里。处理器经 [`crate::auth::exec`] 把查询投递到
//! 本会话的 user worker（`roadmap/01-worker-execution.md` §4.3），journalctl 子进程继承 worker
//! 的 uid，可见范围由操作系统给出——这正是 `design.md` §5.1「授权外包给操作系统」。
//! 若在主进程（root）里查，任何登录用户都能看到全机日志，授权模型直接失效。
//!
//! 结果集变少既不是错误也无法从响应里看出来，前端须依据
//! `UserCapabilities::can_read_journal` 主动提示。
//!
//! # 能力缺失
//!
//! 「本机有没有 journalctl」由 worker 侧判断：那里的 provider 为 `None` 时返回
//! `capability_unavailable{journal}`（501），本文件原样透传，不做可用性预判。
//!
//! `logs.follow` 走 WS（`ws/channels/`），不在本文件。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::log::{BootInfo, LogEntryDetail, LogPage, LogQuery};
use strixmaid_types::rpc::{self, CursorParams};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege};
use crate::error::ApiResult;

/// 构建日志路由。挂到 `/api/v1` 之下（路径已含 `/logs` 前缀）。
///
/// 状态是 [`AuthState`]：本模块要的不是 provider，而是「把查询送进本会话的 worker」的能力。
pub fn router(auth: Arc<AuthState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(query_logs))
        .routes(routes!(log_entry))
        .routes(routes!(list_boots))
        .with_state(auth)
}

/// 查询日志
///
/// 由新到旧一页；翻页带上 `cursor`（上一页的 `next_cursor`）与**相同的过滤条件**。
/// `limit` 缺省 100、上限 1000。`q` 是字面量关键字（不是正则），大小写不敏感。
/// 结果集只含登录用户可见的条目（见模块文档）。
#[utoipa::path(
    get,
    path = "/logs",
    tag = "logs",
    params(LogQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "一页日志（范围已由 journald ACL 按登录用户裁剪）", body = LogPage),
        (status = 400, description = "参数不合法（limit 越界、since > until、boot / cursor 格式错）", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 501, description = "本机没有 journalctl", body = ApiError),
        (status = 504, description = "journalctl 超时", body = ApiError),
    ),
)]
pub async fn query_logs(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<LogPage>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::LOG_QUERY, query).await?,
    ))
}

/// 单条日志全字段
///
/// 游标需 URL 编码。已被轮转淘汰的条目返回 404；**对登录用户不可见的条目同样是 404**——
/// journalctl 的行为就是「查不到」，服务端不去区分「不存在」与「无权看见」，
/// 区分本身就会泄露日志的存在。
#[utoipa::path(
    get,
    path = "/logs/entry/{cursor}",
    tag = "logs",
    params(
        ("cursor" = String, Path, description = "journald 游标（`__CURSOR`），需 URL 编码"),
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "全字段详情", body = LogEntryDetail),
        (status = 400, description = "游标格式不合法", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 404, description = "游标对应的条目不存在，或对登录用户不可见", body = ApiError),
        (status = 501, description = "本机没有 journalctl", body = ApiError),
    ),
)]
pub async fn log_entry(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(cursor): Path<String>,
) -> ApiResult<Json<LogEntryDetail>> {
    Ok(Json(
        exec::call(
            &auth,
            &session,
            Privilege::User,
            rpc::LOG_ENTRY,
            CursorParams { cursor },
        )
        .await?,
    ))
}

/// boot 列表
///
/// 按 `index` 升序，`0` 为本次启动。`boot_id` 可直接作为查询参数 `boot` 的值。
/// 同样受 ACL 影响：读不到系统日志的用户只会看到自己有条目的那些 boot。
#[utoipa::path(
    get,
    path = "/logs/boots",
    tag = "logs",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "boot 列表", body = Vec<BootInfo>),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 501, description = "本机没有 journalctl", body = ApiError),
    ),
)]
pub async fn list_boots(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
) -> ApiResult<Json<Vec<BootInfo>>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::LOG_BOOTS, ()).await?,
    ))
}
