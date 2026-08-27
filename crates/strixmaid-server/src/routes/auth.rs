//! `/api/v1/auth/*` —— §9.1 的六个认证端点，challenge-response 协议见 §5.2。
//!
//! 全部薄壳：DTO 在 `strixmaid-types::auth`，状态机在 `strixmaid-core::session`。
//! 本文件不碰任何明文凭据：`AuthRespondReq.responses` 里的 `Zeroizing<String>`
//! 按值 move 进 IPC 消息，不复制、不打印。
//!
//! # 审计写入点 2（`roadmap/02-audit.md` §4.1）
//!
//! 登录成功 / 失败、提权成功 / 失败、登出各写一条。这里是**认证事件唯一的
//! 写入点**——写操作那一半在 [`crate::auth::exec`] 里。
//!
//! 三条规矩：
//!
//! 1. **一次认证尝试只写一条。** PAM 是多轮对话，`status = "more"` 的那些轮
//!    什么都不写——一次登录不该在审计里变成三行。只有终局（拿到 token、或者
//!    被拒）才落一条。
//! 2. **绝不写 `params`。** `design.md` §5.3：审计不得含凭据，认证事件只记
//!    用户名与结果。`Zeroizing<String>` 连 `Serialize` 都没实现，编译期就到不了
//!    审计；但「顺手把整个请求体 `format!("{:?}")` 进 detail」这类绕过是人写得出来的，
//!    所以这里干脆一个字段都不给认证事件填 `params`。
//! 3. **失败要分清「被拒」还是「出错」。** 见 [`auth_outcome`]。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Extension, FromRef, State};
use axum::http::{HeaderMap, StatusCode, header::USER_AGENT};
use strixmaid_core::session::{ClientMeta, ElevateOutcome, LoginOutcome, Session};
use strixmaid_core::store::AuditOutcome;
use strixmaid_types::auth::{
    AuthOutcome, AuthRespondReq, AuthStartReq, AuthStartResp, SessionInfo,
};
use strixmaid_types::ipc::IpcPromptResponse;
use strixmaid_types::{ApiError, ErrorCode};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::audit::{self, Record};
use crate::auth::exec::{RequestOrigin, error_detail};
use crate::auth::{AuthState, CurrentSession};
use crate::error::ApiResult;

/// 登录事件的 `action`。成功与失败共用一个动作名，靠 `result` 列区分——
/// 分成两个动作名的话，「这个账号今天被尝试登录了多少次」要查两遍再相加。
const ACTION_LOGIN: &str = "auth.login";
/// 提权事件的 `action`。
const ACTION_ELEVATE: &str = "auth.elevate";
/// 登出事件的 `action`。
const ACTION_LOGOUT: &str = "auth.logout";

/// 构建 auth 路由树。**自带状态**，返回的是 `OpenApiRouter<()>`，
/// 调用方直接 `.merge()` 进 `/api/v1`，不需要 `AppState` 参与。
pub fn router(state: Arc<AuthState>) -> OpenApiRouter<()> {
    let state = AuthRoutes {
        auth: state,
    };
    OpenApiRouter::new()
        .routes(routes!(start))
        .routes(routes!(respond))
        .routes(routes!(elevate_start))
        .routes(routes!(elevate_respond))
        .routes(routes!(logout))
        .routes(routes!(session))
        .with_state(state)
}

/// 认证路由的状态：[`AuthState`] 加一张登录用户名的便签。
///
/// `pub` 只是因为处理器签名里出现了它（处理器要被 `utoipa` 收集，必须是 `pub`）；
/// 字段与构造都不对外，调用方仍然只给 [`router`] 一个 [`AuthState`]。
#[derive(Clone)]
pub struct AuthRoutes {
    auth: Arc<AuthState>,
}

/// 让 [`CurrentSession`] 与 [`RequestOrigin`] 这两个 extractor 照常工作。
impl FromRef<AuthRoutes> for Arc<AuthState> {
    fn from_ref(st: &AuthRoutes) -> Arc<AuthState> {
        Arc::clone(&st.auth)
    }
}

/// 把 HTTP 层的回应搬进 IPC 消息：逐项 move，`Zeroizing<String>` 不产生副本。
fn into_ipc(req: AuthRespondReq) -> Vec<IpcPromptResponse> {
    req.responses
        .into_iter()
        .map(|r| IpcPromptResponse {
            id: r.id,
            value: r.value,
        })
        .collect()
}

