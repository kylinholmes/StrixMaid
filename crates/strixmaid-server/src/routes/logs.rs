//! `/api/v1/logs/*` —— journald 日志（`docs/design.md` §9.1「日志」组）。
//!
//! 薄壳：全部逻辑在 `strixmaid_core::providers::log`。能看到多少日志由 journald ACL 裁决，
//! 不在 `systemd-journal` / `adm` 组的用户只看到自己的条目且**不报错**——前端要依据
//! `UserCapabilities::can_read_journal` 提示。`logs.follow` 走 WS，不在本文件。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use strixmaid_core::providers::log::LogProvider;
use strixmaid_types::ApiError;
use strixmaid_types::log::{BootInfo, LogEntryDetail, LogPage, LogQuery};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::{ApiErr, ApiResult};

/// 日志路由的状态。`provider` 为 `None` 表示没有 journalctl，
/// 所有端点返回 501 `capability_unavailable{journal}`。
pub struct LogsState {
    pub provider: Option<Arc<dyn LogProvider>>,
}

impl LogsState {
    pub fn new(provider: Option<Arc<dyn LogProvider>>) -> Self {
        Self { provider }
    }

    fn provider(&self) -> Result<&dyn LogProvider, ApiErr> {
        self.provider.as_deref().ok_or_else(|| {
            ApiError::capability_unavailable("journal", "本机没有可用的 journalctl").into()
        })
    }
}

/// 构建日志路由。挂到 `/api/v1` 之下（路径已含 `/logs` 前缀）。
pub fn router(state: Arc<LogsState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(query_logs))
        .routes(routes!(log_entry))
        .routes(routes!(list_boots))
        .with_state(state)
}

/// 查询日志
///
/// 由新到旧一页；翻页带上 `cursor`（上一页的 `next_cursor`）与**相同的过滤条件**。
/// `limit` 缺省 100、上限 1000。`q` 是字面量关键字（不是正则），大小写不敏感。
#[utoipa::path(
    get,
    path = "/logs",
    tag = "logs",
    params(LogQuery),
    responses(
        (status = 200, description = "一页日志", body = LogPage),
        (status = 400, description = "参数不合法（limit 越界、since > until、boot / cursor 格式错）", body = ApiError),
        (status = 501, description = "本机没有 journalctl", body = ApiError),
        (status = 504, description = "journalctl 超时", body = ApiError),
    ),
)]
pub async fn query_logs(
    State(state): State<Arc<LogsState>>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<LogPage>> {
    Ok(Json(state.provider()?.query(&query).await?))
}

/// 单条日志全字段
///
/// 游标需 URL 编码。已被轮转淘汰的条目返回 404。
#[utoipa::path(
    get,
    path = "/logs/entry/{cursor}",
    tag = "logs",
    params(
        ("cursor" = String, Path, description = "journald 游标（`__CURSOR`），需 URL 编码"),
    ),
    responses(
        (status = 200, description = "全字段详情", body = LogEntryDetail),
        (status = 400, description = "游标格式不合法", body = ApiError),
        (status = 404, description = "游标对应的条目不存在", body = ApiError),
        (status = 501, description = "本机没有 journalctl", body = ApiError),
    ),
)]
pub async fn log_entry(
    State(state): State<Arc<LogsState>>,
    Path(cursor): Path<String>,
) -> ApiResult<Json<LogEntryDetail>> {
    Ok(Json(state.provider()?.entry(&cursor).await?))
}

/// boot 列表
///
/// 按 `index` 升序，`0` 为本次启动。`boot_id` 可直接作为查询参数 `boot` 的值。
#[utoipa::path(
    get,
    path = "/logs/boots",
    tag = "logs",
    responses(
        (status = 200, description = "boot 列表", body = Vec<BootInfo>),
        (status = 501, description = "本机没有 journalctl", body = ApiError),
    ),
)]
pub async fn list_boots(State(state): State<Arc<LogsState>>) -> ApiResult<Json<Vec<BootInfo>>> {
    Ok(Json(state.provider()?.boots().await?))
}
