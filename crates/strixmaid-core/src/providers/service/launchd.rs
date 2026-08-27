//! macOS 的 [`ServiceProvider`] 实现：`launchctl` 子进程。
//!
//! # 概念映射
//!
//! launchd 与 systemd 的模型只重合一半，重合的部分照搬，不重合的部分**如实报缺失**，
//! 不拿相近的东西冒充：
//!
//! | systemd | launchd | 说明 |
//! |---|---|---|
//! | unit 名 | label（`com.apple.Finder`） | 见下「为什么给 label 加 `.service` 后缀」 |
//! | `--system` / `--user` | `system` / `gui/<uid>` 域 | [`UnitScope`] 直接对应 |
//! | active/inactive/failed | 有无 PID + 上次退出码 | [`state_from_list`] |
//! | enabled/disabled | `launchctl print-disabled` | launchd 没有 `static` / `masked` |
//! | unit 文件 | `.plist` | 路径来自 `launchctl print` 的 `path =` |
//! | 依赖图 | **没有** | `unit_deps` 返回 `capability_unavailable` |
//! | cgroup 用量 | **没有** | `UnitDetail::cgroup` 为 `None` |
//! | `PropertiesChanged` 信号 | **没有** | `subscribe` 返回永远安静的 receiver |
//!
//! # 为什么给 label 加 `.service` 后缀
//!
//! DTO 的 `unit_type` 取自 unit 名的最后一段后缀（[`unit_type_of`]），
//! 而 launchd 的 label 是反向域名：直接用 `com.apple.Finder` 会让 `unit_type`
//! 变成 `Finder` 这种垃圾值，`?type=service` 过滤也就废了。
//!
//! 因此对外一律报 `com.apple.Finder.service`，调 `launchctl` 前再剥掉后缀
//! （[`strip_suffix`] / [`to_label`]）。代价是名字比原生的长七个字符，
//! 换来的是**API 契约在两个平台上完全一致**——前端不需要知道自己连的是 Linux 还是 Mac。
//!
//! # 权限
//!
//! 非 root 时 `system` 域不可读（`launchctl print system` 直接拒绝），
//! `gui/<uid>` 域正常。写操作被拒时映射成 `PermissionDenied` 并置 `can_retry_elevated`，
//! 与 Linux 侧 polkit 拒绝的表现一致。

use std::collections::HashMap;

use async_trait::async_trait;
use strixmaid_types::service::{
    UnitAction, UnitActionResp, UnitActiveState, UnitDetail, UnitEnableState, UnitFile,
    UnitFileFragment, UnitListQuery, UnitLoadState, UnitScope, UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::broadcast;

use super::super::{Probe, Provider};
use super::{
    CALL_TIMEOUT, EVENT_CAPACITY, ServiceEvent, ServiceProvider, UnitDeps, apply_list_query,
    unit_type_of, validate_unit_name,
};

/// 对外暴露的 unit 名后缀，见模块文档。
const LABEL_SUFFIX: &str = ".service";

/// `launchctl` 实现。
pub struct Launchctl {
    /// 事件通道。launchd 没有事件源，这里只是为了让 `subscribe()` 返回一个
    /// 不会 `Closed` 的 receiver——WS hub 因此不需要区分实现。
    events: broadcast::Sender<ServiceEvent>,
    /// 当前进程 uid，用于拼 `gui/<uid>` 域。
    uid: u32,
}

impl Default for Launchctl {
    fn default() -> Self {
        Self::new()
    }
}

impl Launchctl {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Launchctl {
            events,
            // SAFETY: getuid 无副作用。
            uid: unsafe { libc::getuid() },
        }
    }

    /// 作用域对应的 launchd 域名。
    fn domain(&self, scope: UnitScope) -> String {
        match scope {
            UnitScope::System => "system".to_owned(),
            UnitScope::User => format!("gui/{}", self.uid),
        }
    }

    /// 跑一次 `launchctl`，带 [`CALL_TIMEOUT`] 超时。
    ///
    /// 返回 `(成功?, stdout, stderr)`。命令本身起不来才返回 `Err`。
    async fn run(&self, args: &[&str]) -> ApiResult<(bool, String, String)> {
        let fut = tokio::process::Command::new("launchctl")
            .args(args)
            .output();
        let out = tokio::time::timeout(CALL_TIMEOUT, fut)
            .await
            .map_err(|_| {
                ApiError::internal(format!("launchctl {} 超时", args.join(" ")))
                    .with_detail(format!("超过 {CALL_TIMEOUT:?}"))
            })?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ApiError::capability_unavailable("launchd", "找不到 launchctl")
                } else {
                    ApiError::internal("启动 launchctl 失败").with_detail(e.to_string())
                }
            })?;
        Ok((
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// `launchctl print-disabled <domain>` → label → 是否被禁用。
    ///
    /// 读不到（例如非 root 读 system 域）时返回空表，此时全部 unit 的
    /// `enable_state` 为 `None`——「不知道」，而不是「已启用」。
    async fn disabled_map(&self, scope: UnitScope) -> HashMap<String, bool> {
        let domain = self.domain(scope);
        match self.run(&["print-disabled", &domain]).await {
            Ok((true, stdout, _)) => parse_print_disabled(&stdout),
            _ => HashMap::new(),
        }
    }
}

#[async_trait]
impl Provider for Launchctl {
    fn id(&self) -> &'static str {
        "launchd"
    }

    /// `launchctl list` 能出结果即可用。
    async fn probe(&self) -> Probe {
        match self.run(&["list"]).await {
            Ok((true, stdout, _)) if stdout.lines().count() > 1 => Probe::Available,
            Ok((_, _, stderr)) => {
                Probe::unavailable(format!("launchctl list 未返回服务：{}", stderr.trim()))
            }
            Err(e) => Probe::unavailable(e.message),
        }
    }
}