/// 登录时记录的客户端信息。`ConnectInfo` 只有在 `into_make_service_with_connect_info`
/// 下才有，因此是可选的。
fn client_meta(headers: &HeaderMap, addr: Option<SocketAddr>) -> ClientMeta {
    ClientMeta {
        user_agent: headers
            .get(USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        remote_addr: addr.map(|a| a.to_string()),
    }
}

/// 认证事件的结果映射。
///
/// 不能直接用 [`audit::outcome_of`]：PAM 拒绝在 API 层是 401 `unauthenticated`，
/// 而那个函数会把它算成 `error`。审计里这两类必须分开——「密码输错了」是系统
/// 按预期工作，「helper 起不来」是这台机器坏了。混成一类，事后就分不清
/// 一串失败记录是有人在爆破，还是 PAM 那天崩了。
fn auth_outcome(e: &ApiError) -> AuditOutcome {
    match e.code {
        ErrorCode::Unauthenticated | ErrorCode::PermissionDenied | ErrorCode::ElevationRequired => {
            AuditOutcome::Denied
        }
        _ => AuditOutcome::Error,
    }
}

/// 认证事件的记录模板。**永远不带 `params`**（见模块文档第 2 条）。
fn auth_record<'a>(action: &'a str, err: Option<&ApiError>) -> Record<'a> {
    match err {
        // detail 里是 PAM 的错误文本（`pam_strerror`），不含凭据。
        Some(e) => Record::new(action, auth_outcome(e)).detail(error_detail(e)),
        None => Record::new(action, AuditOutcome::Ok),
    }
}

/// 会话尚未建立时的认证事件（登录失败、以及登录成功之前的那一刻）。
async fn record_login(st: &AuthRoutes, username: &str, origin: &RequestOrigin, err: &ApiError) {
    audit::record_anonymous(
        st.auth.sessions.store(),
        &st.auth.sessions.config().node_id,
        username,
        origin.as_deref(),
        auth_record(ACTION_LOGIN, Some(err)),
    )
    .await;
}

/// 会话已经存在时的认证事件（登录成功、提权、登出）。
async fn record_session(
    st: &AuthRoutes,
    session: &Session,
    origin: &RequestOrigin,
    action: &str,
    err: Option<&ApiError>,
) {
    audit::record(
        st.auth.sessions.store(),
        session,
        origin.as_deref(),
        auth_record(action, err),
    )
    .await;
}

/// 开始登录
///
/// 拉起一个 PAM 对话，返回第一轮提示。`session` 是短生命周期的认证会话 id（60 秒），
/// 不是登录 token。
#[utoipa::path(
    post,
    path = "/auth/start",
    tag = "auth",
    request_body = AuthStartReq,
    responses(
        (status = 200, description = "第一轮提示", body = AuthStartResp),
        (status = 400, description = "用户名为空", body = ApiError),
        (status = 401, description = "PAM 在第一轮就拒绝了（如用户不存在）", body = ApiError),
        (status = 501, description = "PAM helper 不可用，无法登录", body = ApiError),
    ),
)]
pub async fn start(
    State(st): State<AuthRoutes>,
    headers: HeaderMap,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(req): Json<AuthStartReq>,
) -> ApiResult<Json<AuthStartResp>> {
    let username = req.username.trim().to_owned();
    // 空用户名不写审计：请求连 PAM 都没碰到，没有「一次登录尝试」这回事。
    if username.is_empty() {
        return Err(ApiError::invalid_request("用户名不能为空").into());
    }
    let addr = connect_info.map(|Extension(ConnectInfo(a))| a);
    let origin = RequestOrigin::resolve(&headers, addr, &st.auth.trusted_proxies);

    match st
        .auth
        .sessions
        .login_start(&username, client_meta(&headers, addr))
        .await
    {
        Ok((session, prompts)) => {
            // 对话刚开始，还没有结果可记；先把用户名记在便签上，
            // 等 /auth/respond 出结果时才知道该往审计里写谁。
            Ok(Json(AuthStartResp { session, prompts }))
        }
        Err(e) => {
            // 第一轮就被拒（用户不存在、账户锁定）：这次尝试到此为止，
            // 后面不会再有 /auth/respond，所以这一条就是它唯一的记录。
            let e = ApiError::from(e);
            record_login(&st, &username, &origin, &e).await;
            Err(e.into())
        }
    }
}

