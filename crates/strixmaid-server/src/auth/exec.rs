//! 请求的执行出口（`roadmap/01-worker-execution.md` §4.3）。
//!
//! # 服务端在这里只做一件事：选哪个 worker
//!
//! `design.md` §5.1 明确「不自建 RBAC」。授权全部外包给操作系统：worker 以登录
//! 用户的身份运行，polkit / journald ACL / 文件权限 / 内核信号权限各自裁决。
//! 因此本模块**不含任何「这个用户能不能做这件事」的判断**——它只按操作是读是写
//! 挑一个 worker，剩下的交给内核。
//!
//! 唯一的例外是 [`Privilege::Admin`] 且会话未提权：此时根本没有 admin worker
//! 可派，只能返回 403 `elevation_required` 让前端弹提权对话框。这不是权限判断，
//! 是「没有可用的执行者」。
//!
//! # 这是全部写操作的唯一出口
//!
//! `02-audit.md` 的审计写入点就设在这里——所有写操作都从 [`call`] / [`call_escalating`]
//! 过一遍，审计因此不会漏，也不用散落到每个路由里。
//!
//! 写入的规则见 [`should_audit`]，落库的字段见 [`describe`]。两条不变式值得单独说：
//!
//! - **一次用户操作只写一条记录。** [`call_escalating`] 内部可能调用两次 worker
//!   （先以用户身份、被内核拒后再以管理身份），但那是同一次用户操作的两次尝试，
//!   不是两件事。写两条会让「这台机器上今天重启了几次 nginx」这种最基本的问题
//!   数出错误的答案。做法是把审计写在**公开入口**里，真正干活的 [`call_inner`]
//!   一个字都不写。
//! - **读操作不审计。** 读的量比写大两三个数量级（列表页每几秒刷一次），
//!   全记下来只会把真正要看的那几条淹掉，还把 SQLite 的写入压力变成常态。
//!   「谁看过什么」要靠访问日志，不是审计日志。
//!
//! # 错误原样透传
//!
//! worker 里产生的 [`ApiError`]（错误码、`detail`、`can_retry_elevated`）
//! **一个字段都不重新包装**。polkit 拒绝的原因、journald 的可见性提示、
//! 内核的 `EPERM`，都要原封不动到达前端——中间层擅自改写会把可操作的错误
//! 变成「内部错误」。

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRef, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use strixmaid_core::session::{Session, WorkerHandle};
use strixmaid_types::{ApiError, ApiResult, ErrorCode, rpc};

use super::AuthState;
use super::audit::{self, Record};

/// 这次调用需要哪种身份的 worker。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    /// user worker：uid = 登录用户。读操作，以及「操作自己的东西」这类写操作。
    User,
    /// admin worker：uid = 0。改主机名 / 时区、电源、系统级 unit 操作等。
    /// 会话未提权时没有这个 worker，调用直接 403。
    Admin,
}

// ===========================================================================
// 请求来源
// ===========================================================================

/// 一次请求的来源地址，审计记录的 `remote_addr` 列。
///
/// # 为什么单独包一层，而不是直接传 `HeaderMap` + `SocketAddr`
///
/// 「要不要采信 `X-Forwarded-For`」是一处安全判断（见 [`audit::remote_addr`]），
/// 只能有一个实现。把判断的结果——一个字符串——封进本类型，调用点就不可能
/// 「顺手直接用 `ConnectInfo`」或者「顺手直接读 XFF」而绕过那个判断。
///
/// # 怎么拿到它
///
/// 它是一个 axum extractor，处理器加一个参数即可，**不需要中间件**：
/// `ConnectInfo` 由 `main.rs::serve` 的 `into_make_service_with_connect_info`
/// 放进 extensions，请求头本来就在 `Parts` 里，两样都在此处就地取用。
///
/// ```ignore
/// pub async fn signal(
///     State(auth): State<Arc<AuthState>>,
///     Extension(session): Extension<Session>,
///     origin: RequestOrigin,          // ← 只加这一个参数
///     Json(req): Json<SignalReq>,
/// ) -> ApiResult<StatusCode> {
///     exec::call_escalating_from::<_, ()>(&auth, &session, &origin, rpc::PROC_SIGNAL, params).await?;
///     Ok(StatusCode::NO_CONTENT)
/// }
/// ```
///
/// 尚未改成这样的路由走 [`call`] / [`call_escalating`]，审计照写，只是
/// `remote_addr` 为空——**缺一列地址，好过为了这一列去改所有处理器的签名**。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestOrigin(Option<String>);

