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
//!
//! # 为什么只有两个图标端点不经 worker
//!
//! 与 [`crate::routes::processes`] 里 `icon` 的理由逐条相同：图标是**可执行映像
//! 自身的属性**，不是服务的、更不是用户的。同一个 `spoolsv.exe` 对所有登录用户
//! 都是同一张图，没有「随用户而变」的语义，也就没有必须落到 worker 里执行的理由；
//! 放在主进程则让缓存跨会话共用，而不是每个会话各热一遍。
//!
//! 服务这边还多一条：图标的来源是 `HKLM\SYSTEM\CurrentControlSet\Services` 与
//! `System32` 下的文件，两者都是**全局只读**的机器状态，与 `/services/{unit}`
//! 那些要经 polkit / SCM 裁决的操作不是一回事。
//!
//! **鉴权不开特例**：两个端点与本模块其余端点一样挂在 `routes::api_v1` 的
//! `protected` 组里，缺少或无效的 `Authorization: Bearer` 一律 401。
//!
//! # 为什么通用图标是**另一个**端点，而不是按名字那个端点的回落
//!
//! 因为回落会把「这是这个服务自己的图标」和「这是一张所有服务共用的通用图」
//! 混成同一个 200 响应，调用方无从区分。分成两个端点之后，
//! `/services/icon/{name}` 的 404 就是一个明确的事实：这个服务没有自己的图标。
//! 拿什么顶上是**展示层**的决定，由前端在收到 404 后去取 `/services/icon-generic`
//! ——那是一次请求、一次浏览器缓存，不是每行一次。
//!
//! 本机实测：317 个能解析出可执行文件的服务里只有 17 个取得到自己的图标，
//! 其余（含 239 个由 `svchost.exe` 托管的）一律 404。也就是说**404 才是常态**。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use strixmaid_core::providers::service::UnitDeps as CoreUnitDeps;
use strixmaid_core::providers::service::icon::ServiceIcons;
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::rpc::{self, ScopeParams, UnitActionParams, UnitParams};
use strixmaid_types::service::{
    TimerEntry, UnitActionReq, UnitActionResp, UnitDetail, UnitFile, UnitListQuery, UnitScope,
    UnitSummary,
};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege, RequestOrigin};
use crate::error::ApiResult;