#[async_trait]
impl ServiceProvider for Launchctl {
    async fn list_units(&self, query: &UnitListQuery) -> ApiResult<Vec<UnitSummary>> {
        let scope = query.scope.unwrap_or(UnitScope::System);
        let (ok, stdout, stderr) = self.run(&["list"]).await?;
        if !ok {
            return Err(ApiError::internal("launchctl list 失败").with_detail(stderr.trim()));
        }
        let disabled = self.disabled_map(scope).await;
        let units = parse_list(&stdout, scope, &disabled);
        Ok(apply_list_query(units, query))
    }

    async fn unit_detail(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDetail> {
        validate_unit_name(unit)?;
        let label = to_label(unit);
        let target = format!("{}/{label}", self.domain(scope));
        let (ok, stdout, stderr) = self.run(&["print", &target]).await?;
        if !ok {
            return Err(not_found_or_denied(unit, &stderr));
        }
        let printed = PrintOutput::parse(&stdout);
        let disabled = self.disabled_map(scope).await;

        Ok(UnitDetail {
            summary: printed.to_summary(unit, scope, disabled.get(&label).copied()),
            fragment_path: printed.get("path").map(str::to_owned),
            drop_in_paths: Vec::new(),
            main_pid: printed.get("pid").and_then(|v| v.parse().ok()),
            // launchd 不记录状态变更时刻
            active_enter_ts: None,
            state_change_ts: None,
            n_restarts: printed.get("runs").and_then(|v| v.parse().ok()),
            result: printed.get("last exit code").map(str::to_owned),
            exit_code: printed.get("last exit code").and_then(|v| v.parse().ok()),
            documentation: Vec::new(),
            user: printed.get("username").map(str::to_owned),
            // launchd 没有 cgroup 这一层
            cgroup: None,
        })
    }

    async fn unit_file(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitFile> {
        validate_unit_name(unit)?;
        let target = format!("{}/{}", self.domain(scope), to_label(unit));
        let (ok, stdout, stderr) = self.run(&["print", &target]).await?;
        if !ok {
            return Err(not_found_or_denied(unit, &stderr));
        }
        let path = PrintOutput::parse(&stdout)
            .get("path")
            .map(str::to_owned)
            .ok_or_else(|| {
                ApiError::not_found(format!("{unit} 没有对应的 plist 文件")).with_detail(
                    "launchctl print 的输出里没有 path 字段，通常意味着这是一个纯运行时注册的服务",
                )
            })?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::PermissionDenied => {
                    ApiError::permission_denied(format!("没有权限读取 {path}"))
                        .with_detail(e.to_string())
                        .retry_elevated()
                }
                std::io::ErrorKind::NotFound => {
                    ApiError::not_found(format!("plist 文件 {path} 不存在"))
                }
                _ => ApiError::internal(format!("读取 {path} 失败")).with_detail(e.to_string()),
            })?;

        Ok(UnitFile {
            unit: unit.to_owned(),
            fragment: Some(UnitFileFragment { path, content }),
            // launchd 没有 drop-in 机制
            drop_ins: Vec::new(),
        })
    }