impl RequestOrigin {
    /// 来源不可知（非 HTTP 上下文，或调用方还没接上 extractor）。
    pub const fn unknown() -> Self {
        RequestOrigin(None)
    }

    /// 按 `trusted_proxies` 判定来源地址。
    pub fn resolve(
        headers: &HeaderMap,
        peer: Option<SocketAddr>,
        trusted_proxies: &[String],
    ) -> Self {
        RequestOrigin(audit::remote_addr(headers, peer, trusted_proxies))
    }

    /// 给审计写入用。
    pub fn as_deref(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

impl<S> FromRequestParts<S> for RequestOrigin
where
    S: Send + Sync,
    Arc<AuthState>: FromRef<S>,
{
    /// 取不到地址不是错误：审计记录少一列，请求照常处理。
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = Arc::<AuthState>::from_ref(state);
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| *addr);
        Ok(RequestOrigin::resolve(
            &parts.headers,
            peer,
            &auth.trusted_proxies,
        ))
    }
}

// ===========================================================================
// 调用
// ===========================================================================

/// 在指定身份的 worker 里执行一次 RPC。
///
/// 来源地址未知，审计记录的 `remote_addr` 为空；处理器能拿到 [`RequestOrigin`]
/// 时请用 [`call_from`]。
///
/// 失败的几种情形：
///
/// | 情形 | 结果 |
/// |---|---|
/// | `Admin` 但会话未提权 | 403 `elevation_required` |
/// | worker 进程已死 | 401 `unauthenticated`，并使会话失效 |
/// | worker 返回错误 | 原样透传 |
pub async fn call<P, R>(
    auth: &AuthState,
    session: &Session,
    privilege: Privilege,
    method: &str,
    params: P,
) -> ApiResult<R>
where
    P: Serialize,
    R: DeserializeOwned,
{
    // 写操作走这里就会丢掉审计的来源地址列。读操作没有这个问题——它们根本不审计。
    // 不做成编译期约束（那要给读写分两套签名，把简单的读处理器也拖复杂），
    // 改为运行期出声：新增写端点时若忘了取 `RequestOrigin`，日志里会立刻看到。
    debug_assert!(
        !should_audit(privilege, method),
        "写操作 {method} 应当用 call_from 并传入 RequestOrigin，否则审计记录缺来源地址"
    );
    if should_audit(privilege, method) {
        tracing::warn!(method, "写操作未提供请求来源，审计记录的 remote_addr 将为空");
    }

    call_from(
        auth,
        session,
        &RequestOrigin::unknown(),
        privilege,
        method,
        params,
    )
    .await
}

/// 同 [`call`]，并把请求来源写进审计。
pub async fn call_from<P, R>(
    auth: &AuthState,
    session: &Session,
    origin: &RequestOrigin,
    privilege: Privilege,
    method: &str,
    params: P,
) -> ApiResult<R>
where
    P: Serialize,
    R: DeserializeOwned,
{
    let params = to_value(method, params)?;

    // 读操作在这里就返回，连一次 JSON 遍历都不做。
    if !should_audit(privilege, method) {
        return call_inner(auth, session, privilege, method, params).await;
    }

    let result = call_inner::<R>(auth, session, privilege, method, params.clone()).await;
    // 审计在**结果已定、返回之前**写：请求失败（包括未提权被挡在门外的 403）
    // 同样是一条要留痕的事实，`roadmap/02` §7 的验收标准把它算作一条记录。
    write_audit(
        auth,
        session,
        origin,
        method,
        &params,
        result.as_ref().map(|_| ()),
    )
    .await;
    result
}

