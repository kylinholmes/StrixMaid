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
    TimerEntry, TimerSource, UnitAction, UnitActionResp, UnitActiveState, UnitDetail,
    UnitEnableState, UnitFile, UnitFileFragment, UnitListQuery, UnitLoadState, UnitScope,
    UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::sync::broadcast;

use super::super::{Probe, Provider};
use super::{
    CALL_TIMEOUT, EVENT_CAPACITY, ServiceEvent, ServiceProvider, UnitDeps, apply_list_query,
    unit_type_of,
};

/// 对外暴露的 unit 名后缀，见模块文档。
const LABEL_SUFFIX: &str = ".service";

/// 人话描述缓存的有效期。构建一次要解析几百个 plist，不能每次列表都做。
const FRIENDLY_TTL: std::time::Duration = std::time::Duration::from_secs(300);

/// `launchctl` 实现。
pub struct Launchctl {
    /// 事件通道。launchd 没有事件源，这里只是为了让 `subscribe()` 返回一个
    /// 不会 `Closed` 的 receiver——WS hub 因此不需要区分实现。
    events: broadcast::Sender<ServiceEvent>,
    /// 当前进程 uid，用于拼 `gui/<uid>` 域。
    uid: u32,
    /// label → 人话描述 的缓存（[`Friendly`]）。
    friendly: std::sync::Mutex<Option<(std::time::Instant, std::sync::Arc<Friendly>)>>,
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
            friendly: std::sync::Mutex::new(None),
        }
    }

    /// 取（或重建）人话描述映射。
    async fn friendly_names(&self) -> std::sync::Arc<Friendly> {
        if let Some((at, map)) = self
            .friendly
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            && at.elapsed() < FRIENDLY_TTL
        {
            return map;
        }
        let map = std::sync::Arc::new(build_friendly().await);
        *self.friendly.lock().unwrap_or_else(|p| p.into_inner()) =
            Some((std::time::Instant::now(), std::sync::Arc::clone(&map)));
        map
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

    /// 另一个作用域(system ⇄ user)。
    fn other(scope: UnitScope) -> UnitScope {
        match scope {
            UnitScope::System => UnitScope::User,
            UnitScope::User => UnitScope::System,
        }
    }

    /// `launchctl print` 出某个 label,先按请求的 scope 找,「找不到」再退到另一个域。
    ///
    /// 为什么要退：`launchctl list`(列表页的数据源)是**跨域**的——以用户身份跑时
    /// 一次列出用户的 GUI agent(真实域 `gui/<uid>`)和系统 daemon(`system`)。
    /// 列表里的 `scope` 只是「请求视角」,未必是该服务真正所在的域,因此详情/文件
    /// 必须两个域都试过才能断定「不存在」。返回**实际生效的域** + print 输出。
    async fn print_resolving(
        &self,
        scope: UnitScope,
        unit: &str,
    ) -> ApiResult<(UnitScope, String)> {
        let label = to_label(unit);
        let primary = format!("{}/{label}", self.domain(scope));
        let (ok, stdout, stderr) = self.run(&["print", &primary]).await?;
        if ok {
            return Ok((scope, stdout));
        }
        // 权限被拒是确定性结论,不必再换域试
        if looks_denied(&stderr) {
            return Err(not_found_or_denied(unit, &stderr));
        }
        let alt = Self::other(scope);
        let alt_target = format!("{}/{label}", self.domain(alt));
        let (ok2, stdout2, stderr2) = self.run(&["print", &alt_target]).await?;
        if ok2 {
            return Ok((alt, stdout2));
        }
        Err(not_found_or_denied(unit, &stderr2))
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
        let friendly = self.friendly_names().await;
        let units = parse_list(&stdout, scope, &disabled, Some(&friendly));
        Ok(apply_list_query(units, query))
    }

    async fn unit_detail(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDetail> {
        validate_label(unit)?;
        let label = to_label(unit);
        let (found_scope, stdout) = match self.print_resolving(scope, unit).await {
            Ok(v) => v,
            // `launchctl print` 只认已加载的 job。定时任务表来自 plist 扫描,
            // 盘上有文件但没加载的(atrun 这类默认关闭的 daemon)也得能看详情——
            // 语义对齐 systemd 的「文件在盘上但未加载」(inactive/dead)。
            Err(e) if e.code == ErrorCode::NotFound => {
                let Some((file_scope, path)) = find_plist(&label, scope).await else {
                    return Err(e);
                };
                let disabled = self.disabled_map(file_scope).await;
                let enable_state = disabled.get(&label).map(|d| {
                    if *d {
                        UnitEnableState::Disabled
                    } else {
                        UnitEnableState::Enabled
                    }
                });
                return Ok(UnitDetail {
                    summary: UnitSummary {
                        unit_type: unit_type_of(unit).to_owned(),
                        description: label.clone(),
                        name: unit.to_owned(),
                        load_state: UnitLoadState::Loaded,
                        active_state: UnitActiveState::Inactive,
                        sub_state: "dead".to_owned(),
                        enable_state,
                        scope: file_scope,
                    },
                    fragment_path: Some(path),
                    drop_in_paths: Vec::new(),
                    main_pid: None,
                    active_enter_ts: None,
                    state_change_ts: None,
                    n_restarts: None,
                    result: None,
                    exit_code: None,
                    documentation: Vec::new(),
                    user: None,
                    cgroup: None,
                });
            }
            Err(e) => return Err(e),
        };
        let printed = PrintOutput::parse(&stdout);
        let disabled = self.disabled_map(found_scope).await;

        Ok(UnitDetail {
            summary: printed.to_summary(unit, found_scope, disabled.get(&label).copied()),
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
        validate_label(unit)?;
        let path = match self.print_resolving(scope, unit).await {
            Ok((_, stdout)) => PrintOutput::parse(&stdout)
                .get("path")
                .map(str::to_owned)
                .ok_or_else(|| {
                    ApiError::not_found(format!("{unit} 没有对应的 plist 文件")).with_detail(
                        "launchctl print 的输出里没有 path 字段，通常意味着这是一个纯运行时注册的服务",
                    )
                })?,
            // 未加载但盘上有 plist 的 job(见 unit_detail 的同款回退)
            Err(e) if e.code == ErrorCode::NotFound => {
                let Some((_, path)) = find_plist(&to_label(unit), scope).await else {
                    return Err(e);
                };
                path
            }
            Err(e) => return Err(e),
        };
        let content = read_plist_text(&path).await?;

        Ok(UnitFile {
            unit: unit.to_owned(),
            fragment: Some(UnitFileFragment { path, content }),
            // launchd 没有 drop-in 机制
            drop_ins: Vec::new(),
        })
    }

    async fn unit_deps(&self, _scope: UnitScope, unit: &str) -> ApiResult<UnitDeps> {
        validate_label(unit)?;
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
        validate_label(unit)?;
        // 不支持的操作先判——它们与 unit 是否存在无关,不该被域解析的 404 抢答
        match action {
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
            _ => {}
        }
        let label = to_label(unit);
        // 先解析服务实际所在的域(列表的 scope 只是请求视角,见 print_resolving),
        // 对着错误的域 kickstart 会得到误导性的「找不到」
        let (found_scope, _) = self.print_resolving(scope, unit).await?;
        let target = format!("{}/{label}", self.domain(found_scope));

        let args: Vec<String> = match action {
            // kickstart 对已停的服务是启动，对在跑的是无操作
            UnitAction::Start => vec!["kickstart".into(), target.clone()],
            UnitAction::Stop => vec!["kill".into(), "SIGTERM".into(), target.clone()],
            // -k 表示「先杀掉再拉起」，这才是 restart 的语义
            UnitAction::Restart => vec!["kickstart".into(), "-k".into(), target.clone()],
            UnitAction::Enable => vec!["enable".into(), target.clone()],
            UnitAction::Disable => vec!["disable".into(), target.clone()],
            // 上面已经拦截
            UnitAction::Reload | UnitAction::Mask | UnitAction::Unmask => unreachable!(),
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

    /// macOS 的定时任务 = plist 里带 `StartCalendarInterval` / `StartInterval` 的 launchd job。
    ///
    /// 直接扫 plist 目录而不是逐个 `launchctl print`：几百个 job 逐个起子进程太慢，
    /// 而 plist 是磁盘上的静态事实。`active` 用 `launchctl list` 的 label 集合判定。
    async fn list_timers(&self, scope: UnitScope) -> ApiResult<Vec<TimerEntry>> {
        // `launchctl list` 列的是**调用者所在域**：user 域随时可读；system 域只有
        // root 能对上。判定不了就报 None——把系统守护进程标成「未生效」是撒谎。
        // SAFETY: geteuid 无副作用。
        let can_see_domain = scope == UnitScope::User || unsafe { libc::geteuid() } == 0;
        let loaded: Option<std::collections::HashSet<String>> = if can_see_domain {
            match self.run(&["list"]).await {
                Ok((true, stdout, _)) => Some(
                    stdout
                        .lines()
                        .skip(1)
                        .filter_map(|l| l.split('\t').nth(2))
                        .map(|s| s.trim().to_owned())
                        .collect(),
                ),
                _ => None,
            }
        } else {
            None
        };

        let now = unix_now();
        let mut out: Vec<TimerEntry> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for dir in plist_dirs(scope) {
            let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(ent)) = rd.next_entry().await {
                let path = ent.path();
                if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                    continue;
                }
                let Ok(bytes) = tokio::fs::read(&path).await else {
                    continue;
                };
                let Some(job) = TimedJob::parse(&bytes) else {
                    continue;
                };
                // 同 label 以先扫到的目录为准（管理员目录优先于系统目录）
                if !seen.insert(job.label.clone()) {
                    continue;
                }
                out.push(TimerEntry {
                    name: to_unit_name(&job.label),
                    source: TimerSource::Launchd,
                    next_ts: next_calendar_ts(&job.calendar, now, 400),
                    active: loaded.as_ref().map(|l| l.contains(&job.label)),
                    schedule: job.schedule,
                    // launchd 不记录上次触发时刻
                    last_ts: None,
                    target: job.program,
                    scope,
                });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        // launchd 没有事件流。返回的 receiver 永远收不到消息，但也不会 Closed
        // （self.events 这个 Sender 与 provider 同生命周期）。
        self.events.subscribe()
    }
}

// ---------------------------------------------------------------------------
// 定时任务：plist 解析与日历推算
// ---------------------------------------------------------------------------

/// 各作用域的 plist 目录，按优先级排序（同 label 先扫到的赢）。
fn plist_dirs(scope: UnitScope) -> Vec<std::path::PathBuf> {
    match scope {
        UnitScope::System => vec![
            "/Library/LaunchDaemons".into(),
            "/System/Library/LaunchDaemons".into(),
        ],
        UnitScope::User => {
            let mut v: Vec<std::path::PathBuf> = Vec::new();
            if let Ok(home) = std::env::var("HOME")
                && !home.is_empty()
            {
                v.push(format!("{home}/Library/LaunchAgents").into());
            }
            v.push("/Library/LaunchAgents".into());
            v.push("/System/Library/LaunchAgents".into());
            v
        }
    }
}

/// label → 人话描述的两个诚实来源。
///
/// launchd 的 label 是反向域名，本身没有描述字段；但 (1) plist 里的 `Program`
/// 告诉你这个服务到底跑哪个二进制，(2) `application.<bundleid>.<pid>.<pid>` 这类
/// GUI 应用 job 可以用 bundle id 反查 /Applications 里的应用名（QQ、Clash Verge）。
/// 都查不到时调用方回落 label 本身——不编。
pub struct Friendly {
    /// label → 程序路径（来自 plist 的 `Program` / `ProgramArguments[0]`）。
    by_label: HashMap<String, String>,
    /// bundle id → 应用名（`.app` 目录名去掉后缀）。
    by_bundle: HashMap<String, String>,
}

impl Friendly {
    /// label 的人话描述。查不到返回 `None`。
    pub fn describe(&self, label: &str) -> Option<String> {
        if let Some(p) = self.by_label.get(label) {
            return Some(p.clone());
        }
        if let Some(rest) = label.strip_prefix("application.") {
            let bundle = strip_runtime_tail(rest);
            if let Some(app) = self.by_bundle.get(bundle) {
                return Some(app.clone());
            }
            // 至少把运行时挂上去的 pid / UUID 尾巴剁掉
            if bundle != rest {
                return Some(bundle.to_owned());
            }
        }
        None
    }
}

/// 剁掉 label 尾部的运行时段：纯数字（pid）或十六进制-连字符串（UUID）。
/// `com.tencent.qq.15515648.15516136` → `com.tencent.qq`。
fn strip_runtime_tail(s: &str) -> &str {
    let mut head = s;
    loop {
        let Some(i) = head.rfind('.') else { return head };
        let tail = &head[i + 1..];
        let runtime = !tail.is_empty()
            && (tail.chars().all(|c| c.is_ascii_digit())
                || (tail.len() >= 8
                    && tail.contains('-')
                    && tail.chars().all(|c| c.is_ascii_hexdigit() || c == '-')));
        if !runtime || i == 0 {
            return head;
        }
        head = &head[..i];
    }
}

/// 扫 plist 目录与应用目录，构建 [`Friendly`]。
async fn build_friendly() -> Friendly {
    let mut by_label: HashMap<String, String> = HashMap::new();
    for scope in [UnitScope::System, UnitScope::User] {
        for dir in plist_dirs(scope) {
            let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(ent)) = rd.next_entry().await {
                let path = ent.path();
                if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                    continue;
                }
                let Ok(bytes) = tokio::fs::read(&path).await else {
                    continue;
                };
                let Ok(v) = plist::Value::from_reader(std::io::Cursor::new(bytes.as_slice()))
                else {
                    continue;
                };
                let Some(d) = v.as_dictionary() else { continue };
                let Some(label) = d.get("Label").and_then(plist::Value::as_string) else {
                    continue;
                };
                let program = d
                    .get("Program")
                    .and_then(plist::Value::as_string)
                    .map(str::to_owned)
                    .or_else(|| {
                        d.get("ProgramArguments")
                            .and_then(plist::Value::as_array)
                            .and_then(|a| a.first())
                            .and_then(plist::Value::as_string)
                            .map(str::to_owned)
                    });
                if let Some(p) = program {
                    by_label.entry(label.to_owned()).or_insert(p);
                }
            }
        }
    }

    let mut by_bundle: HashMap<String, String> = HashMap::new();
    let mut app_dirs = vec!["/Applications".to_owned(), "/System/Applications".to_owned()];
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        app_dirs.push(format!("{home}/Applications"));
    }
    for dir in app_dirs {
        let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(ent)) = rd.next_entry().await {
            let path = ent.path();
            let Some(stem) = path
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|n| n.strip_suffix(".app"))
            else {
                continue;
            };
            let Ok(bytes) = tokio::fs::read(path.join("Contents/Info.plist")).await else {
                continue;
            };
            let Ok(v) = plist::Value::from_reader(std::io::Cursor::new(bytes.as_slice())) else {
                continue;
            };
            if let Some(b) = v
                .as_dictionary()
                .and_then(|d| d.get("CFBundleIdentifier"))
                .and_then(plist::Value::as_string)
            {
                by_bundle.entry(b.to_owned()).or_insert(stem.to_owned());
            }
        }
    }
    Friendly {
        by_label,
        by_bundle,
    }
}

