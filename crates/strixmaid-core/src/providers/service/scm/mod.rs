//! Windows 的 [`ServiceProvider`] 实现：直接调服务控制管理器（SCM）的 API。
//!
//! # 概念映射
//!
//! 四条实现路径里 SCM 与 systemd 的模型**最接近**——它同样有一张全机服务表、
//! 有启动类型、有真正的依赖图，甚至有「禁用」这一档（正对应 systemd 的 mask）。
//! 因此对得上的部分照搬，对不上的照旧**如实报缺失**，不拿相近的东西冒充：
//!
//! | systemd | Windows SCM | 说明 |
//! |---|---|---|
//! | unit 名 | 服务名 + `.service` 后缀 | 见下「为什么给服务名加后缀」 |
//! | `Description` | `QueryServiceConfig2W(SERVICE_CONFIG_DESCRIPTION)`，退回 `DisplayName` | 列表里只用 `DisplayName`，见下「列表为什么不逐个查描述」 |
//! | active / inactive / failed | `dwCurrentState` + `dwWin32ExitCode` | [`map::state_of`] |
//! | enabled / disabled / masked / static | `dwStartType` | [`map::enable_state_of`]，**`SERVICE_DISABLED` → `masked`** |
//! | 依赖图 | `lpDependencies` + `EnumDependentServicesW` | 有正向也有反向，不像 launchd |
//! | unit 文件 | 注册表 `HKLM\SYSTEM\CurrentControlSet\Services\<名字>` | 见 [`regkey`] 的模块文档 |
//! | drop-in | **没有** | `drop_in_paths` / `drop_ins` 恒为空 |
//! | cgroup 用量 | **没有** | `UnitDetail::cgroup` 为 `None` |
//! | `NRestarts` / `ActiveEnterTimestamp` / `StateChangeTimestamp` | **没有** | 三个字段恒为 `None`，见下 |
//! | `Documentation=` | **没有** | `documentation` 恒为空数组 |
//! | `--user` 作用域 | **没有** | 见下「作用域」 |
//! | `*.timer` | 计划任务（另一个子系统） | 见下「定时任务」 |
//! | `PropertiesChanged` 信号 | **没有总线信号** | 见下「变更事件靠轮询」 |
//!
//! # 为什么给服务名加 `.service` 后缀
//!
//! 与 launchd 侧完全同一个理由（见 `launchd.rs` 的模块文档）：DTO 的 `unit_type`
//! 取自 unit 名的最后一段后缀，而 Windows 服务名里带点的一大把
//! （`NVDisplay.ContainerLocalSystem`、`OneDrive.Sync`），直接用就会让 `unit_type`
//! 变成 `ContainerLocalSystem` 这种垃圾值，`?type=service` 过滤随之报废。
//! 因此对外一律报 `Spooler.service`，调 SCM 前再剥掉后缀（[`map::to_service_name`]）。
//! 代价是名字长八个字符，换来的是 **API 契约在三个平台上完全一致**。
//!
//! # 列表为什么不逐个查描述与启动类型
//!
//! `EnumServicesStatusExW` 一次给出全部服务的名字、显示名与运行状态，
//! 但**不给启动类型**。逐个 `OpenServiceW` + `QueryServiceConfigW` 意味着
//! 三百个服务就是六百次 RPC，列表接口会从毫秒级掉到秒级。
//!
//! 取舍：启动类型改从注册表批量读——`HKLM\SYSTEM\CurrentControlSet\Services\<名字>\Start`
//! 是 `REG_DWORD`，取值与 `dwStartType` **是同一个东西**（SCM 自己就是把它写在那里的），
//! 一次 `RegGetValueW` 不到十微秒。这不是近似，是同一份数据的另一个读法。
//!
//! 描述则不做这个替换：注册表里的 `Description` 值经常是
//! `@%SystemRoot%\system32\xxx.dll,-123` 这样的资源引用，要 `SHLoadIndirectString`
//! 才能变成人话，而 `QueryServiceConfig2W` 是替我们做了这件事的那个 API。
//! 所以**列表用 `DisplayName`**（枚举时白送、已解析、对人可读），
//! **详情才查真正的 `Description`**。两者都是服务自报的名字，不存在编造。
//!
//! # 作用域
//!
//! Windows 没有 `systemctl --user` 的对应物：SCM 只有一张全机服务表。
//! 那些名字带随机后缀的「每用户服务实例」（`OneSyncSvc_1a2b3c`）不是等价物——
//! 它们由系统按模板自动派生，用户既不能自己装一个，也没有管理入口
//! （这一判断与 `capability::has_user_units` 在 Windows 上恒返回 `false` 是同一条）。
//! 因此本实现只服务 [`UnitScope::System`]，收到 `scope=user` 时返回
//! `CapabilityUnavailable` 而不是把系统服务改个标签冒充用户服务。
//!
//! # 三个时间戳与重启次数为什么是 `None`
//!
//! SCM 的服务记录里根本没有这些字段：它不保存「上次进入运行态的时刻」，
//! 也不保存「累计重启了几次」。`SERVICE_CONFIG_FAILURE_ACTIONS` 只描述
//! **将来**失败时该怎么办（重启 / 跑命令 / 重启机器）与计数的重置周期，
//! 不是已经发生过多少次的计数器。能凑出近似值的途径都不诚实：
//! 主进程的创建时刻是「进程起来的时刻」而不是「服务进入运行态的时刻」，
//! 系统日志里的服务控制管理器事件又依赖日志没被轮转掉。所以报 `None`。
//!
//! # 定时任务
//!
//! Windows 的定时任务是**计划任务**（Task Scheduler），与服务分属两个子系统。
//! 它确实可以读（`schtasks /query /fo CSV /v` 子进程，或 COM 的 `ITaskService`），
//! 但 [`TimerSource`] 目前只有 `SystemdTimer` 与 `Launchd` 两个取值，**没有 Windows 的**。
//! 把计划任务塞进这两个里的任何一个都是在撒谎，而给 types crate 加一个
//! `ScheduledTask` 变体是 API 契约改动（OpenAPI、前端代码生成都要跟着变），
//! 不在本模块的职责范围内。因此 [`ServiceControlManager::list_timers`] **返回空表**，
//! 契约补齐后再实现采集。
//!
//! # 变更事件靠轮询
//!
//! systemd 的 `services.changed` 由 `PropertiesChanged` 信号驱动，SCM 没有总线。
//! 两个候选都不合用：
//!
//! - `NotifyServiceStatusChangeW` 要对**每个服务**单独注册一个 APC 回调。
//!   三百个服务就是三百次注册、三百个常驻句柄，而且每次回调触发后必须重新注册；
//!   SCM 级（`SC_HANDLE` 指向管理器本身）的通知只报服务的**创建与删除**，
//!   不报状态变化，正好不是我们要的。
//! - `SubscribeServiceChangeNotifications` 同样是逐服务订阅。
//!
//! 因此这里选**轮询 + 差分**：后台任务每 [`POLL_INTERVAL`] 枚举一次全部服务，
//! 与上一轮比对，只把**变化了的**那几条广播出去。一次枚举是一个 RPC，
//! 代价与「读一次服务列表页」相同，而前端的服务页正要靠它更新——
//! 与 `cli.rs` 那条「降级路径返回永远安静的 receiver」的判断不同，
//! 这里轮询是值得的。轮询任务在**首次 `subscribe()` 时才启动**（与 `bus.rs`
//! 的惰性启动同理），并在最后一个订阅者离开后自行退出。
//!
//! # 权限
//!
//! 打开句柄时**按需申请**权限：只读路径只要
//! `SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS`（默认授予所有已认证用户），
//! 操作路径才加 `SERVICE_START` / `SERVICE_STOP` / `SERVICE_CHANGE_CONFIG`。
//! 一上来就申请 `SERVICE_ALL_ACCESS` 会让非管理员**连服务列表都拉不出来**。
//! 被拒时映射成 `PermissionDenied` 并置 `can_retry_elevated`，与 Linux 侧
//! polkit 拒绝的表现一致。