    async fn unit_deps(&self, _scope: UnitScope, unit: &str) -> ApiResult<UnitDeps> {
        validate_unit_name(unit)?;
        Err(
            ApiError::capability_unavailable("launchd", "launchd 没有依赖关系图").with_detail(
                "systemd 的 Requires / Wants / After 在 launchd 里没有对应概念；\
             服务之间的先后由 XPC 按需拉起决定，不是可枚举的静态关系。",
            ),
        )
    }

    async fn unit_action(
        &self,
        scope: UnitScope,
        unit: &str,
        action: UnitAction,
    ) -> ApiResult<UnitActionResp> {
        validate_unit_name(unit)?;
        let label = to_label(unit);
        let target = format!("{}/{label}", self.domain(scope));

        let args: Vec<String> = match action {
            // kickstart 对已停的服务是启动，对在跑的是无操作
            UnitAction::Start => vec!["kickstart".into(), target.clone()],
            UnitAction::Stop => vec!["kill".into(), "SIGTERM".into(), target.clone()],
            // -k 表示「先杀掉再拉起」，这才是 restart 的语义
            UnitAction::Restart => vec!["kickstart".into(), "-k".into(), target.clone()],
            UnitAction::Enable => vec!["enable".into(), target.clone()],
            UnitAction::Disable => vec!["disable".into(), target.clone()],
            UnitAction::Reload => {
                return Err(unsupported_action(
                    "reload",
                    "launchd 没有「重载配置」的概念；改了 plist 之后需要 restart",
                ));
            }
            UnitAction::Mask | UnitAction::Unmask => {
                return Err(unsupported_action(
                    if action == UnitAction::Mask {
                        "mask"
                    } else {
                        "unmask"
                    },
                    "launchd 没有 mask（把 unit 链到 /dev/null）这一层；\
                     最接近的是 disable，但它只阻止开机自启，不阻止按需拉起",
                ));
            }
        };

        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let (ok, _, stderr) = self.run(&argv).await?;
        if !ok {
            return Err(action_error(unit, action, &stderr));
        }

        Ok(UnitActionResp {
            unit: unit.to_owned(),
            action,
            // launchctl 是同步执行的，没有 job 对象
            job: None,
            // 操作刚下去，此刻读状态多半还是旧值，不如不报
            active_state: None,
        })
    }

    async fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        // launchd 没有事件流。返回的 receiver 永远收不到消息，但也不会 Closed
        // （self.events 这个 Sender 与 provider 同生命周期）。
        self.events.subscribe()
    }
}

// ---------------------------------------------------------------------------
// 名字转换
// ---------------------------------------------------------------------------

/// 对外的 unit 名 → launchd label（剥掉 `.service` 后缀）。
pub fn to_label(unit: &str) -> String {
    unit.strip_suffix(LABEL_SUFFIX).unwrap_or(unit).to_owned()
}

