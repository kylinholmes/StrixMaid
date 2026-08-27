//! `/api/v1/terminals/*` —— PTY 终端的生命周期管理（`design.md` §9.1「终端」组）。
//!
//! 终端的**字节流**不走这里，走 [`crate::ws::terminal`] 的独立 WS。本模块只管开、列、
//! 改尺寸、关四件事。
//!
//! # 身份：谁来决定，以及为什么不能是 worker
//!
//! `roadmap/03-terminal.md` §4.2 的规则：
//!
//! | `CreateTerminalReq.user` | 走哪个 worker |
//! |---|---|
//! | `None` 或等于会话用户 | user worker（**不需要提权**） |
//! | 其他用户 | admin worker，**要求会话已提权**，否则 403 `elevation_required` |
//!
//! 「以自己的身份开 shell」等价于该用户 SSH 登录到这台机器，本来就是他的权利，
//! 因此不要求提权（Q20 的结论）。
//!
//! 判断放在**这里**而不是 worker 里：worker 只按「收到的是哪条连接」决定身份，
//! 自己不看 `user` 参数做准入判断。两处都判就是两套鉴权，而两套鉴权迟早会不一致——
//! 不一致的那一侧就是提权漏洞（`design.md` §5.1）。
//!
//! # 为什么本模块要自己写审计
//!
//! `roadmap/02-audit.md` §4.1 说审计写入点只在 [`crate::auth::exec`] 的调用出口与认证路由。
//! 终端是**第三个**写入点，这是刻意的例外：`term.open` 的应答附带一个 fd，必须走
//! `WorkerHandle::call_with_fds`，而 `exec::call` 的签名（`R: DeserializeOwned`）
//! 天生带不了 fd。把 fd 塞进 `exec` 会让每个普通读请求都背上一个用不到的 fd 通道，
//! 代价比在这里多写两处审计大得多。
//!
//! 例外仅限 `terminal.open` / `terminal.close` 两个动作，且仍然满足 §7 的
//! 「一次用户操作恰好一条记录」——`term.*` 没有登记进 `exec` 的 `WRITE_METHODS`，
//! 不会有第二处再记一遍。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use strixmaid_core::session::Session;
use strixmaid_core::store::{AuditOutcome, Store};
use strixmaid_core::terminal::{CloseReason, TerminalRegistry};
use strixmaid_types::rpc::TermOpenParams;
use strixmaid_types::terminal::{CreateTerminalReq, CreateTerminalResp, ResizeReq, TerminalInfo};
use strixmaid_types::{ApiError, ErrorCode};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege, RequestOrigin};
use crate::auth::{audit, audit::Record};
use crate::error::ApiResult;

/// 终端路由的状态。
#[derive(Clone)]
pub struct TerminalState {
    /// 注册表（core）。
    pub registry: Arc<TerminalRegistry>,
    /// 选 worker 与作废会话都要它。
    pub auth: Arc<AuthState>,
    /// 审计要写库。见模块文档「为什么本模块要自己写审计」。
    pub store: Store,
}

/// [`RequestOrigin`] 这个 extractor 要从路由状态里取出 [`AuthState`]
/// （它需要 `trusted_proxies` 才能判定该不该采信 `X-Forwarded-For`）。
/// 没有这个实现，带 `origin` 参数的处理器根本不满足 axum 的 `Handler` 约束。
impl axum::extract::FromRef<TerminalState> for Arc<AuthState> {
    fn from_ref(st: &TerminalState) -> Self {
        st.auth.clone()
    }
}

impl TerminalState {
    pub fn new(registry: Arc<TerminalRegistry>, auth: Arc<AuthState>, store: Store) -> Self {
        TerminalState {
            registry,
            auth,
            store,
        }
    }
}

/// 构建终端路由。挂到 `/api/v1` 之下（路径已含 `/terminals` 前缀）。
pub fn router(state: TerminalState) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(create_terminal))
        .routes(routes!(list_terminals))
        .routes(routes!(delete_terminal))
        .routes(routes!(resize_terminal))
        .with_state(state)
}

