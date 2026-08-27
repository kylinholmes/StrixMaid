//! systemd unit 管理（`docs/design.md` §9.1「服务」组）。
//!
//! 数据来源优先 `zbus` 走 `org.freedesktop.systemd1`，连不上 bus 时降级 `systemctl` 子进程
//! （`docs/design.md` §4）。两条路径必须产出**同一套**类型，因此这里的字段全部按
//! 「systemctl 也能拿到」为下限设计，systemd 独有的属性一律 `Option`。

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// unit 作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitScope {
    /// 系统级（`systemctl`）。
    #[default]
    System,
    /// 用户级（`systemctl --user`）。仅当 [`crate::capability::SystemCapabilities::user_units`]
    /// 为 `true` 时可用——它要求 helper 能 setuid 后连 session bus。
    User,
}

/// unit 的加载状态（systemd `LoadState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitLoadState {
    /// 已成功加载 unit 文件。
    Loaded,
    /// 找不到 unit 文件（多半是名字打错，或包已卸载）。
    NotFound,
    /// unit 文件里有非法设置。
    BadSetting,
    /// 加载出错（权限 / IO）。
    Error,
    /// 已被 mask，指向 `/dev/null`，无法启动。
    Masked,
    /// systemd 报了本枚举未覆盖的值。保底项，避免新版本 systemd 让反序列化整体失败。
    #[serde(other)]
    Unknown,
}

/// unit 的活动状态（systemd `ActiveState`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitActiveState {
    /// 正在运行。
    Active,
    /// 正在重载配置。
    Reloading,
    /// 未运行（正常停止）。
    Inactive,
    /// 异常退出或启动失败。健康聚合会把它计入 `unit.failed`。
    Failed,
    /// 正在启动。
    Activating,
    /// 正在停止。
    Deactivating,
    /// 未覆盖的值，见 [`UnitLoadState::Unknown`]。
    #[serde(other)]
    Unknown,
}

/// unit 文件的启用状态（`systemctl is-enabled` / systemd `UnitFileState`）。
///
/// 注意它是**三值以上**的：`static` 与 `indirect` 既不是 enabled 也不是 disabled，
/// 前端不要简化成一个 checkbox。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitEnableState {
    /// 已 enable（有持久化的符号链接）。
    Enabled,
    /// 仅本次启动有效的 enable。
    EnabledRuntime,
    /// 通过 link 引入。
    Linked,
    /// 仅本次启动有效的 link。
    LinkedRuntime,
    /// 是别的 unit 的别名。
    Alias,
    /// 已 mask，无法启动。
    Masked,
    /// 仅本次启动有效的 mask。
    MaskedRuntime,
    /// 没有 `[Install]` 段，无法 enable/disable——按钮应禁用而不是报错。
    Static,
    /// 由别的 unit 间接拉起。
    Indirect,
    /// 未 enable。
    Disabled,
    /// 由 generator 动态生成。
    Generated,
    /// 运行时临时创建，不存在于磁盘。
    Transient,
    /// 未覆盖的值，见 [`UnitLoadState::Unknown`]。
    #[serde(other)]
    Unknown,
}

/// `GET /api/v1/services` 列表项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitSummary {
    /// 完整 unit 名，**含后缀**。这是路径参数 `{unit}` 的取值，需 URL 编码（`@` 实例名里有特殊字符）。
    #[schema(example = "nginx.service")]
    pub name: String,
    /// unit 类型，即 `name` 的后缀：`service` / `socket` / `timer` / `target` / `mount` / `path` …
    #[schema(example = "service")]
    pub unit_type: String,
    /// `Description` 属性；未设置时 systemd 会回落成 unit 名本身。
    #[schema(example = "A high performance web server and a reverse proxy server")]
    pub description: String,
    /// 加载状态。
    pub load_state: UnitLoadState,
    /// 活动状态。
    pub active_state: UnitActiveState,
    /// 子状态，取值由 unit 类型决定（service 有 `running`/`exited`/`dead`/`failed`…）。
    /// **不要**枚举它——systemd 各版本取值不同，原样展示即可。
    #[schema(example = "running")]
    pub sub_state: String,
    /// 启用状态。`systemctl` 降级路径下逐个查询代价高，可能为 `None`（表示「未查询」而非「未启用」）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_state: Option<UnitEnableState>,
    /// 所属作用域。
    pub scope: UnitScope,
}

/// `GET /api/v1/services/{unit}` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct UnitDetail {
    /// 列表里已有的那部分字段，JSON 中平铺（`serde(flatten)`）。
    #[serde(flatten)]
    pub summary: UnitSummary,
    /// 主 unit 文件路径（`FragmentPath`）。transient unit 为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/usr/lib/systemd/system/nginx.service")]
    pub fragment_path: Option<String>,
    /// drop-in 覆盖文件路径列表（`DropInPaths`），按生效顺序。空数组表示没有覆盖。
    #[serde(default)]
    pub drop_in_paths: Vec<String>,
    /// 主进程 pid（`MainPID`）。未运行时为 `None`（systemd 报 0，这里归一成 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 1234)]
    pub main_pid: Option<u32>,
    /// 进入当前 active 状态的时刻（`ActiveEnterTimestamp`）。从未启动过为 `None`。
    /// 前端的「已运行 N 天」由 `now - active_enter_ts` 算出。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_enter_ts: Option<i64>,
    /// 最近一次状态变更时刻（`StateChangeTimestamp`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state_change_ts: Option<i64>,
    /// 累计重启次数（`NRestarts`）。频繁重启是排障的重要信号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 0)]
    pub n_restarts: Option<u32>,
    /// 上次运行结果（`Result`）：`success` / `exit-code` / `signal` / `timeout` / `oom-kill` …
    /// 原样透传，不枚举。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "success")]
    pub result: Option<String>,
    /// 主进程退出码（`ExecMainStatus`）。仅在已退出且以正常方式结束时有意义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// `Documentation` 属性，man 页 / URL 列表，供前端做外链。
    #[serde(default)]
    pub documentation: Vec<String>,
    /// 运行身份（`User=`）。未设置即 root，为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 该 unit 的 cgroup 资源占用。降级到 `systemctl` 或 cgroup 不可读时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cgroup: Option<CgroupUsage>,
}

