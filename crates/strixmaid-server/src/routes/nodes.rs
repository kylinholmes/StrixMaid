//! `/api/v1/nodes` —— 节点登记与在线状态（roadmap/05 §3.3）。
//!
//! # 写操作需管理访问
//!
//! 与 `GET /audit` 同一例外理由（见 `routes/audit.rs` 模块文档）：`nodes` 表在
//! 主进程的 SQLite 里，没有对应的 OS 权限可以外包给 worker，只能按会话的
//! `elevated` 判断。登记节点会签发一个能持续写入指标库的 token，删除节点会
//! 使 Agent 掉线——都不是普通用户该做的事。
//!
//! # token 只出现一次
//!
//! `POST` 的响应是 token 唯一一次以明文出现的地方；库里只存 sha256。
//! 丢了 token 没有找回，只能删掉节点重新登记。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use rand::Rng as _;
use strixmaid_core::session::{Session, hash_token};
use strixmaid_core::store::{NodeKind, Store};
use strixmaid_types::agent::{CreateNodeReq, CreateNodeResp, NodeInfo};
use strixmaid_types::{ApiError, ErrorCode};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::RequestOrigin;
use crate::auth::{audit, audit::Record};
use crate::error::ApiResult;
use crate::ws::agent::AgentRegistry;

/// 节点路由的状态。
#[derive(Clone)]
pub struct NodesState {
    pub store: Store,
    pub registry: Arc<AgentRegistry>,
    /// [`RequestOrigin`] 提取器需要它判定可信代理。
    pub auth: Arc<AuthState>,
}

impl axum::extract::FromRef<NodesState> for Arc<AuthState> {
    fn from_ref(st: &NodesState) -> Self {
        st.auth.clone()
    }
}

impl NodesState {
    pub fn new(store: Store, registry: Arc<AgentRegistry>, auth: Arc<AuthState>) -> Self {
        NodesState {
            store,
            registry,
            auth,
        }
    }
}

/// 构建节点路由。挂到 `/api/v1` 之下（路径已含 `/nodes` 前缀）。
pub fn router(state: NodesState) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(list_nodes))
        .routes(routes!(create_node))
        .routes(routes!(delete_node))
        .with_state(state)
}

/// 节点列表
///
/// 含本机（`local`）与已登记的 Agent。`online` 来自内存注册表（此刻是否有
/// 存活连接），`last_seen` 优先取内存里更新鲜的值。
#[utoipa::path(
    get,
    path = "/nodes",
    tag = "nodes",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "节点列表", body = Vec<NodeInfo>),
        (status = 401, description = "未认证", body = ApiError),
    ),
)]
pub async fn list_nodes(
    State(st): State<NodesState>,
    Extension(_session): Extension<Session>,
) -> ApiResult<Json<Vec<NodeInfo>>> {
    let nodes = st
        .store
        .list_nodes()
        .await
        .map_err(|e| ApiError::internal("读取节点列表失败").with_detail(e.to_string()))?;
    Ok(Json(
        nodes
            .into_iter()
            .map(|n| {
                let kind = n.kind.as_str().to_owned();
                NodeInfo {
                    online: n.kind == NodeKind::Local || st.registry.online(&n.id),
                    last_seen: st.registry.last_seen(&n.id).or(n.last_seen),
                    id: n.id,
                    name: n.name,
                    kind,
                    created_ts: n.created_at,
                }
            })
            .collect(),
    ))
}

/// 登记节点
///
/// 生成预共享 token（**仅在本响应里出现一次**，服务端只存 hash）。
/// Agent 配置的 `node_id` / `token` 从响应里抄。需要管理访问。
#[utoipa::path(
    post,
    path = "/nodes",
    tag = "nodes",
    request_body = CreateNodeReq,
    security(("bearer" = [])),
    responses(
        (status = 201, description = "已登记，token 只出现这一次", body = CreateNodeResp),
        (status = 400, description = "id 不合法或 name 为空", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 403, description = "会话未提权（`elevation_required`）", body = ApiError),
        (status = 409, description = "id 已存在", body = ApiError),
    ),
)]
pub async fn create_node(
    State(st): State<NodesState>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Json(req): Json<CreateNodeReq>,
) -> ApiResult<(StatusCode, Json<CreateNodeResp>)> {
    if !session.elevated {
        return Err(ApiError::elevation_required("登记节点需要管理访问").into());
    }
    let result = do_create(&st, &req).await;

    let outcome = match &result {
        Ok(_) => strixmaid_core::store::AuditOutcome::Ok,
        Err(e) => audit::outcome_of(&Err(e)),
    };
    let mut rec = Record::new("node.create", outcome);
    if let Ok(resp) = &result {
        rec = rec.target(resp.id.clone());
    } else if let Some(id) = &req.id {
        rec = rec.target(id.clone());
    }
    rec = rec.params(serde_json::json!({ "name": req.name }));
    if let Err(e) = &result {
        rec = rec.detail(e.message.clone());
    }
    audit::record(&st.store, &session, origin.as_deref(), rec).await;

    let resp = result?;
    Ok((StatusCode::CREATED, Json(resp)))
}

