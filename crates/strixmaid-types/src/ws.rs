//! WebSocket 控制面协议（`docs/design.md` §9.2）。
//!
//! ```text
//! WS /ws                  控制面，多路复用
//! WS /ws/terminal/{id}    终端专用连接（纯二进制流，不走本模块的 envelope）
//! ```
//!
//! 控制面上的每一帧都是一个 [`WsEnvelope`] 的 JSON 文本帧：
//!
//! ```jsonc
//! { "v": 1,
//!   "t": "sub",              // sub | unsub | data | req | resp | err | ping
//!   "ch": "metrics.live",    // 频道
//!   "id": 42,                // 关联请求与响应
//!   "d": { }                 // payload
//! }
//! ```
//!
//! 写操作**一律走 REST**（幂等、易调试、好审计），WS 只承载实时流与订阅管理
//! （`docs/design.md` §9.1 末尾）。`req`/`resp` 因此只用于订阅期间的轻量查询，
//! 不是 REST 的替代通道。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 控制面协议版本，对应 envelope 的 `v` 字段。不兼容变更时递增。
pub const WS_PROTOCOL_VERSION: u8 = 1;

/// 消息类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WsMsgType {
    /// 客户端 → 服务端：订阅 `ch` 频道，`d` 为该频道的订阅参数。
    Sub,
    /// 客户端 → 服务端：退订 `ch` 频道，`d` 通常为空。
    Unsub,
    /// 服务端 → 客户端：频道推送。`ch` 必填，`d` 为该频道的 payload。
    Data,
    /// 客户端 → 服务端：一次性请求，`id` 必填。
    Req,
    /// 服务端 → 客户端：对 `req` 的应答，`id` 与请求相同。
    Resp,
    /// 服务端 → 客户端：错误。`d` 为 [`crate::ApiError`]；
    /// 若由某个 `req`/`sub` 触发则带上相同的 `id`，否则为连接级错误。
    Err,
    /// 双向心跳。收到后原样回一帧即可。用于穿过会掐空闲连接的反向代理。
    Ping,
}

/// 控制面 envelope。
///
/// 字段名刻意用单字母（`v`/`t`/`ch`/`id`/`d`），因为 `metrics.live` 按 2s 推送、
/// 高频小包，字段名开销占比不低。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct WsEnvelope {
    /// 协议版本，见 [`WS_PROTOCOL_VERSION`]。收到不认识的版本应回 `err` 并断开。
    #[schema(example = 1)]
    pub v: u8,
    /// 消息类型。
    pub t: WsMsgType,
    /// 频道名。`sub` / `unsub` / `data` 必填；`req` / `resp` / `err` / `ping` 可为 `None`。
    ///
    /// 类型是 `String` 而非枚举：未知频道要能被解析出来，才有可能带着正确的 `id`
    /// 回一个 `err`——若在 envelope 层就反序列化失败，连是谁的错误都对不上。
    /// 已知取值见 [`WsChannel`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "metrics.live")]
    pub ch: Option<String>,
    /// 关联 id，由**发起方**分配、在单条连接内唯一。`resp` / `err` 原样回带。
    /// `data` 推送不需要它，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 42_u64)]
    pub id: Option<u64>,
    /// payload。形状由 `t` + `ch` 共同决定，见 [`WsChannel`] 的文档。
    /// 无 payload 时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub d: Option<serde_json::Value>,
}

impl WsEnvelope {
    /// 构造一条 `data` 推送。
    pub fn data(ch: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            v: WS_PROTOCOL_VERSION,
            t: WsMsgType::Data,
            ch: Some(ch.into()),
            id: None,
            d: Some(payload),
        }
    }

    /// 构造一条 `err`。`id` 传上游请求的 id，连接级错误传 `None`。
    ///
    /// 序列化失败（[`crate::ApiError`] 全是普通字段，实际不可能失败）时 `d` 为 `None`。
    pub fn err(id: Option<u64>, error: &crate::ApiError) -> Self {
        Self {
            v: WS_PROTOCOL_VERSION,
            t: WsMsgType::Err,
            ch: None,
            id,
            d: serde_json::to_value(error).ok(),
        }
    }
}

/// 控制面已知频道（`docs/design.md` §9.2）。
///
/// 定义成枚举便于服务端 `match`，但 [`WsEnvelope::ch`] 保持 `String`——理由见该字段文档。
/// 用 [`Self::as_str`] 与 `str::parse`（[`std::str::FromStr`] 实现）在两者间转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum WsChannel {
    /// 实时指标，**每 2s 推送一次**（采集间隔可配 1–60s）。
    ///
    /// - `sub` 的 `d`：可为空，表示订阅全部序列。
    /// - `data` 的 `d`：[`crate::metrics::MetricSnapshot`]。
    #[serde(rename = "metrics.live")]
    MetricsLive,
    /// 日志跟随（`journalctl -f`）。
    ///
    /// - `sub` 的 `d`：[`crate::log::LogQuery`]（`cursor` / `limit` 被忽略，从「现在」开始跟）。
    /// - `data` 的 `d`：[`crate::log::LogEntry`] 数组，可能一次推多条。
    #[serde(rename = "logs.follow")]
    LogsFollow,
    /// unit 状态变更，由 zbus 的属性信号驱动（无信号时降级为轮询）。
    ///
    /// - `sub` 的 `d`：可为空。
    /// - `data` 的 `d`：[`crate::service::UnitSummary`] 数组，只含**发生变化**的 unit。
    #[serde(rename = "services.changed")]
    ServicesChanged,
    /// 健康状态变更。
    ///
    /// - `data` 的 `d`：[`crate::system::HealthReport`]（全量替换，不是增量）。
    #[serde(rename = "system.health")]
    SystemHealth,
    /// 实时进程列表。
    ///
    /// - `sub` 的 `d`：[`crate::process::ProcessListQuery`]。
    /// - `data` 的 `d`：[`crate::process::ProcessSummary`] 数组（全量替换）。
    #[serde(rename = "processes.live")]
    ProcessesLive,
}

impl WsChannel {
    /// 线格式频道名。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MetricsLive => "metrics.live",
            Self::LogsFollow => "logs.follow",
            Self::ServicesChanged => "services.changed",
            Self::SystemHealth => "system.health",
            Self::ProcessesLive => "processes.live",
        }
    }
}

impl std::str::FromStr for WsChannel {
    type Err = crate::ApiError;

    /// 解析频道名。未知频道返回 [`crate::ErrorCode::InvalidRequest`]，
    /// 调用方应把它包成一条 `err` 回给客户端，而不是断开连接。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "metrics.live" => Self::MetricsLive,
            "logs.follow" => Self::LogsFollow,
            "services.changed" => Self::ServicesChanged,
            "system.health" => Self::SystemHealth,
            "processes.live" => Self::ProcessesLive,
            other => {
                return Err(crate::ApiError::invalid_request(format!(
                    "未知频道 {other}"
                )));
            }
        })
    }
}

impl std::fmt::Display for WsChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
