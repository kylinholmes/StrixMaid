//! `/api/v1/processes/*` —— 进程列表 / 详情 / 信号 / renice / 图标。
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
//!
//! # 为什么只有 [`icon`] 不经 worker
//!
//! 图标是**可执行映像自身的属性**，不是进程的、更不是用户的：同一个
//! `chrome.exe` 对所有登录用户都是同一张图。它与 `cwd` / `environ` / `fds`
//! 那几个「可见性由内核按 uid 裁决」的字段是两回事，没有「随用户而变」的语义
//! 可言，因此也没有必须落到 worker 里执行的理由。
//!
//! 反过来，放在主进程有两个实打实的好处：
//!
//! - **缓存能跨会话共用。** 缓存挂在 provider 实例上（`strixmaid_core::providers::process::icon`），
//!   而 worker 是一会话一个；放进 worker 就变成每个会话各热一遍，同一张图标
//!   在 N 个会话里要提取 N 次，正好与「别在服务刚起来时抢资源」相反。
//! - **预热能在服务启动时做。** worker 要等用户登录才存在，那时预热已经晚了。
//!
//! 代价要如实写明：主进程的身份（Windows 服务下是 LocalSystem）比登录用户宽，
//! 因此**理论上**能读到某个登录用户读不到的映像的图标。这一点之所以可以接受，
//! 是因为端点只对**正在运行的同名进程**给图标，而进程名本来就是全局可见的
//! （`GET /processes` 对所有会话给出同一张进程表），多出来的信息仅仅是那张
//! 32×32 的图，不含文件内容、路径或任何权限相关的事实。
//!
//! **鉴权不开特例**：图标端点与本模块其余端点一样挂在 `routes::api_v1` 的
//! `protected` 组里，缺少或无效的 `Authorization: Bearer` 一律 401。

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use strixmaid_core::providers::process::ProcProvider;
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
/// 两组路由的状态不同，因此分开建再 `merge`：
///
/// - 列表 / 详情 / 信号 / renice 的状态是 [`AuthState`]，处理器用它找本会话的 worker；
/// - [`icon`] 的状态是主进程自己的 [`ProcProvider`]，图标缓存挂在它上面（见该函数的文档）。
///
/// 两组**鉴权完全一致**：整棵 `/processes/*` 都在 `routes::api_v1` 的 `protected`
/// 组里，由 `protect_openapi` → `require_auth` 统一要求 `Authorization: Bearer`。
/// 图标端点没有任何特例。
pub fn router(auth: Arc<AuthState>, proc: ProcProvider) -> OpenApiRouter<()> {
    let icons = OpenApiRouter::new()
        .routes(routes!(icon))
        .with_state(proc);
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(detail))
        .routes(routes!(signal))
        .routes(routes!(renice))
        .with_state(auth)
        .merge(icons)
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