/// launchd label → 对外的 unit 名（补上 `.service` 后缀）。
pub fn to_unit_name(label: &str) -> String {
    format!("{label}{LABEL_SUFFIX}")
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 由 `launchctl list` 的一行推出运行状态。
///
/// 该行三列是 `PID`、`Status`、`Label`：
/// - PID 是数字 → 正在运行；
/// - PID 是 `-`、Status 为 0 → 正常退出后待命（launchd 的常态，按需拉起）；
/// - PID 是 `-`、Status 非 0 → 上次异常退出。
pub fn state_from_list(pid: &str, status: &str) -> (UnitActiveState, String) {
    if pid.parse::<u32>().is_ok() {
        return (UnitActiveState::Active, "running".to_owned());
    }
    match status.parse::<i32>() {
        Ok(0) | Err(_) => (UnitActiveState::Inactive, "dead".to_owned()),
        Ok(code) => (UnitActiveState::Failed, format!("exited({code})")),
    }
}

/// 解析 `launchctl list` 的输出。首行是表头，跳过。
pub fn parse_list(
    stdout: &str,
    scope: UnitScope,
    disabled: &HashMap<String, bool>,
) -> Vec<UnitSummary> {
    stdout
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let (pid, status, label) = (cols.next()?, cols.next()?, cols.next()?);
            let label = label.trim();
            if label.is_empty() {
                return None;
            }
            let (active_state, sub_state) = state_from_list(pid, status);
            let name = to_unit_name(label);
            Some(UnitSummary {
                unit_type: unit_type_of(&name).to_owned(),
                // launchd 的服务没有描述字段，label 本身就是唯一的说明
                description: label.to_owned(),
                name,
                load_state: UnitLoadState::Loaded,
                active_state,
                sub_state,
                enable_state: disabled.get(label).map(|d| {
                    if *d {
                        UnitEnableState::Disabled
                    } else {
                        UnitEnableState::Enabled
                    }
                }),
                scope,
            })
        })
        .collect()
}

/// 解析 `launchctl print-disabled <domain>` 的输出。
///
/// 形如 `"com.apple.Siri.agent" => enabled`，label 带引号。
pub fn parse_print_disabled(stdout: &str) -> HashMap<String, bool> {
    stdout
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once("=>")?;
            let label = left.trim().trim_matches('"');
            if label.is_empty() {
                return None;
            }
            match right.trim() {
                "disabled" => Some((label.to_owned(), true)),
                "enabled" => Some((label.to_owned(), false)),
                // 还有 "disabled (removed)" 之类的变体，按是否以 disabled 开头判断
                other if other.starts_with("disabled") => Some((label.to_owned(), true)),
                _ => None,
            }
        })
        .collect()
}

/// `launchctl print` 的输出，扁平化成 `key => value`。
///
/// 原始输出是缩进的嵌套块（`environment = { ... }`）。我们只要顶层的标量字段，
/// 因此**只收 `key = value` 且 value 不是 `{` 的行**，嵌套块整体忽略。
/// 这样既不需要写一个真正的解析器，也不会把内层的同名键误当成顶层字段。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PrintOutput {
    fields: HashMap<String, String>,
}

impl PrintOutput {
    /// 解析。
    pub fn parse(stdout: &str) -> PrintOutput {
        let mut fields = HashMap::new();
        let mut depth: i32 = 0;
        for line in stdout.lines() {
            let trimmed = line.trim();
            // 先处理块的进出。首行 `gui/501/com.apple.Finder = {` 会把 depth 抬到 1，
            // 顶层字段因此在 depth == 1 上。
            if trimmed.ends_with('{') {
                depth += 1;
                continue;
            }
            if trimmed == "}" || trimmed == "};" {
                depth -= 1;
                continue;
            }
            if depth != 1 {
                continue;
            }
            if let Some((k, v)) = trimmed.split_once('=') {
                let (k, v) = (k.trim(), v.trim());
                if !k.is_empty() && !v.is_empty() {
                    fields.insert(k.to_owned(), v.to_owned());
                }
            }
        }
        PrintOutput { fields }
    }

    /// 取一个字段。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// 由 print 输出构造 [`UnitSummary`]。
    ///
    /// `state = running` 是最可靠的信号；没有它就退回「有没有 pid」。
    pub fn to_summary(&self, unit: &str, scope: UnitScope, disabled: Option<bool>) -> UnitSummary {
        let running = self.get("state") == Some("running") || self.get("pid").is_some();
        let (active_state, sub_state) = if running {
            (UnitActiveState::Active, "running".to_owned())
        } else {
            match self
                .get("last exit code")
                .and_then(|v| v.parse::<i32>().ok())
            {
                Some(code) if code != 0 => (UnitActiveState::Failed, format!("exited({code})")),
                _ => (UnitActiveState::Inactive, "dead".to_owned()),
            }
        };
        UnitSummary {
            unit_type: unit_type_of(unit).to_owned(),
            description: self
                .get("bundle id")
                .map(str::to_owned)
                .unwrap_or_else(|| to_label(unit)),
            name: unit.to_owned(),
            load_state: UnitLoadState::Loaded,
            active_state,
            sub_state,
            enable_state: disabled.map(|d| {
                if d {
                    UnitEnableState::Disabled
                } else {
                    UnitEnableState::Enabled
                }
            }),
            scope,
        }
    }
}