/// 在 plist 目录里找 label 对应的文件，请求的作用域优先。
/// 文件名几乎总是 `<label>.plist`，直配不上再逐个解析 `Label`（慢路径，冷门 unit 才走）。
async fn find_plist(label: &str, prefer: UnitScope) -> Option<(UnitScope, String)> {
    let scopes = [prefer, Launchctl::other(prefer)];
    for scope in scopes {
        for dir in plist_dirs(scope) {
            let candidate = dir.join(format!("{label}.plist"));
            if tokio::fs::metadata(&candidate).await.is_ok() {
                return Some((scope, candidate.to_string_lossy().into_owned()));
            }
        }
    }
    for scope in scopes {
        for dir in plist_dirs(scope) {
            let Ok(mut rd) = tokio::fs::read_dir(&dir).await else {
                continue;
            };
            while let Ok(Some(ent)) = rd.next_entry().await {
                let path = ent.path();
                if path.extension().and_then(|e| e.to_str()) != Some("plist") {
                    continue;
                }
                let Ok(bytes) = tokio::fs::read(&path).await else {
                    continue;
                };
                let same = plist::Value::from_reader(std::io::Cursor::new(bytes.as_slice()))
                    .ok()
                    .and_then(|v| {
                        v.as_dictionary()
                            .and_then(|d| d.get("Label"))
                            .and_then(plist::Value::as_string)
                            .map(|s| s == label)
                    })
                    .unwrap_or(false);
                if same {
                    return Some((scope, path.to_string_lossy().into_owned()));
                }
            }
        }
    }
    None
}