/// 回应登录提示
///
/// 返回 `status = "complete"`（含 Bearer token）或 `status = "more"`（继续追问）。
/// 认证失败是 401。
#[utoipa::path(
    post,
    path = "/auth/respond",
    tag = "auth",
    request_body = AuthRespondReq,
    responses(
        (status = 200, description = "认证完成或需要更多回应", body = AuthOutcome),
        (status = 401, description = "认证失败", body = ApiError),
        (status = 404, description = "认证会话不存在或已超时", body = ApiError),
    ),
)]
pub async fn respond(
    State(st): State<AuthRoutes>,
    origin: RequestOrigin,
    Json(req): Json<AuthRespondReq>,
) -> ApiResult<Json<AuthOutcome>> {
    let pending_id = req.session.clone();
    // **必须在 respond 之前取**：终局（成功或失败）时 core 会把这条 pending 摘掉，
    // 之后再问就拿不到用户名了。登录失败那条审计正需要它。
    let attempted = st.auth.sessions.pending_username(&pending_id).await;

    match st.auth.sessions.login_respond(&pending_id, into_ipc(req)).await {
        Ok(LoginOutcome::Complete { token, session }) => {
            // 用 PAM 返回的规范用户名，不是请求里那个：`Alice` 与 `alice`
            // 可能是同一个账户，审计里必须落成同一个名字才能按用户过滤。
            record_session(&st, &session, &origin, ACTION_LOGIN, None).await;
            Ok(Json(AuthOutcome::Complete {
                token,
                user: session.user,
            }))
        }
        // 多轮对话中间态：一次登录不该在审计里变成三行，等终局再写。
        Ok(LoginOutcome::More {
            pending_id,
            prompts,
        }) => Ok(Json(AuthOutcome::More {
            session: pending_id,
            prompts,
        })),
        Err(e) => {
            let e = ApiError::from(e);
            match &attempted {
                Some(username) => record_login(&st, username, &origin, &e).await,
                // pending id 是伪造或早已过期的，PAM 根本没收到任何凭据。
                // 没有用户名可记，写一条 username 为空的记录只会污染审计表。
                None => tracing::debug!("认证会话不存在，跳过审计"),
            }
            Err(e.into())
        }
    }
}

/// 开始提权
///
/// 对当前会话再走一次 PAM 对话（sudo 语义：用自己的密码；传别的用户名则是 su 语义）。
/// 成功后会话获得 admin worker，`elevated = true`。
#[utoipa::path(
    post,
    path = "/auth/elevate/start",
    tag = "auth",
    request_body = AuthStartReq,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "第一轮提示", body = AuthStartResp),
        (status = 401, description = "未登录", body = ApiError),
        (status = 501, description = "PAM helper 不可用", body = ApiError),
    ),
)]
pub async fn elevate_start(
    State(st): State<AuthRoutes>,
    current: CurrentSession,
    origin: RequestOrigin,
    Json(req): Json<AuthStartReq>,
) -> ApiResult<Json<AuthStartResp>> {
    let username = req.username.trim();
    let username = if username.is_empty() {
        None
    } else {
        Some(username)
    };
    match st
        .auth
        .sessions
        .elevate_start(&current.session.token_hash, username)
        .await
    {
        Ok((session, prompts)) => Ok(Json(AuthStartResp { session, prompts })),
        Err(e) => {
            // 第一轮就被拒（不在 elevate_groups、helper 不可用）：后面不会有
            // /auth/elevate/respond，这一条就是这次提权尝试唯一的记录。
            let e = ApiError::from(e);
            record_session(&st, &current.session, &origin, ACTION_ELEVATE, Some(&e)).await;
            Err(e.into())
        }
    }
}