pub mod map;
pub mod regkey;
pub mod sys;

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use strixmaid_types::service::{
    TimerEntry, UnitAction, UnitActionResp, UnitDetail, UnitFile, UnitFileFragment, UnitListQuery,
    UnitLoadState, UnitScope, UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::sync::broadcast;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_DEPENDENT_SERVICES_RUNNING, ERROR_FILE_NOT_FOUND,
    ERROR_INVALID_SERVICE_CONTROL, ERROR_PATH_NOT_FOUND, ERROR_SERVICE_ALREADY_RUNNING,
    ERROR_SERVICE_CANNOT_ACCEPT_CTRL, ERROR_SERVICE_DATABASE_LOCKED, ERROR_SERVICE_DISABLED,
    ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE,
    ERROR_SERVICE_REQUEST_TIMEOUT,
};
use windows_sys::Win32::System::Services::{
    SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_ACCEPT_PARAMCHANGE,
    SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_CONTROL_PARAMCHANGE, SERVICE_CONTROL_STOP,
    SERVICE_DEMAND_START, SERVICE_DISABLED, SERVICE_ENUMERATE_DEPENDENTS, SERVICE_PAUSE_CONTINUE,
    SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STOP, SERVICE_STOPPED,
};

use crate::platform::windows::registry::{HKLM, reg_dword};

use super::super::{Probe, Provider};
use super::{
    CALL_TIMEOUT, EVENT_CAPACITY, ServiceEvent, ServiceProvider, UnitDeps, apply_list_query,
    summary_for_vanished, unit_type_of,
};

/// 轮询间隔。5 秒是「服务页看起来是活的」与「不为一个观测面板持续打扰 SCM」
/// 之间的折衷：一次全量枚举约等于一次列表请求的代价。
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// restart 时等待服务停稳的上限。超过它说明服务卡在 `STOP_PENDING`，
/// 此时**不**继续 start——那样只会得到一个「已经在运行」的假成功。
const STOP_TIMEOUT: Duration = CALL_TIMEOUT;

/// 等待停止时的轮询间隔。SCM 的 `dwWaitHint` 常常给到数秒，但实际停下往往很快，
/// 250ms 一次既不会把 restart 拖长，也不会把 SCM 问烂。
const STOP_POLL: Duration = Duration::from_millis(250);

/// 一次操作的整体上限。比 [`STOP_TIMEOUT`] 宽裕一倍有余，
/// 保证「停止超时」这个更具体的错误先报出来，而不是被外层超时抢答。
const ACTION_TIMEOUT: Duration = Duration::from_secs(45);

/// 只读打开服务所需的权限。默认的服务安全描述符把这两项授予
/// `NT AUTHORITY\Authenticated Users`，因此非管理员也能看列表与详情。
const READ_ACCESS: u32 = SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS;

/// SCM 实现。
#[derive(Debug)]
pub struct ServiceControlManager {
    shared: Arc<Shared>,
}

/// 与轮询任务共享的部分。
#[derive(Debug)]
struct Shared {
    /// 事件通道。
    events: broadcast::Sender<ServiceEvent>,
    /// 轮询任务是否在跑。用互斥量而不是原子量，是因为任务退出与
    /// `subscribe()` 启动之间要在**同一把锁下**判定订阅者数量，
    /// 否则会出现「任务刚决定退出、同时来了新订阅者」的漏启动。
    polling: std::sync::Mutex<bool>,
}

impl Default for ServiceControlManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceControlManager {
    /// 构造。此时**不**连 SCM——连接留给 [`Provider::probe`] 与各次调用，
    /// 与 `SystemdBus::connect` 的「构造即连接」不同：SCM 句柄很便宜，
    /// 每次调用现开现关比常驻一个句柄更省心（不需要处理句柄失效后的重连）。
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        ServiceControlManager {
            shared: Arc::new(Shared {
                events,
                polling: std::sync::Mutex::new(false),
            }),
        }
    }

    /// 事件发送端（WS hub 也可以直接 `subscribe()`）。
    pub fn event_sender(&self) -> &broadcast::Sender<ServiceEvent> {
        &self.shared.events
    }

    /// 确保轮询任务已启动。只在有订阅者时启动，任务在最后一个订阅者离开后自行退出。
    fn ensure_poller(&self) {
        let mut running = self
            .shared
            .polling
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *running {
            return;
        }
        *running = true;
        drop(running);
        spawn_poller(Arc::clone(&self.shared));
    }
}

#[async_trait]
impl Provider for ServiceControlManager {
    /// 必须是 `"scm"`：`capability::probe_all` 按这个 id 点亮 `systemd` 能力位。
    fn id(&self) -> &'static str {
        "scm"
    }

    /// 能连上 SCM 且能枚举出服务即可用。
    ///
    /// 连不上基本只有一种可能——进程跑在没有 `SC_MANAGER_CONNECT` 的受限令牌下
    /// （AppContainer、某些沙箱）。那种情况下服务页整体隐藏，
    /// 与 Linux 上没有 systemd 时一致。
    async fn probe(&self) -> Probe {
        let r = tokio::task::spawn_blocking(|| {
            let mgr = sys::open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)?;
            sys::enum_services(&mgr).map(|v| v.len())
        })
        .await;
        match r {
            Ok(Ok(n)) if n > 0 => Probe::Available,
            Ok(Ok(_)) => Probe::unavailable("服务控制管理器可连接，但一个服务都枚举不到"),
            Ok(Err(e)) => Probe::unavailable(format!("连接服务控制管理器失败：{e}")),
            Err(e) => Probe::unavailable(format!("探测任务异常：{e}")),
        }
    }
}

