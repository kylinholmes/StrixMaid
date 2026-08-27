//! 审计日志（`docs/design.md` §8 `audit_log` 表、§9.1「审计」组）。
//!
//! `GET /api/v1/audit` **需要管理访问**；未提权返回 [`crate::ErrorCode::ElevationRequired`]。
//!
//! 记录的是「谁、以什么身份、对什么、做了什么、结果如何」。
//! **[`AuditEntry::params`] 里绝不允许出现明文凭据**（`docs/design.md` §5.3）——
//! 认证类操作只记录用户名与成败，不记录任何 prompt 的 value。

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// 操作结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditResult {
    /// 成功。
    Ok,
    /// 被拒绝（未提权、polkit / OS 判定无权）。**这是审计里最值得看的一类**。
    Denied,
    /// 执行出错（目标不存在、底层组件失败）。
    Error,
}

/// 一条审计记录，与 `audit_log` 表一一对应。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditEntry {
    /// 自增主键，同时用作翻页游标（见 [`AuditQuery::before_id`]）。
    #[schema(example = 10_234_i64)]
    pub id: i64,
    /// 操作发生的时刻。
    #[schema(example = 1_756_252_800_i64)]
    pub ts: i64,
    /// 目标节点 id。MVP 恒为 `"local"`。
    #[schema(example = "local")]
    pub node_id: String,
    /// 发起者的系统用户名。
    #[schema(example = "alice")]
    pub username: String,
    /// 发起者的 uid。历史记录里可能缺失，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 1000)]
    pub uid: Option<u32>,
    /// 操作时会话是否处于提权状态。
    pub elevated: bool,
    /// 操作标识，点分层级，与 REST 端点对应：`service.start` / `process.kill` /
    /// `file.write` / `auth.login` / `auth.elevate` / `system.power`。
    #[schema(example = "service.start")]
    pub action: String,
    /// 操作对象：unit 名 / pid / 文件路径。无明确对象（如 `auth.login`）时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "nginx.service")]
    pub target: Option<String>,
    /// 补充参数，任意 JSON 对象。
    ///
    /// **禁止写入明文密码、token 或其它凭据。**
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub params: Option<serde_json::Value>,
    /// 结果。
    pub result: AuditResult,
    /// 结果说明：失败原因、polkit action id 等。成功时通常为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 请求来源地址。经反向代理时可能是代理地址或为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_addr: Option<String>,
}

/// `GET /api/v1/audit` 的查询参数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct AuditQuery {
    /// 起始时刻（**含**），unix 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<i64>,
    /// 结束时刻（**不含**），unix 秒。
    ///
    /// 区间是左闭右开 `[since, until)`，与指标查询（`design.md` §7.2）一致。
    /// 闭区间会把恰好落在 `until` 那一秒的记录带进来，按整点分页时相邻两页会重叠。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// 按发起者用户名精确过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "alice")]
    pub username: Option<String>,
    /// 按 `action` 前缀过滤，如 `service.` 命中所有服务操作。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "service.")]
    pub action: Option<String>,
    /// 按结果过滤。查 `denied` 是排查权限问题的主要入口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<AuditResult>,
    /// 翻页游标：只返回 `id` 严格小于该值的记录（由新到旧）。缺省从最新一条开始。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_id: Option<i64>,
    /// 每页条数。缺省 100，上限由服务端限制。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = 100)]
    pub limit: Option<u32>,
}

/// `GET /api/v1/audit` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditPage {
    /// 本页记录，按 `id` **降序**（新的在前）。
    #[serde(default)]
    pub entries: Vec<AuditEntry>,
    /// 下一页的 [`AuditQuery::before_id`]。为 `None` 表示已到最旧一条。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_before_id: Option<i64>,
}
