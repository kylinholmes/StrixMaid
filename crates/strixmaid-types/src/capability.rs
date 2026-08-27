//! 两层能力探测（`docs/design.md` §6）。
//!
//! - **system 层**：进程启动时探测一次，回答「这台机器上有没有这个东西」。
//! - **user 层**：会话建立时探测，回答「当前登录用户能不能用」。**未认证时为 `null`**——
//!   `GET /api/v1/capabilities` 不要求认证，因为登录页本身就得靠它判断能不能登录。
//!
//! 前端必须区分二者，体验完全不同、不可混淆：
//!
//! - system 层为 `false` → **隐藏页面**（这台机器根本没有 systemd，服务页毫无意义）；
//! - system 层为 `true` 但 user 层为 `false` → **显示但禁用**，给出提权入口与可操作的说明
//!   （「你的账户不在 `systemd-journal` 或 `adm` 组，因此只能看到自己的日志。启用管理访问后可查看全部。」）。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `GET /api/v1/capabilities` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Capabilities {
    /// 机器级能力，启动时探测一次，与登录者无关。
    pub system: SystemCapabilities,
    /// 当前会话的用户级能力，会话建立时探测；提权后需要重新下发（`elevated` 会变）。
    ///
    /// **未认证时为 `null`，接口仍返回 200。** 这不是可有可无的宽松处理：
    /// 若 [`SystemCapabilities::helper`] 为 `false`，登录**根本不可能成功**，
    /// 登录页必须先拿到 `system` 层能力才能显示「PAM helper 不可用，无法登录」，
    /// 而不是让用户对着一个神秘失败的登录框反复试。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<UserCapabilities>,
}

/// 机器级能力。字段名即 [`crate::ApiError::capability`] 的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
pub struct SystemCapabilities {
    /// 有 systemd（能连上 `org.freedesktop.systemd1`，或至少有可用的 `systemctl`）。
    /// 为 `false` 时隐藏服务页。
    #[schema(example = true)]
    pub systemd: bool,
    /// 有 journald（`journalctl` 可执行）。为 `false` 时隐藏日志页。
    #[schema(example = true)]
    pub journal: bool,
    /// `strixmaid-helper` 已部署且 IPC socket 可用。
    /// 为 `false` 时无法 PAM 认证、无法 setuid，登录与终端都不可用。
    #[schema(example = true)]
    pub helper: bool,
    /// 系统上有 polkit。为 `false` 时提权只能靠「以 root 身份跑 admin worker」，
    /// 且 systemd 操作的授权裁决退化为纯 uid 判断。
    #[schema(example = true)]
    pub polkit: bool,
    /// 支持用户级 unit（`systemd --user`）。前提是 helper 能 `pam_open_session` 并 setuid 后连 session bus。
    /// 为 `false` 时服务页隐藏 `scope=user` 选项。
    #[schema(example = true)]
    pub user_units: bool,
    /// 检测到 podman。P0 不做容器管理，此位仅用于前端预留入口。
    #[schema(example = false)]
    pub podman: bool,
}

/// 当前会话的用户级能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserCapabilities {
    /// 系统 uid。
    #[schema(example = 1000)]
    pub uid: u32,
    /// 系统用户名。
    #[schema(example = "alice")]
    pub name: String,
    /// 所属组名列表（含主组）。前端可据此生成「你不在 xxx 组」这类具体说明。
    #[serde(default)]
    pub groups: Vec<String>,
    /// 能否读取**全量**日志（在 `systemd-journal` / `adm` / `wheel` 组，或有 journald ACL）。
    /// 为 `false` 时日志页仍可用，但只能看到自己的条目——必须在 UI 上明说，否则用户会以为日志丢了。
    #[schema(example = false)]
    pub can_read_journal: bool,
    /// 能否管理 unit（polkit 判定 `org.freedesktop.systemd1.manage-units` 为 yes 或 auth_admin）。
    /// 为 `false` 时服务页的操作按钮禁用，但列表仍可看。
    #[schema(example = false)]
    pub can_manage_units: bool,
    /// 能否走 `/api/v1/auth/elevate/*` 提权（通常等价于「在 sudo / wheel 组」或 polkit 允许 auth_admin）。
    /// 为 `false` 时**不要**显示提权入口，否则用户点了必然失败。
    #[schema(example = true)]
    pub can_elevate: bool,
    /// 当前是否已提权。提权成功后此位翻为 `true`，`can_manage_units` 等通常随之变 `true`。
    #[schema(example = false)]
    pub elevated: bool,
}

/// user 层能力的**实测**结果（`roadmap/01-worker-execution.md` §4.6）。
///
/// 与 `derive_user_caps` 的按组推导互补：推导快、离线、但只是猜；实测要在
/// **user worker 内**真的去试（试读系统日志、看 session bus 在不在），
/// 贵一些但准。两者合并时**实测值覆盖推导值**。
///
/// 每个字段都是 `Option`：`None` 表示「这一项没测出结论」，此时沿用推导值，
/// 而不是当成 `false`——把「没测出来」和「测出来是否」混为一谈，
/// 会让前端把一项其实可用的能力灰掉。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UserProbe {
    /// 能否读到**系统**日志（而不只是自己的）。
    ///
    /// 判据见 worker 侧实现：能读到内核日志即说明有系统日志的可见权限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_read_journal: Option<bool>,
    /// 能否管理服务单元。polkit 的裁决无法离线探测，实测只在「已经是 root」时给 true。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub can_manage_units: Option<bool>,
    /// 是否支持用户级 unit：`/run/user/<uid>` 的 session bus 可连。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_units: Option<bool>,
    /// worker 实际运行的 uid。**用来端到端证明请求确实在该用户身份下执行**——
    /// 它与会话用户不符，说明 worker 路由错了。
    pub uid: u32,
}