#[async_trait]
impl ServiceProvider for ServiceControlManager {
    async fn list_units(&self, query: &UnitListQuery) -> ApiResult<Vec<UnitSummary>> {
        require_system_scope(query.scope.unwrap_or(UnitScope::System))?;
        let units = blocking("列出服务", CALL_TIMEOUT, collect_summaries).await?;
        Ok(apply_list_query(units, query))
    }

    async fn unit_detail(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDetail> {
        require_system_scope(scope)?;
        map::validate_service_unit(unit)?;
        let unit = unit.to_owned();
        blocking("读服务详情", CALL_TIMEOUT, move || detail_blocking(&unit)).await
    }

    /// unit 文件 = 服务的注册表键（见 [`regkey`] 的模块文档）。
    ///
    /// `drop_ins` 恒为空：Windows 没有「同一份定义被多个文件分层覆盖」的机制。
    /// 主键下的 `Parameters` 子键**不是** drop-in（它是服务自己的配置，不覆盖服务定义），
    /// 因此与主键一起渲染进同一个 fragment，而不是伪装成一条 drop-in。
    async fn unit_file(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitFile> {
        require_system_scope(scope)?;
        map::validate_service_unit(unit)?;
        let owned = unit.to_owned();
        blocking("读服务注册表定义", CALL_TIMEOUT, move || {
            let service = map::to_service_name(&owned);
            // 先确认服务确实存在：注册表里有键但 SCM 里没有的残留是有的
            // （卸载不干净），对它报 404 比给一段孤儿配置更准确。
            let mgr = sys::open_manager(SC_MANAGER_CONNECT)
                .map_err(|e| map_error(&owned, "连接服务控制管理器", &e))?;
            let _svc = sys::open_service(&mgr, &service, READ_ACCESS)
                .map_err(|e| map_error(&owned, "打开服务", &e))?;

            let content =
                regkey::render_service(&service).map_err(|e| map_error(&owned, "读注册表", &e))?;
            Ok(UnitFile {
                unit: owned.clone(),
                fragment: Some(UnitFileFragment {
                    path: map::service_key_display(&service),
                    content,
                }),
                drop_ins: Vec::new(),
            })
        })
        .await
    }

    async fn unit_deps(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDeps> {
        require_system_scope(scope)?;
        map::validate_service_unit(unit)?;
        let unit = unit.to_owned();
        blocking("读服务依赖", CALL_TIMEOUT, move || deps_blocking(&unit)).await
    }

    async fn unit_action(
        &self,
        scope: UnitScope,
        unit: &str,
        action: UnitAction,
    ) -> ApiResult<UnitActionResp> {
        require_system_scope(scope)?;
        map::validate_service_unit(unit)?;
        let unit = unit.to_owned();
        blocking("执行服务操作", ACTION_TIMEOUT, move || {
            action_blocking(&unit, action)
        })
        .await
    }

    /// 见模块文档「定时任务」：计划任务在 [`TimerSource`] 里没有对应取值，
    /// 因此**返回空表**而不是把它冒充成 systemd timer 或 launchd job。
    ///
    /// [`TimerSource`]: strixmaid_types::service::TimerSource
    async fn list_timers(&self, scope: UnitScope) -> ApiResult<Vec<TimerEntry>> {
        require_system_scope(scope)?;
        tracing::debug!(
            "Windows 的定时任务是计划任务（Task Scheduler），TimerSource 尚无对应取值，返回空表"
        );
        Ok(Vec::new())
    }

    /// 订阅变更事件。首次调用时启动轮询任务，见模块文档「变更事件靠轮询」。
    async fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        // 先拿 receiver 再启动任务：这样任务里「还有没有订阅者」的判定
        // 一定能看见本次订阅，不会出现启动后立刻自我了断。
        let rx = self.shared.events.subscribe();
        self.ensure_poller();
        rx
    }
}

// ---------------------------------------------------------------------------
// 阻塞实现（全部在 spawn_blocking 里跑）
// ---------------------------------------------------------------------------

/// 把一段同步的 SCM 调用丢进 `spawn_blocking` 并套上超时。
///
/// SCM 的 API 全是同步 RPC，在 async fn 里直接调会占住 tokio 的工作线程；
/// 一次 `EnumServicesStatusExW` 在负载高的机器上可以是几十毫秒，
/// restart 的等待更是以秒计。
///
/// 超时到期只让**调用方**拿到 `Timeout`：`spawn_blocking` 的任务无法取消，
/// 那个线程会自己跑完再退出（SCM 的 RPC 本身有服务端超时，不会永久挂住）。
async fn blocking<T, F>(what: &str, limit: Duration, f: F) -> ApiResult<T>
where
    F: FnOnce() -> ApiResult<T> + Send + 'static,
    T: Send + 'static,
{
    let handle = tokio::task::spawn_blocking(f);
    match tokio::time::timeout(limit, handle).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            Err(ApiError::internal(format!("{what}的后台任务异常")).with_detail(e.to_string()))
        }
        Err(_) => Err(ApiError::new(
            ErrorCode::Timeout,
            format!("{what}超时（{}s）", limit.as_secs()),
        )),
    }
}

/// Windows 只有系统作用域，见模块文档「作用域」。
fn require_system_scope(scope: UnitScope) -> ApiResult<()> {
    if scope == UnitScope::System {
        return Ok(());
    }
    Err(
        ApiError::capability_unavailable("scm", "Windows 没有「每用户的服务管理器」")
            .with_detail(
                "SCM 只有一张全机范围的服务表，没有 systemctl --user 的对应物；\
                 名字带随机后缀的每用户服务实例由系统按模板派生，用户无法自行安装或管理。\
                 capability 里的 user_units 在 Windows 上恒为 false，前端不应给出作用域切换。",
            ),
    )
}

/// 枚举全部服务并组装成摘要。
fn collect_summaries() -> ApiResult<Vec<UnitSummary>> {
    let mgr = sys::open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
        .map_err(|e| map_error("*", "连接服务控制管理器", &e))?;
    let entries = sys::enum_services(&mgr).map_err(|e| map_error("*", "枚举服务", &e))?;
    Ok(entries.iter().map(summary_of).collect())
}

