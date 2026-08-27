//! ServiceProvider：systemd unit 的列表 / 详情 / unit 文件 / 依赖 / 操作 / 变更事件。
//!
//! 两条实现路径（`docs/design.md` §4）：
//!
//! - [`bus::SystemdBus`]：主路径，`zbus` 直连 `org.freedesktop.systemd1`。有属性信号与 job 事件，
//!   `services.changed` 频道靠它驱动；cgroup 用量直读 `/sys/fs/cgroup`。
//! - [`cli::SystemctlCli`]：降级路径，`systemctl ... --output=json` 子进程（需 systemd ≥ 246）。
//!   没有事件流，`subscribe()` 返回一个永远安静的 receiver。
//!
//! 两条路径必须产出**同一套** DTO（`strixmaid_types::service`），因此过滤、枚举解析、
//! unit 名校验这些与来源无关的逻辑全部放在本文件，两个实现只负责「取原始数据」。
//!
//! # 作用域与 uid
//!
//! [`UnitScope::User`] 走 session bus / `systemctl --user`。session bus 用 EXTERNAL(uid) 认证，
//! root 主进程连不上其他用户的 bus（`docs/design.md` §5.4）。因此这里只实现「本进程 uid 自己的
//! user manager」，构造时可用 `with_user_uid` 指定 uid——跨用户由 worker 机制解决：worker 以登录
//! 用户身份运行，在它里面构造的 provider 天然就是那个用户的。

pub mod bus;
pub mod cgroup;
pub mod cli;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use strixmaid_types::service::{
    UnitAction, UnitActionResp, UnitActiveState, UnitDetail, UnitEnableState, UnitFile,
    UnitListQuery, UnitLoadState, UnitScope, UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::broadcast;

use super::Provider;

/// 单次 bus / 子进程调用的超时。systemd 正常响应在毫秒级，超过这个值基本是 systemd 卡住了
/// （例如 `daemon-reload` 期间或 D-Bus 代理无响应），此时返回 504 比无限等强。
pub const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 变更事件去抖窗口。一次 restart 会连发十几条 `PropertiesChanged`，
/// 200ms 内的信号合并成一批，再统一取一次当前状态。
pub const EVENT_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(200);

/// 事件 broadcast 通道容量。慢消费者（WS 连接写不动）会收到 `Lagged`，
/// 丢掉的只是中间状态，下一批事件仍是最新状态，所以容量不用大。
pub const EVENT_CAPACITY: usize = 64;

/// unit 依赖关系。属性名与 systemd `org.freedesktop.systemd1.Unit` 接口一一对应。
///
/// server 侧对应 `GET /api/v1/services/{unit}/deps` 的响应 DTO
/// （types crate 里没有这一项，暂由 server 路由文件定义并转换）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnitDeps {
    /// unit 名。
    pub unit: String,
    /// 强依赖：本 unit 启动时它们必须成功启动。
    pub requires: Vec<String>,
    /// 类似 `requires`，但要求对方**已经**处于 active。
    pub requisite: Vec<String>,
    /// 弱依赖：尽量拉起，失败不影响本 unit。
    pub wants: Vec<String>,
    /// 绑定：对方停止时本 unit 也停止。
    pub binds_to: Vec<String>,
    /// 从属：对方停止 / 重启时本 unit 跟随。
    pub part_of: Vec<String>,
    /// 反向：哪些 unit `Requires` 本 unit。
    pub required_by: Vec<String>,
    /// 反向：哪些 unit `Wants` 本 unit。
    pub wanted_by: Vec<String>,
    /// 反向：哪些 unit `BindsTo` 本 unit。
    pub bound_by: Vec<String>,
    /// 互斥：本 unit 启动时它们会被停止。
    pub conflicts: Vec<String>,
    /// 反向互斥。
    pub conflicted_by: Vec<String>,
    /// 顺序：本 unit 在它们之前启动。
    pub before: Vec<String>,
    /// 顺序：本 unit 在它们之后启动。
    pub after: Vec<String>,
    /// 本 unit（socket / timer / path）触发的 unit。
    pub triggers: Vec<String>,
    /// 触发本 unit 的 unit。
    pub triggered_by: Vec<String>,
}