/// 构建服务路由。挂到 `/api/v1` 之下（路径已含 `/services` 前缀）。
///
/// 两组路由的状态不同，因此分开建再 `merge`：
///
/// - 列表 / 详情 / 单元文件 / 依赖 / 操作的状态是 [`AuthState`]，本模块要的不是
///   provider，而是「把调用送进哪个 worker」的能力；
/// - 两个图标端点的状态是主进程自己的 [`ServiceIcons`]，图标缓存挂在它上面
///   （见模块文档）。
///
/// 两组**鉴权完全一致**，图标端点没有任何特例。
pub fn router(auth: Arc<AuthState>, icons: ServiceIcons) -> OpenApiRouter<()> {
    // 静态段 `/services/icon-generic` 与 `/services/icon/{name}` 同样与 `{unit}`
    // 同层，理由与下面 `/services/timers` 的注释一致。
    let icon_routes = OpenApiRouter::new()
        .routes(routes!(icon))
        .routes(routes!(icon_generic))
        .with_state(icons);
    OpenApiRouter::new()
        .routes(routes!(list_units))
        // 静态段 `/services/timers` 与 `{unit}` 同层：axum(matchit) 静态优先，
        // 不会被当成 unit 名
        .routes(routes!(list_timers))
        .routes(routes!(unit_detail))
        .routes(routes!(unit_file))
        .routes(routes!(unit_deps))
        .routes(routes!(unit_action))
        .with_state(auth)
        .merge(icon_routes)
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

/// 定时任务列表
///
/// systemd `*.timer`（Linux）与 launchd 定时 job（macOS）统一成一张表。字段缺失都有含义：
/// `schedule` 为空数组 = 拿不到调度规则（systemctl 降级路径）；`next_ts` 为 `null` =
/// 推算不了（timer 已停、launchd `StartInterval` 型）；`last_ts` 为 `null` = 来源不记录。
/// cron 是规划中的第三来源，实现前不出现。
#[utoipa::path(
    get,
    path = "/services/timers",
    tag = "services",
    params(ScopeQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "定时任务列表", body = Vec<TimerEntry>),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 501, description = "本机没有服务管理器", body = ApiError),
        (status = 503, description = "服务管理器暂时不可达", body = ApiError),
    ),
)]
pub async fn list_timers(
    State(auth): State<Arc<AuthState>>,
    Extension(session): Extension<Session>,
    Query(q): Query<ScopeQuery>,
) -> ApiResult<Json<Vec<TimerEntry>>> {
    let params = ScopeParams {
        scope: q.scope.unwrap_or_default(),
    };
    Ok(Json(
        exec::call(
            &auth,
            &session,
            Privilege::User,
            rpc::SERVICE_TIMERS,
            params,
        )
        .await?,
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

/// 服务图标
///
/// 按服务名取**这个服务自己**的一张 32×32 PNG（名字取自 `UnitSummary.name`，
/// 带不带 `.service` 后缀都可以）。图标来自它 `ImagePath` 指向的可执行文件。
///
/// 取不到一律 404，且这是常态而非异常：由 `svchost.exe` 托管的服务（本机 317 个
/// 里有 239 个）与没有界面的守护进程都不带图标资源。前端据此改取
/// `/services/icon-generic` 那张通用齿轮。
#[utoipa::path(
    get,
    path = "/services/icon/{name}",
    tag = "services",
    security(("bearer" = [])),
    params(("name" = String, Path, description = "服务名，取自 `UnitSummary.name`，如 `Spooler` 或 `Spooler.service`")),
    responses(
        // body 写 `String` 而不是 `Vec<u8>` 的理由与 `processes::icon` 相同：
        // 后者会被 utoipa 展开成整数数组，`openapi-typescript` 据此生成 `number[]`。
        (status = 200, description = "32×32 的 PNG 图标（响应体是原始 PNG 字节）", content_type = "image/png", body = String),
        (status = 400, description = "服务名不合法（含路径分隔符、`..` 或过长）", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 404, description = "服务不存在、它是驱动、或它的可执行文件里没有图标资源；\
                                      非 Windows 平台恒为此项", body = ApiError),
    ),
)]
pub async fn icon(
    State(icons): State<ServiceIcons>,
    // 不用这个值，但**必须**提取：它是「本端点与其余 /services/* 一样需要会话」
    // 这件事在代码里的凭据。日后若有人把这条路由挪出受保护组，这里会立刻失败，
    // 而不是悄悄变成一个匿名可读的端点。
    Extension(_session): Extension<Session>,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    png_response(icons.icon_png(&name).await?)
}

/// 通用服务图标
///
/// 一张所有服务共用的齿轮，与 `services.msc` 里那对齿轮是同一张图
/// （Windows 上从 `%SystemRoot%\System32\filemgmt.dll` 运行时提取）。
///
/// 前端在 `/services/icon/{name}` 回 404 时取它顶上。它与具体服务无关，
/// 因此**整张表只需要取一次**，再由浏览器缓存住。
///
/// 非 Windows 平台恒为 404：那两个平台上没有一张属于「服务」这个类别的系统图形
/// 可取，拿通用可执行文件图顶替就是在展示层编数据。
#[utoipa::path(
    get,
    path = "/services/icon-generic",
    tag = "services",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "32×32 的 PNG 齿轮（响应体是原始 PNG 字节）", content_type = "image/png", body = String),
        (status = 401, description = "未认证", body = ApiError),
        (status = 404, description = "本平台没有可用的通用服务图标（Linux / macOS 恒为此项）", body = ApiError),
    ),
)]
pub async fn icon_generic(
    State(icons): State<ServiceIcons>,
    Extension(_session): Extension<Session>,
) -> ApiResult<Response> {
    png_response(icons.generic_icon_png().await?)
}