/// 读 plist 原文。二进制 plist 转成等价的 XML 文本——「原文」对人不可读时，
/// 给等价的可读形式比给一段乱码更接近「不改写」的本意。
async fn read_plist_text(path: &str) -> ApiResult<String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => {
            ApiError::permission_denied(format!("没有权限读取 {path}"))
                .with_detail(e.to_string())
                .retry_elevated()
        }
        std::io::ErrorKind::NotFound => ApiError::not_found(format!("plist 文件 {path} 不存在")),
        _ => ApiError::internal(format!("读取 {path} 失败")).with_detail(e.to_string()),
    })?;
    let bytes = match String::from_utf8(bytes) {
        Ok(s) => return Ok(s),
        Err(e) => e.into_bytes(),
    };
    let v = plist::Value::from_reader(std::io::Cursor::new(bytes.as_slice()))
        .map_err(|e| ApiError::internal(format!("解析 {path} 失败")).with_detail(e.to_string()))?;
    let mut out = Vec::new();
    v.to_writer_xml(&mut out)
        .map_err(|e| ApiError::internal(format!("转写 {path} 失败")).with_detail(e.to_string()))?;
    String::from_utf8(out)
        .map_err(|e| ApiError::internal(format!("转写 {path} 失败")).with_detail(e.to_string()))
}