/// `services.changed` 频道的一条推送：**发生变化**的 unit 的当前状态。
///
/// 序列化为 `UnitSummary` 数组（`#[serde(transparent)]`），与
/// `strixmaid_types::ws::WsChannel::ServicesChanged` 文档描述的 payload 形状一致；
/// 每个 [`UnitSummary`] 自带 `scope`，所以不再单独带作用域字段。
///
/// 被 systemd 从内存移除的 unit（transient unit 跑完、session scope 结束）以
/// `load_state = not_found` / `active_state = inactive` / `sub_state = "dead"` 出现——
/// 这与 `systemctl status` 对已消失 unit 的报告一致，前端可据此删行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceEvent {
    /// 变化的 unit，去抖窗口内同一 unit 只出现一次。
    pub units: Vec<UnitSummary>,
}

/// 服务管理能力。
///
/// 除 [`Self::subscribe`] 外，所有方法都接受 `scope`；实现要把 [`UnitScope::User`]
/// 映射到本进程 uid 的 user manager，连不上时返回 `ErrorCode::Unavailable`。
#[async_trait]
pub trait ServiceProvider: Provider {
    /// 列出 unit（已加载的 + 仅存在于磁盘的 unit 文件），并按 `query` 过滤。
    async fn list_units(&self, query: &UnitListQuery) -> ApiResult<Vec<UnitSummary>>;