/// 两个图标端点共用的响应构造。
fn png_response(png: std::sync::Arc<Vec<u8>>) -> ApiResult<Response> {
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            // 浏览器端再缓存 5 分钟，与服务端的 TTL 同一档，省掉大部分往返。
            // 通用齿轮其实一辈子不变，但不值得为它单开一档——它本来就只取一次。
            (header::CACHE_CONTROL, "max-age=300"),
        ],
        png.as_slice().to_vec(),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use strixmaid_core::providers::service::icon as svc_icon;
    use strixmaid_core::session::{ClientMeta, Session};
    use strixmaid_types::ErrorCode;
    use strixmaid_types::auth::AuthUser;
    use tower::ServiceExt as _;

    /// 造一个会话。两个图标端点都不看它的任何字段，只要求它在——存在本身就是
    /// 「这两条路由在受保护组里」的凭据。
    fn session() -> Session {
        Session {
            token_hash: "hash".into(),
            node: "local".into(),
            user: AuthUser {
                uid: 1000,
                gid: 1000,
                username: "alice".into(),
                groups: Vec::new(),
            },
            elevated: false,
            elevated_ts: None,
            authed_ts: 1_700_000_000,
            created_ts: 1_700_000_000,
            last_active_ts: 1_700_000_000,
            meta: ClientMeta {
                user_agent: None,
                remote_addr: Some("127.0.0.1:1234".into()),
            },
            session_opened: false,
        }
    }

    /// 只挂两条图标路由（其余端点要 worker，不在本文件的测试范围里）。
    fn app() -> Router {
        let (router, _) = OpenApiRouter::new()
            .nest(
                "/api/v1",
                OpenApiRouter::new()
                    .routes(routes!(icon))
                    .routes(routes!(icon_generic))
                    .with_state(ServiceIcons::new()),
            )
            .split_for_parts();
        router
    }

    /// 发一次请求，返回状态码、Content-Type、Cache-Control 与响应体。
    async fn get_on(app: Router, uri: &str) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
        let mut req = Request::get(uri).body(Body::empty()).unwrap();
        // 生产里由认证中间件注入，这里手工塞进去。
        req.extensions_mut().insert(session());
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let header_of = |name: header::HeaderName| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let ct = header_of(header::CONTENT_TYPE);
        let cc = header_of(header::CACHE_CONTROL);
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, ct, cc, bytes.to_vec())
    }

    async fn get_icon(uri: &str) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
        get_on(app(), uri).await
    }

    fn 错误码(body: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(body).expect("错误体应当是 JSON");
        v["code"].as_str().unwrap_or_default().to_owned()
    }

    fn 断言是_png(body: &[u8]) {
        assert_eq!(
            &body[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "响应体不是合法 PNG"
        );
        assert_eq!(&body[12..16], b"IHDR");
        let w = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
        let h = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
        assert_eq!((w, h), (32, 32), "图标尺寸不是 32×32");
    }

    #[tokio::test]
    async fn 服务名不合法时返回_400() {
        // `/` 会被路由本身吃掉（多一段路径，匹配不到），所以用百分号编码送进处理器。
        for bad in [
            "%2E%2E%2Fpasswd",      // ../passwd
            "Spooler%5CParameters", // Spooler\Parameters，拼注册表键路径的口子
            "a%2Fb",                // a/b
            "%2E%2E",               // ..
        ] {
            let (status, _, _, body) = get_icon(&format!("/api/v1/services/icon/{bad}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} 应当被拒绝");
            assert_eq!(错误码(&body), ErrorCode::InvalidRequest.as_str());
        }

        let 超长 = "a".repeat(300);
        let (status, _, _, body) = get_icon(&format!("/api/v1/services/icon/{超长}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "超长名字应当被拒绝");
        assert_eq!(错误码(&body), ErrorCode::InvalidRequest.as_str());
    }

    #[tokio::test]
    async fn 不存在的服务返回_404() {
        let (status, _, _, body) = get_icon("/api/v1/services/icon/绝不会有这个服务_9f3a1c").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(错误码(&body), ErrorCode::NotFound.as_str());
    }

    #[tokio::test]
    async fn 通用图标端点给出一张_png() {
        let (status, ct, cc, body) = get_icon("/api/v1/services/icon-generic").await;
        if !svc_icon::available() {
            assert_eq!(
                status,
                StatusCode::NOT_FOUND,
                "本平台不提供服务图标，通用端点也该 404，而不是画一张顶上"
            );
            return;
        }
        if status != StatusCode::OK {
            eprintln!("本机取不到通用服务图标（{status}），跳过");
            return;
        }
        assert_eq!(ct.as_deref(), Some("image/png"));
        assert_eq!(cc.as_deref(), Some("max-age=300"), "浏览器侧也要缓存");
        断言是_png(&body);
    }

    /// 真机：找一个确实带图标的服务，验证按名字那条路给出的是 PNG。
    ///
    /// 绝大多数服务没有自己的图标（本机 317 个里只有 17 个有），因此这里是
    /// 「扫一遍，找得到就断言，找不到就跳过」，而不是写死某个服务名。
    #[cfg(windows)]
    #[tokio::test]
    async fn 取到服务图标时是_png_且带缓存头() {
        use strixmaid_core::platform::windows::registry::{HKLM, reg_subkeys};
        use strixmaid_core::providers::service::scm::map::SERVICES_KEY;

        let app = app();
        for name in reg_subkeys(HKLM, SERVICES_KEY) {
            if svc_icon::validate_service_name(&name).is_err() {
                continue;
            }
            let uri = format!("/api/v1/services/icon/{}", 转义路径段(&name));
            let (status, ct, cc, body) = get_on(app.clone(), &uri).await;
            if status != StatusCode::OK {
                continue;
            }
            assert_eq!(ct.as_deref(), Some("image/png"), "{name}");
            assert_eq!(cc.as_deref(), Some("max-age=300"), "{name}");
            断言是_png(&body);
            eprintln!("{name} 有自己的图标");
            return;
        }
        eprintln!("本机没有任何一个服务取得到自己的图标，跳过");
    }

    /// 百分号转义。服务名里有空格、括号、逗号，直接拼进 URI 会解析不出来。
    #[cfg(windows)]
    fn 转义路径段(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    }

    /// `/services/icon-generic` 与 `/services/icon/{name}` 都和 `/services/{unit}`
    /// 同层。matchit 静态段优先，两者不会被当成 unit 名——这条假设一旦不成立，
    /// 点开任何一个服务的详情都会变成取图标，所以要有用例盯着。
    #[tokio::test]
    async fn 图标路径不会被当成_unit_名() {
        let app = app().route("/api/v1/services/{unit}", get(|| async { "详情" }));

        let (status, _, _, body) = get_on(app.clone(), "/api/v1/services/icon-generic").await;
        assert_ne!(
            body.as_slice(),
            "详情".as_bytes(),
            "icon-generic 被当成了 unit 名（状态 {status}）"
        );

        let (_, _, _, body) = get_on(app.clone(), "/api/v1/services/Spooler.service").await;
        assert_eq!(
            body.as_slice(),
            "详情".as_bytes(),
            "普通 unit 名应当仍然走详情那条路由"
        );

        // 名字恰好是 `icon` 的服务仍然走详情：`/services/icon` 只有参数路由匹配得上
        let (_, _, _, body) = get_on(app, "/api/v1/services/icon").await;
        assert_eq!(body.as_slice(), "详情".as_bytes());
    }

    #[tokio::test]
    async fn 两条路由都登记进了_openapi() {
        let (_, api) = OpenApiRouter::<()>::new()
            .nest(
                "/api/v1",
                OpenApiRouter::new()
                    .routes(routes!(icon))
                    .routes(routes!(icon_generic))
                    .with_state(ServiceIcons::new()),
            )
            .split_for_parts();
        for path in [
            "/api/v1/services/icon/{name}",
            "/api/v1/services/icon-generic",
        ] {
            let item = api.paths.paths.get(path).unwrap_or_else(|| {
                panic!(
                    "{path} 没有被 utoipa 收集：{:?}",
                    api.paths.paths.keys().collect::<Vec<_>>()
                )
            });
            // 文档里必须写明 200 回的是 image/png，否则 `openapi-typescript`
            // 会按缺省的 application/json 生成类型，读文档的人也会被误导。
            let spec = serde_json::to_value(item).expect("路径项应当能序列化");
            assert!(
                spec["get"]["responses"]["200"]["content"]
                    .get("image/png")
                    .is_some(),
                "{path} 的 200 响应没有声明 image/png：{spec}"
            );
        }
    }
}