/// 一条枚举结果 → [`UnitSummary`]。
///
/// 启动类型从注册表批量读（见模块文档「列表为什么不逐个查描述与启动类型」）；
/// 读不到时 `enable_state` 为 `None`——那是「不知道」，不是「未启用」。
fn summary_of(e: &sys::ServiceEntry) -> UnitSummary {
    let start_type = reg_dword(HKLM, &map::service_subkey(&e.name), "Start");
    let (active_state, sub_state) = map::state_of(e.status.current_state, e.status.win32_exit_code);
    let name = map::to_unit_name(&e.name);
    UnitSummary {
        unit_type: unit_type_of(&name).to_owned(),
        // 显示名是 SCM 已经解析好的人话；没有时回落服务名（systemd 对无 Description
        // 的 unit 也是回落 unit 名）。
        description: if e.display_name.is_empty() {
            e.name.clone()
        } else {
            e.display_name.clone()
        },
        name,
        load_state: start_type.map_or(UnitLoadState::Loaded, map::load_state_of),
        active_state,
        sub_state,
        enable_state: start_type.and_then(map::enable_state_of),
        scope: UnitScope::System,
    }
}

/// 详情：状态 + 配置 + 描述。
fn detail_blocking(unit: &str) -> ApiResult<UnitDetail> {
    let service = map::to_service_name(unit);
    let mgr = sys::open_manager(SC_MANAGER_CONNECT)
        .map_err(|e| map_error(unit, "连接服务控制管理器", &e))?;
    let svc = sys::open_service(&mgr, &service, READ_ACCESS)
        .map_err(|e| map_error(unit, "打开服务", &e))?;

    let status = sys::query_status(&svc).map_err(|e| map_error(unit, "读服务状态", &e))?;
    let cfg = sys::query_config(&svc).map_err(|e| map_error(unit, "读服务配置", &e))?;
    // 描述取不到是常态（第三方服务经常不写），不该让整次详情失败。
    let description = sys::query_description(&svc).ok().flatten();

    let (active_state, sub_state) = map::state_of(status.current_state, status.win32_exit_code);
    let name = map::to_unit_name(&service);
    let summary = UnitSummary {
        unit_type: unit_type_of(&name).to_owned(),
        description: description
            .or_else(|| cfg.display_name.clone())
            .unwrap_or_else(|| service.clone()),
        name,
        load_state: map::load_state_of(cfg.start_type),
        active_state,
        sub_state,
        enable_state: map::enable_state_of(cfg.start_type),
        scope: UnitScope::System,
    };

    Ok(UnitDetail {
        summary,
        // 「unit 文件」在 Windows 上是注册表键，见 unit_file。
        fragment_path: Some(map::service_key_display(&service)),
        // Windows 没有 drop-in 机制。
        drop_in_paths: Vec::new(),
        main_pid: (status.process_id != 0).then_some(status.process_id),
        // SCM 不记录这三项，见模块文档。
        active_enter_ts: None,
        state_change_ts: None,
        n_restarts: None,
        result: map::result_of(status.current_state, status.win32_exit_code),
        exit_code: map::exit_code_of(
            status.current_state,
            status.win32_exit_code,
            status.service_specific_exit_code,
        ),
        // Windows 服务没有 Documentation= 这样的字段。
        documentation: Vec::new(),
        // `lpServiceStartName`。与 systemd 不同，这里**不**把 LocalSystem 归一成
        // `None`：systemd 的 `None` 意思是「没写 User=，即 root」，而 SCM 的这一项
        // 永远有值，原样报出来对排障更有用（LocalService / NetworkService /
        // 域账户三者的权限差别很大）。
        user: cfg.start_name.clone(),
        // Windows 没有 cgroup 这一层。
        cgroup: None,
    })
}

/// 依赖：正向来自 `lpDependencies`，反向来自 `EnumDependentServicesW`。
fn deps_blocking(unit: &str) -> ApiResult<UnitDeps> {
    let service = map::to_service_name(unit);
    let mgr = sys::open_manager(SC_MANAGER_CONNECT)
        .map_err(|e| map_error(unit, "连接服务控制管理器", &e))?;
    let svc = sys::open_service(&mgr, &service, READ_ACCESS | SERVICE_ENUMERATE_DEPENDENTS)
        .map_err(|e| map_error(unit, "打开服务", &e))?;

    let cfg = sys::query_config(&svc).map_err(|e| map_error(unit, "读服务配置", &e))?;
    let (services, groups) = map::parse_dependencies(&cfg.dependencies);

    // 反向依赖读不到（权限不足以 ENUMERATE_DEPENDENTS）时给空表而不是整体失败——
    // 正向依赖已经拿到了，丢掉它没有好处。
    let dependents = sys::enum_dependents(&svc).unwrap_or_default();

    Ok(UnitDeps {
        unit: unit.to_owned(),
        requires: services.iter().map(|s| map::to_unit_name(s)).collect(),
        required_by: dependents.iter().map(|s| map::to_unit_name(s)).collect(),
        // 加载顺序组放 `after` 而不是 `requires`，且**保留原始写法**（不加 .service）：
        //
        // - 放 after：SCM 对组的要求是「尝试启动组内全部成员后，至少有一个在跑」，
        //   比 `Requires=` 弱；而「本服务排在该组之后启动」是确定成立的。
        // - 不加后缀：组不是服务，没有可查询的详情，加了后缀会让前端生成一个
        //   点进去必然 404 的链接。`+` 前缀正是它与 unit 名的区分标记。
        after: groups.iter().map(|g| format!("+{g}")).collect(),
        // 以下关系 Windows 没有对应概念：SCM 只有「启动前需要谁」这一种边。
        requisite: Vec::new(),
        wants: Vec::new(),
        binds_to: Vec::new(),
        part_of: Vec::new(),
        wanted_by: Vec::new(),
        bound_by: Vec::new(),
        conflicts: Vec::new(),
        conflicted_by: Vec::new(),
        before: Vec::new(),
        triggers: Vec::new(),
        triggered_by: Vec::new(),
    })
}

/// 一次操作需要的访问权限。**按需申请**，见模块文档「权限」。
fn desired_access(action: UnitAction) -> u32 {
    match action {
        UnitAction::Start => SERVICE_START | SERVICE_QUERY_STATUS,
        UnitAction::Stop => SERVICE_STOP | SERVICE_QUERY_STATUS,
        UnitAction::Restart => SERVICE_START | SERVICE_STOP | SERVICE_QUERY_STATUS,
        // PARAMCHANGE 这类「非停止」的控制码要的是 SERVICE_PAUSE_CONTINUE。
        UnitAction::Reload => SERVICE_PAUSE_CONTINUE | SERVICE_QUERY_STATUS,
        UnitAction::Enable | UnitAction::Disable | UnitAction::Mask | UnitAction::Unmask => {
            SERVICE_CHANGE_CONFIG | SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS
        }
    }
}

