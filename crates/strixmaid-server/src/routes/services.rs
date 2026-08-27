//! `/api/v1/services/*` —— systemd unit 管理（`docs/design.md` §9.1「服务」组）。
//!
//! # 执行路径
//!
//! 处理器不再持有 provider，而是经 [`crate::auth::exec`] 把调用投递到**本会话的 worker**
//! （`roadmap/01-worker-execution.md` §4.3）。worker 的 uid 就是登录用户，它连 system bus 时
//! zbus 的 EXTERNAL 认证携带的正是该 uid，于是 polkit 裁决的对象是**真实的登录用户**而不是
//! 服务进程——这正是 `design.md` §5.1 要的效果：授权外包给操作系统，服务端不含权限判断。
//!
//! `scope=user` 也因此自然成立：worker 连的是 `/run/user/<uid>/bus`，uid 即 worker 自身。
//!
//! # 能力缺失
//!
//! 「本机有没有 systemd」由 worker 侧判断：那里的 provider 为 `None` 时返回
//! `capability_unavailable{systemd}`（501），本文件原样透传，不做任何可用性预判。
//! 探测结果与真正执行操作的进程保持在同一侧，避免主进程「以为有」而 worker「其实没有」。
//!
//! # 参数校验
//!
//! 仍然分两处：查询参数由 types 的 DTO（`UnitListQuery` / [`ScopeQuery`]）在反序列化时校验，
//! unit 名由 core 的 `validate_unit_name` 在 provider 入口校验——后者现在发生在 worker 内。
//!
//! `{unit}` 路径参数需 URL 编码（实例名里有 `@`、转义名里有 `\`），axum 的 `Path` 会解码。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use serde::{Deserialize, Serialize};
use strixmaid_core::providers::service::UnitDeps as CoreUnitDeps;
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::rpc::{self, UnitActionParams, UnitParams};
use strixmaid_types::service::{
    UnitActionReq, UnitActionResp, UnitDetail, UnitFile, UnitListQuery, UnitScope, UnitSummary,
};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege, RequestOrigin};
use crate::error::ApiResult;

/// 构建服务路由。挂到 `/api/v1` 之下（路径已含 `/services` 前缀）。
///
/// 状态是 [`AuthState`]：本模块要的不是 provider，而是「把调用送进哪个 worker」的能力。
pub fn router(auth: Arc<AuthState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(list_units))
        .routes(routes!(unit_detail))
        .routes(routes!(unit_file))
        .routes(routes!(unit_deps))
        .routes(routes!(unit_action))
        .with_state(auth)
}

/// 单 unit 端点的作用域参数。
#[derive(Debug, Clone, Copy, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ScopeQuery {
    /// 作用域，缺省 `system`。`user` 指登录用户自己的 user manager。
    #[serde(default)]
    pub scope: Option<UnitScope>,
}

/// `GET /api/v1/services/{unit}/deps` 的响应体：unit 依赖关系。
///
/// 字段与 systemd `org.freedesktop.systemd1.Unit` 的同名属性一一对应。
/// types crate 里没有这一项，暂由本文件定义（core 侧是同形的
/// [`strixmaid_core::providers::service::UnitDeps`]）。
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UnitDeps {
    /// unit 名。
    #[schema(example = "nginx.service")]
    pub unit: String,
    /// 强依赖：本 unit 启动时它们必须成功启动。
    pub requires: Vec<String>,
    /// 类似 `requires`，但要求对方已经 active。
    pub requisite: Vec<String>,
    /// 弱依赖。
    pub wants: Vec<String>,
    /// 对方停止时本 unit 也停止。
    pub binds_to: Vec<String>,
    /// 对方停止 / 重启时本 unit 跟随。
    pub part_of: Vec<String>,
    /// 反向：哪些 unit `Requires` 本 unit。
    pub required_by: Vec<String>,
    /// 反向：哪些 unit `Wants` 本 unit。
    pub wanted_by: Vec<String>,
    /// 反向：哪些 unit `BindsTo` 本 unit。
    pub bound_by: Vec<String>,
    /// 互斥。
    pub conflicts: Vec<String>,
    /// 反向互斥。
    pub conflicted_by: Vec<String>,
    /// 顺序：本 unit 在它们之前启动。
    pub before: Vec<String>,
    /// 顺序：本 unit 在它们之后启动。
    pub after: Vec<String>,
    /// 本 unit 触发的 unit（socket / timer / path）。
    pub triggers: Vec<String>,
    /// 触发本 unit 的 unit。
    pub triggered_by: Vec<String>,
}