/// `StartCalendarInterval` 的一条规则。`None` = 通配（launchd 语义）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CalRule {
    minute: Option<u8>,
    hour: Option<u8>,
    /// 1..=31
    day: Option<u8>,
    /// 0..=7，0 与 7 都是周日（launchd 文档如此）
    weekday: Option<u8>,
    /// 1..=12
    month: Option<u8>,
}

impl CalRule {
    fn date_matches(&self, tm: &libc::tm) -> bool {
        self.month.is_none_or(|m| i32::from(m) == tm.tm_mon + 1)
            && self.day.is_none_or(|d| i32::from(d) == tm.tm_mday)
            && self.weekday.is_none_or(|w| i32::from(w % 7) == tm.tm_wday)
    }
    fn matches(&self, tm: &libc::tm) -> bool {
        self.date_matches(tm)
            && self.hour.is_none_or(|h| i32::from(h) == tm.tm_hour)
            && self.minute.is_none_or(|m| i32::from(m) == tm.tm_min)
    }
}

/// 一条带定时触发的 launchd job。
struct TimedJob {
    label: String,
    /// 人可读调度行（`StartInterval=300s` / `StartCalendarInterval=Hour=3 Minute=15`）
    schedule: Vec<String>,
    /// 推算 `next_ts` 用的日历规则。纯 `StartInterval` 型为空——
    /// 它的下次触发取决于 job 装载时刻，launchd 不暴露，诚实报 `None`。
    calendar: Vec<CalRule>,
    program: Option<String>,
}