    /// unit 详情，含 cgroup 用量（bus 路径直读 `/sys/fs/cgroup`）。
    async fn unit_detail(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDetail>;

    /// unit 文件原文：主文件 + drop-in。
    async fn unit_file(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitFile>;

    /// 依赖关系。
    async fn unit_deps(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDeps>;

    /// 执行操作。授权由 polkit 裁决：被拒时返回 `ErrorCode::PermissionDenied`
    /// 并置 `can_retry_elevated`。
    async fn unit_action(
        &self,
        scope: UnitScope,
        unit: &str,
        action: UnitAction,
    ) -> ApiResult<UnitActionResp>;

    /// 订阅变更事件（`services.changed`）。
    ///
    /// bus 路径：首次调用时才向 systemd `Subscribe()` 并启动监听任务，之后常驻。
    /// CLI 路径：返回的 receiver 永远收不到消息，但也不会 `Closed`——
    /// WS hub 不需要区分两种实现。
    async fn subscribe(&self) -> broadcast::Receiver<ServiceEvent>;
}

/// 选择 service provider：先试 bus，失败降级 CLI，都不行返回 `None`
/// （此时 capabilities 的 `systemd` 为 `false`，服务页整体隐藏）。
pub async fn pick_service_provider() -> Option<Arc<dyn ServiceProvider>> {
    match bus::SystemdBus::connect().await {
        Ok(b) => match b.probe().await {
            super::Probe::Available => {
                tracing::info!("service provider: systemd (zbus)");
                return Some(Arc::new(b));
            }
            other => tracing::warn!(
                ?other,
                "systemd bus 可连但 systemd1 无 owner，尝试 systemctl"
            ),
        },
        Err(e) => tracing::warn!(error = %e, "连接 system bus 失败，尝试 systemctl"),
    }

    let cli = cli::SystemctlCli::new();
    match cli.probe().await {
        super::Probe::Unavailable { reason } => {
            tracing::warn!(reason, "systemctl 也不可用，服务能力关闭");
            None
        }
        probe => {
            tracing::info!(?probe, "service provider: systemctl (降级)");
            Some(Arc::new(cli))
        }
    }
}

// ---------------------------------------------------------------------------
// 与数据来源无关的公共逻辑
// ---------------------------------------------------------------------------

/// systemd 认识的 unit 类型后缀。
pub const UNIT_TYPES: &[&str] = &[
    "service",
    "socket",
    "device",
    "mount",
    "automount",
    "swap",
    "target",
    "path",
    "timer",
    "slice",
    "scope",
];

/// 取 unit 名的类型后缀（`nginx.service` → `service`）。没有点时返回空串。
pub fn unit_type_of(name: &str) -> &str {
    name.rsplit_once('.').map(|(_, t)| t).unwrap_or("")
}

/// 校验 unit 名：字符集 `[A-Za-z0-9:_.@\\-]`、长度 ≤ 255、后缀必须是已知类型。
///
/// 这不是安全边界（子进程参数走 `Command::arg`，bus 调用有类型），而是为了对乱输入
/// 早点返回 400，而不是把一个 `../../etc/passwd` 交给 systemd 去报「no such unit」。
pub fn validate_unit_name(name: &str) -> ApiResult<()> {
    if name.is_empty() || name.len() > 255 {
        return Err(ApiError::invalid_request("unit 名为空或过长"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'_' | b'.' | b'@' | b'\\' | b'-'))
    {
        return Err(ApiError::invalid_request(format!(
            "unit 名含非法字符: {name}"
        )));
    }
    let ty = unit_type_of(name);
    if !UNIT_TYPES.contains(&ty) {
        return Err(ApiError::invalid_request(format!(
            "unit 名必须带类型后缀（如 .service），收到: {name}"
        )));
    }
    Ok(())
}

/// 解析 systemd `LoadState` 字符串。
pub fn parse_load_state(s: &str) -> UnitLoadState {
    match s {
        "loaded" => UnitLoadState::Loaded,
        "not-found" => UnitLoadState::NotFound,
        "bad-setting" => UnitLoadState::BadSetting,
        "error" => UnitLoadState::Error,
        "masked" => UnitLoadState::Masked,
        _ => UnitLoadState::Unknown,
    }
}

/// 解析 systemd `ActiveState` 字符串。
pub fn parse_active_state(s: &str) -> UnitActiveState {
    match s {
        "active" => UnitActiveState::Active,
        "reloading" => UnitActiveState::Reloading,
        "inactive" => UnitActiveState::Inactive,
        "failed" => UnitActiveState::Failed,
        "activating" => UnitActiveState::Activating,
        "deactivating" => UnitActiveState::Deactivating,
        _ => UnitActiveState::Unknown,
    }
}

/// 解析 systemd `UnitFileState` 字符串。空串表示「systemd 没给」，返回 `None`。
pub fn parse_enable_state(s: &str) -> Option<UnitEnableState> {
    Some(match s {
        "" => return None,
        "enabled" => UnitEnableState::Enabled,
        "enabled-runtime" => UnitEnableState::EnabledRuntime,
        "linked" => UnitEnableState::Linked,
        "linked-runtime" => UnitEnableState::LinkedRuntime,
        "alias" => UnitEnableState::Alias,
        "masked" => UnitEnableState::Masked,
        "masked-runtime" => UnitEnableState::MaskedRuntime,
        "static" => UnitEnableState::Static,
        "indirect" => UnitEnableState::Indirect,
        "disabled" => UnitEnableState::Disabled,
        "generated" => UnitEnableState::Generated,
        "transient" => UnitEnableState::Transient,
        _ => UnitEnableState::Unknown,
    })
}

/// 用 `ListUnitFiles` / `list-unit-files` 的结果推断一个 unit 的启用状态。
///
/// 实例 unit（`getty@tty1.service`）本身不在 unit 文件列表里，回落到模板（`getty@.service`）。
pub fn lookup_enable_state(
    files: &std::collections::HashMap<String, String>,
    name: &str,
) -> Option<UnitEnableState> {
    if let Some(s) = files.get(name) {
        return parse_enable_state(s);
    }
    let (prefix, rest) = name.split_once('@')?;
    let ty = unit_type_of(rest);
    files
        .get(&format!("{prefix}@.{ty}"))
        .and_then(|s| parse_enable_state(s))
}

/// unit 文件路径 → unit 名。跳过模板（`foo@.service`），它们不是可操作的 unit。
pub fn unit_file_basename(path: &str) -> Option<&str> {
    let name = path.rsplit('/').next()?;
    if name.contains("@.") {
        return None;
    }
    Some(name)
}

/// 为「只有 unit 文件、尚未加载进内存」的 unit 造一条摘要。
///
/// `systemctl status` 对这种 unit 报 `Loaded: loaded (...; disabled)` / `Active: inactive (dead)`，
/// 这里与之一致；masked 的报 `masked`。描述没法不加载就拿到，回落成 unit 名（systemd 自身的做法）。
pub fn summary_for_unloaded_file(name: &str, state: &str, scope: UnitScope) -> UnitSummary {
    let enable_state = parse_enable_state(state);
    let load_state = match enable_state {
        Some(UnitEnableState::Masked | UnitEnableState::MaskedRuntime) => UnitLoadState::Masked,
        _ => UnitLoadState::Loaded,
    };
    UnitSummary {
        name: name.to_owned(),
        unit_type: unit_type_of(name).to_owned(),
        description: name.to_owned(),
        load_state,
        active_state: UnitActiveState::Inactive,
        sub_state: "dead".to_owned(),
        enable_state,
        scope,
    }
}

/// 为已从 systemd 内存中消失、磁盘上也没有文件的 unit 造一条摘要（见 [`ServiceEvent`]）。
pub fn summary_for_vanished(name: &str, scope: UnitScope) -> UnitSummary {
    UnitSummary {
        name: name.to_owned(),
        unit_type: unit_type_of(name).to_owned(),
        description: name.to_owned(),
        load_state: UnitLoadState::NotFound,
        active_state: UnitActiveState::Inactive,
        sub_state: "dead".to_owned(),
        enable_state: None,
        scope,
    }
}

/// 按 [`UnitListQuery`] 过滤并按名字排序。两条路径共用，保证过滤语义一致。
pub fn apply_list_query(mut units: Vec<UnitSummary>, q: &UnitListQuery) -> Vec<UnitSummary> {
    let needle = q.q.as_deref().map(str::to_lowercase);
    units.retain(|u| {
        if let Some(t) = &q.unit_type
            && u.unit_type != *t
        {
            return false;
        }
        if let Some(s) = q.state
            && u.active_state != s
        {
            return false;
        }
        if let Some(enabled) = q.enabled {
            let matches = match u.enable_state {
                Some(UnitEnableState::Enabled | UnitEnableState::EnabledRuntime) => enabled,
                Some(UnitEnableState::Disabled) => !enabled,
                // static / indirect / None：既不算 enabled 也不算 disabled。
                _ => false,
            };
            if !matches {
                return false;
            }
        }
        if let Some(n) = &needle
            && !u.name.to_lowercase().contains(n)
            && !u.description.to_lowercase().contains(n)
        {
            return false;
        }
        true
    });
    units.sort_by(|a, b| a.name.cmp(&b.name));
    units
}

/// 把一个 `Future` 套上 [`CALL_TIMEOUT`]，超时映射为 `ErrorCode::Timeout`。
pub(crate) async fn with_timeout<T>(
    what: &str,
    fut: impl std::future::Future<Output = ApiResult<T>>,
) -> ApiResult<T> {
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(ApiError::new(
            strixmaid_types::ErrorCode::Timeout,
            format!("{what} 超时（{}s）", CALL_TIMEOUT.as_secs()),
        )),
    }
}

/// 读 unit 文件原文。非 UTF-8 内容做有损转换而不是报错——unit 文件里偶尔有 Latin-1 注释。
pub(crate) async fn read_unit_fragment(
    path: &str,
) -> ApiResult<strixmaid_types::service::UnitFileFragment> {
    let bytes = tokio::fs::read(path).await.map_err(|e| {
        let msg = format!("读取 {path} 失败");
        match e.kind() {
            std::io::ErrorKind::NotFound => ApiError::not_found(msg),
            std::io::ErrorKind::PermissionDenied => {
                ApiError::permission_denied(msg).retry_elevated()
            }
            _ => ApiError::internal(msg).with_detail(e.to_string()),
        }
    })?;
    Ok(strixmaid_types::service::UnitFileFragment {
        path: path.to_owned(),
        content: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

/// 微秒 epoch（systemd 的 `*Timestamp` 属性）→ unix 秒；0 表示「从未发生」→ `None`。
pub(crate) fn usec_to_ts(usec: u64) -> Option<i64> {
    (usec != 0).then_some((usec / 1_000_000) as i64)
}

/// systemd 用 `u64::MAX` 表示「未设置 / 不适用」（`MemoryMax=infinity`、`MemoryCurrent=[not set]`）。
pub(crate) fn opt_u64(v: u64) -> Option<u64> {
    (v != u64::MAX).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn unit(name: &str, active: UnitActiveState, enable: Option<UnitEnableState>) -> UnitSummary {
        UnitSummary {
            name: name.into(),
            unit_type: unit_type_of(name).into(),
            description: format!("desc of {name}"),
            load_state: UnitLoadState::Loaded,
            active_state: active,
            sub_state: "running".into(),
            enable_state: enable,
            scope: UnitScope::System,
        }
    }

    #[test]
    fn unit_type_and_validation() {
        assert_eq!(unit_type_of("nginx.service"), "service");
        assert_eq!(unit_type_of("dev-sda1.device"), "device");
        assert_eq!(unit_type_of("noext"), "");
        assert!(validate_unit_name("ssh.service").is_ok());
        assert!(validate_unit_name("getty@tty1.service").is_ok());
        assert!(validate_unit_name("dev-disk-by\\x2dlabel-root.device").is_ok());
        assert!(validate_unit_name("-.mount").is_ok());
        assert!(validate_unit_name("").is_err());
        assert!(validate_unit_name("nginx").is_err());
        assert!(validate_unit_name("../etc/passwd.service").is_err());
        assert!(validate_unit_name("a b.service").is_err());
    }

    #[test]
    fn state_parsers() {
        assert_eq!(parse_load_state("not-found"), UnitLoadState::NotFound);
        assert_eq!(parse_load_state("bad-setting"), UnitLoadState::BadSetting);
        assert_eq!(parse_load_state("whatever"), UnitLoadState::Unknown);
        assert_eq!(
            parse_active_state("deactivating"),
            UnitActiveState::Deactivating
        );
        assert_eq!(parse_enable_state(""), None);
        assert_eq!(
            parse_enable_state("enabled-runtime"),
            Some(UnitEnableState::EnabledRuntime)
        );
        assert_eq!(parse_enable_state("bad"), Some(UnitEnableState::Unknown));
    }

    #[test]
    fn enable_state_lookup_falls_back_to_template() {
        let files: HashMap<String, String> = [
            ("getty@.service".to_owned(), "enabled".to_owned()),
            ("ssh.service".to_owned(), "disabled".to_owned()),
        ]
        .into();
        assert_eq!(
            lookup_enable_state(&files, "ssh.service"),
            Some(UnitEnableState::Disabled)
        );
        assert_eq!(
            lookup_enable_state(&files, "getty@tty1.service"),
            Some(UnitEnableState::Enabled)
        );
        assert_eq!(lookup_enable_state(&files, "nope.service"), None);
        assert_eq!(
            unit_file_basename("/usr/lib/systemd/system/getty@.service"),
            None
        );
        assert_eq!(
            unit_file_basename("/etc/systemd/system/foo.service"),
            Some("foo.service")
        );
    }

    #[test]
    fn list_query_filters() {
        let units = vec![
            unit(
                "b.service",
                UnitActiveState::Active,
                Some(UnitEnableState::Enabled),
            ),
            unit(
                "a.service",
                UnitActiveState::Inactive,
                Some(UnitEnableState::Disabled),
            ),
            unit(
                "c.timer",
                UnitActiveState::Active,
                Some(UnitEnableState::Static),
            ),
            unit("d.socket", UnitActiveState::Failed, None),
        ];

        let all = apply_list_query(units.clone(), &UnitListQuery::default());
        assert_eq!(
            all.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
            ["a.service", "b.service", "c.timer", "d.socket"]
        );

        let q = UnitListQuery {
            unit_type: Some("service".into()),
            ..Default::default()
        };
        assert_eq!(apply_list_query(units.clone(), &q).len(), 2);

        let q = UnitListQuery {
            state: Some(UnitActiveState::Failed),
            ..Default::default()
        };
        assert_eq!(apply_list_query(units.clone(), &q)[0].name, "d.socket");

        // static 既不是 enabled 也不是 disabled。
        let q = UnitListQuery {
            enabled: Some(true),
            ..Default::default()
        };
        assert_eq!(apply_list_query(units.clone(), &q).len(), 1);
        let q = UnitListQuery {
            enabled: Some(false),
            ..Default::default()
        };
        assert_eq!(apply_list_query(units.clone(), &q).len(), 1);

        let q = UnitListQuery {
            q: Some("DESC OF C".into()),
            ..Default::default()
        };
        assert_eq!(apply_list_query(units, &q)[0].name, "c.timer");
    }

    #[test]
    fn synthetic_summaries() {
        let s = summary_for_unloaded_file("foo.service", "masked", UnitScope::System);
        assert_eq!(s.load_state, UnitLoadState::Masked);
        assert_eq!(s.enable_state, Some(UnitEnableState::Masked));
        let s = summary_for_vanished("run-u1.service", UnitScope::User);
        assert_eq!(s.load_state, UnitLoadState::NotFound);
        assert_eq!(s.scope, UnitScope::User);
        // ServiceEvent 是透明数组。
        let ev = ServiceEvent { units: vec![s] };
        let json = serde_json::to_value(&ev).unwrap();
        assert!(json.is_array());
        assert_eq!(json[0]["name"], "run-u1.service");
    }

    #[test]
    fn helpers() {
        assert_eq!(usec_to_ts(0), None);
        assert_eq!(usec_to_ts(1_787_784_865_018_322), Some(1_787_784_865));
        assert_eq!(opt_u64(u64::MAX), None);
        assert_eq!(opt_u64(7), Some(7));
    }
}