/// 进程图标
///
/// 按**映像名**取一张 32×32 的 PNG（Windows 上就是 `chrome.exe` 这种带后缀的名字，
/// 即 `ProcessSummary.name`）。取不到一律 404——系统进程没有图标是常态，不是异常。
#[utoipa::path(
    get,
    path = "/processes/icon/{name}",
    tag = "processes",
    security(("bearer" = [])),
    params(("name" = String, Path, description = "映像名，取自 `ProcessSummary.name`，如 `chrome.exe`")),
    responses(
        // body 写 `String` 而不是 `Vec<u8>`：后者会被 utoipa 展开成
        // 「整数数组」，`openapi-typescript` 据此生成 `number[]`，与真实的
        // 二进制响应对不上。utoipa 5 的响应宏不支持写 `format: binary`，
        // `String` 是最接近的表达（OpenAPI 里二进制体惯例就是 string）。
        (status = 200, description = "32×32 的 PNG 图标（响应体是原始 PNG 字节）", content_type = "image/png", body = String),
        (status = 400, description = "名字不合法（含路径分隔符、`..` 或过长）", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 404, description = "没有正在运行的同名进程、取不到它的可执行文件或图标，\
                                      或本平台不提供进程图标（Linux / macOS）", body = ApiError),
    ),
)]
pub async fn icon(
    State(proc): State<ProcProvider>,
    // 不用这个值，但**必须**提取：它是「本端点与其余 /processes/* 一样需要会话」
    // 这件事在代码里的凭据。日后若有人把这条路由挪出受保护组，这里会立刻失败，
    // 而不是悄悄变成一个匿名可读的端点。
    Extension(_session): Extension<Session>,
    Path(name): Path<String>,
) -> ApiResult<Response> {
    let png = proc.icon_png(&name).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            // 浏览器端再缓存 5 分钟，与服务端的 TTL 同一档，省掉大部分往返。
            // 不加 `private`：响应带 `Authorization` 时共享缓存本来就不得存储
            // （RFC 9111 §3.5），再写一遍只是噪声。
            (header::CACHE_CONTROL, "max-age=300"),
        ],
        png.as_slice().to_vec(),
    )
        .into_response())
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

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use strixmaid_core::providers::process::icon;
    use strixmaid_core::session::{ClientMeta, Session};
    use strixmaid_types::ErrorCode;
    use strixmaid_types::auth::AuthUser;
    use tower::ServiceExt as _;

    /// 造一个会话。图标端点不看它的任何字段，只要求它在——存在本身就是
    /// 「这条路由在受保护组里」的凭据。
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

    /// 只挂图标那一条路由（其余三个要 worker，不在本文件的测试范围里）。
    fn app() -> Router {
        let (router, _) = OpenApiRouter::new()
            .nest(
                "/api/v1",
                OpenApiRouter::new()
                    .routes(routes!(icon))
                    .with_state(ProcProvider::new()),
            )
            .split_for_parts();
        router
    }

    /// 发一次请求，返回状态码、Content-Type、Cache-Control 与响应体。
    async fn get(uri: &str) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
        let mut req = Request::get(uri).body(Body::empty()).unwrap();
        // 生产里由认证中间件注入，这里手工塞进去。
        req.extensions_mut().insert(session());
        let resp = app().oneshot(req).await.unwrap();
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

    fn 错误码(body: &[u8]) -> String {
        let v: serde_json::Value = serde_json::from_slice(body).expect("错误体应当是 JSON");
        v["code"].as_str().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn 名字不合法时返回_400() {
        // 分隔符、`..`、超长，三类都要挡下。`/` 会被路由本身吃掉（多一段路径，
        // 匹配不到），所以这里用百分号编码把它送进处理器。
        for bad in [
            "%2E%2E%2Fpasswd",          // ../passwd
            "C%3A%5CWindows%5Ccalc.exe", // C:\Windows\calc.exe
            "a%2Fb.exe",                // a/b.exe
            "%2E%2E",                   // ..
        ] {
            let (status, _, _, body) = get(&format!("/api/v1/processes/icon/{bad}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad} 应当被拒绝");
            assert_eq!(错误码(&body), ErrorCode::InvalidRequest.as_str());
        }

        let 超长 = "a".repeat(300);
        let (status, _, _, body) = get(&format!("/api/v1/processes/icon/{超长}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "超长名字应当被拒绝");
        assert_eq!(错误码(&body), ErrorCode::InvalidRequest.as_str());
    }

    #[tokio::test]
    async fn 不存在的名字返回_404() {
        let (status, _, _, body) =
            get("/api/v1/processes/icon/绝不会有这个进程_9f3a1c.exe").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(错误码(&body), ErrorCode::NotFound.as_str());
    }

    #[tokio::test]
    async fn 取到图标时是_png_且带缓存头() {
        if !icon::available() {
            eprintln!("本平台不提供进程图标，跳过");
            return;
        }
        // 本进程自己一定在进程表里；测试二进制未必带图标资源，因此再备一个
        // explorer.exe，两个都取不到就跳过。
        let mut 候选: Vec<String> = Vec::new();
        if let Some(me) = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        {
            候选.push(me);
        }
        候选.push("explorer.exe".to_owned());

        for name in &候选 {
            let (status, ct, cc, body) = get(&format!("/api/v1/processes/icon/{name}")).await;
            if status != StatusCode::OK {
                continue;
            }
            assert_eq!(ct.as_deref(), Some("image/png"));
            assert_eq!(cc.as_deref(), Some("max-age=300"), "浏览器侧也要缓存 5 分钟");
            assert_eq!(
                &body[..8],
                &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
                "响应体不是合法 PNG"
            );
            assert_eq!(&body[12..16], b"IHDR");
            let w = u32::from_be_bytes([body[16], body[17], body[18], body[19]]);
            let h = u32::from_be_bytes([body[20], body[21], body[22], body[23]]);
            assert_eq!((w, h), (icon::ICON_SIZE, icon::ICON_SIZE));
            return;
        }
        eprintln!("本机没有任何候选程序能取到图标，跳过");
    }

    #[tokio::test]
    async fn 路由登记进了_openapi() {
        let (_, api) = OpenApiRouter::<()>::new()
            .nest(
                "/api/v1",
                OpenApiRouter::new()
                    .routes(routes!(icon))
                    .with_state(ProcProvider::new()),
            )
            .split_for_parts();
        let path = api
            .paths
            .paths
            .get("/api/v1/processes/icon/{name}")
            .unwrap_or_else(|| {
                panic!(
                    "图标端点没有被 utoipa 收集：{:?}",
                    api.paths.paths.keys().collect::<Vec<_>>()
                )
            });
        // 文档里必须写明 200 回的是 image/png，否则 `openapi-typescript`
        // 会按缺省的 application/json 生成类型，读文档的人也会被误导。
        let spec = serde_json::to_value(path).expect("路径项应当能序列化");
        assert!(
            spec["get"]["responses"]["200"]["content"]
                .get("image/png")
                .is_some(),
            "200 响应没有声明 image/png：{spec}"
        );
    }
}