impl TimedJob {
    /// 解析一个 plist（XML 或二进制）。不是定时 job（没有任何定时键）返回 `None`。
    fn parse(bytes: &[u8]) -> Option<TimedJob> {
        let v = plist::Value::from_reader(std::io::Cursor::new(bytes)).ok()?;
        let d = v.as_dictionary()?;
        let label = d.get("Label")?.as_string()?.to_owned();

        let mut schedule = Vec::new();
        let mut calendar = Vec::new();
        if let Some(iv) = d
            .get("StartInterval")
            .and_then(plist::Value::as_signed_integer)
        {
            schedule.push(format!("StartInterval={iv}s"));
        }
        match d.get("StartCalendarInterval") {
            Some(plist::Value::Dictionary(one)) => {
                let (rule, line) = cal_rule(one);
                calendar.push(rule);
                schedule.push(line);
            }
            Some(plist::Value::Array(arr)) => {
                for it in arr {
                    if let Some(dd) = it.as_dictionary() {
                        let (rule, line) = cal_rule(dd);
                        calendar.push(rule);
                        schedule.push(line);
                    }
                }
            }
            _ => {}
        }
        if schedule.is_empty() {
            return None;
        }

        let program = d
            .get("Program")
            .and_then(plist::Value::as_string)
            .map(str::to_owned)
            .or_else(|| {
                d.get("ProgramArguments")
                    .and_then(plist::Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(plist::Value::as_string)
                    .map(str::to_owned)
            });
        Some(TimedJob {
            label,
            schedule,
            calendar,
            program,
        })
    }
}

/// 一个 `StartCalendarInterval` 字典 → 规则 + 人可读行。
fn cal_rule(d: &plist::Dictionary) -> (CalRule, String) {
    let g = |k: &str| {
        d.get(k)
            .and_then(plist::Value::as_signed_integer)
            .and_then(|v| u8::try_from(v).ok())
    };
    let r = CalRule {
        minute: g("Minute"),
        hour: g("Hour"),
        day: g("Day"),
        weekday: g("Weekday"),
        month: g("Month"),
    };
    let mut parts = Vec::new();
    for (k, v) in [
        ("Month", r.month),
        ("Day", r.day),
        ("Weekday", r.weekday),
        ("Hour", r.hour),
        ("Minute", r.minute),
    ] {
        if let Some(v) = v {
            parts.push(format!("{k}={v}"));
        }
    }
    let line = if parts.is_empty() {
        // 全通配 = 每分钟触发（launchd 语义）
        "StartCalendarInterval=每分钟".to_owned()
    } else {
        format!("StartCalendarInterval={}", parts.join(" "))
    };
    (r, line)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// unix 秒 → 本地时间字段。
fn local_tm(t: i64) -> Option<libc::tm> {
    let time = t as libc::time_t;
    // SAFETY: zeroed tm 是合法初值，localtime_r 只写入它。
    let mut tm = unsafe { std::mem::zeroed::<libc::tm>() };
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        None
    } else {
        Some(tm)
    }
}

/// 从 `from` 起找下一个命中任意规则的整分（**本地时区**，launchd 的日历就是墙钟）。
///
/// 逐分钟扫太慢：日期不匹配就跳到次日零点、小时不匹配跳到下个整点
/// （DST 造成的跳跃偏差由下一轮 `local_tm` 自我修正）。
/// 上限 `horizon_days` 天内找不到返回 `None`（如 `Day=30 Month=2` 这种永不发生的规则）。
pub fn next_calendar_ts(rules: &[CalRule], from: i64, horizon_days: i64) -> Option<i64> {
    if rules.is_empty() {
        return None;
    }
    let end = from + horizon_days * 86_400;
    let mut t = (from / 60 + 1) * 60;
    while t < end {
        let tm = local_tm(t)?;
        if !rules.iter().any(|r| r.date_matches(&tm)) {
            t += 86_400
                - i64::from(tm.tm_hour) * 3600
                - i64::from(tm.tm_min) * 60
                - i64::from(tm.tm_sec);
            continue;
        }
        if !rules
            .iter()
            .any(|r| r.date_matches(&tm) && r.hour.is_none_or(|h| i32::from(h) == tm.tm_hour))
        {
            t += 3600 - i64::from(tm.tm_min) * 60 - i64::from(tm.tm_sec);
            continue;
        }
        if rules.iter().any(|r| r.matches(&tm)) {
            return Some(t);
        }
        t += 60;
    }
    None
}

// ---------------------------------------------------------------------------
// 名字转换
// ---------------------------------------------------------------------------

/// launchd 的 label 校验。
///
/// 共享的 `validate_unit_name` 按 systemd 的 unit 名字符集拦,对 launchd 太严:
/// label 可以含空格(`Clash Verge`)等 systemd 不允许的字符——按那套规则,
/// 列表里能看到的服务点开详情却报 400。这里只拦真正危险的:
/// 空 / 超长、控制字符、`/`(会拆坏 `domain/label` 目标格式)、`-` 开头
/// (会被 launchctl 当成选项)。
pub fn validate_label(unit: &str) -> ApiResult<()> {
    if unit.is_empty() || unit.len() > 256 {
        return Err(ApiError::invalid_request(format!("unit 名长度不合法: {unit}")));
    }
    if unit.starts_with('-') || unit.chars().any(|c| c.is_control() || c == '/') {
        return Err(ApiError::invalid_request(format!("unit 名含非法字符: {unit}")));
    }
    Ok(())
}

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
    friendly: Option<&Friendly>,
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
                // label 没有描述字段:能查到就给程序路径 / 应用名,查不到回落 label
                description: friendly
                    .and_then(|f| f.describe(label))
                    .unwrap_or_else(|| label.to_owned()),
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
        assert!(super::super::validate_unit_name(&to_unit_name("com.apple.Finder")).is_ok());
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