impl From<CoreUnitDeps> for UnitDeps {
    fn from(d: CoreUnitDeps) -> Self {
        Self {
            unit: d.unit,
            requires: d.requires,
            requisite: d.requisite,
            wants: d.wants,
            binds_to: d.binds_to,
            part_of: d.part_of,
            required_by: d.required_by,
            wanted_by: d.wanted_by,
            bound_by: d.bound_by,
            conflicts: d.conflicts,
            conflicted_by: d.conflicted_by,
            before: d.before,
            after: d.after,
            triggers: d.triggers,
            triggered_by: d.triggered_by,
        }
    }
}

/// unit 列表
///
/// 已加载的 unit 与仅存在于磁盘的 unit 文件合并后按名字排序；可按类型 / 活动状态 /
/// 是否开机自启 / 关键字过滤。`scope=user` 需要 user manager 可达。
#[utoipa::path(
    get,
    path = "/services",
    tag = "services",
    params(UnitListQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "unit 列表", body = Vec<UnitSummary>),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 501, description = "本机没有 systemd", body = ApiError),
        (status = 503, description = "systemd 暂时不可达（bus 断开 / user manager 未启动）", body = ApiError),
        (status = 504, description = "systemd 无响应", body = ApiError),
    ),
)]
pub async fn list_units(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Query(query): Query<UnitListQuery>,
) -> ApiResult<Json<Vec<UnitSummary>>> {
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::SERVICE_LIST, query).await?,
    ))
}

