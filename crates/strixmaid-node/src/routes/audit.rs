//! `/api/v1/audit` —— 审计日志查询与保留期清理（`docs/design.md` §8 / §9.1「审计」组，
//! `roadmap/02-audit.md` §4.3 / §4.4）。
//!
//! # 为什么这里有一处「服务端自己做的授权判断」
//!
//! `roadmap/01-worker-execution.md` 落地之后，服务端主进程**不再判断任何权限**：
//! 请求一律投递给以登录用户（或提权后的 root）身份运行的 worker，能不能做由操作系统
//! 说了算（`design.md` §5.1「授权外包给操作系统」）。本文件是这条规则**唯一的例外**，
//! 也是全服务端唯一一处基于会话状态（`session.elevated`）而非 worker 结果的判断。
//!
//! 例外的理由是审计表的位置：`audit_log` 在**主进程自己的 SQLite 库**里，不是文件系统上
//! 某个带 mode/ACL 的对象，也不是 systemd / journald 这类自带鉴权的系统接口。没有任何
//! OS 权限可以外包——把查询丢进 user worker 也没用，worker 根本碰不到这个库；而库文件
//! 归 root，用文件权限判断的结果永远是「谁都不能读」。于是只剩一个选择：在这里按会话
//! 是否提权判断，未提权返回 403 `elevation_required`（`design.md` §9.1：`GET /audit`
//! 需管理访问）。
//!
//! **这不是漏改的残留，请不要「顺手统一」掉它。** 反过来，也不要把这个例外扩散到别的
//! 端点：其它端点凡是想在主进程里判断权限的，都应该改成经 worker 执行。
//!
//! # 为什么分页按 `id DESC` 而不是 `ts DESC`
//!
//! `design.md` §8 已给出结论，这里复述理由，因为它直接决定了游标字段的选择：
//! `audit_log.id` 是 AUTOINCREMENT，与写入顺序**严格一致**且唯一；而 `ts` 只有秒级
//! 精度，同一秒内写入的多条记录顺序不定。若用 `ts` 做游标，翻页时同一秒的记录会重复
//! 出现或整段漏掉——审计日志恰恰最容易在同一秒里连着写好几条（一次操作 = 一条，
//! 而批量操作是常态）。`idx_audit_ts` 仍然有用，它服务的是 `since` / `until` 范围过滤。
//!
//! 对外的游标字段叫 `before_id`（[`AuditQuery::before_id`]），语义是「只要 id 严格小于
//! 它的记录」，把上一页的 `next_before_id` 原样传回即可。
//!
//! # 两套同名类型
//!
//! [`strixmaid_core::store`] 里的 `AuditEntry` / `AuditPage` 是**库表映射**，
//! [`strixmaid_types::audit`] 里的同名类型是 **API DTO**。二者刻意不合并：前者的
//! `params` 是一段 JSON 文本、`uid` 是 `i64`（SQLite 只有有符号整数），后者的 `params`
//! 是结构化的 `serde_json::Value`、`uid` 是 `u32`（与 `AuthUser::uid` 一致）。
//! 转换集中在本文件的 [`entry_to_dto`]。
//!
//! # 保留期清理
//!
//! [`spawn_prune_task`] 起一个每小时一次的后台任务，删掉早于 `audit.retention_days`
//! 的记录。它和查询端点是同一件事的两面（一个决定能看到什么、一个决定还留着什么），
//! 因此放在同一个文件里。

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Extension, Query, State};
use serde_json::Value;
use strixmaid_core::session::Session;
use strixmaid_core::store::{
    AuditEntry as StoredEntry, AuditFilter, AuditOutcome, AuditPage as StoredPage, Store,
    StoreError, now_unix,
};
use strixmaid_types::ApiError;
use strixmaid_types::audit::{AuditEntry, AuditPage, AuditQuery, AuditResult};
use tokio::task::JoinHandle;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::error::ApiResult;

/// 清理任务的执行间隔。
///
/// 一小时是刻意取的粗粒度：保留期以**天**为单位（最小 7 天），把清理跑得更勤既不会让
/// 数据更准，只会平白多占写连接——而写池只有一条连接，与审计写入本身共用。
pub const PRUNE_INTERVAL: Duration = Duration::from_secs(3600);