        let units = parse_list(LIST, UnitScope::System, &disabled, None);
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

    const TIMED_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
	<key>Label</key><string>com.example.nightly</string>
	<key>ProgramArguments</key><array><string>/usr/local/bin/backup</string><string>--all</string></array>
	<key>StartCalendarInterval</key><dict>
		<key>Hour</key><integer>3</integer>
		<key>Minute</key><integer>15</integer>
	</dict>
</dict></plist>"#;

    #[test]
    fn 解析定时_plist() {
        let job = TimedJob::parse(TIMED_PLIST.as_bytes()).expect("是定时 job");
        assert_eq!(job.label, "com.example.nightly");
        assert_eq!(job.schedule, ["StartCalendarInterval=Hour=3 Minute=15"]);
        assert_eq!(
            job.calendar,
            [CalRule {
                hour: Some(3),
                minute: Some(15),
                ..Default::default()
            }]
        );
        assert_eq!(job.program.as_deref(), Some("/usr/local/bin/backup"));

        // StartInterval 型：有调度行、没有日历规则（next 推算不了）
        let iv = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
	<key>Label</key><string>com.example.poll</string>
	<key>Program</key><string>/bin/echo</string>
	<key>StartInterval</key><integer>300</integer>
</dict></plist>"#;
        let job = TimedJob::parse(iv.as_bytes()).unwrap();
        assert_eq!(job.schedule, ["StartInterval=300s"]);
        assert!(job.calendar.is_empty());

        // 没有任何定时键 → 不是定时 job
        let plain = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
	<key>Label</key><string>com.example.daemon</string>
	<key>Program</key><string>/bin/echo</string>
</dict></plist>"#;
        assert!(TimedJob::parse(plain.as_bytes()).is_none());
    }