/// unit 详情
///
/// 含主进程 pid、时间戳、重启次数、上次结果，以及直读 `/sys/fs/cgroup` 的 CPU / 内存 / 任务数。
/// `cgroup.cpu_percent` 需要两次采样：首次请求为 `null`，之后每次相对上一次计算。
/// 差分基线是 worker 内的实例状态，因此「首次」是**每个会话各一次**。
#[utoipa::path(
    get,
    path = "/services/{unit}",
    tag = "services",
    params(
        ("unit" = String, Path, description = "完整 unit 名，含后缀，需 URL 编码", example = "nginx.service"),
        ScopeQuery,
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "unit 详情", body = UnitDetail),
        (status = 400, description = "unit 名不合法", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 404, description = "unit 不存在", body = ApiError),
        (status = 501, description = "本机没有 systemd", body = ApiError),
        (status = 503, description = "systemd 暂时不可达", body = ApiError),
    ),
)]
pub async fn unit_detail(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(unit): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> ApiResult<Json<UnitDetail>> {
    let params = UnitParams {
        scope: q.scope.unwrap_or_default(),
        unit,
    };
    Ok(Json(
        exec::call(
            &auth,
            &session,
            Privilege::User,
            rpc::SERVICE_DETAIL,
            params,
        )
        .await?,
    ))
}

/// unit 文件原文
///
/// 主文件（`FragmentPath`）与 drop-in 覆盖文件（`DropInPaths`）的原文，不做任何解析。
/// transient unit 没有主文件，`fragment` 为 `null`。
/// 读文件的是 worker，因此可读性由文件权限对登录用户裁决。
#[utoipa::path(
    get,
    path = "/services/{unit}/file",
    tag = "services",
    params(
        ("unit" = String, Path, description = "完整 unit 名，含后缀，需 URL 编码", example = "nginx.service"),
        ScopeQuery,
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "unit 文件", body = UnitFile),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "文件对登录用户不可读", body = ApiError),
        (status = 404, description = "unit 不存在", body = ApiError),
        (status = 501, description = "本机没有 systemd", body = ApiError),
    ),
)]
pub async fn unit_file(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(unit): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> ApiResult<Json<UnitFile>> {
    let params = UnitParams {
        scope: q.scope.unwrap_or_default(),
        unit,
    };
    Ok(Json(
        exec::call(&auth, &session, Privilege::User, rpc::SERVICE_FILE, params).await?,
    ))
}

/// unit 依赖关系
///
/// `Requires` / `Wants` / `After` / `Before` / `TriggeredBy` 等属性原样返回，供前端画依赖图。
#[utoipa::path(
    get,
    path = "/services/{unit}/deps",
    tag = "services",
    params(
        ("unit" = String, Path, description = "完整 unit 名，含后缀，需 URL 编码", example = "nginx.service"),
        ScopeQuery,
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "依赖关系", body = UnitDeps),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 404, description = "unit 不存在", body = ApiError),
        (status = 501, description = "本机没有 systemd", body = ApiError),
    ),
)]
pub async fn unit_deps(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(unit): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> ApiResult<Json<UnitDeps>> {
    let params = UnitParams {
        scope: q.scope.unwrap_or_default(),
        unit,
    };
    // worker 回传的是 core 的同形结构，这里转成带 `ToSchema` 的对外 DTO；
    // 等它迁进 types（实施约定 5）后这一步连同 `From` 一起删掉。
    let deps: CoreUnitDeps =
        exec::call(&auth, &session, Privilege::User, rpc::SERVICE_DEPS, params).await?;
    Ok(Json(deps.into()))
}

/// 对 unit 执行操作
///
/// `start` / `stop` / `restart` / `reload` 是异步的：返回只表示 job 已入队，
/// 终态请订阅 WS `services.changed`。`enable` / `disable` / `mask` / `unmask` 会改写符号链接
/// 并触发 `daemon-reload`。
///
/// 能不能做由 polkit 对**登录用户**裁决，服务端不预判。
#[utoipa::path(
    post,
    path = "/services/{unit}/action",
    tag = "services",
    params(
        ("unit" = String, Path, description = "完整 unit 名，含后缀，需 URL 编码", example = "nginx.service"),
        ScopeQuery,
    ),
    security(("bearer" = [])),
    request_body = UnitActionReq,
    responses(
        (status = 200, description = "job 已入队（bus）或已执行完（systemctl）", body = UnitActionResp),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "被拒。通常是 `permission_denied`——polkit 以登录用户的身份拒绝了本次操作；\
未提权时带 `can_retry_elevated=true`，提示前端「启用管理访问后重试」。若升级重试的瞬间 admin worker \
恰好已因提权超时被回收，则为 `elevation_required`", body = ApiError),
        (status = 404, description = "unit 不存在", body = ApiError),
        (status = 409, description = "systemd 拒绝（已 mask、无 ExecReload 等）", body = ApiError),
        (status = 501, description = "本机没有 systemd", body = ApiError),
    ),
)]
pub async fn unit_action(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Path(unit): Path<String>,
    Query(q): Query<ScopeQuery>,
    origin: RequestOrigin,
    Json(req): Json<UnitActionReq>,
) -> ApiResult<Json<UnitActionResp>> {
    let params = UnitActionParams {
        scope: q.scope.unwrap_or_default(),
        unit,
        action: req.action,
    };
    // 为什么是 `call_escalating` 而不是无脑 `Privilege::Admin`：
    //
    // 1. `scope=user` 时操作的是登录用户自己的 user manager，本来就不需要 root
    //    （`roadmap` §4.1 带 `*` 的说明）。写死 Admin 会让「重启自己的 user unit」
    //    也弹一次提权，是明显的倒退。
    // 2. 即便 `scope=system`，能否执行也该由 polkit 说了算，而不是由服务端替它猜。
    //    发行版的 polkit 规则、unit 自带的 `PolicyKit` 授权、`wheel` 组的默认放行，
    //    都可能让登录用户本人就有权操作——写死 Admin 等于在服务端复刻一份权限矩阵，
    //    正是 `design.md` §5.1 禁止的「自建 RBAC」。
    //
    // 所以：先以登录用户的身份试，只有内核 / polkit 真的回了 `permission_denied`
    // 才考虑换 admin worker；未提权则把原始拒绝理由原样返回并标上 `can_retry_elevated`。
    Ok(Json(
        exec::call_escalating_from(&auth, &session, &origin, rpc::SERVICE_ACTION, params).await?,
    ))
}