fn action_blocking(unit: &str, action: UnitAction) -> ApiResult<UnitActionResp> {
    let service = map::to_service_name(unit);
    let mgr = sys::open_manager(SC_MANAGER_CONNECT)
        .map_err(|e| map_error(unit, "连接服务控制管理器", &e))?;

    // reload 的「该服务接不接受配置变更」是一个**只读**判断，先用只读句柄问清楚：
    // 否则非管理员会先撞上 ACCESS_DENIED，看不到真正的原因。
    if action == UnitAction::Reload {
        let probe = sys::open_service(&mgr, &service, SERVICE_QUERY_STATUS)
            .map_err(|e| map_error(unit, "打开服务", &e))?;
        let st = sys::query_status(&probe).map_err(|e| map_error(unit, "读服务状态", &e))?;
        if st.controls_accepted & SERVICE_ACCEPT_PARAMCHANGE == 0 {
            return Err(ApiError::capability_unavailable(
                "scm",
                format!("{unit} 不接受「配置已变更」通知"),
            )
            .with_detail(
                "该服务没有在 dwControlsAccepted 里声明 SERVICE_ACCEPT_PARAMCHANGE，\
                 与 systemd 里 unit 未声明 ExecReload 同理：reload 无处可下发。\
                 不要静默降级成 restart——那会真的中断服务。",
            ));
        }
    }

    let svc = sys::open_service(&mgr, &service, desired_access(action))
        .map_err(|e| map_error(unit, "打开服务", &e))?;

    match action {
        UnitAction::Start => start_idempotent(unit, &svc)?,
        UnitAction::Stop => stop_idempotent(unit, &svc)?,
        UnitAction::Restart => {
            stop_idempotent(unit, &svc)?;
            wait_until_stopped(unit, &svc)?;
            start_idempotent(unit, &svc)?;
        }
        UnitAction::Reload => {
            sys::control_service(&svc, SERVICE_CONTROL_PARAMCHANGE)
                .map_err(|e| map_error(unit, "下发配置变更通知", &e))?;
        }
        UnitAction::Enable => set_start_type(unit, &svc, SERVICE_AUTO_START)?,
        UnitAction::Disable => set_start_type(unit, &svc, SERVICE_DEMAND_START)?,
        UnitAction::Mask => set_start_type(unit, &svc, SERVICE_DISABLED)?,
        UnitAction::Unmask => {
            // 只有确实被禁用的才动。对一个 AUTO_START 的服务执行 unmask 若无脑
            // 写成 DEMAND_START，等于顺手把它的开机自启关了——那是用户没要求的副作用。
            let cfg = sys::query_config(&svc).map_err(|e| map_error(unit, "读服务配置", &e))?;
            if cfg.start_type == SERVICE_DISABLED {
                set_start_type(unit, &svc, SERVICE_DEMAND_START)?;
            }
        }
    }

    // 操作刚下去，此刻读到的多半还是中间态（START_PENDING 之类），但那正是
    // 「当前状态」，比不给强。读不到就不给。
    let active_state = sys::query_status(&svc)
        .ok()
        .map(|st| map::state_of(st.current_state, st.win32_exit_code).0);

    Ok(UnitActionResp {
        unit: unit.to_owned(),
        action,
        // SCM 没有 job 对象：控制码是同步下发的，没有可供追踪的句柄。
        job: None,
        active_state,
    })
}

/// 启动。已经在跑视为成功——与 systemd 对一个 active unit 执行 start 的表现一致。
fn start_idempotent(unit: &str, svc: &sys::ScHandle) -> ApiResult<()> {
    match sys::start_service(svc) {
        Ok(()) => Ok(()),
        Err(e) if code_of(&e) == ERROR_SERVICE_ALREADY_RUNNING => Ok(()),
        Err(e) => Err(map_error(unit, "启动服务", &e)),
    }
}

/// 停止。本来就没在跑视为成功（同上）。
fn stop_idempotent(unit: &str, svc: &sys::ScHandle) -> ApiResult<()> {
    match sys::control_service(svc, SERVICE_CONTROL_STOP) {
        Ok(_) => Ok(()),
        Err(e) if code_of(&e) == ERROR_SERVICE_NOT_ACTIVE => Ok(()),
        Err(e) => Err(map_error(unit, "停止服务", &e)),
    }
}

/// 轮询等到服务真的停下。
///
/// `ControlService` 只是把 `SERVICE_CONTROL_STOP` 投递给服务，返回时服务多半还在
/// `STOP_PENDING`。restart 若不等这一步，紧接着的 `StartServiceW` 会撞上
/// 「服务正在停止」而失败——或者更糟，SCM 认为它还在跑，直接返回成功，
/// 于是一次 restart 什么也没发生。
fn wait_until_stopped(unit: &str, svc: &sys::ScHandle) -> ApiResult<()> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    loop {
        let st = sys::query_status(svc).map_err(|e| map_error(unit, "读服务状态", &e))?;
        if st.current_state == SERVICE_STOPPED {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ApiError::new(
                ErrorCode::Timeout,
                format!("{unit} 在 {}s 内没有停下来", STOP_TIMEOUT.as_secs()),
            )
            .with_detail(format!(
                "当前状态码 {}；restart 不会在服务尚未停稳时启动它，请稍后重试或先单独 stop",
                st.current_state
            )));
        }
        std::thread::sleep(STOP_POLL);
    }
}

/// 改启动类型（enable / disable / mask / unmask 共用）。
fn set_start_type(unit: &str, svc: &sys::ScHandle, target: u32) -> ApiResult<()> {
    let cfg = sys::query_config(svc).map_err(|e| map_error(unit, "读服务配置", &e))?;
    if !map::is_configurable(cfg.start_type) {
        return Err(ApiError::invalid_request(format!(
            "{unit} 的启动类型不可更改"
        ))
        .with_detail(
            "它是引导期组件（BOOT_START / SYSTEM_START），由内核加载顺序决定何时启动，\
             等同于 systemd 里没有 [Install] 段的 static unit：改它会让下次开机起不来。\
             前端应把启用/禁用按钮置灰，而不是把这次失败当成故障。",
        ));
    }
    sys::change_start_type(svc, target).map_err(|e| map_error(unit, "修改启动类型", &e))
}

// ---------------------------------------------------------------------------
// 轮询任务
// ---------------------------------------------------------------------------