// ---------------------------------------------------------------------------
// 错误映射
// ---------------------------------------------------------------------------

/// `launchctl print` 失败：分不清「没这个服务」与「没权限看这个域」时按输出判断。
fn not_found_or_denied(unit: &str, stderr: &str) -> ApiError {
    if looks_denied(stderr) {
        return ApiError::permission_denied(format!("没有权限查看 {unit}"))
            .with_detail(stderr.trim())
            .retry_elevated();
    }
    ApiError::not_found(format!("服务 {unit} 不存在或未加载")).with_detail(stderr.trim())
}

/// 操作失败。
fn action_error(unit: &str, action: UnitAction, stderr: &str) -> ApiError {
    // 顺序要紧：权限判断在前。被拒时 launchctl 有时也会说「找不到」——
    // 那是因为无权访问该 domain 而看不见它，语义上仍是权限问题。
    if looks_denied(stderr) {
        return ApiError::permission_denied(format!("系统拒绝对 {unit} 执行 {action:?}"))
            .with_detail(stderr.trim())
            .retry_elevated();
    }
    if looks_missing(stderr) {
        return ApiError::not_found(format!("服务 {unit} 不存在或未加载"))
            .with_detail(stderr.trim());
    }
    ApiError::internal(format!("对 {unit} 执行 {action:?} 失败")).with_detail(stderr.trim())
}

/// launchd 不支持的操作。
fn unsupported_action(action: &str, why: &str) -> ApiError {
    ApiError::capability_unavailable("launchd", format!("launchd 不支持 {action}")).with_detail(why)
}

/// launchctl 的输出看起来是「没有这个服务」。
///
/// `launchctl kickstart` 对不存在的 label 报的是
/// `Could not find service "x" in domain for system`，退出码非零。
/// 这是**调用方给错了名字**，属于 404，不是 500——把它归进「内部错误」
/// 会让前端以为服务端出了故障，而实际上只需要换个正确的 unit 名。
fn looks_missing(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    ["could not find", "no such", "not found", "nosuchprocess"]
        .iter()
        .any(|n| lower.contains(n))
}