async fn do_create(st: &NodesState, req: &CreateNodeReq) -> Result<CreateNodeResp, ApiError> {
    if req.name.trim().is_empty() {
        return Err(ApiError::invalid_request("name 不能为空"));
    }
    let id = match &req.id {
        Some(id) => {
            validate_node_id(id)?;
            id.clone()
        }
        None => random_hex(6),
    };
    let existing = st
        .store
        .get_node(&id)
        .await
        .map_err(|e| ApiError::internal("查询节点失败").with_detail(e.to_string()))?;
    if existing.is_some() {
        return Err(ApiError::new(
            ErrorCode::Conflict,
            format!("节点 {id} 已存在"),
        ));
    }

    // 32 字节随机 token；只存 hash（design.md §5.3 的同一纪律）。
    let token = random_hex(32);
    st.store
        .upsert_node(&id, req.name.trim(), NodeKind::Agent, Some(&hash_token(&token)))
        .await
        .map_err(|e| ApiError::internal("写入节点失败").with_detail(e.to_string()))?;
    Ok(CreateNodeResp { id, token })
}

/// 删除节点
///
/// Agent 的当前连接会在下一帧因 token 失效……并不会：连接建立时已完成鉴权，
/// 删除只保证**新的**连接被拒。该节点已汇聚的 series 与桶数据保留（历史仍可查），
/// 由保留期清理自然过期。需要管理访问。
#[utoipa::path(
    delete,
    path = "/nodes/{id}",
    tag = "nodes",
    params(("id" = String, Path, description = "节点 id")),
    security(("bearer" = [])),
    responses(
        (status = 204, description = "已删除"),
        (status = 400, description = "不能删除 local", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 403, description = "会话未提权（`elevation_required`）", body = ApiError),
        (status = 404, description = "节点不存在", body = ApiError),
    ),
)]
pub async fn delete_node(
    State(st): State<NodesState>,
    Extension(session): Extension<Session>,
    origin: RequestOrigin,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if !session.elevated {
        return Err(ApiError::elevation_required("删除节点需要管理访问").into());
    }
    let result = do_delete(&st, &id).await;

    let outcome = match &result {
        Ok(()) => strixmaid_core::store::AuditOutcome::Ok,
        Err(e) => audit::outcome_of(&Err(e)),
    };
    let mut rec = Record::new("node.delete", outcome).target(id.clone());
    if let Err(e) = &result {
        rec = rec.detail(e.message.clone());
    }
    audit::record(&st.store, &session, origin.as_deref(), rec).await;

    result?;
    Ok(StatusCode::NO_CONTENT)
}

async fn do_delete(st: &NodesState, id: &str) -> Result<(), ApiError> {
    if id == "local" {
        return Err(ApiError::invalid_request("不能删除本机节点"));
    }
    let deleted = st
        .store
        .delete_node(id)
        .await
        .map_err(|e| ApiError::internal("删除节点失败").with_detail(e.to_string()))?;
    if !deleted {
        return Err(ApiError::not_found(format!("节点 {id} 不存在")));
    }
    Ok(())
}

/// 节点 id 的合法形状：1–64 个 `[a-z0-9._-]`。它会出现在 series 表与 URL 里，
/// 限制住比事后转义省心。
fn validate_node_id(id: &str) -> Result<(), ApiError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
        && id != "local";
    if ok {
        Ok(())
    } else {
        Err(ApiError::invalid_request(
            "节点 id 须为 1–64 个小写字母 / 数字 / `.` `_` `-`，且不能是 local",
        ))
    }
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 节点_id_校验() {
        assert!(validate_node_id("web-01").is_ok());
        assert!(validate_node_id("a.b_c-1").is_ok());
        assert!(validate_node_id("local").is_err());
        assert!(validate_node_id("").is_err());
        assert!(validate_node_id("UPPER").is_err());
        assert!(validate_node_id("has space").is_err());
        assert!(validate_node_id(&"x".repeat(65)).is_err());
    }

    #[test]
    fn 随机_hex_长度与字符集() {
        let t = random_hex(32);
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_hex(32), random_hex(32));
    }
}
