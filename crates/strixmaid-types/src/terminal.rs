//! 终端会话（`docs/design.md` §9.1「终端」组、§9.2）。
//!
//! PTY 跑在 **worker 进程**里（uid = 登录用户，见 `docs/design.md` §2.2），
//! 由 helper setuid 后 fork 出来；主进程只做转发。
//!
//! 终端的字节流走**独立的** `WS /ws/terminal/{id}`，不进 `/ws` 控制面多路复用——
//! 它是纯二进制流、延迟敏感、生命周期独立于页面，塞进多路复用会让流控与背压难以处理
//! （`docs/design.md` §9.2）。因此本模块只定义 REST 侧的生命周期管理类型。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `POST /api/v1/terminals` 的请求体。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct CreateTerminalReq {
    /// 要启动的 shell 的绝对路径。缺省用目标用户 passwd 项里的 shell，
    /// 再回落到 `/bin/sh`。路径不存在返回 [`crate::ErrorCode::InvalidRequest`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/bin/bash")]
    pub shell: Option<String>,
    /// 以哪个用户身份运行。缺省为当前会话的登录用户（即 user worker）。
    ///
    /// 传别的用户（典型是 `"root"`）会路由到 **admin worker**，因此**要求会话已提权**；
    /// 未提权时返回 [`crate::ErrorCode::ElevationRequired`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "root")]
    pub user: Option<String>,
}

/// `POST /api/v1/terminals` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CreateTerminalResp {
    /// 终端 id，用于拼 `WS /ws/terminal/{id}` 以及后续的 resize / delete。
    /// **仅在本浏览器会话内有效**，别的会话拿到也用不了。
    #[schema(example = "t_7f3a91")]
    pub id: String,
}

/// `GET /api/v1/terminals` 的列表项：本会话当前存活的终端。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TerminalInfo {
    /// 终端 id。
    #[schema(example = "t_7f3a91")]
    pub id: String,
    /// 实际启动的 shell 路径。
    #[schema(example = "/bin/bash")]
    pub shell: String,
    /// 实际运行身份的用户名。
    #[schema(example = "alice")]
    pub user: String,
    /// 运行身份的 uid。`0` 表示这是一个提权终端，前端应显著标记。
    #[schema(example = 1000)]
    pub uid: u32,
    /// 当前列数。
    #[schema(example = 120)]
    pub cols: u16,
    /// 当前行数。
    #[schema(example = 32)]
    pub rows: u16,
    /// 创建时刻。
    pub created_ts: i64,
    /// 最近一次有字节流经过的时刻，用于空闲回收。
    pub last_active_ts: i64,
    /// 当前是否有 WS 连接挂在上面。
    ///
    /// 为 `false` 不等于终端已死——刷新页面后终端仍在，回看缓冲还留着
    /// （`docs/design.md` §13 步骤 23），重新连上即可继续。
    pub attached: bool,
}

/// `POST /api/v1/terminals/{id}/resize` 的请求体。
///
/// 走 REST 而不是塞进终端 WS 流，是因为写操作一律走 REST（幂等、易调试、好审计），
/// 见 `docs/design.md` §9.1 末尾。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResizeReq {
    /// 列数，必须 > 0。
    #[schema(example = 120)]
    pub cols: u16,
    /// 行数，必须 > 0。
    #[schema(example = 32)]
    pub rows: u16,
}