/// 回应提权提示
///
/// 与 `/auth/respond` 同形。成功时 `token` 回传当前 token（不轮换）。
#[utoipa::path(
    post,
    path = "/auth/elevate/respond",
    tag = "auth",
    request_body = AuthRespondReq,
    security(("bearer" = [])),
    responses(
        (status = 200, description = "提权完成或需要更多回应", body = AuthOutcome),
        (status = 401, description = "未登录或认证失败", body = ApiError),
        (status = 403, description = "无法创建 admin worker（helper 不是 root）", body = ApiError),
        (status = 404, description = "认证会话不存在或已超时", body = ApiError),
    ),
)]
pub async fn elevate_respond(
    State(st): State<AuthRoutes>,
    current: CurrentSession,
    origin: RequestOrigin,
    Json(req): Json<AuthRespondReq>,
) -> ApiResult<Json<AuthOutcome>> {
    let pending_id = req.session.clone();
    match st
        .auth
        .sessions
        .elevate_respond(&pending_id, into_ipc(req))
        .await
    {
        Ok(ElevateOutcome::Complete(session)) => {
            // 记的是提权后的会话快照，`elevated` 列因此为真——审计里能直接看出
            // 这条记录之后该会话进入了管理状态。
            record_session(&st, &session, &origin, ACTION_ELEVATE, None).await;
            Ok(Json(AuthOutcome::Complete {
                token: current.token.0,
                user: session.user,
            }))
        }
        // 中间态，同 /auth/respond：等终局再写。
        Ok(ElevateOutcome::More {
            pending_id,
            prompts,
        }) => Ok(Json(AuthOutcome::More {
            session: pending_id,
            prompts,
        })),
        Err(e) => {
            let e = ApiError::from(e);
            record_session(&st, &current.session, &origin, ACTION_ELEVATE, Some(&e)).await;
            Err(e.into())
        }
    }
}

/// 登出
///
/// 终止本会话的 worker、关闭 PAM 会话、删除会话记录。幂等：会话已不存在时同样 204。
#[utoipa::path(
    post,
    path = "/auth/logout",
    tag = "auth",
    security(("bearer" = [])),
    responses(
        (status = 204, description = "已登出"),
        (status = 401, description = "未登录", body = ApiError),
    ),
)]
pub async fn logout(
    State(st): State<AuthRoutes>,
    current: CurrentSession,
    origin: RequestOrigin,
) -> ApiResult<StatusCode> {
    st.auth.sessions.logout(&current.session.token_hash).await;
    // 登出没有失败这一说（幂等），所以只有 ok 这一种结果。
    // 用登出**之前**的会话快照：那才是「谁登出了、当时是不是提权状态」。
    record_session(&st, &current.session, &origin, ACTION_LOGOUT, None).await;
    Ok(StatusCode::NO_CONTENT)
}

/// 当前会话
///
/// 本会话在当前节点上的认证状态；每次调用都会刷新活跃时间。
#[utoipa::path(
    get,
    path = "/auth/session",
    tag = "auth",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "会话信息", body = SessionInfo),
        (status = 401, description = "未登录或已过期", body = ApiError),
    ),
)]
pub async fn session(current: CurrentSession) -> Json<SessionInfo> {
    Json(current.session.info())
}

#[cfg(test)]
mod tests {
    //! 只验证 HTTP 层：状态码、错误体形状、OpenAPI 收集、中间件。
    //! 完整状态机（登录 → 提权 → 超时 → 登出）在 core 的 `session::tests` 里用假 helper 测。

    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use axum::routing::get;
    use futures::future::BoxFuture;
    use strixmaid_core::session::{
        HelperConn, HelperLauncher, SessionError, SessionManager, SessionManagerConfig,
    };
    use strixmaid_core::store::Store;
    use strixmaid_types::ErrorCode;
    use tower::ServiceExt as _;
    use utoipa::Modify as _;

    use crate::auth::SECURITY_SCHEME;