/// launchctl 的输出看起来是权限被拒。
fn looks_denied(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "permission denied",
        "not permitted",
        "operation not permitted",
        "eperm",
    ]
    .iter()
    .any(|n| lower.contains(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::ErrorCode;

    const LIST: &str = "PID\tStatus\tLabel
-\t0\tcom.apple.SafariHistoryServiceAgent
1394\t0\tcom.apple.progressd
-\t78\tcom.example.crashed
639\t0\tcom.apple.Finder
";

    const PRINT: &str = r#"gui/501/com.apple.Finder = {
	active count = 7
	path = /System/Library/LaunchAgents/com.apple.Finder.plist
	type = LaunchAgent
	state = running
	bundle id = com.apple.finder

	program = /System/Library/CoreServices/Finder.app/Contents/MacOS/Finder
	inherited environment = {
		SSH_AUTH_SOCK => /var/run/com.apple.launchd.MPBa7K1tvA/Listeners
	}

	environment = {
		XPC_SERVICE_NAME => com.apple.Finder
		path = /this/is/inside/a/block
	}

	pid = 639
	runs = 1
};
"#;

    #[test]
    fn 名字加后缀与剥后缀是可逆的() {
        assert_eq!(to_unit_name("com.apple.Finder"), "com.apple.Finder.service");
        assert_eq!(to_label("com.apple.Finder.service"), "com.apple.Finder");
        // 已经没有后缀时不该再剥一层
        assert_eq!(to_label("com.apple.Finder"), "com.apple.Finder");
        // 加了后缀之后 unit_type 才是有意义的值
        assert_eq!(unit_type_of(&to_unit_name("com.apple.Finder")), "service");
        // 生成的名字必须通过公共校验
        assert!(validate_unit_name(&to_unit_name("com.apple.Finder")).is_ok());
    }

    #[test]
    fn 运行状态推断() {
        assert_eq!(state_from_list("639", "0").0, UnitActiveState::Active);
        assert_eq!(state_from_list("-", "0").0, UnitActiveState::Inactive);
        let (state, sub) = state_from_list("-", "78");
        assert_eq!(state, UnitActiveState::Failed);
        assert_eq!(sub, "exited(78)");
        // 状态列不是数字时按「待命」处理，不编造失败
        assert_eq!(state_from_list("-", "?").0, UnitActiveState::Inactive);
    }

    #[test]
    fn 解析_list() {
        let mut disabled = HashMap::new();
        disabled.insert("com.apple.Finder".to_owned(), false);
        disabled.insert("com.example.crashed".to_owned(), true);

        let units = parse_list(LIST, UnitScope::System, &disabled);
        assert_eq!(units.len(), 4, "表头要跳过");

        let finder = units
            .iter()
            .find(|u| u.name == "com.apple.Finder.service")
            .unwrap();
        assert_eq!(finder.active_state, UnitActiveState::Active);
        assert_eq!(finder.sub_state, "running");
        assert_eq!(finder.enable_state, Some(UnitEnableState::Enabled));
        assert_eq!(finder.unit_type, "service");

        let crashed = units
            .iter()
            .find(|u| u.name == "com.example.crashed.service")
            .unwrap();
        assert_eq!(crashed.active_state, UnitActiveState::Failed);
        assert_eq!(crashed.enable_state, Some(UnitEnableState::Disabled));

        // print-disabled 里没提到的，enable_state 是「不知道」而不是「已启用」
        let unknown = units
            .iter()
            .find(|u| u.name == "com.apple.progressd.service")
            .unwrap();
        assert_eq!(unknown.enable_state, None);
    }

    #[test]
    fn 解析_print_disabled() {
        let raw = r#"
	disabled services = {
		"com.apple.ManagedClientAgent.enrollagent" => disabled
		"88L2Q4487U.com.tencent.WeWorkMac.IPCHelper" => enabled
		"com.apple.Siri.agent" => enabled
		"com.old.removed" => disabled (removed)
	}
"#;
        let m = parse_print_disabled(raw);
        assert_eq!(
            m.get("com.apple.ManagedClientAgent.enrollagent"),
            Some(&true)
        );
        assert_eq!(m.get("com.apple.Siri.agent"), Some(&false));
        assert_eq!(
            m.get("88L2Q4487U.com.tencent.WeWorkMac.IPCHelper"),
            Some(&false),
            "带团队 ID 前缀的 label 也要认"
        );
        assert_eq!(m.get("com.old.removed"), Some(&true), "disabled 的变体");
        assert_eq!(m.len(), 4, "`disabled services = {{` 那行不是条目");
    }

    #[test]
    fn 解析_print_只取顶层字段() {
        let p = PrintOutput::parse(PRINT);
        assert_eq!(
            p.get("path"),
            Some("/System/Library/LaunchAgents/com.apple.Finder.plist"),
            "嵌套块里的同名 path 不能覆盖顶层的"
        );
        assert_eq!(p.get("state"), Some("running"));
        assert_eq!(p.get("pid"), Some("639"));
        assert_eq!(p.get("bundle id"), Some("com.apple.finder"));
        assert_eq!(p.get("runs"), Some("1"));
        // 嵌套块内的键不该出现在顶层
        assert_eq!(p.get("XPC_SERVICE_NAME"), None);
        assert_eq!(p.get("SSH_AUTH_SOCK"), None);
    }

    #[test]
    fn print_转_summary() {
        let p = PrintOutput::parse(PRINT);
        let s = p.to_summary("com.apple.Finder.service", UnitScope::User, Some(false));
        assert_eq!(s.active_state, UnitActiveState::Active);
        assert_eq!(s.description, "com.apple.finder");
        assert_eq!(s.enable_state, Some(UnitEnableState::Enabled));
        assert_eq!(s.scope, UnitScope::User);

        // 没在跑且上次非零退出 → failed
        let stopped = PrintOutput::parse("x = {\n\tlast exit code = 2\n};\n");
        let s = stopped.to_summary("a.service", UnitScope::System, None);
        assert_eq!(s.active_state, UnitActiveState::Failed);
        assert_eq!(s.sub_state, "exited(2)");
        // 没在跑且上次正常退出 → inactive（launchd 的常态）
        let idle = PrintOutput::parse("x = {\n\tlast exit code = 0\n};\n");
        assert_eq!(
            idle.to_summary("a.service", UnitScope::System, None)
                .active_state,
            UnitActiveState::Inactive
        );
    }

    /// 不存在的 unit 必须是 404，不能是 500。
    ///
    /// 这条是补票：验收脚本用 `cron.service` 探测提权门禁时，macOS 上没有这个
    /// 服务，launchctl 回「Could not find service」，而 `action_error` 当时只认
    /// 「被拒」，其余一律 internal——于是「名字写错了」被报成了「服务端故障」。
    #[test]
    fn 找不到服务时报_404_而不是_500() {
        let e = action_error(
            "cron.service",
            UnitAction::Restart,
            "Could not find service \"cron\" in domain for system",
        );
        assert_eq!(e.code, ErrorCode::NotFound, "detail={:?}", e.detail);
        assert!(e.detail.is_some(), "要把 launchctl 的原话带上");

        // 权限判断优先：无权访问某个 domain 时 launchctl 也可能说「找不到」
        let e = action_error("x.service", UnitAction::Start, "Operation not permitted");
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);

        // 真正说不清的失败仍是 500
        let e = action_error("x.service", UnitAction::Start, "Input/output error");
        assert_eq!(e.code, ErrorCode::Internal);
    }

    #[test]
    fn 找不到的判据() {
        assert!(looks_missing("Could not find service \"cron\" in domain for system"));
        assert!(looks_missing("No such process"));
        assert!(!looks_missing("Operation not permitted"));
        assert!(!looks_missing(""));
    }

    #[test]
    fn 权限拒绝识别() {
        assert!(looks_denied(
            "Could not print domain: 1: Operation not permitted"
        ));
        assert!(looks_denied("Permission denied"));
        assert!(!looks_denied("Could not find service"));
    }

    #[tokio::test]
    async fn 本机探测与列表() {
        let l = Launchctl::new();
        assert_eq!(l.id(), "launchd");
        assert_eq!(l.probe().await, Probe::Available);

        let units = l
            .list_units(&UnitListQuery {
                scope: Some(UnitScope::User),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!units.is_empty(), "本机至少有一堆用户级 agent");
        assert!(units.iter().all(|u| u.name.ends_with(".service")));
        assert!(units.iter().all(|u| u.unit_type == "service"));
        eprintln!("本机 launchd 服务 {} 个，前 3 个：", units.len());
        for u in units.iter().take(3) {
            eprintln!("  {} {:?} {}", u.name, u.active_state, u.sub_state);
        }
    }

    #[tokio::test]
    async fn 不支持的操作报能力缺失而不是失败() {
        use strixmaid_types::ErrorCode;
        let l = Launchctl::new();
        for action in [UnitAction::Reload, UnitAction::Mask, UnitAction::Unmask] {
            let err = l
                .unit_action(UnitScope::User, "com.example.x.service", action)
                .await
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::CapabilityUnavailable, "{action:?}");
            assert!(err.detail.is_some(), "{action:?} 必须说明为什么不支持");
        }
        // 依赖图同理
        let err = l
            .unit_deps(UnitScope::User, "com.example.x.service")
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::CapabilityUnavailable);
    }

    #[tokio::test]
    async fn 订阅返回永不关闭的_receiver() {
        let l = Launchctl::new();
        let mut rx = l.subscribe().await;
        // 没有事件源，但通道必须是打开的（不是 Closed）
        assert!(matches!(
            rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
    }
}
