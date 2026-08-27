//! `/api/v1/auth/*` —— §9.1 的六个认证端点，challenge-response 协议见 §5.2。
//!
//! 全部薄壳：DTO 在 `strixmaid-types::auth`，状态机在 `strixmaid-core::session`。
//! 本文件不碰任何明文凭据：`AuthRespondReq.responses` 里的 `Zeroizing<String>`
//! 按值 move 进 IPC 消息，不复制、不打印。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, StatusCode, header::USER_AGENT};
use strixmaid_core::session::{ClientMeta, ElevateOutcome, LoginOutcome};
use strixmaid_types::ApiError;
use strixmaid_types::auth::{
    AuthOutcome, AuthRespondReq, AuthStartReq, AuthStartResp, SessionInfo,
};
use strixmaid_types::ipc::IpcPromptResponse;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::{AuthState, CurrentSession};
use crate::error::ApiResult;

/// 构建 auth 路由树。**自带状态**，返回的是 `OpenApiRouter<()>`，
/// 调用方直接 `.merge()` 进 `/api/v1`，不需要 `AppState` 参与。
pub fn router(state: Arc<AuthState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(start))
        .routes(routes!(respond))
        .routes(routes!(elevate_start))
        .routes(routes!(elevate_respond))
        .routes(routes!(logout))
        .routes(routes!(session))
        .with_state(state)
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
    State(auth): State<Arc<AuthState>>,
    headers: HeaderMap,
    connect_info: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(req): Json<AuthStartReq>,
) -> ApiResult<Json<AuthStartResp>> {
    let username = req.username.trim();
    if username.is_empty() {
        return Err(ApiError::invalid_request("用户名不能为空").into());
    }
    let meta = client_meta(&headers, connect_info.map(|Extension(ConnectInfo(a))| a));
    let (session, prompts) = auth
        .sessions
        .login_start(username, meta)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AuthStartResp { session, prompts }))
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
    State(auth): State<Arc<AuthState>>,
    Json(req): Json<AuthRespondReq>,
) -> ApiResult<Json<AuthOutcome>> {
    let pending_id = req.session.clone();
    let outcome = auth
        .sessions
        .login_respond(&pending_id, into_ipc(req))
        .await
        .map_err(ApiError::from)?;
    Ok(Json(match outcome {
        LoginOutcome::Complete { token, session } => AuthOutcome::Complete {
            token,
            user: session.user,
        },
        LoginOutcome::More {
            pending_id,
            prompts,
        } => AuthOutcome::More {
            session: pending_id,
            prompts,
        },
    }))
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
    State(auth): State<Arc<AuthState>>,
    current: CurrentSession,
    Json(req): Json<AuthStartReq>,
) -> ApiResult<Json<AuthStartResp>> {
    let username = req.username.trim();
    let username = if username.is_empty() {
        None
    } else {
        Some(username)
    };
    let (session, prompts) = auth
        .sessions
        .elevate_start(&current.session.token_hash, username)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(AuthStartResp { session, prompts }))
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
    State(auth): State<Arc<AuthState>>,
    current: CurrentSession,
    Json(req): Json<AuthRespondReq>,
) -> ApiResult<Json<AuthOutcome>> {
    let pending_id = req.session.clone();
    let outcome = auth
        .sessions
        .elevate_respond(&pending_id, into_ipc(req))
        .await
        .map_err(ApiError::from)?;
    Ok(Json(match outcome {
        ElevateOutcome::Complete(session) => AuthOutcome::Complete {
            token: current.token.0,
            user: session.user,
        },
        ElevateOutcome::More {
            pending_id,
            prompts,
        } => AuthOutcome::More {
            session: pending_id,
            prompts,
        },
    }))
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
    State(auth): State<Arc<AuthState>>,
    current: CurrentSession,
) -> ApiResult<StatusCode> {
    auth.sessions.logout(&current.session.token_hash).await;
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

    /// 起不来的 helper：让 /auth/start 走到 501。
    struct BrokenLauncher;
    impl HelperLauncher for BrokenLauncher {
        fn launch(&self) -> BoxFuture<'_, Result<HelperConn, SessionError>> {
            Box::pin(async {
                Err(SessionError::HelperUnavailable("测试：没有 helper".into()))
            })
        }
    }

    async fn state() -> Arc<AuthState> {
        let store = Store::open_in_memory().await.unwrap();
        let cfg = SessionManagerConfig {
            pam_service: "strixmaid-test".into(),
            worker_exe: None,
            open_session: false,
            idle_timeout: std::time::Duration::from_secs(60),
            elevated_idle_timeout: std::time::Duration::from_secs(30),
            pending_timeout: std::time::Duration::from_secs(60),
            node_id: "local".into(),
        };
        let sessions = SessionManager::new(store, cfg, Arc::new(BrokenLauncher))
            .await
            .unwrap();
        AuthState::new(sessions)
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
}