    /// 起不来的 helper：让 /auth/start 走到 501（能力不可用 = 这台机器坏了）。
    struct BrokenLauncher;
    impl HelperLauncher for BrokenLauncher {
        fn launch(&self) -> BoxFuture<'_, Result<HelperConn, SessionError>> {
            Box::pin(async {
                Err(SessionError::HelperUnavailable("测试：没有 helper".into()))
            })
        }
    }

    /// 一上来就拒绝的 helper：模拟 PAM 在第一轮就否掉（用户不存在、账户锁定）。
    struct DenyingLauncher;
    impl HelperLauncher for DenyingLauncher {
        fn launch(&self) -> BoxFuture<'_, Result<HelperConn, SessionError>> {
            Box::pin(async { Err(SessionError::AuthFailed("测试：认证失败".into())) })
        }
    }

    async fn state() -> Arc<AuthState> {
        state_with(Arc::new(BrokenLauncher)).await
    }

    async fn state_with(launcher: Arc<dyn HelperLauncher>) -> Arc<AuthState> {
        let store = Store::open_in_memory().await.unwrap();
        let cfg = SessionManagerConfig {
            elevate_groups: strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            pam_service: "strixmaid-test".into(),
            worker_exe: None,
            open_session: false,
            idle_timeout: std::time::Duration::from_secs(60),
            elevated_idle_timeout: std::time::Duration::from_secs(30),
            pending_timeout: std::time::Duration::from_secs(60),
            node_id: "local".into(),
        };
        let sessions = SessionManager::new(store, cfg, launcher).await.unwrap();
        AuthState::new(sessions, Vec::new())
    }

    async fn audit_rows(auth: &AuthState) -> Vec<strixmaid_core::store::AuditEntry> {
        auth.sessions
            .store()
            .audit_query(&strixmaid_core::store::AuditFilter::default())
            .await
            .unwrap()
            .entries
    }

    async fn body_json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    fn post(path: &str, body: &str, token: Option<&str>) -> Request<Body> {
        let mut b = Request::post(path).header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        b.body(Body::from(body.to_string())).unwrap()
    }

    #[tokio::test]
    async fn openapi_收集到全部六个端点() {
        let st = state().await;
        let (_, doc) = OpenApiRouter::new()
            .nest("/api/v1", router(st))
            .split_for_parts();
        let mut paths: Vec<&str> = doc.paths.paths.keys().map(String::as_str).collect();
        paths.sort_unstable();
        assert_eq!(
            paths,
            vec![
                "/api/v1/auth/elevate/respond",
                "/api/v1/auth/elevate/start",
                "/api/v1/auth/logout",
                "/api/v1/auth/respond",
                "/api/v1/auth/session",
                "/api/v1/auth/start",
            ]
        );
        // 受保护端点带 bearer 安全要求
        let session_path = &doc.paths.paths["/api/v1/auth/session"];
        let get = session_path.get.as_ref().unwrap();
        assert!(get.security.as_ref().is_some_and(|s| !s.is_empty()));
        // SecurityAddon 能把方案写进 components
        let mut doc = doc;
        crate::auth::SecurityAddon.modify(&mut doc);
        assert!(
            doc.components
                .unwrap()
                .security_schemes
                .contains_key(SECURITY_SCHEME)
        );
    }

    #[tokio::test]
    async fn start_在_helper_不可用时返回_501_能力_helper() {
        let st = state().await;
        let app = router(st).split_for_parts().0;
        let resp = app
            .oneshot(post("/auth/start", r#"{"username":"alice"}"#, None))
            .await
            .unwrap();
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(json["code"], "capability_unavailable");
        assert_eq!(json["capability"], "helper");
    }

    #[tokio::test]
    async fn start_空用户名_400_且_respond_未知会话_404() {
        let st = state().await;
        let app = router(st).split_for_parts().0;
        let resp = app
            .clone()
            .oneshot(post("/auth/start", r#"{"username":"  "}"#, None))
            .await
            .unwrap();
        assert_eq!(body_json(resp).await.0, StatusCode::BAD_REQUEST);

        let resp = app
            .oneshot(post(
                "/auth/respond",
                r#"{"session":"nope","responses":[{"id":0,"value":"x"}]}"#,
                None,
            ))
            .await
            .unwrap();
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json["code"], ErrorCode::NotFound.as_str());
    }

    #[tokio::test]
    async fn 受保护端点无_token_或坏_token_都是_401() {
        let st = state().await;
        let app = router(st).split_for_parts().0;
        for (path, token) in [
            ("/auth/session", None),
            ("/auth/session", Some("garbage")),
            ("/auth/logout", None),
            ("/auth/elevate/start", Some("garbage")),
        ] {
            let req = if path == "/auth/session" {
                let mut b = Request::get(path);
                if let Some(t) = token {
                    b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
                }
                b.body(Body::empty()).unwrap()
            } else {
                post(path, r#"{"username":"alice"}"#, token)
            };
            let resp = app.clone().oneshot(req).await.unwrap();
            let (status, json) = body_json(resp).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{path} {token:?}");
            assert_eq!(json["code"], "unauthenticated", "{path} {token:?}");
        }
    }

    #[tokio::test]
    async fn 中间件_protect_对已匹配路由返回_401_未匹配仍_404() {
        let st = state().await;
        let inner = axum::Router::new().route("/secret", get(|| async { "ok" }));
        let app = crate::auth::middleware::protect(inner, st);
        let resp = app
            .clone()
            .oneshot(Request::get("/secret").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let (status, json) = body_json(resp).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json["code"], "unauthenticated");
        let resp = app
            .oneshot(Request::get("/nothing").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ------------------------------------------------------------ 审计

    #[tokio::test]
    async fn 登录失败写一条_denied_且只记用户名() {
        let st = state_with(Arc::new(DenyingLauncher)).await;
        let app = router(st.clone()).split_for_parts().0;
        let resp = app
            .oneshot(post("/auth/start", r#"{"username":"alice"}"#, None))
            .await
            .unwrap();
        assert_eq!(body_json(resp).await.0, StatusCode::UNAUTHORIZED);

        let rows = audit_rows(&st).await;
        assert_eq!(rows.len(), 1, "一次登录尝试对应恰好一条记录");
        let e = &rows[0];
        assert_eq!(e.action, ACTION_LOGIN);
        assert_eq!(e.username, "alice", "失败时用请求里的用户名，那是唯一已知的信息");
        assert_eq!(
            e.result,
            AuditOutcome::Denied,
            "PAM 拒绝是「被拒」，不是「出错」"
        );
        assert_eq!(e.params, None, "认证事件绝不带 params（design.md §5.3）");
        assert_eq!(e.uid, None, "还没认证成功，没有 uid 可记");
        assert!(!e.elevated);
        assert!(
            e.detail.as_deref().is_some_and(|d| d.contains("测试：认证失败")),
            "detail 要带上 PAM 的错误文本"
        );
    }

    #[tokio::test]
    async fn helper_不可用记成_error_而不是_denied() {
        // 「密码输错了」与「这台机器坏了」混成一类，事后就分不清
        // 一串失败记录是有人在爆破还是 PAM 崩了。
        let st = state().await;
        let app = router(st.clone()).split_for_parts().0;
        let resp = app
            .oneshot(post("/auth/start", r#"{"username":"alice"}"#, None))
            .await
            .unwrap();
        assert_eq!(body_json(resp).await.0, StatusCode::NOT_IMPLEMENTED);

        let rows = audit_rows(&st).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].result, AuditOutcome::Error);
    }

    #[tokio::test]
    async fn 空用户名与未知会话都不写审计() {
        let st = state().await;
        let app = router(st.clone()).split_for_parts().0;

        // 连 PAM 都没碰到，没有「一次登录尝试」这回事
        app.clone()
            .oneshot(post("/auth/start", r#"{"username":"  "}"#, None))
            .await
            .unwrap();
        // 伪造的 pending id：PAM 没收到任何凭据，也没有用户名可记
        app.oneshot(post(
            "/auth/respond",
            r#"{"session":"nope","responses":[{"id":0,"value":"x"}]}"#,
            None,
        ))
        .await
        .unwrap();

        assert!(audit_rows(&st).await.is_empty());
    }

    #[tokio::test]
    async fn 审计里不可能出现凭据() {
        const SECRET: &str = "hunter2-绝密口令";

        let st = state_with(Arc::new(DenyingLauncher)).await;
        let app = router(st.clone()).split_for_parts().0;

        // 一次失败的登录（会写一条记录）+ 一次带着密码的 respond
        app.clone()
            .oneshot(post("/auth/start", r#"{"username":"alice"}"#, None))
            .await
            .unwrap();
        app.oneshot(post(
            "/auth/respond",
            &format!(r#"{{"session":"nope","responses":[{{"id":0,"value":"{SECRET}"}}]}}"#),
            None,
        ))
        .await
        .unwrap();

        let rows = audit_rows(&st).await;
        assert!(!rows.is_empty(), "要确实有记录，否则这个测试什么都没验证");
        let dump = serde_json::to_string(&rows).unwrap();
        assert!(!dump.contains(SECRET), "审计表里出现了明文口令：{dump}");
        assert!(!dump.contains("responses"), "请求体不该整个进审计：{dump}");
        for e in &rows {
            assert_eq!(e.params, None, "认证事件的 params 永远为空");
        }
    }
}