    #[test]
    fn 日历推算_下一个命中的整分() {
        let now = unix_now();
        // 每小时的第 30 分
        let rule = [CalRule {
            minute: Some(30),
            ..Default::default()
        }];
        let next = next_calendar_ts(&rule, now, 2).expect("一小时内必有一次");
        assert!(next > now && next <= now + 3600 + 60);
        let tm = local_tm(next).unwrap();
        assert_eq!((tm.tm_min, tm.tm_sec), (30, 0));

        // 每天 03:15
        let rule = [CalRule {
            hour: Some(3),
            minute: Some(15),
            ..Default::default()
        }];
        let next = next_calendar_ts(&rule, now, 3).unwrap();
        let tm = local_tm(next).unwrap();
        assert_eq!((tm.tm_hour, tm.tm_min), (3, 15));

        // 2 月 30 日永不发生
        let rule = [CalRule {
            month: Some(2),
            day: Some(30),
            ..Default::default()
        }];
        assert_eq!(next_calendar_ts(&rule, now, 400), None);

        // 空规则集（StartInterval 型）不编数据
        assert_eq!(next_calendar_ts(&[], now, 400), None);
    }

    #[tokio::test]
    async fn 本机定时任务扫描() {
        let l = Launchctl::new();
        let timers = l.list_timers(UnitScope::User).await.unwrap();
        // macOS 上系统 LaunchAgents 里总有几个带 StartCalendarInterval 的
        eprintln!("本机 user 域定时 job {} 个，前 5 个：", timers.len());
        for t in timers.iter().take(5) {
            eprintln!("  {} {:?} next={:?}", t.name, t.schedule, t.next_ts);
        }
        for t in &timers {
            assert_eq!(t.source, TimerSource::Launchd);
            assert!(t.name.ends_with(".service"));
            assert!(!t.schedule.is_empty(), "{}: 没有调度行就不该出现", t.name);
            assert_eq!(t.last_ts, None, "launchd 不记录上次触发，不该有值");
        }
    }

    /// 列表(`launchctl list`)是跨域的,详情必须能对「scope 报错了」的服务做域回退。
    /// 复现:进程页看到的 GUI 应用(application.*)在列表里被标成 system,
    /// 点详情却在 system 域找不到 → 之前直接报 404。
    #[tokio::test]
    async fn 详情跨域回退() {
        let l = Launchctl::new();
        let Ok((true, stdout, _)) = l.run(&["list"]).await else {
            return;
        };
        let Some(label) = stdout
            .lines()
            .skip(1)
            .filter_map(|x| x.split('\t').nth(2))
            .map(str::trim)
            .find(|s| s.starts_with("application."))
        else {
            eprintln!("跳过:本机没有正在运行的 GUI 应用 job");
            return;
        };
        let unit = to_unit_name(label);
        // 用「错误」的 system 视角查——必须回退到 gui 域并命中
        let d = l.unit_detail(UnitScope::System, &unit).await.unwrap();
        assert_eq!(d.summary.scope, UnitScope::User, "application.* 实际在 gui 域");
        assert_eq!(d.summary.name, unit);
    }

    /// 用户实测:`Clash Verge.service`(label 带空格)列表里能看到,
    /// 点详情却报「unit 名含非法字符」——launchd 不能用 systemd 的字符集校验。
    #[test]
    fn label_校验放行空格拦住危险字符() {
        assert!(validate_label("Clash Verge.service").is_ok());
        assert!(validate_label("com.apple.Finder.service").is_ok());
        assert!(validate_label("").is_err());
        assert!(validate_label("-flag.service").is_err(), "会被当成选项");
        assert!(validate_label("a/b.service").is_err(), "会拆坏 domain/label");
        assert!(validate_label("a\nb.service").is_err(), "控制字符");
    }