/// 新建终端
///
/// 在会话用户的 worker 里开一个 PTY 并启动 shell。返回的 `id` 用于拼
/// `WS /ws/terminal/{id}`，**只在本会话内有效**。
#[utoipa::path(
    post,
    path = "/terminals",
    tag = "terminals",
    request_body = CreateTerminalReq,
    security(("bearer" = [])),
    responses(
        (status = 201, description = "已创建", body = CreateTerminalResp),
        (status = 400, description = "shell 不合法（不存在、或不在 /etc/shells 里）", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "指定了其他用户但会话未提权", body = ApiError),
        (status = 409, description = "本会话的终端数已达上限", body = ApiError),
    ),
)]
pub async fn create_terminal(
    State(st): State<TerminalState>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Json(req): Json<CreateTerminalReq>,
) -> ApiResult<(StatusCode, Json<CreateTerminalResp>)> {
    let privilege = privilege_for(&session, req.user.as_deref())?;

    // 尺寸：创建时给一个能用的默认值，浏览器附着后立刻用 FitAddon 量出真实值再 resize。
    // 不让客户端在创建时传尺寸，是因为那时它还没渲染终端，量出来的必然是错的。
    let params = TermOpenParams {
        shell: req.shell.clone(),
        user: req.user.clone(),
        cols: DEFAULT_COLS,
        rows: DEFAULT_ROWS,
    };

    let worker = exec::worker_for(&st.auth, &session, privilege, strixmaid_types::rpc::TERM_OPEN)
        .await
        .inspect_err(|e| {
            // 选 worker 就失败（典型是未提权）也是一次要留痕的尝试。
            tracing::debug!(error = %e.message, "开终端前置检查失败");
        });

    let result = match worker {
        Ok(w) => st.registry.open(&session.token_hash, &w, params).await,
        Err(e) => Err(e),
    };

    audit_open(&st, &session, &origin, &req, privilege, &result).await;

    let info = result?;
    Ok((StatusCode::CREATED, Json(CreateTerminalResp { id: info.id })))
}

/// 终端列表
///
/// 只返回**本会话**的终端。别的会话的终端既看不到也用不了。
#[utoipa::path(
    get,
    path = "/terminals",
    tag = "terminals",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "本会话的终端", body = Vec<TerminalInfo>),
        (status = 401, description = "未认证", body = ApiError),
    ),
)]
pub async fn list_terminals(
    State(st): State<TerminalState>,
    Extension(session): Extension<Session>,
) -> ApiResult<Json<Vec<TerminalInfo>>> {
    Ok(Json(st.registry.list_for(&session.token_hash)))
}

/// 关闭终端
///
/// 向 worker 发 `term.close`，`SIGHUP` 进程组并回收 PTY。附着中的 WS 会随之关闭。
#[utoipa::path(
    delete,
    path = "/terminals/{id}",
    tag = "terminals",
    params(("id" = String, Path, description = "终端 id")),
    security(("bearer" = [])),
    responses(
        (status = 204, description = "已关闭"),
        (status = 401, description = "未认证", body = ApiError),
        (status = 404, description = "终端不存在或已关闭", body = ApiError),
    ),
)]
pub async fn delete_terminal(
    State(st): State<TerminalState>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let result = st
        .registry
        .close(&session.token_hash, &id, CloseReason::Deleted)
        .await;

    audit::record(
        &st.store,
        &session,
        origin.as_deref(),
        Record::new("terminal.close", audit::outcome_of(&result.as_ref().map(|_| ())))
            .target(id.clone())
            .detail(CloseReason::Deleted.as_str()),
    )
    .await;

    result?;
    Ok(StatusCode::NO_CONTENT)
}

