//! Agent ⇄ Server 的 WS 协议与节点管理 DTO（roadmap/05）。
//!
//! Agent 主动连接 `WS /ws/agent`，鉴权走 `Sec-WebSocket-Protocol: bearer, <token>`，
//! token 与 `nodes.token_hash` 比对（不是 PAM 会话）。协议复用
//! [`crate::ws::WsEnvelope`]，`ch` 取本模块的 `CH_*` 常量：
//!
//! | 方向 | `ch` | `d` |
//! |---|---|---|
//! | Agent → Server | `agent.hello` | [`AgentHello`]，连接后首帧 |
//! | Server → Agent | `agent.resume` | [`AgentResume`] |
//! | Agent → Server | `agent.rows` | [`AgentRows`]，补发与常规推送共用 |
//! | Agent → Server | `agent.snapshot` | [`crate::metrics::MetricSnapshot`] |
//! | Server → Agent | `agent.request` | `{ method, params }`，`id` 在 envelope 上 |
//!
//! # 补发语义
//!
//! `since_ts` 是 Server 端该节点 `m_1m` 的最大 `ts`（无数据为 0）。Agent 从
//! **`ts >= since_ts`** 重发——不是严格大于：`agent.rows` 一帧一个事务，但同一个
//! `ts` 的行可能跨帧（series 多于每帧行数上限时），崩溃可能留下「最大 ts 只写了
//! 一半」的状态。多发的一桶由 [`m_1m` 的 UPSERT] 幂等吸收。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::capability::SystemCapabilities;
use crate::metrics::MetricLayer;

/// Agent 连接的路径。
pub const AGENT_WS_PATH: &str = "/ws/agent";

/// 连接后 Agent 的第一帧。
pub const CH_AGENT_HELLO: &str = "agent.hello";
/// Server 告知从哪继续补发。
pub const CH_AGENT_RESUME: &str = "agent.resume";
/// 一批 `m_1m` 行。
pub const CH_AGENT_ROWS: &str = "agent.rows";
/// 每个采集周期一帧的瞬时快照。
pub const CH_AGENT_SNAPSHOT: &str = "agent.snapshot";
/// Server → Agent 的一次性请求（MVP 只允许 `host.info` 与 `caps.probe`）。
pub const CH_AGENT_REQUEST: &str = "agent.request";

/// 一帧 `agent.rows` 最多多少行。
pub const ROWS_PER_FRAME: usize = 1000;

/// `agent.hello`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentHello {
    /// 节点稳定标识，必须与 token 所属的 `nodes.id` 一致，不一致即断开。
    pub node_id: String,
    /// 显示名。
    pub node_name: String,
    /// Agent 版本（`CARGO_PKG_VERSION`）。
    pub version: String,
    /// 该节点的 system 层能力（信息性；Agent 上没有 helper 是常态）。
    #[serde(default)]
    pub caps: SystemCapabilities,
}

/// `agent.resume`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResume {
    /// Server 端该节点 `m_1m` 的最大 `ts`；无数据时 0。补发语义见模块文档。
    pub since_ts: i64,
}

/// `agent.rows` 里的一条 series 元数据。行以下标引用它——每帧行数多、
/// series 数少，先列表再引用比每行重复字符串省得多。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSeriesDesc {
    /// 指标名。
    pub metric: String,
    /// 规范化标签串。
    #[serde(default)]
    pub labels: String,
    /// 单位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// `agent.rows` 里的一行桶数据。字段与 `m_1m` 表一致，`s` 是
/// [`AgentRows::series`] 的下标。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AgentRowItem {
    /// series 下标。
    pub s: u32,
    /// 桶起始时间，unix 秒。
    pub ts: i64,
    /// 采样点数。
    pub cnt: i64,
    /// 桶内最小值。
    pub min: f64,
    /// 桶内最大值。
    pub max: f64,
    /// 桶内累加值。
    pub sum: f64,
    /// 桶内中位数。
    pub med: f64,
}

/// `agent.rows`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRows {
    /// 分层。MVP 只允许 `m_1m`——粗层由 Server 自己的每分钟聚合生成。
    pub layer: MetricLayer,
    /// 本帧涉及的 series。
    pub series: Vec<AgentSeriesDesc>,
    /// 行，`s` 引用 `series` 下标；应按 `(ts, s)` 升序。
    pub rows: Vec<AgentRowItem>,
}

// ============================ 节点管理（REST） ============================

/// `GET /api/v1/nodes` 的列表项。**绝不包含 token 或其 hash**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NodeInfo {
    /// 节点 id：`"local"` 或登记时指定 / 生成的标识。
    #[schema(example = "web-01")]
    pub id: String,
    /// 显示名。
    #[schema(example = "Web 服务器 1")]
    pub name: String,
    /// `"local"` 或 `"agent"`。
    #[schema(example = "agent")]
    pub kind: String,
    /// 此刻是否有存活的 Agent 连接。`local` 恒为 `true`。
    pub online: bool,
    /// 最近一次心跳（unix 秒）。从未连接过为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<i64>,
    /// 登记时间。
    pub created_ts: i64,
}

/// `POST /api/v1/nodes` 的请求体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateNodeReq {
    /// 节点 id。缺省由服务端生成随机 id。Agent 配置里的 `node_id` 必须等于它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "web-01")]
    pub id: Option<String>,
    /// 显示名。
    #[schema(example = "Web 服务器 1")]
    pub name: String,
}

/// `POST /api/v1/nodes` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateNodeResp {
    /// 节点 id。
    pub id: String,
    /// 预共享 token。**只在本响应里出现一次**，服务端只存 hash；
    /// 抄进 Agent 配置的 `token` 字段。
    pub token: String,
}