/// 起一个后台任务，每 [`POLL_INTERVAL`] 枚举一次服务并广播变化。
fn spawn_poller(shared: Arc<Shared>) {
    tokio::spawn(async move {
        // 第一轮只建基线，不发事件：订阅者要的是「有什么变了」，
        // 而不是订阅那一刻的全量快照（全量快照走 list_units）。
        let mut prev: Option<HashMap<String, UnitSummary>> = None;
        loop {
            match tokio::task::spawn_blocking(collect_summaries).await {
                Ok(Ok(units)) => {
                    let now: HashMap<String, UnitSummary> =
                        units.into_iter().map(|u| (u.name.clone(), u)).collect();
                    if let Some(old) = prev.as_ref() {
                        let changed = diff(old, &now);
                        if !changed.is_empty() {
                            tracing::debug!(n = changed.len(), "SCM 轮询发现服务状态变化");
                            // 没有订阅者时 send 返回 Err，忽略即可。
                            let _ = shared.events.send(ServiceEvent { units: changed });
                        }
                    }
                    prev = Some(now);
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "SCM 轮询枚举失败，保留上一轮基线"),
                Err(e) => tracing::warn!(error = %e, "SCM 轮询任务异常"),
            }

            tokio::time::sleep(POLL_INTERVAL).await;

            // 最后一个订阅者走了就退出，别为没人看的页面一直问 SCM。
            // 判定与清标志必须在同一把锁下，否则会与 ensure_poller 竞争出
            // 「以为在跑、其实已退出」的空窗。
            if shared.events.receiver_count() == 0 {
                let mut running = shared.polling.lock().unwrap_or_else(|p| p.into_inner());
                if shared.events.receiver_count() == 0 {
                    *running = false;
                    tracing::debug!("没有订阅者，SCM 轮询退出");
                    return;
                }
            }
        }
    });
}

/// 两轮快照的差分：变了的 + 新出现的 + 消失的。
///
/// 消失的用 [`summary_for_vanished`] 造一条 `not_found` / `inactive` 的记录，
/// 与 bus 路径对「被 systemd 移除的 unit」的报法一致，前端据此删行。
fn diff(old: &HashMap<String, UnitSummary>, now: &HashMap<String, UnitSummary>) -> Vec<UnitSummary> {
    let mut changed: Vec<UnitSummary> = Vec::new();
    for (name, u) in now {
        if old.get(name) != Some(u) {
            changed.push(u.clone());
        }
    }
    for name in old.keys() {
        if !now.contains_key(name) {
            changed.push(summary_for_vanished(name, UnitScope::System));
        }
    }
    changed.sort_by(|a, b| a.name.cmp(&b.name));
    changed
}

// ---------------------------------------------------------------------------
// 错误映射
// ---------------------------------------------------------------------------

/// 取 Win32 错误码。
fn code_of(e: &io::Error) -> u32 {
    e.raw_os_error().unwrap_or(0) as u32
}