/// 「先以自己的身份试，被内核拒了再用管理身份重试」。
///
/// `roadmap/01-worker-execution.md` §4.1 给 `proc.signal` / `proc.renice` 定的规则：
/// **向自己的进程发信号是普通用户的正当操作，不该要求提权**。因此先走 user worker；
/// 只有内核真的回了 `EPERM`（映射成 `PermissionDenied`）才考虑升级。
///
/// - 会话已提权 → 换 admin worker 重试一次；
/// - 未提权 → 把原错误返回，但带上 `can_retry_elevated`，前端就知道「提权能解决」。
///
/// 这条规则是 roadmap 在 `design.md` 之外新增的（§8 未决问题 1）。替代方案是
/// 一律走 admin worker，代价是用户杀自己的进程也要提权——那是明显的倒退。
/// 审计里会带上请求来源（`origin`）。这里没有不带 origin 的变体：
/// 走这条路的方法全是写操作，一律要留下来源地址。
pub async fn call_escalating_from<P, R>(
    auth: &AuthState,
    session: &Session,
    origin: &RequestOrigin,
    method: &str,
    params: P,
) -> ApiResult<R>
where
    P: Serialize,
    R: DeserializeOwned,
{
    let params = to_value(method, params)?;
    let (result, used_admin) = escalate(auth, session, method, params.clone()).await;

    // 审计条件与 [`should_audit`] 一致：写操作要记，**以管理身份做成的事也要记**
    // （「以 root 身份做的事没有『不值得记』的」）。后半条不能省——升级重试的方法
    // 已不全是写操作：日志读取也走这条路，而它一旦升级就是 root 在读系统日志。
    // 而且**只在这里写一次**——内部那两次 worker 调用是同一次用户操作的两次尝试，
    // 不是两件事。
    if is_write(method) || used_admin {
        write_audit(
            auth,
            session,
            origin,
            method,
            &params,
            result.as_ref().map(|_| ()),
        )
        .await;
    }
    result
}

/// 升级重试的本体。**不写审计**（见模块文档的「一次用户操作只写一条」）。
///
/// 返回的第二个值是「这次有没有真的用上管理身份」，调用方据此决定是否留痕：
/// 只看方法名不够——同一个方法既可能以用户身份做成，也可能升级后才做成。
async fn escalate<R>(
    auth: &AuthState,
    session: &Session,
    method: &str,
    params: Value,
) -> (ApiResult<R>, bool)
where
    R: DeserializeOwned,
{
    let first = call_inner::<R>(auth, session, Privilege::User, method, params.clone()).await;
    let Err(e) = first else {
        return (first, false);
    };
    if e.code != ErrorCode::PermissionDenied {
        return (Err(e), false);
    }

    if auth
        .sessions
        .admin_worker(&session.token_hash)
        .await
        .is_some()
    {
        return (
            call_inner(auth, session, Privilege::Admin, method, params).await,
            true,
        );
    }
    // 保留内核给出的原始说明（哪个进程、为什么拒），只补上「提权可解」这个信号。
    (Err(e.retry_elevated()), false)
}

/// 选 worker、发 RPC、解析结果。**不写审计**：它会被 [`escalate`] 调用两次。
async fn call_inner<R>(
    auth: &AuthState,
    session: &Session,
    privilege: Privilege,
    method: &str,
    params: Value,
) -> ApiResult<R>
where
    R: DeserializeOwned,
{
    let worker = worker_for(auth, session, privilege, method).await?;

    let value = worker.call(method, params).await?;

    serde_json::from_value(value).map_err(|e| {
        ApiError::internal(format!("无法解析 {method} 的返回值")).with_detail(e.to_string())
    })
}

/// 取这次调用该用的 worker。
///
/// 单独抽出来是因为**终端**用不了 [`call`]：`term.open` 的应答附带一个 fd，
/// 得走 `WorkerHandle::call_with_fds`，绕不过泛型的 `R: DeserializeOwned`。
/// 但「选哪个 worker」以及「worker 没了怎么办」这两条规则必须和普通调用完全一致——
/// 复制一份迟早会与这里长得不一样，而不一样的那一份就是 bug。
///
/// `method` 只用于出错时的 detail，方便从日志定位是谁触发的。
pub async fn worker_for(
    auth: &AuthState,
    session: &Session,
    privilege: Privilege,
    method: &str,
) -> ApiResult<Arc<WorkerHandle>> {
    let worker = match privilege {
        Privilege::User => auth.sessions.user_worker(&session.token_hash).await,
        Privilege::Admin => match auth.sessions.admin_worker(&session.token_hash).await {
            Some(w) => Some(w),
            None => return Err(elevation_required()),
        },
    };

    // user worker 不在 = 会话的执行者没了。会话本身已经没有意义，直接作废，
    // 让客户端重新登录；留着它只会让后续每个请求都失败得莫名其妙。
    let Some(worker) = worker else {
        auth.sessions.logout(&session.token_hash).await;
        return Err(ApiError::unauthenticated("会话的 worker 已退出，请重新登录")
            .with_detail(format!("method={method}")));
    };
    Ok(worker)
}