/// unit 的 cgroup 资源占用（`/sys/fs/cgroup/<ControlGroup>/`）。
///
/// 每个字段都可能为 `None`：cgroup v1/v2 暴露的文件不同，容器内还可能整棵树都不可读。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
pub struct CgroupUsage {
    /// 累计 CPU 时间，**纳秒**（`cpu.stat` 的 `usage_usec` × 1000 或 systemd `CPUUsageNSec`）。
    /// 这是单调累加值，算占用率要自己做两次采样求差。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 12_345_678_900_u64)]
    pub cpu_usage_nsec: Option<u64>,
    /// 最近一个采样窗口内的 CPU 占用，**百分比**。单核跑满为 100.0，
    /// 多核可超过 100.0（4 核跑满 = 400.0）。服务端未做两次采样时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 3.5)]
    pub cpu_percent: Option<f64>,
    /// 当前内存占用，字节（`memory.current` / systemd `MemoryCurrent`）。
    /// 注意它**含 page cache**，通常比 RSS 大。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 268_435_456_u64)]
    pub memory_current_bytes: Option<u64>,
    /// 历史峰值内存，字节（`memory.peak`）。cgroup v1 或旧内核为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_peak_bytes: Option<u64>,
    /// 内存上限，字节（`memory.max` / `MemoryMax`）。未设限（`max`）时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit_bytes: Option<u64>,
    /// 当前任务（线程）数（`pids.current` / `TasksCurrent`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_current: Option<u64>,
    /// 任务数上限（`TasksMax`）。未设限为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_limit: Option<u64>,
    /// cgroup 路径（systemd `ControlGroup`），如 `/system.slice/nginx.service`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/system.slice/nginx.service")]
    pub path: Option<String>,
}

/// `GET /api/v1/services/{unit}/file` 的响应体：unit 文件原文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitFile {
    /// unit 名。
    pub unit: String,
    /// 主 unit 文件。transient unit 无主文件时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fragment: Option<UnitFileFragment>,
    /// drop-in 覆盖文件，按生效顺序。
    #[serde(default)]
    pub drop_ins: Vec<UnitFileFragment>,
}

/// 一个 unit 文件片段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitFileFragment {
    /// 绝对路径。
    #[schema(example = "/usr/lib/systemd/system/nginx.service")]
    pub path: String,
    /// 文件原文（UTF-8）。**不做任何解析或改写**，前端按 ini 高亮即可。
    pub content: String,
}

/// unit 操作。
///
/// 前六个是运行时操作，`enable`/`disable`/`mask`/`unmask` 会改写磁盘上的符号链接。
/// 全部需要 polkit 授权；未提权且 polkit 判定 auth_admin 时返回
/// [`crate::ErrorCode::ElevationRequired`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UnitAction {
    /// 启动。
    Start,
    /// 停止。
    Stop,
    /// 重启。
    Restart,
    /// 重载配置（unit 未声明 `ExecReload` 时会失败，不要静默降级成 restart）。
    Reload,
    /// 开机自启。
    Enable,
    /// 取消开机自启。
    Disable,
    /// 屏蔽（链到 `/dev/null`），此后任何启动尝试都会失败。
    Mask,
    /// 解除屏蔽。
    Unmask,
}

impl UnitAction {
    /// 是否会改写磁盘状态（写符号链接），而非仅影响当前运行时。
    pub const fn is_persistent(self) -> bool {
        matches!(
            self,
            Self::Enable | Self::Disable | Self::Mask | Self::Unmask
        )
    }
}

/// `POST /api/v1/services/{unit}/action` 的请求体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitActionReq {
    /// 要执行的操作。
    pub action: UnitAction,
}

/// `POST /api/v1/services/{unit}/action` 的响应体。
///
/// systemd 的启停是**异步**的：接口返回只代表 job 已入队，不代表服务已经起来。
/// 前端应订阅 WS `services.changed` 频道等待终态，而不是立刻重新拉详情。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitActionResp {
    /// 被操作的 unit 名。
    pub unit: String,
    /// 实际执行的操作。
    pub action: UnitAction,
    /// systemd 返回的 job 对象路径。降级到 `systemctl`（同步执行）时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/org/freedesktop/systemd1/job/4213")]
    pub job: Option<String>,
    /// 操作后立即读到的活动状态。异步 job 尚未完成时通常还是旧值，仅供参考。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_state: Option<UnitActiveState>,
}

/// `GET /api/v1/services` 的查询参数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct UnitListQuery {
    /// 按 unit 类型过滤，取值即后缀名（`service` / `timer` / `socket` …）。
    /// 缺省返回所有类型。
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    #[param(example = "service")]
    pub unit_type: Option<String>,
    /// 按活动状态过滤。缺省不过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<UnitActiveState>,
    /// 按是否开机自启过滤。`true` 匹配 `enabled` / `enabled_runtime`，
    /// `false` 匹配 `disabled`；`static` / `indirect` 等既不匹配 `true` 也不匹配 `false`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// 关键字，对 unit 名与 `description` 做大小写不敏感的子串匹配。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "nginx")]
    pub q: Option<String>,
    /// 作用域，缺省为 [`UnitScope::System`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<UnitScope>,
}
