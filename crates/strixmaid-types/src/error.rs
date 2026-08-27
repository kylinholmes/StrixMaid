//! 统一错误类型。
//!
//! 设计要点（`docs/design.md` §6）：前端必须能区分「能力不存在」与「能力存在但当前用户无权」，
//! 二者体验完全不同——前者隐藏页面，后者显示但禁用并给出提权入口。因此错误码是**语义分类**，
//! 而不是 HTTP 状态码的同义反复：
//!
//! | 语义 | [`ErrorCode`] | 前端应做的事 |
//! |---|---|---|
//! | 未认证 / token 失效 | [`ErrorCode::Unauthenticated`] | 跳登录页 |
//! | 需要提权（管理访问） | [`ErrorCode::ElevationRequired`] | 弹提权对话框，成功后重试 |
//! | OS 拒绝（polkit / EACCES / journald ACL） | [`ErrorCode::PermissionDenied`] | 显示原因，若 `can_elevate` 则引导提权 |
//! | 能力不存在（本机没装 systemd 等） | [`ErrorCode::CapabilityUnavailable`] | 隐藏整个页面 |

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// API 结果别名。
pub type ApiResult<T> = Result<T, ApiError>;

/// 错误码。这是**稳定的机器可读契约**，前端按它分支；`message` 只用于展示，不要拿去做判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// 请求没带 token、token 无效或已过期。→ HTTP 401。
    ///
    /// 与 [`Self::PermissionDenied`] 的区别：这里是「不知道你是谁」。
    Unauthenticated,

    /// 已认证，但该操作要求管理访问（admin worker），当前会话尚未提权。→ HTTP 403。
    ///
    /// 见 `docs/design.md` §2.2：「未提权的写操作返回 403 + 需要管理访问」。
    /// 前端应引导走 `/api/v1/auth/elevate/*`。
    ElevationRequired,

    /// **操作系统**拒绝了这次操作：polkit 判定 not-authorized、`EACCES`/`EPERM`、
    /// journald ACL 不允许读全量日志等。→ HTTP 403。
    ///
    /// 见 `docs/design.md` §1 原则 3「授权外包给操作系统」——本项目不自建 RBAC，
    /// 因此这类错误的裁决者永远是 OS，不是 StrixMaid。
    PermissionDenied,

    /// 这台机器上**根本没有**该能力：没装 systemd、没有 journald、helper 未部署……
    /// → HTTP 501（Not Implemented，语义上「本服务端不提供」）。
    ///
    /// [`ApiError::capability`] 会给出具体能力名，取值与
    /// [`crate::capability::SystemCapabilities`] 的字段名一致。
    CapabilityUnavailable,

    /// 目标对象不存在：unit / pid / 终端 id / 文件路径。→ HTTP 404。
    NotFound,

    /// 请求参数不合法：时间区间倒置、limit 越界、枚举值无法识别等。→ HTTP 400。
    InvalidRequest,

    /// 与当前状态冲突：终端 id 已存在、认证会话已完成又被重复 respond 等。→ HTTP 409。
    Conflict,

    /// 依赖的外部组件超时：zbus 调用、`journalctl` 子进程、远程 Agent 无响应。→ HTTP 504。
    Timeout,

    /// 能力存在但此刻不可用：systemd 在线但 bus 断了、Agent 节点离线。→ HTTP 503。
    ///
    /// 与 [`Self::CapabilityUnavailable`] 的区别：这是**暂时的**，重试可能成功，页面不该隐藏。
    Unavailable,

    /// 服务端内部错误。→ HTTP 500。`detail` 不应包含堆栈或敏感路径。
    Internal,
}

impl ErrorCode {
    /// 线格式字符串，与 serde 序列化结果一致。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::ElevationRequired => "elevation_required",
            Self::PermissionDenied => "permission_denied",
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::NotFound => "not_found",
            Self::InvalidRequest => "invalid_request",
            Self::Conflict => "conflict",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }

    /// 建议的 HTTP 状态码。
    ///
    /// 放在 types crate 是为了让 server 与 worker 用同一张映射表——它属于协议的一部分，
    /// 不是 server 的实现细节。
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            Self::ElevationRequired | Self::PermissionDenied => 403,
            Self::CapabilityUnavailable => 501,
            Self::NotFound => 404,
            Self::InvalidRequest => 400,
            Self::Conflict => 409,
            Self::Timeout => 504,
            Self::Unavailable => 503,
            Self::Internal => 500,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 统一错误响应体。所有非 2xx 的 REST 响应、以及 WS `t = "err"` 消息的 `d` 字段，都是这个形状。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// 机器可读的错误分类。
    pub code: ErrorCode,

    /// 面向用户的一句话说明，可直接展示。**不得包含明文凭据**（见 `docs/design.md` §5.3）。
    #[schema(example = "当前账户不在 systemd-journal 组，只能看到自己的日志")]
    pub message: String,

    /// 补充细节：底层错误原文、polkit action id、`systemctl` 的 stderr 等。
    /// 为 `None` 表示没有更多信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,

    /// 仅当 `code` 为 [`ErrorCode::CapabilityUnavailable`] 时有值：缺失的能力名，
    /// 取值与 [`crate::capability::SystemCapabilities`] 的字段名一致（如 `"systemd"`、`"journal"`）。
    /// 前端据此隐藏对应页面。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "systemd")]
    pub capability: Option<String>,

    /// 提示前端「提权后重试即可成功」。通常与 [`ErrorCode::ElevationRequired`] 或
    /// [`ErrorCode::PermissionDenied`] 同时出现；为 `false` 表示提权也无济于事。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub can_retry_elevated: bool,
}

impl ApiError {
    /// 构造一个只有码与消息的错误。
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
            capability: None,
            can_retry_elevated: false,
        }
    }

    /// 附加细节。
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// 标记「提权后重试可能成功」。
    #[must_use]
    pub fn retry_elevated(mut self) -> Self {
        self.can_retry_elevated = true;
        self
    }

    /// 未认证。
    pub fn unauthenticated(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthenticated, message)
    }

    /// 需要管理访问（提权）。自动置 `can_retry_elevated = true`。
    pub fn elevation_required(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::ElevationRequired, message).retry_elevated()
    }

    /// 操作系统拒绝。`can_retry_elevated` 由调用方按 polkit / ACL 的实际情况决定。
    pub fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::PermissionDenied, message)
    }

    /// 能力不存在。`capability` 必须给出，前端据此隐藏页面。
    pub fn capability_unavailable(
        capability: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            capability: Some(capability.into()),
            ..Self::new(ErrorCode::CapabilityUnavailable, message)
        }
    }

    /// 目标不存在。
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// 请求参数不合法。
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    /// 内部错误。
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }

    /// 建议的 HTTP 状态码，等价于 `self.code.http_status()`。
    pub const fn http_status(&self) -> u16 {
        self.code.http_status()
    }
}