/// 参数序列化。失败是本进程的 bug（请求根本没发出去），不写审计——
/// 审计记的是「对系统做了什么」，而这里什么都没发生。
fn to_value<P: Serialize>(method: &str, params: P) -> ApiResult<Value> {
    serde_json::to_value(params).map_err(|e| {
        ApiError::internal(format!("无法序列化 {method} 的参数")).with_detail(e.to_string())
    })
}

/// 需要管理访问。
///
/// `can_retry_elevated` 为 true：提权之后重试**就能成功**，前端据此弹提权对话框
/// 而不是显示一条死路。
fn elevation_required() -> ApiError {
    ApiError::new(
        ErrorCode::ElevationRequired,
        "该操作需要管理访问（尚未启用）",
    )
    .with_detail("启用管理访问后重试；这一步会要求你再输一次密码")
    .retry_elevated()
}

// ===========================================================================
// 审计
// ===========================================================================

/// `roadmap/01-worker-execution.md` §4.1 表里标「写」的全部方法。
///
/// 写成表而不是「按方法名前缀猜」：新增一个方法时，忘了登记的后果是**审计里
/// 悄悄少一类操作**，而这种缺失没有任何外部症状，只有事后追查时才发现。
/// 表放在这里，评审新方法时一眼能看到自己漏了什么。
const WRITE_METHODS: &[&str] = &[
    rpc::HOST_SET_HOSTNAME,
    rpc::HOST_SET_TIMEZONE,
    rpc::HOST_POWER,
    rpc::PROC_SIGNAL,
    rpc::PROC_RENICE,
    rpc::SERVICE_ACTION,
];

/// 可以当作 `target` 的参数名，按优先级排列。
///
/// 目标单独成列而不是埋在 `params` 的 JSON 里，是为了让「这台机器上谁动过
/// nginx.service」变成一次索引查询，而不是全表扫 JSON。
const TARGET_KEYS: &[&str] = &["unit", "pid", "hostname", "timezone"];

fn is_write(method: &str) -> bool {
    WRITE_METHODS.contains(&method)
}

/// 这次调用要不要写审计。
///
/// 两个条件取并集，各自堵一个漏洞：
///
/// - **方法是写操作**：`service.action` 在 `scope = user` 时走的是 user worker
///   （用户操作自己的 unit 不需要 root），只看 `privilege` 会把这类成功的写操作
///   整个漏掉；
/// - **用了管理身份**：将来新增的 admin 方法哪怕忘了登记进 [`WRITE_METHODS`]，
///   也仍然会留痕。以 root 身份做的事没有「不值得记」的。
fn should_audit(privilege: Privilege, method: &str) -> bool {
    privilege == Privilege::Admin || is_write(method)
}

/// 审计记录里与这次调用有关的三个字段。
#[derive(Debug, PartialEq, Eq)]
struct Described {
    /// 落 `action` 列。
    action: String,
    /// 落 `target` 列。
    target: Option<String>,
    /// 落 `params` 列：**去掉 target 之后**剩下的部分。
    params: Option<Value>,
}

/// 把 RPC 方法名与参数拆成审计的 `action` / `target` / `params`。
///
/// # 为什么 `service.action` 要展开成 `service.<action>`
///
/// 不展开的话，审计表里每一次启动、停止、屏蔽都长成同一个 `service.action`，
/// 想知道「谁 mask 过这个 unit」就必须逐条解析 `params` 的 JSON——按动作过滤
/// （`AuditFilter::action`）与按前缀统计全都失效。`design.md` §8 给的例子
/// 也正是 `service.start` 这种粒度。展开后 `action` 里已经有了动作，
/// 就把它从 `params` 里删掉，同一件事不记两遍。
fn describe(method: &str, params: &Value) -> Described {
    let mut obj: Map<String, Value> = match params {
        Value::Object(m) => m.clone(),
        // 无参方法（`()` 序列化成 null）与非对象参数：没有可拆的字段。
        _ => Map::new(),
    };

    let mut action = method.to_owned();
    if method == rpc::SERVICE_ACTION
        && let Some(v) = obj.remove("action")
    {
        match v.as_str().filter(|s| is_action_token(s)).map(str::to_owned) {
            Some(a) => action = format!("service.{a}"),
            // 形状不对（客户端传了非法值、或将来改成了对象）：退回原样，
            // 宁可 action 粗一点，也不能让审计里出现一个编造的动作名。
            None => {
                obj.insert("action".to_owned(), v);
            }
        }
    }

    let target = TARGET_KEYS
        .iter()
        .copied()
        .find(|k| obj.get(*k).is_some_and(is_scalar))
        .and_then(|k| obj.remove(k))
        .and_then(|v| scalar_to_string(&v));

    Described {
        action,
        target,
        params: if obj.is_empty() {
            None
        } else {
            Some(Value::Object(obj))
        },
    }
}