/// 改终端尺寸
///
/// 与 WS 里的 `{"t":"resize"}` 完全等价，供没有附着 WS 时使用
/// （`roadmap/03-terminal.md` §4.4）。**不写审计**：改窗口大小不是对系统做了什么，
/// 把它记下来只会把真正要看的记录淹掉。
#[utoipa::path(
    post,
    path = "/terminals/{id}/resize",
    tag = "terminals",
    params(("id" = String, Path, description = "终端 id")),
    request_body = ResizeReq,
    security(("bearer" = [])),
    responses(
        (status = 204, description = "已生效"),
        (status = 400, description = "行列数必须大于 0", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 404, description = "终端不存在或已关闭", body = ApiError),
    ),
)]
pub async fn resize_terminal(
    State(st): State<TerminalState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    Json(req): Json<ResizeReq>,
) -> ApiResult<StatusCode> {
    st.registry
        .resize(&session.token_hash, &id, req.cols, req.rows)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ===========================================================================
// 内部
// ===========================================================================

/// 创建时的默认尺寸。浏览器附着后会立刻用实测值 resize 覆盖它。
const DEFAULT_COLS: u16 = 80;
/// 见 [`DEFAULT_COLS`]。
const DEFAULT_ROWS: u16 = 24;

/// 判定这次开终端该用哪个 worker（`roadmap/03-terminal.md` §4.2）。
///
/// **这是本模块唯一的准入判断，也是唯一该有的那一处。** 判断结果只体现为
/// 「投给哪个 worker」，worker 拿到之后不再判一次（见模块文档）。
pub(crate) fn privilege_for(
    session: &Session,
    want_user: Option<&str>,
) -> Result<Privilege, ApiError> {
    let Some(want) = want_user else {
        return Ok(Privilege::User);
    };
    // 显式写自己的用户名与不写，语义上是同一件事。
    if want == session.user.username {
        return Ok(Privilege::User);
    }
    if !session.elevated {
        return Err(ApiError::new(
            ErrorCode::ElevationRequired,
            format!("以 {want} 的身份开终端需要管理访问"),
        )
        .with_detail("先通过 POST /auth/elevate 提权，再重试"));
    }
    Ok(Privilege::Admin)
}

/// 写一条 `terminal.open`。
///
/// `target` 是**实际的目标用户**而不是请求里那个可能为 `None` 的字段：
/// 事后翻审计的人关心的是「开了谁的 shell」，不是「请求里写没写」。
async fn audit_open(
    st: &TerminalState,
    session: &Session,
    origin: &RequestOrigin,
    req: &CreateTerminalReq,
    privilege: Privilege,
    result: &Result<TerminalInfo, ApiError>,
) {
    let target = req
        .user
        .clone()
        .unwrap_or_else(|| session.user.username.clone());

    let mut params = serde_json::Map::new();
    if let Some(shell) = &req.shell {
        params.insert("shell".into(), shell.clone().into());
    }
    if privilege == Privilege::Admin {
        // 让「这是一个提权终端」在审计里一眼可见，不必去比对 target 和 actor。
        params.insert("elevated".into(), true.into());
    }
    if let Ok(info) = result {
        params.insert("id".into(), info.id.clone().into());
        params.insert("uid".into(), info.uid.into());
        // 实际启动的 shell：请求里没写时这是唯一能知道开了什么的地方。
        params.insert("resolved_shell".into(), info.shell.clone().into());
    }

    let outcome = match result {
        Ok(_) => AuditOutcome::Ok,
        Err(e) => audit::outcome_of(&Err(e)),
    };
    let mut rec = Record::new("terminal.open", outcome)
        .target(target)
        .params(serde_json::Value::Object(params));
    if let Err(e) = result {
        rec = rec.detail(e.message.clone());
    }
    audit::record(&st.store, session, origin.as_deref(), rec).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    use strixmaid_core::session::ClientMeta;
    use strixmaid_types::auth::AuthUser;

    fn session(username: &str, elevated: bool) -> Session {
        Session {
            token_hash: "hash-for-test".into(),
            node: "local".into(),
            user: AuthUser {
                uid: 1000,
                gid: 1000,
                username: username.into(),
                groups: vec!["wheel".into()],
            },
            elevated,
            elevated_ts: None,
            authed_ts: 0,
            created_ts: 0,
            last_active_ts: 0,
            meta: ClientMeta {
                user_agent: None,
                remote_addr: None,
            },
            session_opened: false,
        }
    }

    #[test]
    fn 不指定用户时走_user_worker() {
        let s = session("alice", false);
        assert_eq!(privilege_for(&s, None).unwrap(), Privilege::User);
    }

    #[test]
    fn 指定自己时不需要提权() {
        // 前端把当前用户名填进去是很自然的写法，不该因此被要求提权——
        // 那等于让用户为「开自己的 shell」付出管理密码。
        let s = session("alice", false);
        assert_eq!(privilege_for(&s, Some("alice")).unwrap(), Privilege::User);
    }

    #[test]
    fn 指定他人且未提权时被挡下() {
        let s = session("alice", false);
        let e = privilege_for(&s, Some("root")).unwrap_err();
        assert_eq!(e.code, ErrorCode::ElevationRequired);
        assert!(e.message.contains("root"), "错误信息要说清是谁：{}", e.message);
    }

    #[test]
    fn 指定他人且已提权时走_admin_worker() {
        let s = session("alice", true);
        assert_eq!(privilege_for(&s, Some("root")).unwrap(), Privilege::Admin);
    }

    #[test]
    fn 提权与否不改变开自己终端的路径() {
        // 已提权的会话开自己的终端仍然该走 user worker：走 admin worker 会让
        // shell 以 root 起来再 setuid 回去，多一次不必要的特权经过。
        let s = session("alice", true);
        assert_eq!(privilege_for(&s, None).unwrap(), Privilege::User);
        assert_eq!(privilege_for(&s, Some("alice")).unwrap(), Privilege::User);
    }
}