/// 审计路由的状态：一个存储句柄。
///
/// 这里**不需要** [`crate::auth::AuthState`]：本端点不经 worker（见模块文档），
/// 它要的只是读库的能力。会话由认证中间件放进 extensions，处理器直接取。
pub struct AuditState {
    store: Store,
}

impl AuditState {
    /// 包一层。
    pub fn new(store: Store) -> AuditState {
        AuditState { store }
    }
}

/// `GET /audit`，状态已注入。挂到 `/api/v1` 之下（路径已含 `/audit` 前缀）。
///
/// **必须挂在鉴权中间件之内**（`routes::mod` 的 `protected` 子树）：处理器用
/// `Extension<Session>` 取会话，没有中间件注入就取不到。
pub fn router(state: Arc<AuditState>) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(list_audit))
        .with_state(state)
}

/// 查询审计日志
///
/// **需要管理访问**：会话未提权时返回 403 `elevation_required`，前端应引导用户提权后重试
/// （错误体的 `retry_elevated` 为 `true`）。理由见模块文档——审计表在主进程库里，
/// 没有可外包给操作系统的权限。
///
/// 结果按 `id` 降序（新的在前）。翻页把上一页的 `next_before_id` 原样传回 `before_id`，
/// 并保持其余过滤条件不变；`next_before_id` 为空表示已到最旧一条。
/// `limit` 缺省 100、上限 1000，`0` 或越界都是 400。
/// 时间范围是左闭右开的 `[since, until)`。
#[utoipa::path(
    get,
    path = "/audit",
    tag = "audit",
    params(AuditQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "一页审计记录，按 id 降序", body = AuditPage),
        (status = 400, description = "参数不合法（limit 为 0 或超过 1000、since > until）", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 403, description = "会话未提权（`elevation_required`）", body = ApiError),
        (status = 500, description = "读取 audit_log 失败", body = ApiError),
    ),
)]
pub async fn list_audit(
    State(state): State<Arc<AuditState>>,
    Extension(session): Extension<Session>,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Json<AuditPage>> {
    // 授权判断只此一处，且刻意放在处理器最前面而不是中间件里：
    // 需要提权的端点目前只有这一个，为它单独加一层 `require_elevated` 中间件，
    // 会让「哪些路由要提权」这件事散到两个文件里，反而更难看清。
    if !session.elevated {
        return Err(ApiError::elevation_required("查询审计日志需要管理访问").into());
    }

    let filter = to_filter(&query)?;
    // StoreError 没有对外的错误码映射（它是实现细节：SQL 语法、连接池、迁移），
    // 一律归为 500，细节进 detail 与 journald，不向客户端解释库内部发生了什么。
    // 这与 `metrics::engine` 的做法一致。
    let page = state.store.audit_query(&filter).await.map_err(|e| {
        ApiError::internal("读取审计日志失败").with_detail(e.to_string())
    })?;
    Ok(Json(page_to_dto(page)))
}

// ===========================================================================
// 参数与 DTO 转换
// ===========================================================================

/// 把空串当作「没填」。
///
/// 表单里清空一个输入框，浏览器发出的是 `username=` 而不是把参数去掉。把它当成
/// 「用户名等于空串」会查出零条记录，看着像「没有匹配」，实际是过滤条件本身有问题。
fn non_empty(v: &Option<String>) -> Option<String> {
    v.as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// [`AuditQuery`]（API DTO）→ [`AuditFilter`]（存储层）。
///
/// `limit` 的越界处理与 `logs` 端点（`providers::log::normalize_limit`）保持一致：
/// **不静默夹取，而是 400**。夹取会让「我要 5000 条」的调用方拿到 1000 条却以为是全部，
/// 翻页逻辑随之出错；报错则一眼看得见。
fn to_filter(q: &AuditQuery) -> Result<AuditFilter, ApiError> {
    let limit = match q.limit {
        None => AuditFilter::DEFAULT_LIMIT,
        Some(0) => return Err(ApiError::invalid_request("limit 不能为 0")),
        Some(n) if i64::from(n) > AuditFilter::MAX_LIMIT => {
            return Err(ApiError::invalid_request(format!(
                "limit 超过上限 {}",
                AuditFilter::MAX_LIMIT
            )));
        }
        Some(n) => i64::from(n),
    };

    if let (Some(since), Some(until)) = (q.since, q.until)
        && since > until
    {
        return Err(ApiError::invalid_request("since 不能晚于 until"));
    }

    Ok(AuditFilter {
        // node_id 不开放给查询参数：MVP 里只有 `local` 一个节点（`design.md` §8），
        // 加一个恒等于常量的过滤器只是徒增 API 表面。多节点落地时再补。
        node_id: None,
        username: non_empty(&q.username),
        // `AuditQuery::action` 的文档语义是**前缀**（`service.` 命中全部服务操作），
        // 因此走 `action_prefix` 而不是精确匹配的 `action`。前缀是精确匹配的超集：
        // 填完整动作名 `service.restart` 依然只会命中它自己。
        action: None,
        action_prefix: non_empty(&q.action),
        result: q.result.map(result_to_outcome),
        since: q.since,
        until: q.until,
        cursor: q.before_id,
        limit,
    })
}

/// API 的结果枚举 → 存储层的结果枚举。
fn result_to_outcome(r: AuditResult) -> AuditOutcome {
    match r {
        AuditResult::Ok => AuditOutcome::Ok,
        AuditResult::Denied => AuditOutcome::Denied,
        AuditResult::Error => AuditOutcome::Error,
    }
}

/// 存储层的结果枚举 → API 的结果枚举。
fn outcome_to_result(o: AuditOutcome) -> AuditResult {
    match o {
        AuditOutcome::Ok => AuditResult::Ok,
        AuditOutcome::Denied => AuditResult::Denied,
        AuditOutcome::Error => AuditResult::Error,
    }
}

/// 库表行 → API DTO。
fn entry_to_dto(e: StoredEntry) -> AuditEntry {
    AuditEntry {
        id: e.id,
        ts: e.ts,
        node_id: e.node_id,
        username: e.username,
        // SQLite 只有有符号整数，库里的 uid 是 `i64`；DTO 用 `u32`（与 `AuthUser::uid`
        // 一致）。理论上装不下的值（负数、超过 u32）只可能来自手工改库，这时宁可当作
        // 「没有 uid」也不要让整条记录读不出来——审计记录的价值在于「还看得见」。
        uid: e.uid.and_then(|v| u32::try_from(v).ok()),
        elevated: e.elevated,
        action: e.action,
        target: e.target,
        params: e.params.map(parse_params),
        result: outcome_to_result(e.result),
        detail: e.detail,
        remote_addr: e.remote_addr,
    }
}

/// `params` 列是一段 JSON 文本，DTO 要的是结构化值。
///
/// 解析失败时退化成 JSON 字符串而不是丢弃或报错：这一列由写入方保证是 JSON，真出现
/// 非法内容说明写入侧有 bug，而**排查这个 bug 的人正是在读审计日志**——把原文原样交给
/// 他，比给他一个 500 或一个空字段有用得多。
fn parse_params(raw: String) -> Value {
    match serde_json::from_str::<Value>(&raw) {
        Ok(v) => v,
        Err(_) => Value::String(raw),
    }
}

/// 一页库表行 → 一页 DTO。
fn page_to_dto(page: StoredPage) -> AuditPage {
    AuditPage {
        entries: page.entries.into_iter().map(entry_to_dto).collect(),
        next_before_id: page.next_cursor,
    }
}

// ===========================================================================
// 保留期清理（roadmap/02 §4.4）
// ===========================================================================

/// 跑一轮清理，返回删除条数。
///
/// 边界按「此刻」算：删掉 `ts < now - retention_secs` 的记录。抽成独立函数是为了能被
/// 直接调用（测试里不必等一个小时的定时器），也让接线方在需要时可以手动触发一次。
pub async fn prune_once(store: &Store, retention_secs: i64) -> Result<u64, StoreError> {
    store.audit_prune(now_unix() - retention_secs).await
}

/// 起审计清理后台任务：每 [`PRUNE_INTERVAL`] 一次，删掉超出保留期的记录。
///
/// `retention_secs` 取 `config.audit.retention_secs()`。返回的 [`JoinHandle`] 通常
/// 直接丢弃——任务与进程同生共死。
pub fn spawn_prune_task(store: Store, retention_secs: i64) -> JoinHandle<()> {
    spawn_prune_task_every(store, retention_secs, PRUNE_INTERVAL)
}

/// 同 [`spawn_prune_task`]，但间隔可指定。
///
/// 存在的唯一理由是可测：定时逻辑若只能以「一小时」运行，就等于没被测过。
pub fn spawn_prune_task_every(
    store: Store,
    retention_secs: i64,
    interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(prune_loop(store, retention_secs, interval))
}

/// 清理循环。
async fn prune_loop(store: Store, retention_secs: i64, interval: Duration) {
    let mut ticker = tokio::time::interval(interval);
    // 进程被挂起（笔记本合盖、容器暂停）后错过的 tick 不补跑：清理是幂等的，
    // 补跑 N 次只是连着删 N 次空集合。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // 第一次 tick 立即返回，因此进程启动时就会清一次——否则一个每天重启的进程
        // 永远等不到第一个小时，保留期形同虚设。
        ticker.tick().await;
        match prune_once(&store, retention_secs).await {
            // 稳态下每小时都是 0 条，记 info 会把日志刷满，降到 debug。
            Ok(0) => tracing::debug!("审计清理：无超期记录"),
            Ok(n) => tracing::info!(removed = n, retention_secs, "审计清理完成"),
            // 清理失败不退出循环，更不该让服务挂掉：磁盘满或库被锁住是暂时的，
            // 下一轮多半就好了；而一个退出的清理任务不会有人发现。
            Err(e) => tracing::error!(error = %e, "审计清理失败，将在下一轮重试"),
        }
    }
}

#[cfg(test)]
mod tests {
    //! 只验证 HTTP 层与转换：提权判断、分页游标、参数校验、清理边界。
    //! `audit_query` / `audit_prune` 的 SQL 语义在 core 的 `store::tests` 里测。

    use super::*;

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use strixmaid_core::session::{ClientMeta, Session};
    use strixmaid_core::store::NewAuditEntry;
    use strixmaid_types::ErrorCode;
    use strixmaid_types::auth::AuthUser;
    use tower::ServiceExt as _;

    /// 造一个会话；`elevated` 是被测的关键位。
    fn session(elevated: bool) -> Session {
        Session {
            token_hash: "hash".into(),
            node: "local".into(),
            user: AuthUser {
                uid: 1000,
                gid: 1000,
                username: "alice".into(),
                groups: vec!["wheel".into()],
            },
            elevated,
            elevated_ts: elevated.then_some(1_700_000_000),
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

    /// 建库 + 写入 `n` 条记录（动作在 `service.start` / `auth.login` 间交替）。
    async fn store_with(n: usize) -> Store {
        let store = Store::open_in_memory().await.unwrap();
        for i in 0..n {
            let (action, outcome) = if i % 2 == 0 {
                ("service.start", AuditOutcome::Ok)
            } else {
                ("auth.login", AuditOutcome::Denied)
            };
            let entry = NewAuditEntry::new("local", "alice", action, outcome)
                .actor(1000, true)
                .target(format!("t{i}"))
                .params(format!(r#"{{"i":{i}}}"#));
            store.audit_write(&entry).await.unwrap();
        }
        store
    }

    /// 把 router 摊平成可 `oneshot` 的 `Router`。
    fn app(store: Store) -> Router {
        let (router, _) = OpenApiRouter::new()
            .nest("/api/v1", router(Arc::new(AuditState::new(store))))
            .split_for_parts();
        router
    }

    /// 发一次请求；会话直接塞进 extensions（生产里由认证中间件注入）。
    async fn get(store: Store, uri: &str, elevated: bool) -> (StatusCode, serde_json::Value) {
        let mut req = Request::get(uri).body(Body::empty()).unwrap();
        req.extensions_mut().insert(session(elevated));
        let resp = app(store).oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, json)
    }

    #[tokio::test]
    async fn 未提权返回_403_elevation_required() {
        let (status, body) = get(store_with(3).await, "/api/v1/audit", false).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], ErrorCode::ElevationRequired.as_str());
        // 前端据此弹提权对话框并重试。
        assert_eq!(body["can_retry_elevated"], true);
        // 未提权时不该泄露任何记录内容。
        assert!(body.get("entries").is_none());
    }

    #[tokio::test]
    async fn 已提权可查到记录且按_id_降序() {
        let (status, body) = get(store_with(5).await, "/api/v1/audit", true).await;
        assert_eq!(status, StatusCode::OK);
        let page: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.entries.len(), 5);
        let ids: Vec<i64> = page.entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![5, 4, 3, 2, 1], "必须按 id 降序");
        // 一页装得下时没有下一页。
        assert_eq!(page.next_before_id, None);
        // params 是解析后的 JSON 对象，不是字符串。
        assert_eq!(page.entries[0].params.as_ref().unwrap()["i"], 4);
        assert_eq!(page.entries[0].uid, Some(1000));
    }

    #[tokio::test]
    async fn 分页游标可连续翻到末页() {
        let store = store_with(5).await;

        let (_, body) = get(store.clone(), "/api/v1/audit?limit=2", true).await;
        let p1: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(p1.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![5, 4]);
        // 游标是本页最后一条的 id：下一页取 id 严格小于它的记录。
        assert_eq!(p1.next_before_id, Some(4));

        let (_, body) = get(store.clone(), "/api/v1/audit?limit=2&before_id=4", true).await;
        let p2: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(p2.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 2]);
        assert_eq!(p2.next_before_id, Some(2));

        let (_, body) = get(store.clone(), "/api/v1/audit?limit=2&before_id=2", true).await;
        let p3: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(p3.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![1]);
        // 末页没有下一页游标。
        assert_eq!(p3.next_before_id, None);
    }

    #[tokio::test]
    async fn 过滤条件生效() {
        let store = store_with(6).await;

        // action 是前缀匹配：`service.` 命中全部服务操作。
        let (_, body) = get(store.clone(), "/api/v1/audit?action=service.", true).await;
        let page: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.entries.len(), 3);
        assert!(page.entries.iter().all(|e| e.action == "service.start"));

        // 填完整动作名同样工作（前缀是精确匹配的超集）。
        let (_, body) = get(store.clone(), "/api/v1/audit?action=auth.login", true).await;
        let page: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.entries.len(), 3);

        // result 过滤：denied 是排查权限问题的主要入口。
        let (_, body) = get(store.clone(), "/api/v1/audit?result=denied", true).await;
        let page: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.entries.len(), 3);
        assert!(page.entries.iter().all(|e| e.result == AuditResult::Denied));

        // 空串等于没填，不是「等于空串」。
        let (_, body) = get(store.clone(), "/api/v1/audit?username=&action=", true).await;
        let page: AuditPage = serde_json::from_value(body).unwrap();
        assert_eq!(page.entries.len(), 6);

        // 用户名不匹配时是空页，不是错误。
        let (status, body) = get(store.clone(), "/api/v1/audit?username=bob", true).await;
        assert_eq!(status, StatusCode::OK);
        let page: AuditPage = serde_json::from_value(body).unwrap();
        assert!(page.entries.is_empty());
    }

    #[tokio::test]
    async fn limit_越界返回_400() {
        for uri in ["/api/v1/audit?limit=0", "/api/v1/audit?limit=1001"] {
            let (status, body) = get(store_with(1).await, uri, true).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body["code"], ErrorCode::InvalidRequest.as_str(), "{uri}");
        }
        // 上限本身合法。
        let (status, _) = get(store_with(1).await, "/api/v1/audit?limit=1000", true).await;
        assert_eq!(status, StatusCode::OK);
    }

    #[test]
    fn limit_缺省取_100_且不静默夹取() {
        let f = to_filter(&AuditQuery::default()).unwrap();
        assert_eq!(f.limit, AuditFilter::DEFAULT_LIMIT);

        let over = AuditQuery {
            limit: Some(5_000),
            ..AuditQuery::default()
        };
        assert_eq!(
            to_filter(&over).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn since_晚于_until_返回_400() {
        let q = AuditQuery {
            since: Some(200),
            until: Some(100),
            ..AuditQuery::default()
        };
        assert_eq!(to_filter(&q).unwrap_err().code, ErrorCode::InvalidRequest);

        // 相等是合法输入（左闭右开，结果为空页）。
        let q = AuditQuery {
            since: Some(100),
            until: Some(100),
            ..AuditQuery::default()
        };
        assert!(to_filter(&q).is_ok());
    }

    #[test]
    fn params_非法_json_退化成字符串而不是丢弃() {
        let e = StoredEntry {
            id: 1,
            ts: 0,
            node_id: "local".into(),
            username: "alice".into(),
            uid: Some(-1),
            elevated: false,
            action: "x".into(),
            target: None,
            params: Some("{ 坏掉的".into()),
            result: AuditOutcome::Error,
            detail: None,
            remote_addr: None,
        };
        let dto = entry_to_dto(e);
        assert_eq!(dto.params, Some(Value::String("{ 坏掉的".into())));
        // 装不下的 uid 当作缺失，整条记录仍然可读。
        assert_eq!(dto.uid, None);
    }

    #[tokio::test]
    async fn prune_once_按保留期算边界并返回条数() {
        let store = Store::open_in_memory().await.unwrap();
        let now = now_unix();
        // 三条：远超期、刚好落在边界外、还在保留期内。
        for ts in [now - 10_000, now - 101, now - 10] {
            let mut e = NewAuditEntry::new("local", "alice", "service.start", AuditOutcome::Ok);
            e.ts = Some(ts);
            store.audit_write(&e).await.unwrap();
        }

        // 保留 100 秒：边界是 now-100，`ts < 边界` 的两条被删。
        let removed = prune_once(&store, 100).await.unwrap();
        assert_eq!(removed, 2);

        let page = store
            .audit_query(&AuditFilter::default())
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(page.entries[0].ts >= now - 100);

        // 幂等：再跑一次没有可删的。
        assert_eq!(prune_once(&store, 100).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn 清理任务按间隔反复执行() {
        let store = Store::open_in_memory().await.unwrap();
        let now = now_unix();

        // 用很短的间隔跑真实的调度循环——不是等一小时，而是验证「循环确实会再来一轮」。
        let handle = spawn_prune_task_every(store.clone(), 100, Duration::from_millis(20));

        // 第一轮：启动即执行（interval 的首个 tick 不等待），此时库是空的。
        // 之后写入一条超期记录，它必须被**后续某一轮**清掉，而不是等到进程重启。
        let mut e = NewAuditEntry::new("local", "alice", "service.start", AuditOutcome::Ok);
        e.ts = Some(now - 10_000);
        store.audit_write(&e).await.unwrap();

        let mut cleaned = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let page = store.audit_query(&AuditFilter::default()).await.unwrap();
            if page.entries.is_empty() {
                cleaned = true;
                break;
            }
        }
        handle.abort();
        assert!(cleaned, "清理任务应在若干轮内删掉超期记录");
    }

    #[tokio::test]
    async fn openapi_收录端点与提权错误() {
        let store = Store::open_in_memory().await.unwrap();
        let (_, doc) = OpenApiRouter::new()
            .nest("/api/v1", router(Arc::new(AuditState::new(store))))
            .split_for_parts();

        let paths: Vec<&str> = doc.paths.paths.keys().map(String::as_str).collect();
        assert_eq!(paths, vec!["/api/v1/audit"]);

        let op = doc.paths.paths["/api/v1/audit"]
            .get
            .as_ref()
            .expect("应有 GET");
        assert_eq!(op.tags.as_deref(), Some(&["audit".to_string()][..]));
        // 403 必须出现在文档里：它是本端点最容易被撞上的响应。
        assert!(op.responses.responses.contains_key("403"));
        assert!(op.security.is_some(), "应声明 bearer 安全要求");
        // 查询参数应完整收录（AuditQuery 的七个字段）。
        let params = op.parameters.as_ref().expect("应有查询参数");
        let mut names: Vec<&str> = params.iter().map(|p| p.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            vec!["action", "before_id", "limit", "result", "since", "until", "username"]
        );
    }
}