/// Win32 错误 → [`ApiError`]。
///
/// 三条原则：
///
/// 1. **幂等的两个码不走这里**——`ERROR_SERVICE_ALREADY_RUNNING` 与
///    `ERROR_SERVICE_NOT_ACTIVE` 在调用点就被判成成功（见 [`start_idempotent`] /
///    [`stop_idempotent`]），与 systemd 对已启动 unit 执行 start 的表现一致。
/// 2. **权限判断在前**：被拒时给 `can_retry_elevated`，前端才有提权入口。
/// 3. **提权救不了的不要置 `can_retry_elevated`**：`ERROR_SERVICE_DISABLED`
///    就是典型——以管理员重试一百次也一样被拦，必须先 unmask。
fn map_error(unit: &str, what: &str, e: &io::Error) -> ApiError {
    let code = code_of(e);
    let detail = format!("Win32 错误 {code}：{e}");
    match code {
        ERROR_ACCESS_DENIED => ApiError::permission_denied(format!("没有权限{what}（{unit}）"))
            .with_detail(format!(
                "{detail}。服务的启停与配置修改要求管理员令牌，\
                 提权后重试即可（与 Linux 侧 polkit 拒绝同理）。"
            ))
            .retry_elevated(),

        ERROR_SERVICE_DOES_NOT_EXIST | ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => {
            ApiError::not_found(format!("服务 {unit} 不存在")).with_detail(detail)
        }

        ERROR_SERVICE_DISABLED => ApiError::permission_denied(format!("服务 {unit} 已被禁用"))
            .with_detail(format!(
                "{detail}。Windows 的「已禁用」等同 systemd 的 masked：连手动启动都会被 SCM 拦下。\
                 先对它执行 unmask（把启动类型改回「手动」）再启动——提权本身解决不了这个问题。"
            )),

        ERROR_SERVICE_MARKED_FOR_DELETE => {
            ApiError::new(ErrorCode::Conflict, format!("服务 {unit} 已被标记删除"))
                .with_detail(format!("{detail}。它会在最后一个句柄关闭后消失，重启系统可彻底清除。"))
        }

        ERROR_DEPENDENT_SERVICES_RUNNING => ApiError::new(
            ErrorCode::Conflict,
            format!("还有别的服务依赖 {unit}，SCM 拒绝停止它"),
        )
        .with_detail(format!(
            "{detail}。与 systemd 会连带停止 BindsTo 的 unit 不同，SCM 不做级联停止；\
             请先停掉依赖方（详情页的「被谁依赖」一栏列出了它们）。"
        )),

        ERROR_SERVICE_CANNOT_ACCEPT_CTRL | ERROR_INVALID_SERVICE_CONTROL => ApiError::new(
            ErrorCode::Conflict,
            format!("{unit} 当前状态不接受这个控制码"),
        )
        .with_detail(format!(
            "{detail}。服务正处在启动 / 停止的中间态，或根本没声明接受该控制码；稍后重试。"
        )),

        ERROR_SERVICE_REQUEST_TIMEOUT => {
            ApiError::new(ErrorCode::Timeout, format!("{unit} 没有及时响应控制请求"))
                .with_detail(format!("{detail}。服务进程可能卡住了。"))
        }

        ERROR_SERVICE_DATABASE_LOCKED => ApiError::new(
            ErrorCode::Unavailable,
            "服务数据库正被锁定（有安装程序正在改服务配置）",
        )
        .with_detail(detail),

        _ => ApiError::internal(format!("{what}失败（{unit}）")).with_detail(detail),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::service::{UnitActiveState, UnitEnableState};

    /// 本机实测：列出服务。
    ///
    /// 不断言任何具体服务存在——Server Core、容器镜像、精简版系统上
    /// Spooler / EventLog 之类都可能被裁掉。只断言「数量合理」与「字段自洽」。
    #[tokio::test]
    async fn 本机能列出服务() {
        let scm = ServiceControlManager::new();
        let units = scm
            .list_units(&UnitListQuery::default())
            .await
            .expect("列出服务失败");
        assert!(units.len() > 50, "只列出 {} 个服务", units.len());

        for u in &units {
            assert!(u.name.ends_with(".service"), "unit 名要带后缀：{}", u.name);
            assert_eq!(u.unit_type, "service");
            assert!(!u.description.is_empty(), "{} 的描述是空的", u.name);
            assert_eq!(u.scope, UnitScope::System);
        }
        // 名字必须有序且唯一（apply_list_query 排过序）
        let mut names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "服务名不该重复");

        let 在跑 = units
            .iter()
            .filter(|u| u.active_state == UnitActiveState::Active)
            .count();
        let 自启 = units
            .iter()
            .filter(|u| u.enable_state == Some(UnitEnableState::Enabled))
            .count();
        let 禁用 = units
            .iter()
            .filter(|u| u.enable_state == Some(UnitEnableState::Masked))
            .count();
        assert!(在跑 > 5, "在跑的服务只有 {在跑} 个，不合常理");
        assert!(自启 > 5, "开机自启的服务只有 {自启} 个，不合常理");
        eprintln!(
            "本机服务 {} 个：在跑 {在跑}，自启 {自启}，禁用(=masked) {禁用}",
            units.len()
        );
        for u in units.iter().filter(|u| u.active_state == UnitActiveState::Active).take(5) {
            eprintln!(
                "  {} | {} | {:?}/{} | {:?}",
                u.name, u.description, u.active_state, u.sub_state, u.enable_state
            );
        }
        // 被禁用的服务其 load_state 必须是 masked（与 systemd 一致）
        for u in units.iter().filter(|u| u.enable_state == Some(UnitEnableState::Masked)) {
            assert_eq!(u.load_state, UnitLoadState::Masked, "{}", u.name);
        }
    }

    /// 过滤器走的是共享的 `apply_list_query`，这里只验证它确实被接上了。
    #[tokio::test]
    async fn 列表过滤生效() {
        let scm = ServiceControlManager::new();
        let all = scm.list_units(&UnitListQuery::default()).await.unwrap();
        let running = scm
            .list_units(&UnitListQuery {
                state: Some(UnitActiveState::Active),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(running.len() < all.len(), "不可能全部服务都在跑");
        assert!(running.iter().all(|u| u.active_state == UnitActiveState::Active));

        // 类型过滤：SCM 只产出 service
        let timers = scm
            .list_units(&UnitListQuery {
                unit_type: Some("timer".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(timers.is_empty());
    }

    /// 本机实测：对第一个在跑的服务取详情。
    #[tokio::test]
    async fn 本机能读服务详情() {
        let scm = ServiceControlManager::new();
        let units = scm.list_units(&UnitListQuery::default()).await.unwrap();
        let target = units
            .iter()
            .find(|u| u.active_state == UnitActiveState::Active)
            .expect("本机总该有在跑的服务");

        let d = scm
            .unit_detail(UnitScope::System, &target.name)
            .await
            .expect("读详情失败");
        assert_eq!(d.summary.name, target.name);
        assert_eq!(d.summary.active_state, UnitActiveState::Active);
        assert!(d.main_pid.is_some(), "在跑的服务该有 pid");
        assert!(
            d.fragment_path
                .as_deref()
                .is_some_and(|p| p.starts_with(r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\")),
            "fragment_path 该是注册表键：{:?}",
            d.fragment_path
        );
        // SCM 拿不到的一律 None / 空，不许编
        assert_eq!(d.n_restarts, None);
        assert_eq!(d.active_enter_ts, None);
        assert_eq!(d.state_change_ts, None);
        assert_eq!(d.cgroup, None);
        assert!(d.documentation.is_empty());
        assert!(d.drop_in_paths.is_empty());
        // 运行中不给退出码（那个 0 不是「上次成功退出」）
        assert_eq!(d.exit_code, None);
        assert_eq!(d.result, None);
        eprintln!(
            "{} pid={:?} user={:?} enable={:?}\n  描述: {}",
            d.summary.name, d.main_pid, d.user, d.summary.enable_state, d.summary.description
        );
    }

    /// 本机实测：unit 文件 = 注册表键渲染。
    #[tokio::test]
    async fn 本机能读服务的注册表定义() {
        let scm = ServiceControlManager::new();
        let units = scm.list_units(&UnitListQuery::default()).await.unwrap();
        let target = units
            .iter()
            .find(|u| u.active_state == UnitActiveState::Active)
            .expect("本机总该有在跑的服务");

        let f = scm
            .unit_file(UnitScope::System, &target.name)
            .await
            .expect("读 unit 文件失败");
        assert_eq!(f.unit, target.name);
        // Windows 没有 drop-in
        assert!(f.drop_ins.is_empty());
        let frag = f.fragment.expect("该有主 fragment");
        assert!(frag.path.starts_with(r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\"));
        assert!(
            frag.content.starts_with(&format!("[{}]", frag.path)),
            "首行必须是键路径：{:?}",
            frag.content.lines().next()
        );
        // 任何服务键下都该有 ImagePath 或 Type 之类的值
        assert!(frag.content.lines().count() > 1, "键下一个值都没有？");
        eprintln!("--- {} 的 unit 文件 ---\n{}", target.name, frag.content);
    }

    /// 本机实测：依赖图。找一个真有依赖的服务（几乎每台机器都有）。
    #[tokio::test]
    async fn 本机能读服务依赖() {
        let scm = ServiceControlManager::new();
        let units = scm.list_units(&UnitListQuery::default()).await.unwrap();

        let mut 有正向的 = 0usize;
        let mut 有反向的 = 0usize;
        let mut 有组的 = 0usize;
        for u in units.iter().take(60) {
            let Ok(d) = scm.unit_deps(UnitScope::System, &u.name).await else {
                continue;
            };
            assert_eq!(d.unit, u.name);
            // Windows 只有一种边，其余关系必须是空表而不是伪造
            assert!(d.wants.is_empty() && d.conflicts.is_empty() && d.before.is_empty());
            assert!(d.requisite.is_empty() && d.part_of.is_empty() && d.binds_to.is_empty());
            assert!(d.requires.iter().all(|r| r.ends_with(".service")));
            assert!(d.after.iter().all(|g| g.starts_with('+')), "组名要带 + 前缀");
            if !d.requires.is_empty() {
                if 有正向的 == 0 {
                    eprintln!("{} 依赖 {:?}，加载顺序组 {:?}", u.name, d.requires, d.after);
                }
                有正向的 += 1;
            }
            if !d.required_by.is_empty() {
                有反向的 += 1;
            }
            if !d.after.is_empty() {
                有组的 += 1;
            }
        }
        eprintln!("前 60 个服务里：有正向依赖 {有正向的}，有反向依赖 {有反向的}，依赖加载顺序组 {有组的}");
        assert!(有正向的 > 0, "一台 Windows 上不可能没有任何服务依赖");
    }

    /// 不存在的服务必须是 404，不能是 500。
    #[tokio::test]
    async fn 不存在的服务报_404() {
        let scm = ServiceControlManager::new();
        for r in [
            scm.unit_detail(UnitScope::System, "绝无此服务xyz.service")
                .await
                .err(),
            scm.unit_file(UnitScope::System, "绝无此服务xyz.service")
                .await
                .err(),
            scm.unit_deps(UnitScope::System, "绝无此服务xyz.service")
                .await
                .err(),
        ] {
            let e = r.expect("该失败");
            assert_eq!(e.code, ErrorCode::NotFound, "detail={:?}", e.detail);
        }
    }

    /// 非法 unit 名在碰 SCM 之前就该被拦下。
    #[tokio::test]
    async fn 非法_unit_名报_400() {
        let scm = ServiceControlManager::new();
        let e = scm
            .unit_detail(UnitScope::System, r"..\..\windows\system32.service")
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);
        let e = scm.unit_detail(UnitScope::System, "").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);
    }

    /// 用户作用域在 Windows 上不存在，必须明确报缺失而不是拿系统服务冒充。
    #[tokio::test]
    async fn 用户作用域报能力缺失() {
        let scm = ServiceControlManager::new();
        let e = scm
            .list_units(&UnitListQuery {
                scope: Some(UnitScope::User),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::CapabilityUnavailable);
        assert_eq!(e.capability.as_deref(), Some("scm"));
        let e = scm.list_timers(UnitScope::User).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::CapabilityUnavailable);
    }

    /// 定时任务：契约里没有 Windows 的 TimerSource，只能报空表。
    #[tokio::test]
    async fn 定时任务返回空表() {
        let scm = ServiceControlManager::new();
        assert!(scm.list_timers(UnitScope::System).await.unwrap().is_empty());
    }

    /// 探测：任何 Windows 上都该 Available。
    #[tokio::test]
    async fn 本机探测可用() {
        let scm = ServiceControlManager::new();
        assert_eq!(scm.id(), "scm", "capability::probe_all 按这个 id 点亮能力位");
        let p = scm.probe().await;
        assert_eq!(p, Probe::Available, "SCM 探测应当可用：{p:?}");
    }

    /// 订阅：receiver 不会立刻 `Closed`，轮询任务被拉起。
    ///
    /// 不断言「一定收到事件」——一台空闲机器五秒内可能什么服务都没变，
    /// 那样的断言会变成一个随机失败的测试。
    #[tokio::test]
    async fn 订阅返回可用的_receiver() {
        let scm = ServiceControlManager::new();
        let mut rx = scm.subscribe().await;
        assert!(
            *scm.shared.polling.lock().unwrap(),
            "首次 subscribe 应当把轮询任务拉起来"
        );
        // 第二次订阅不该再起一个任务
        let _rx2 = scm.subscribe().await;

        match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
            Err(_) => {} // 超时 = 这段时间没有服务变化，正常
            Ok(Ok(ev)) => eprintln!("收到 {} 条服务变化", ev.units.len()),
            Ok(Err(e)) => panic!("receiver 不该关闭：{e}"),
        }
    }

    /// 差分只报变化的那几条，消失的报成 `not_found`。
    #[test]
    fn 差分只报变化的() {
        let mk = |name: &str, st: UnitActiveState| UnitSummary {
            name: name.into(),
            unit_type: "service".into(),
            description: name.into(),
            load_state: UnitLoadState::Loaded,
            active_state: st,
            sub_state: "running".into(),
            enable_state: Some(UnitEnableState::Enabled),
            scope: UnitScope::System,
        };
        let old: HashMap<String, UnitSummary> = [
            ("a.service".to_owned(), mk("a.service", UnitActiveState::Active)),
            ("b.service".to_owned(), mk("b.service", UnitActiveState::Active)),
            ("gone.service".to_owned(), mk("gone.service", UnitActiveState::Active)),
        ]
        .into();
        let now: HashMap<String, UnitSummary> = [
            ("a.service".to_owned(), mk("a.service", UnitActiveState::Active)),
            ("b.service".to_owned(), mk("b.service", UnitActiveState::Failed)),
            ("new.service".to_owned(), mk("new.service", UnitActiveState::Active)),
        ]
        .into();

        let changed = diff(&old, &now);
        let names: Vec<&str> = changed.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["b.service", "gone.service", "new.service"], "a 没变，不该出现");
        let gone = changed.iter().find(|u| u.name == "gone.service").unwrap();
        assert_eq!(gone.load_state, UnitLoadState::NotFound);
        assert_eq!(gone.active_state, UnitActiveState::Inactive);
    }

    /// 访问权限必须按操作最小化：只读路径绝不能要 CHANGE_CONFIG / START / STOP。
    #[test]
    fn 访问权限按需申请() {
        assert_eq!(READ_ACCESS, SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS);
        assert_eq!(READ_ACCESS & (SERVICE_CHANGE_CONFIG | SERVICE_START | SERVICE_STOP), 0);
        assert_eq!(desired_access(UnitAction::Start) & SERVICE_STOP, 0, "start 不该要停止权");
        assert_eq!(desired_access(UnitAction::Stop) & SERVICE_START, 0, "stop 不该要启动权");
        assert_ne!(desired_access(UnitAction::Restart) & SERVICE_START, 0);
        assert_ne!(desired_access(UnitAction::Restart) & SERVICE_STOP, 0);
        // PARAMCHANGE 走的是 PAUSE_CONTINUE 这一档，不是 STOP
        assert_ne!(desired_access(UnitAction::Reload) & SERVICE_PAUSE_CONTINUE, 0);
        assert_eq!(desired_access(UnitAction::Reload) & SERVICE_STOP, 0);
        for a in [
            UnitAction::Enable,
            UnitAction::Disable,
            UnitAction::Mask,
            UnitAction::Unmask,
        ] {
            assert_ne!(desired_access(a) & SERVICE_CHANGE_CONFIG, 0);
            assert_eq!(desired_access(a) & (SERVICE_START | SERVICE_STOP), 0);
        }
    }

    /// 错误映射：三类的码、提权标志与说明都要对。
    #[test]
    fn 错误映射() {
        let err = |code: u32| io::Error::from_raw_os_error(code as i32);

        let e = map_error("x.service", "启动服务", &err(ERROR_ACCESS_DENIED));
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated, "被拒时要给提权入口");

        let e = map_error("x.service", "打开服务", &err(ERROR_SERVICE_DOES_NOT_EXIST));
        assert_eq!(e.code, ErrorCode::NotFound);

        // 被禁用 = masked：提权也没用，不能置 can_retry_elevated
        let e = map_error("x.service", "启动服务", &err(ERROR_SERVICE_DISABLED));
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(!e.can_retry_elevated, "提权救不了 masked，别给假希望");
        assert!(e.detail.as_deref().is_some_and(|d| d.contains("unmask")));

        let e = map_error("x.service", "停止服务", &err(ERROR_DEPENDENT_SERVICES_RUNNING));
        assert_eq!(e.code, ErrorCode::Conflict);

        let e = map_error("x.service", "停止服务", &err(ERROR_SERVICE_REQUEST_TIMEOUT));
        assert_eq!(e.code, ErrorCode::Timeout);

        // 没见过的码不许冒充成已知语义
        let e = map_error("x.service", "启动服务", &err(1234));
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(e.detail.as_deref().is_some_and(|d| d.contains("1234")));
    }
}