/// 动作名只允许小写标识符：它会被拼进 `action` 列，而那一列是查询条件。
/// 参数在类型层是 `UnitAction` 枚举，走不到非法值；这里防的是将来有人
/// 把方法参数改成自由字符串。
fn is_action_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

fn is_scalar(v: &Value) -> bool {
    matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

fn scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 错误落进 `detail` 列。
///
/// 带上 `ApiError::detail`：polkit 的 action id、`systemctl` 的 stderr、内核的
/// errno 才是事后排查真正要看的东西，只留一句面向用户的话等于把线索丢掉。
/// 这两个字段都不可能含凭据——`design.md` §5.3 对 `ApiError::message` 有同样的约束。
///
/// 认证路由（`routes/auth.rs`）复用它，两处的 `detail` 列因此长得一样。
/// 它的正经归宿是 `auth::audit`，等那个文件下次改动时搬过去。
pub(crate) fn error_detail(e: &ApiError) -> String {
    match &e.detail {
        Some(d) => format!("{}: {}", e.message, d),
        None => e.message.clone(),
    }
}

/// 写一条审计。失败只记日志，不影响请求结果（`roadmap/02` §4.2）。
async fn write_audit(
    auth: &AuthState,
    session: &Session,
    origin: &RequestOrigin,
    method: &str,
    params: &Value,
    result: Result<(), &ApiError>,
) {
    let described = describe(method, params);
    let mut rec = Record::new(&described.action, audit::outcome_of(&result));
    if let Some(t) = described.target {
        rec = rec.target(t);
    }
    if let Some(p) = described.params {
        rec = rec.params(p);
    }
    if let Err(e) = result {
        rec = rec.detail(error_detail(e));
    }
    audit::record(auth.sessions.store(), session, origin.as_deref(), rec).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use serde_json::json;
    use strixmaid_core::session::{
        ClientMeta, HelperConn, HelperLauncher, SessionError, SessionManager, SessionManagerConfig,
    };
    use strixmaid_core::store::{AuditFilter, AuditOutcome, Store};
    use strixmaid_types::auth::AuthUser;
    use strixmaid_types::service::{UnitAction, UnitScope};

    // ------------------------------------------------------------ 纯函数

    #[test]
    fn service_action_展开成_service_具体动作() {
        let params = serde_json::to_value(rpc::UnitActionParams {
            scope: UnitScope::System,
            unit: "nginx.service".into(),
            action: UnitAction::Restart,
        })
        .unwrap();

        let d = describe(rpc::SERVICE_ACTION, &params);
        assert_eq!(
            d.action, "service.restart",
            "不展开的话审计里所有服务操作长得一模一样，按动作过滤直接失效"
        );
        assert_eq!(d.target.as_deref(), Some("nginx.service"));
        // action 已经在 action 列里，不再重复进 params；unit 也被提成 target
        assert_eq!(d.params, Some(json!({ "scope": "system" })));
    }

    #[test]
    fn 每个_unit_action_都能展开() {
        for (action, expect) in [
            (UnitAction::Start, "service.start"),
            (UnitAction::Stop, "service.stop"),
            (UnitAction::Reload, "service.reload"),
            (UnitAction::Enable, "service.enable"),
            (UnitAction::Mask, "service.mask"),
            (UnitAction::Unmask, "service.unmask"),
        ] {
            let params = serde_json::to_value(rpc::UnitActionParams {
                scope: UnitScope::User,
                unit: "app.service".into(),
                action,
            })
            .unwrap();
            assert_eq!(describe(rpc::SERVICE_ACTION, &params).action, expect);
        }
    }

    #[test]
    fn 非法的动作名不进_action_列() {
        // action 列是查询条件，绝不能由外部字符串直接拼成
        for bad in [json!("re start"), json!("../etc"), json!(3), json!(null)] {
            let params = json!({ "scope": "system", "unit": "a.service", "action": bad });
            let d = describe(rpc::SERVICE_ACTION, &params);
            assert_eq!(d.action, rpc::SERVICE_ACTION, "{bad} 不该被展开");
            assert!(
                d.params.as_ref().is_some_and(|p| p.get("action").is_some()),
                "退回原样时，动作要留在 params 里，不能丢"
            );
        }
    }

    #[test]
    fn target_从参数里提出来并从_params_删掉() {
        let d = describe(rpc::PROC_SIGNAL, &json!({ "pid": 1234, "signal": "term" }));
        assert_eq!(d.action, "proc.signal");
        assert_eq!(d.target.as_deref(), Some("1234"));
        assert_eq!(d.params, Some(json!({ "signal": "term" })));

        let d = describe(
            rpc::HOST_SET_HOSTNAME,
            &json!({ "hostname": "web-02", "pretty_hostname": "网站二号" }),
        );
        assert_eq!(d.target.as_deref(), Some("web-02"));
        assert_eq!(d.params, Some(json!({ "pretty_hostname": "网站二号" })));

        // 只有 target、没有别的参数时 params 为空，不写一个 `{}` 进库
        let d = describe(rpc::HOST_SET_TIMEZONE, &json!({ "timezone": "Asia/Shanghai" }));
        assert_eq!(d.target.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(d.params, None);

        // 无参方法
        let d = describe(rpc::HOST_INFO, &Value::Null);
        assert_eq!(d.action, "host.info");
        assert_eq!(d.target, None);
        assert_eq!(d.params, None);
    }

    #[test]
    fn 读操作不审计_写操作与管理身份都审计() {
        for m in [rpc::HOST_INFO, rpc::PROC_LIST, rpc::LOG_QUERY, rpc::SERVICE_LIST] {
            assert!(
                !should_audit(Privilege::User, m),
                "{m} 是读操作，量比写大两三个数量级，记下来只会淹掉真正要看的记录"
            );
            assert!(should_audit(Privilege::Admin, m), "{m} 用了管理身份就该留痕");
        }
        for m in WRITE_METHODS {
            assert!(should_audit(Privilege::User, m), "{m} 是写操作");
        }
        // scope=user 的 service.action 走 user worker，只看 privilege 会漏掉
        assert!(should_audit(Privilege::User, rpc::SERVICE_ACTION));
    }

    // ------------------------------------------------------------ 落库

    /// 起不来的 helper：会话里没有任何 worker，`call` 因此走「worker 已退出」分支。
    /// 审计要测的是「写几条、写了什么」，与 worker 里发生了什么无关。
    struct BrokenLauncher;
    impl HelperLauncher for BrokenLauncher {
        fn launch(&self) -> BoxFuture<'_, Result<HelperConn, SessionError>> {
            Box::pin(async { Err(SessionError::HelperUnavailable("测试：没有 helper".into())) })
        }
    }

    async fn state() -> Arc<AuthState> {
        let store = Store::open_in_memory().await.unwrap();
        let cfg = SessionManagerConfig {
            elevate_groups: vec!["wheel".to_owned()],
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
        AuthState::new(sessions, Vec::new())
    }

    /// 一个不在 `SessionManager` 里的会话快照：它没有 worker，
    /// 因此每次 `call` 都会在「取 worker」这一步失败——正是我们要的确定性。
    fn session(elevated: bool) -> Session {
        Session {
            token_hash: "hash-for-test".into(),
            node: "local".into(),
            user: AuthUser {
                uid: 1000,
                gid: 1000,
                username: "alice".into(),
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

    async fn entries(auth: &AuthState) -> Vec<strixmaid_core::store::AuditEntry> {
        auth.sessions
            .store()
            .audit_query(&AuditFilter::default())
            .await
            .unwrap()
            .entries
    }

    /// 日志读取也走升级路径之后，最容易踩的坑是「顺手把读也审计了」。
    /// 读的量比写大两三个数量级，全记下来只会把真正要看的记录淹掉——
    /// 只有**真的用上管理身份**那一次才留痕，未升级的读不留。
    #[tokio::test]
    async fn 未升级的日志读取不写审计() {
        let auth = state().await;
        let s = session(false);
        let origin = RequestOrigin(Some("203.0.113.5:44444".into()));

        let r: ApiResult<()> = call_escalating_from(
            &auth,
            &s,
            &origin,
            rpc::LOG_QUERY,
            serde_json::json!({"limit": 50}),
        )
        .await;
        assert!(r.is_err(), "会话没有 worker，这次调用必然失败");
        assert!(
            entries(&auth).await.is_empty(),
            "未升级的读操作不该进审计表"
        );
    }

    #[tokio::test]
    async fn 一次_call_escalating_只产生一条记录() {
        let auth = state().await;
        let s = session(false);
        let origin = RequestOrigin(Some("203.0.113.5:44444".into()));

        let r: ApiResult<()> = call_escalating_from(
            &auth,
            &s,
            &origin,
            rpc::PROC_SIGNAL,
            rpc::SignalParams {
                pid: 4242,
                signal: strixmaid_types::process::SignalName::Term,
            },
        )
        .await;
        assert!(r.is_err(), "会话没有 worker，这次调用必然失败");

        let rows = entries(&auth).await;
        assert_eq!(
            rows.len(),
            1,
            "call_escalating 内部可能调两次 worker，但那是同一次用户操作"
        );
        let e = &rows[0];
        assert_eq!(e.action, "proc.signal");
        assert_eq!(e.target.as_deref(), Some("4242"));
        assert_eq!(e.username, "alice");
        assert_eq!(e.uid, Some(1000));
        assert_eq!(e.remote_addr.as_deref(), Some("203.0.113.5:44444"));
        assert_eq!(e.result, AuditOutcome::Error);
        assert!(e.detail.is_some(), "失败要写清为什么");
    }

    #[tokio::test]
    async fn 未提权访问管理操作_记一条_denied() {
        let auth = state().await;
        let s = session(false);

        // 写操作一律走 call_from：`call` 是读操作专用的，用它写会触发
        // 那条「审计缺来源地址」的断言。这里用 unknown() 是因为本用例
        // 关心的是审计内容，不是地址解析（那有 audit::remote_addr 的专门用例）。
        let r: ApiResult<()> = call_from(
            &auth,
            &s,
            &RequestOrigin::unknown(),
            Privilege::Admin,
            rpc::HOST_SET_HOSTNAME,
            serde_json::json!({ "hostname": "web-02" }),
        )
        .await;
        assert_eq!(r.unwrap_err().code, ErrorCode::ElevationRequired);

        let rows = entries(&auth).await;
        assert_eq!(rows.len(), 1, "被挡在门外也是一次要留痕的尝试");
        assert_eq!(rows[0].action, "host.set_hostname");
        assert_eq!(rows[0].target.as_deref(), Some("web-02"));
        assert_eq!(
            rows[0].result,
            AuditOutcome::Denied,
            "「被拒绝」不能和「出错了」混成一类"
        );
        assert_eq!(rows[0].remote_addr, None, "没接 extractor 的路由缺这一列");
    }

    #[tokio::test]
    async fn 读操作一条记录都不写() {
        let auth = state().await;
        let s = session(false);

        let _: ApiResult<Value> = call(
            &auth,
            &s,
            Privilege::User,
            rpc::PROC_LIST,
            serde_json::json!({}),
        )
        .await;
        let _: ApiResult<Value> =
            call(&auth, &s, Privilege::User, rpc::HOST_INFO, Value::Null).await;

        assert!(entries(&auth).await.is_empty());
    }

    #[tokio::test]
    async fn 内层调用不写审计() {
        // 「一次操作一条记录」靠的就是这个：审计只写在公开入口，
        // 内层的 call_inner 被 escalate 调用两次也不会多出记录。
        let auth = state().await;
        let s = session(false);
        let _: ApiResult<Value> = call_inner(
            &auth,
            &s,
            Privilege::Admin,
            rpc::HOST_POWER,
            serde_json::json!({ "action": "reboot" }),
        )
        .await;
        assert!(entries(&auth).await.is_empty());
    }

    #[test]
    fn 需要提权的错误是可重试的_403() {
        let e = elevation_required();
        assert_eq!(e.code, ErrorCode::ElevationRequired);
        assert_eq!(e.code.http_status(), 403);
        assert!(
            e.can_retry_elevated,
            "前端要靠这个标志决定弹提权对话框还是显示死路"
        );
        assert!(e.detail.is_some(), "要告诉用户下一步做什么");
    }
}