    #[test]
    fn 运行时尾巴剁法() {
        assert_eq!(strip_runtime_tail("com.tencent.qq.15515648.15516136"), "com.tencent.qq");
        assert_eq!(
            strip_runtime_tail("com.tencent.qqexdoc.17034502.17034675.CE3F1B18-56DB-42CF-B485-3733F793A2A9"),
            "com.tencent.qqexdoc",
            "UUID 段也是运行时挂的"
        );
        // 正常反向域名一个段都不能剁
        assert_eq!(strip_runtime_tail("com.apple.Finder"), "com.apple.Finder");
        // 版本号模样的段是名字的一部分?不是——纯数字段没法区分,按运行时处理;
        // 但十六进制字母段(非 UUID 形状)要保住
        assert_eq!(strip_runtime_tail("com.example.abcdef12"), "com.example.abcdef12");
    }

    #[test]
    fn 人话描述来源与回落() {
        let f = Friendly {
            by_label: HashMap::from([(
                "com.apple.newsyslog".to_owned(),
                "/usr/sbin/newsyslog".to_owned(),
            )]),
            by_bundle: HashMap::from([("com.tencent.qq".to_owned(), "QQ".to_owned())]),
        };
        // plist Program 优先
        assert_eq!(f.describe("com.apple.newsyslog").as_deref(), Some("/usr/sbin/newsyslog"));
        // GUI 应用 job:bundle id 反查应用名
        assert_eq!(
            f.describe("application.com.tencent.qq.15515648.15516136").as_deref(),
            Some("QQ")
        );
        // 查不到应用,至少剁掉 pid 尾巴
        assert_eq!(
            f.describe("application.com.unknown.app.123.456").as_deref(),
            Some("com.unknown.app")
        );
        // 一无所知 → None,由调用方回落 label
        assert_eq!(f.describe("com.apple.mystery"), None);

        // 接进 parse_list:描述被替换,查不到的保持 label
        let units = parse_list(
            "PID\tStatus\tLabel\n-\t0\tcom.apple.newsyslog\n-\t0\tcom.apple.mystery\n",
            UnitScope::System,
            &HashMap::new(),
            Some(&f),
        );
        assert_eq!(units[0].description, "/usr/sbin/newsyslog");
        assert_eq!(units[1].description, "com.apple.mystery");
    }

    #[tokio::test]
    async fn 本机人话描述覆盖率() {
        let f = build_friendly().await;
        let l = Launchctl::new();
        let units = l
            .list_units(&UnitListQuery {
                scope: Some(UnitScope::User),
                ..Default::default()
            })
            .await
            .unwrap();
        let described = units.iter().filter(|u| {
            u.description != to_label(&u.name)
        }).count();
        eprintln!(
            "plist 程序映射 {} 条,bundle 映射 {} 条;{}/{} 个 unit 有人话描述",
            f.by_label.len(),
            f.by_bundle.len(),
            described,
            units.len()
        );
        assert!(!f.by_label.is_empty(), "本机总有带 Program 的 plist");
        assert!(described > 0, "至少一部分 unit 该有人话描述");
    }

    /// 复现:定时任务表(plist 扫描)里的 atrun 这类默认不加载的 daemon,
    /// 点详情曾报「不存在或未加载」——`launchctl print` 只认已加载的 job。
    #[tokio::test]
    async fn 未加载但盘上有_plist_的_job_也能看详情() {
        let l = Launchctl::new();
        let unit = "com.apple.atrun.service";
        match l.unit_detail(UnitScope::System, unit).await {
            Ok(d) => {
                assert_eq!(d.summary.name, unit);
                assert!(
                    d.fragment_path
                        .as_deref()
                        .unwrap_or("")
                        .ends_with("com.apple.atrun.plist"),
                    "{:?}",
                    d.fragment_path
                );
                let f = l.unit_file(UnitScope::System, unit).await.unwrap();
                let content = f.fragment.expect("有 plist 就该有内容").content;
                assert!(content.contains("atrun"), "二进制 plist 应转成可读 XML");
            }
            Err(e) if e.code == ErrorCode::NotFound => eprintln!("跳过:本机没有 atrun"),
            Err(e) => panic!("{e:?}"),
        }
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
