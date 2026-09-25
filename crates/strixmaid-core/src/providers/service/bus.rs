//! `SystemdBus`：zbus 直连 `org.freedesktop.systemd1` 的主路径。
//!
//! - 列表：`ListUnits`（已加载）+ `ListUnitFiles`（磁盘上的 unit 文件，含 enable state）合并。
//! - 详情：`LoadUnit` 拿对象路径，再对 `org.freedesktop.systemd1.Unit` 与类型接口
//!   （`.Service` / `.Socket` / …）各做一次 `GetAll`——两次调用取全部属性，而不是二十次 `Get`。
//!   用 `LoadUnit` 而非 `GetUnit`：后者对「有文件但没加载」的 unit 会报 NoSuchUnit，
//!   而 `systemctl status` 正是靠 `LoadUnit` 才能显示这类 unit。
//! - cgroup 用量直读 `/sys/fs/cgroup/<ControlGroup>/`（[`super::cgroup`]），读不到再回落 systemd 属性。
//! - 操作：`StartUnit` 等用 `replace` 模式，返回 job 路径；`EnableUnitFiles` 等之后 `Reload()`。
//! - 事件：`Subscribe()` 后监听 `UnitNew` / `UnitRemoved` / `JobRemoved` 与所有 unit 对象的
//!   `PropertiesChanged`，按 [`super::EVENT_DEBOUNCE`] 去抖后统一取当前状态广播。
//!
//! 授权全部交给 polkit（`docs/design.md` §1 原则 3）：`AccessDenied` /
//! `InteractiveAuthorizationRequired` 映射为 `ErrorCode::PermissionDenied` + `can_retry_elevated`。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use strixmaid_types::service::{
    CgroupUsage, TimerEntry, TimerSource, UnitAction, UnitActionResp, UnitActiveState, UnitDetail,
    UnitFile, UnitListQuery, UnitScope, UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OnceCell, broadcast};
use zbus::names::{BusName, InterfaceName};
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream};

use super::cgroup::CgroupReader;
use super::{
    EVENT_CAPACITY, FlushQueue, ServiceEvent, ServiceProvider, UnitDeps,
    apply_list_query,
    lookup_enable_state, opt_u64, parse_active_state, parse_enable_state, parse_load_state,
    read_unit_fragment, summary_for_unloaded_file, summary_for_vanished, unit_file_basename,
    unit_type_of, usec_to_ts, validate_unit_name, with_timeout,
};
use crate::providers::{Probe, Provider};

/// systemd 在 bus 上的名字。
const SYSTEMD_DEST: &str = "org.freedesktop.systemd1";
/// unit 对象路径前缀。
const UNIT_PATH_PREFIX: &str = "/org/freedesktop/systemd1/unit/";
/// 所有 unit 共有的接口。
const IFACE_UNIT: &str = "org.freedesktop.systemd1.Unit";

/// 监听任务多久检查一次「还有没有人在看」。
///
/// 这是 2026-09-25 事故的**暴露面收敛**：没有订阅者就退场，不再长期持有
/// 对系统总线的信号订阅。间隔取 30 秒——退场晚一点无害，查得太勤则是白费。
const IDLE_CHECK: std::time::Duration = std::time::Duration::from_secs(30);

/// `ListUnits` 的一行：name / description / load / active / sub / following / path / job id / job type / job path。
pub type UnitListEntry = (
    String,
    String,
    String,
    String,
    String,
    String,
    OwnedObjectPath,
    u32,
    String,
    OwnedObjectPath,
);

/// `EnableUnitFiles` 等返回的一条变更：type / symlink / destination。
pub type FileChange = (String, String, String);

/// `org.freedesktop.systemd1.Manager` 里我们用到的子集。
#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait Manager {
    fn list_units(&self) -> zbus::Result<Vec<UnitListEntry>>;
    fn list_unit_files(&self) -> zbus::Result<Vec<(String, String)>>;
    fn get_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn load_unit(&self, name: &str) -> zbus::Result<OwnedObjectPath>;
    fn get_unit_file_state(&self, name: &str) -> zbus::Result<String>;
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn restart_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn reload_unit(&self, name: &str, mode: &str) -> zbus::Result<OwnedObjectPath>;
    fn enable_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<(bool, Vec<FileChange>)>;
    fn disable_unit_files(&self, files: &[&str], runtime: bool) -> zbus::Result<Vec<FileChange>>;
    fn mask_unit_files(
        &self,
        files: &[&str],
        runtime: bool,
        force: bool,
    ) -> zbus::Result<Vec<FileChange>>;
    fn unmask_unit_files(&self, files: &[&str], runtime: bool) -> zbus::Result<Vec<FileChange>>;
    fn reload(&self) -> zbus::Result<()>;
    fn subscribe(&self) -> zbus::Result<()>;
    fn unsubscribe(&self) -> zbus::Result<()>;

    #[zbus(signal)]
    fn unit_new(&self, id: String, unit: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn unit_removed(&self, id: String, unit: OwnedObjectPath) -> zbus::Result<()>;
    #[zbus(signal)]
    fn job_removed(
        &self,
        id: u32,
        job: OwnedObjectPath,
        unit: String,
        result: String,
    ) -> zbus::Result<()>;
}

/// `GetAll` 返回的属性包，按名字取值并做类型转换。
#[derive(Debug, Default)]
struct Props(HashMap<String, OwnedValue>);

impl Props {
    fn take<T: TryFrom<OwnedValue>>(&mut self, key: &str) -> Option<T> {
        self.0.remove(key).and_then(|v| T::try_from(v).ok())
    }
    /// 取原始值（复合类型自己拆）。
    fn value(&mut self, key: &str) -> Option<OwnedValue> {
        self.0.remove(key)
    }
    fn string(&mut self, key: &str) -> String {
        self.take::<String>(key).unwrap_or_default()
    }
    /// 空串视为「未设置」。
    fn opt_string(&mut self, key: &str) -> Option<String> {
        self.take::<String>(key).filter(|s| !s.is_empty())
    }
    fn strings(&mut self, key: &str) -> Vec<String> {
        self.take::<Vec<String>>(key).unwrap_or_default()
    }
    fn u64(&mut self, key: &str) -> Option<u64> {
        self.take::<u64>(key)
    }
    fn u32(&mut self, key: &str) -> Option<u32> {
        self.take::<u32>(key)
    }
    fn i32(&mut self, key: &str) -> Option<i32> {
        self.take::<i32>(key)
    }
}

/// unit 类型 → 带 cgroup / 进程信息的类型接口。target / timer / path / device 没有。
fn type_interface(unit_type: &str) -> Option<&'static str> {
    Some(match unit_type {
        "service" => "org.freedesktop.systemd1.Service",
        "socket" => "org.freedesktop.systemd1.Socket",
        "mount" => "org.freedesktop.systemd1.Mount",
        "swap" => "org.freedesktop.systemd1.Swap",
        "slice" => "org.freedesktop.systemd1.Slice",
        "scope" => "org.freedesktop.systemd1.Scope",
        _ => return None,
    })
}

/// unit 对象路径 → unit 名。systemd 把非 `[A-Za-z0-9]` 的字节转义成 `_XX`（小写 hex）。
pub fn unit_name_from_path(path: &str) -> Option<String> {
    let escaped = path.strip_prefix(UNIT_PATH_PREFIX)?;
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// zbus 错误 → [`ApiError`]。`unit` 只用于拼消息。
pub fn map_zbus_error(e: zbus::Error, unit: &str) -> ApiError {
    match &e {
        zbus::Error::MethodError(name, msg, _) => {
            let detail = msg.clone().unwrap_or_default();
            map_error_name(name.as_str(), detail, unit)
        }
        zbus::Error::FDO(fdo) => match fdo.as_ref() {
            zbus::fdo::Error::AccessDenied(m)
            | zbus::fdo::Error::AuthFailed(m)
            | zbus::fdo::Error::InteractiveAuthorizationRequired(m) => denied(unit, m.clone()),
            zbus::fdo::Error::ServiceUnknown(m) | zbus::fdo::Error::NameHasNoOwner(m) => {
                ApiError::new(ErrorCode::Unavailable, "systemd 不在 bus 上").with_detail(m.clone())
            }
            zbus::fdo::Error::NoReply(m) | zbus::fdo::Error::Timeout(m) => {
                ApiError::new(ErrorCode::Timeout, "systemd 无响应").with_detail(m.clone())
            }
            other => ApiError::internal(format!("systemd 调用失败（{unit}）"))
                .with_detail(other.to_string()),
        },
        zbus::Error::InputOutput(_) | zbus::Error::Connection(..) | zbus::Error::Handshake(_) => {
            ApiError::new(ErrorCode::Unavailable, "systemd bus 连接中断").with_detail(e.to_string())
        }
        _ => ApiError::internal(format!("systemd 调用失败（{unit}）")).with_detail(e.to_string()),
    }
}

/// 按 D-Bus 错误名分类。
fn map_error_name(name: &str, detail: String, unit: &str) -> ApiError {
    match name {
        "org.freedesktop.systemd1.NoSuchUnit" | "org.freedesktop.DBus.Error.FileNotFound" => {
            ApiError::not_found(format!("unit {unit} 不存在")).with_detail(detail)
        }
        "org.freedesktop.DBus.Error.AccessDenied"
        | "org.freedesktop.DBus.Error.AuthFailed"
        | "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired" => denied(unit, detail),
        "org.freedesktop.DBus.Error.NoReply" | "org.freedesktop.DBus.Error.Timeout" => {
            ApiError::new(ErrorCode::Timeout, "systemd 无响应").with_detail(detail)
        }
        "org.freedesktop.DBus.Error.ServiceUnknown"
        | "org.freedesktop.DBus.Error.NameHasNoOwner" => {
            ApiError::new(ErrorCode::Unavailable, "systemd 不在 bus 上").with_detail(detail)
        }
        "org.freedesktop.systemd1.UnitMasked"
        | "org.freedesktop.systemd1.NoSuchJob"
        | "org.freedesktop.systemd1.JobTypeNotApplicable"
        | "org.freedesktop.systemd1.UnitExists"
        | "org.freedesktop.systemd1.OnlyByDependency"
        | "org.freedesktop.systemd1.LoadFailed"
        | "org.freedesktop.systemd1.BadUnitSetting"
        | "org.freedesktop.systemd1.ShuttingDown"
        | "org.freedesktop.systemd1.TransactionIsDestructive"
        | "org.freedesktop.DBus.Error.FileExists" => ApiError::new(
            ErrorCode::Conflict,
            format!("systemd 拒绝了对 {unit} 的操作"),
        )
        .with_detail(format!("{name}: {detail}")),
        _ => ApiError::internal(format!("systemd 调用失败（{unit}）"))
            .with_detail(format!("{name}: {detail}")),
    }
}

fn denied(unit: &str, detail: String) -> ApiError {
    ApiError::permission_denied(format!("需要管理访问：polkit 拒绝了对 {unit} 的操作"))
        .with_detail(detail)
        .retry_elevated()
}

/// 事件监听任务是否已启动（每个作用域一个）。
#[derive(Debug, Default)]
struct ListenerFlags {
    system: bool,
    user: bool,
}

/// 与监听任务共享的部分。
#[derive(Debug)]
struct Shared {
    cgroup: CgroupReader,
    events: broadcast::Sender<ServiceEvent>,
    listeners: Mutex<ListenerFlags>,
}

/// zbus 路径的 service provider。
#[derive(Debug)]
pub struct SystemdBus {
    system: Connection,
    user_uid: u32,
    user: OnceCell<Connection>,
    shared: Arc<Shared>,
}

impl SystemdBus {
    /// 连 system bus。连不上返回 `ErrorCode::Unavailable`（调用方据此降级到 systemctl）。
    pub async fn connect() -> ApiResult<Self> {
        let system = Connection::system().await.map_err(|e| {
            ApiError::new(ErrorCode::Unavailable, "连接 system bus 失败").with_detail(e.to_string())
        })?;
        Ok(Self::from_connection(system))
    }

    /// 用已有连接构造（测试 / worker 复用连接）。user 作用域缺省指向本进程 uid。
    pub fn from_connection(system: Connection) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            system,
            user_uid: nix::unistd::Uid::current().as_raw(),
            user: OnceCell::new(),
            shared: Arc::new(Shared {
                cgroup: CgroupReader::new(),
                events,
                listeners: Mutex::new(ListenerFlags::default()),
            }),
        }
    }

    /// 指定 `scope=user` 对应的 uid。只有本进程 uid（或 root 时的自身）能真正连上，
    /// 其他 uid 会因 EXTERNAL 认证失败而 `Unavailable`——跨用户请走 worker。
    #[must_use]
    pub fn with_user_uid(mut self, uid: u32) -> Self {
        self.user_uid = uid;
        self
    }

    /// `scope=user` 对应的 uid。
    pub fn user_uid(&self) -> u32 {
        self.user_uid
    }

    /// 事件发送端（WS hub 也可以直接 `subscribe()`）。
    pub fn event_sender(&self) -> &broadcast::Sender<ServiceEvent> {
        &self.shared.events
    }

    /// session bus 地址：本进程自己的 uid 优先用 `$XDG_RUNTIME_DIR/bus`，否则按约定 `/run/user/<uid>/bus`。
    fn user_bus_address(uid: u32) -> String {
        let is_self = nix::unistd::Uid::current().as_raw() == uid;
        if is_self
            && let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
            && !dir.is_empty()
        {
            return format!("unix:path={dir}/bus");
        }
        format!("unix:path=/run/user/{uid}/bus")
    }

    /// 懒连接 session bus。失败不缓存，下次调用会重试。
    async fn user_conn(&self) -> ApiResult<&Connection> {
        let uid = self.user_uid;
        self.user
            .get_or_try_init(|| async move {
                let addr = Self::user_bus_address(uid);
                zbus::connection::Builder::address(addr.as_str())
                    .map_err(|e| (e, addr.clone()))?
                    .build()
                    .await
                    .map_err(|e| (e, addr))
            })
            .await
            .map_err(|(e, addr)| {
                ApiError::new(
                    ErrorCode::Unavailable,
                    format!("uid {uid} 的用户级 systemd 不可用（session bus 连不上）"),
                )
                .with_detail(format!("{addr}: {e}"))
            })
    }

    async fn conn(&self, scope: UnitScope) -> ApiResult<Connection> {
        Ok(match scope {
            UnitScope::System => self.system.clone(),
            UnitScope::User => self.user_conn().await?.clone(),
        })
    }

    async fn manager(conn: &Connection) -> ApiResult<ManagerProxy<'static>> {
        ManagerProxy::builder(conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|e| map_zbus_error(e, "manager"))
    }

    /// 对某个对象路径的某个接口做 `GetAll`。
    async fn get_all(
        conn: &Connection,
        path: &OwnedObjectPath,
        iface: &'static str,
    ) -> ApiResult<Props> {
        let proxy = zbus::fdo::PropertiesProxy::builder(conn)
            .destination(SYSTEMD_DEST)
            .and_then(|b| b.path(path.clone()))
            .map_err(|e| map_zbus_error(e, path.as_str()))?
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|e| map_zbus_error(e, path.as_str()))?;
        let map = proxy
            .get_all(InterfaceName::from_static_str_unchecked(iface))
            .await
            .map_err(|e| map_zbus_error(zbus::Error::FDO(Box::new(e)), path.as_str()))?;
        Ok(Props(map))
    }

    /// `LoadUnit` + `GetAll(Unit)`；`LoadState=not-found` 归一成 404。
    async fn load_unit_props(
        &self,
        scope: UnitScope,
        unit: &str,
    ) -> ApiResult<(Connection, OwnedObjectPath, Props)> {
        validate_unit_name(unit)?;
        let conn = self.conn(scope).await?;
        let mgr = Self::manager(&conn).await?;
        let path = mgr
            .load_unit(unit)
            .await
            .map_err(|e| map_zbus_error(e, unit))?;
        let props = Self::get_all(&conn, &path, IFACE_UNIT).await?;
        if props
            .0
            .get("LoadState")
            .and_then(|v| <&str>::try_from(v).ok())
            == Some("not-found")
        {
            return Err(ApiError::not_found(format!("unit {unit} 不存在")));
        }
        Ok((conn, path, props))
    }

    /// 从 `Unit` 接口属性造摘要。`fallback_name` 用于 `Id` 缺失时。
    fn summary_from_props(fallback_name: &str, p: &mut Props, scope: UnitScope) -> UnitSummary {
        let name = p
            .opt_string("Id")
            .unwrap_or_else(|| fallback_name.to_owned());
        let description = p.opt_string("Description").unwrap_or_else(|| name.clone());
        UnitSummary {
            unit_type: unit_type_of(&name).to_owned(),
            description,
            load_state: parse_load_state(&p.string("LoadState")),
            active_state: parse_active_state(&p.string("ActiveState")),
            sub_state: p.string("SubState"),
            enable_state: parse_enable_state(&p.string("UnitFileState")),
            scope,
            name,
        }
    }

    /// 取单个 unit 的当前摘要，**不会**触发加载：`GetUnit` 失败（未加载）时回落到 unit 文件状态，
    /// 连文件也没有则视为已消失。事件流用它，避免 `LoadUnit` 把刚被 GC 的 unit 又拉回来。
    async fn summary_no_load(
        conn: &Connection,
        mgr: &ManagerProxy<'_>,
        name: &str,
        scope: UnitScope,
    ) -> UnitSummary {
        if let Ok(path) = mgr.get_unit(name).await
            && let Ok(mut props) = Self::get_all(conn, &path, IFACE_UNIT).await
        {
            return Self::summary_from_props(name, &mut props, scope);
        }
        match mgr.get_unit_file_state(name).await {
            Ok(state) => summary_for_unloaded_file(name, &state, scope),
            Err(_) => summary_for_vanished(name, scope),
        }
    }

    /// 列表：`ListUnits` ∪ `ListUnitFiles`。
    async fn list_units_raw(conn: &Connection, scope: UnitScope) -> ApiResult<Vec<UnitSummary>> {
        let mgr = Self::manager(conn).await?;
        let (loaded, files) = tokio::try_join!(mgr.list_units(), mgr.list_unit_files())
            .map_err(|e| map_zbus_error(e, "list"))?;
        Ok(merge_lists(loaded, files, scope))
    }

    /// 操作后立刻读一次活动状态，失败就不给。
    async fn active_state_now(
        conn: &Connection,
        mgr: &ManagerProxy<'_>,
        unit: &str,
    ) -> Option<UnitActiveState> {
        let path = mgr.get_unit(unit).await.ok()?;
        let mut props = Self::get_all(conn, &path, IFACE_UNIT).await.ok()?;
        Some(parse_active_state(&props.string("ActiveState")))
    }

    /// 确保某作用域的事件监听任务已启动（同一时刻只有一个）。
    ///
    /// `Subscribe()` 与 match rule 注册在**本调用内**完成后才返回，这样调用方
    /// `subscribe()` 一返回就立刻触发的操作也不会漏掉信号。
    async fn ensure_listener(&self, scope: UnitScope, conn: Connection) {
        spawn_listener(conn, scope, Arc::clone(&self.shared)).await;
    }
}

/// 起一个监听任务。`ensure_listener` 与监听自己的「退场后发现又有人来了」
/// 都走这里——两处必须是同一套语义，分两份写迟早会走样。
///
/// 标志的所有权：本函数抢到标志（`set_listener_flag` 返回 true）就归它，
/// 一路交到 [`run_listener`] 手里，由后者在退场收尾时归还。中途失败则当场归还。
async fn spawn_listener(conn: Connection, scope: UnitScope, shared: Arc<Shared>) {
    if !shared.set_listener_flag(scope, true) {
        return; // 已经有一个在跑
    }
    // setup 里是六连发的总线往返，而本函数被 WS 订阅经 block_in_place
    // 同步等着：不包超时的话，总线一慢每个订阅请求就永久占一个运行时线程。
    match with_timeout("监听建立", ListenerStreams::setup(&conn)).await {
        Ok(streams) => {
            tokio::spawn(run_listener(conn, scope, shared, streams));
        }
        Err(e) => {
            tracing::warn!(?scope, error = %e, "systemd 事件监听启动失败");
            shared.set_listener_flag(scope, false);
        }
    }
}

impl Shared {
    /// 退场协议第一步：**只判断**该不该退场，不动标志。
    ///
    /// 标志要一直攥到退订真正做完为止（见 [`Shared::finish_retire`]）——
    /// 这一条是 2026-09-25 事故复盘里补上的：先清标志再退订的话，窗口里
    /// 来的订阅者会撞上 systemd 的 `AlreadySubscribed`，或者它刚订阅成功就被
    /// 我们这条正在收尾的 `Unsubscribe` 撤销掉，两种都表现为「页面一直没有
    /// 事件」而且不会自愈。
    fn should_retire(&self) -> bool {
        // 与 `ServiceProvider::subscribe` 的顺序配合：那边**先**拿 receiver
        // 再调 `ensure_listener`，所以只要它拿到了 receiver，这里就看得见
        // `receiver_count() > 0`，不会误退场。
        self.events.receiver_count() == 0
    }

    /// 退场协议第二步：退订做完了，归还标志。
    ///
    /// 返回 `true` 表示**退场期间又有人订阅了**——那个订阅者调
    /// `ensure_listener` 时看到标志还被我们占着，于是什么也没做，它现在
    /// 一个事件都收不到。调用方据此替它重开一个监听。
    ///
    /// 检查与归还在同一把锁内完成，否则这个「又有人来了」的判断本身还会有窗口。
    fn finish_retire(&self, scope: UnitScope) -> bool {
        let mut flags = self.listeners.lock().unwrap_or_else(|p| p.into_inner());
        let flag = match scope {
            UnitScope::System => &mut flags.system,
            UnitScope::User => &mut flags.user,
        };
        *flag = false;
        self.events.receiver_count() > 0
    }

    /// 置监听标志；返回值表示**本次调用改变了它**（用于「只启动一次」）。
    fn set_listener_flag(&self, scope: UnitScope, value: bool) -> bool {
        let mut flags = self.listeners.lock().unwrap_or_else(|p| p.into_inner());
        let flag = match scope {
            UnitScope::System => &mut flags.system,
            UnitScope::User => &mut flags.user,
        };
        let changed = *flag != value;
        *flag = value;
        changed
    }
}

/// 四路信号流。**所有权始终交给 [`listen_loop`]，它返回即全部释放**——
/// 这不是风格偏好：这四路流是连接上仅有的有界广播队列，谁在持有它们的
/// 同时 `await` 总线往返，谁就可能复刻 2026-09-25 的环形死锁（流不被
/// poll → 队列满 → zbus 读取任务挂在投递上 → 回复读不出来）。把它们
/// 装进独立结构、以值传给热循环，就是让「退场路径上没有队列」由借用
/// 检查器保证，而不是由注释恳求。
struct SignalStreams {
    unit_new: UnitNewStream,
    unit_removed: UnitRemovedStream,
    job_removed: JobRemovedStream,
    props_changed: MessageStream,
}

/// 监听的全部总线资源：manager proxy（冲刷任务用）+ 四路信号流（热循环用）。
struct ListenerStreams {
    mgr: ManagerProxy<'static>,
    signals: SignalStreams,
}

impl ListenerStreams {
    async fn setup(conn: &Connection) -> ApiResult<Self> {
        let mgr = SystemdBus::manager(conn).await?;
        match mgr.subscribe().await {
            Ok(()) => {}
            // systemd 对同一条连接重复 Subscribe 报 AlreadySubscribed。对我们
            // 这是成功：上一任监听的退订没送达（超时、连接抖动）时订阅本来
            // 就还挂着。当失败处理的话，一次漏掉的退订就会让本频道在进程
            // 余生里再也订不上——每次都撞这个错、每次都放弃。
            Err(zbus::Error::MethodError(ref name, _, _))
                if name.as_str() == "org.freedesktop.systemd1.AlreadySubscribed" => {}
            Err(e) => return Err(map_zbus_error(e, "Subscribe")),
        }

        let unit_new = mgr
            .receive_unit_new()
            .await
            .map_err(|e| map_zbus_error(e, "UnitNew"))?;
        let unit_removed = mgr
            .receive_unit_removed()
            .await
            .map_err(|e| map_zbus_error(e, "UnitRemoved"))?;
        let job_removed = mgr
            .receive_job_removed()
            .await
            .map_err(|e| map_zbus_error(e, "JobRemoved"))?;

        // PropertiesChanged 从每个 unit 对象各自发出，proxy 绑定单个路径接不到，走 match rule。
        // 不设 sender：systemd 之外没有别人会在这个路径命名空间下发信号。
        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface("org.freedesktop.DBus.Properties")
            .and_then(|b| b.member("PropertiesChanged"))
            .and_then(|b| b.path_namespace("/org/freedesktop/systemd1/unit"))
            .map_err(|e| map_zbus_error(e, "PropertiesChanged"))?
            .build();
        let props_changed = MessageStream::for_match_rule(rule, conn, Some(512))
            .await
            .map_err(|e| map_zbus_error(e, "PropertiesChanged"))?;

        Ok(Self {
            mgr,
            signals: SignalStreams {
                unit_new,
                unit_removed,
                job_removed,
                props_changed,
            },
        })
    }
}

/// 合并已加载 unit 与 unit 文件列表。
fn merge_lists(
    loaded: Vec<UnitListEntry>,
    files: Vec<(String, String)>,
    scope: UnitScope,
) -> Vec<UnitSummary> {
    let file_states: HashMap<String, String> = files
        .into_iter()
        .filter_map(|(path, state)| unit_file_basename(&path).map(|n| (n.to_owned(), state)))
        .collect();

    let mut seen = HashSet::with_capacity(loaded.len());
    let mut out = Vec::with_capacity(loaded.len() + file_states.len());
    for (name, description, load, active, sub, _following, _path, _job_id, _job_type, _job_path) in
        loaded
    {
        seen.insert(name.clone());
        out.push(UnitSummary {
            unit_type: unit_type_of(&name).to_owned(),
            description,
            load_state: parse_load_state(&load),
            active_state: parse_active_state(&active),
            sub_state: sub,
            enable_state: lookup_enable_state(&file_states, &name),
            scope,
            name,
        });
    }
    for (name, state) in &file_states {
        // alias 指向的 unit 已经以本名出现过了。
        if !seen.contains(name) && state != "alias" {
            out.push(summary_for_unloaded_file(name, state, scope));
        }
    }
    out
}

/// [`listen_loop`] 的退出方式，决定 [`run_listener`] 的收尾动作。
enum ListenerExit {
    /// 没有订阅者了：退订、归还标志；若退场期间又有人来了则重开。
    Idle,
    /// 连接断了：清标志走人，没有可收尾的。
    Disconnected,
    /// 冲刷任务没了（panic）：订阅与连接都还好，重建一轮即可。
    FlusherGone,
}

/// 监听任务的**生命周期外壳**：每轮 = 起冲刷任务 → 跑热循环 → 收尾。
///
/// 分层是这段代码的全部要点（背景见
/// `docs/incidents/2026-09-25-dbus-wedge.md`）：
///
/// - [`listen_loop`]（热循环）**只**做入队与投递，绝不 `await` 总线往返；
/// - 总线往返集中在 [`spawn_flusher`] 的独立任务里；
/// - 收尾动作（等冲刷收摊、退订、归还标志）全部发生在热循环返回**之后**
///   ——那时四路信号流已随 `listen_loop` 的参数一起释放，连接上没有任何
///   有界队列，这些 `await` 不可能把 zbus 的读取任务挡住。
///
/// 标志（`ListenerFlags`）从 [`spawn_listener`] 抢到手起就归本任务，
/// 直到某条退出路径显式归还。退场（`Idle`）用两步协议：先退订、后归还
/// （[`Shared::finish_retire`]），归还时发现又有人订阅了就地重开——
/// 那个订阅者看到标志被占没敢开新监听，只能由我们替它接上。
async fn run_listener(
    conn: Connection,
    scope: UnitScope,
    shared: Arc<Shared>,
    mut streams: ListenerStreams,
) {
    loop {
        let ListenerStreams { mgr, signals } = streams;
        let (flush_tx, flusher) = spawn_flusher(conn.clone(), scope, Arc::clone(&shared), mgr);
        let exit = listen_loop(&conn, scope, &shared, signals, &flush_tx).await;
        // ← 四路信号流已释放。先断投递口再等冲刷收摊，免得它卡在 recv 上。
        drop(flush_tx);
        let _ = flusher.await;

        match exit {
            ListenerExit::Disconnected => {
                tracing::warn!(?scope, "systemd 事件监听退出（bus 断开）");
                shared.set_listener_flag(scope, false);
                return;
            }
            ListenerExit::FlusherGone => {
                // 不退订：订阅还挂着，setup 会以 AlreadySubscribed 兼容。
                tracing::warn!(?scope, "冲刷任务异常退出，重建监听");
            }
            ListenerExit::Idle => {
                unsubscribe_with_timeout(&conn, scope).await;
                if !shared.finish_retire(scope) {
                    tracing::debug!(?scope, "systemd 事件监听退场（无订阅者）");
                    return;
                }
                // 退场期间来了新订阅者。标志刚被归还，重新抢；抢不到说明
                // 它（或更晚的谁）已经自己开好了新监听，我们功成身退。
                if !shared.set_listener_flag(scope, true) {
                    return;
                }
                tracing::debug!(?scope, "退场途中来了新订阅者，重开监听");
            }
        }

        streams = match with_timeout("监听重建", ListenerStreams::setup(&conn)).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(?scope, error = %e, "重建监听失败");
                shared.set_listener_flag(scope, false);
                return;
            }
        };
    }
}

/// 事件监听热循环。**循环体内绝不 `await` 总线调用**——这是 2026-09-25
/// 总线死锁的根因修复：等回复期间四路流停止被 poll，zbus 的读取任务会在
/// 满队列的投递上挂住，回复就永远读不出来（环形等待，细节见事故记录）。
/// 冲刷经 `flush_tx` 交给独立任务，本循环只做入队、投递与退场判断。
async fn listen_loop(
    conn: &Connection,
    scope: UnitScope,
    shared: &Shared,
    signals: SignalStreams,
    flush_tx: &tokio::sync::mpsc::Sender<HashSet<String>>,
) -> ListenerExit {
    let SignalStreams {
        mut unit_new,
        mut unit_removed,
        mut job_removed,
        mut props_changed,
    } = signals;
    tracing::debug!(?scope, "systemd 事件监听已启动");

    let mut queue = FlushQueue::default();
    let mut idle = tokio::time::interval(IDLE_CHECK);
    idle.tick().await; // interval 的第一个 tick 立刻就绪，丢掉

    loop {
        // 拷一份给 future 用，避免与下面各分支对 `queue` 的可变借用冲突。
        let flush_deadline = queue.deadline();
        let flush_at = async move {
            match flush_deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            Some(sig) = unit_new.next() => {
                if let Ok(a) = sig.args() { queue.mark(a.id().clone()); }
            }
            Some(sig) = unit_removed.next() => {
                if let Ok(a) = sig.args() { queue.mark(a.id().clone()); }
            }
            Some(sig) = job_removed.next() => {
                if let Ok(a) = sig.args() { queue.mark(a.unit().clone()); }
            }
            Some(msg) = props_changed.next() => {
                if let Ok(m) = msg
                    && let Some(p) = m.header().path()
                    && let Some(name) = unit_name_from_path(p.as_str())
                {
                    queue.mark(name);
                }
            }
            _ = flush_at => {
                match flush_tx.try_send(queue.take()) {
                    Ok(()) => {}
                    // 上一批还在飞：放回去，下一个窗口连同新到的一起刷。
                    Err(TrySendError::Full(names)) => queue.requeue(names),
                    Err(TrySendError::Closed(_)) => return ListenerExit::FlusherGone,
                }
            }
            _ = idle.tick() => {
                if conn.is_closed() {
                    return ListenerExit::Disconnected;
                }
                // 没人在看就退场：不再长期持有对系统总线的订阅，暴露面从
                // 「7×24」缩到「有人正看着服务页的那几分钟」。
                if shared.should_retire() {
                    return ListenerExit::Idle;
                }
            }
            else => return ListenerExit::Disconnected,
        }
    }
}

/// 起冲刷任务：拿一批 unit 名 → 总线往返取摘要 → 广播。
///
/// 通道容量 1 + 调用方 `try_send`：同一时刻最多一批在飞，送不进去的名字
/// 放回 [`FlushQueue`] 下个去抖窗口再刷——天然合并，也不会无限 spawn。
fn spawn_flusher(
    conn: Connection,
    scope: UnitScope,
    shared: Arc<Shared>,
    mgr: ManagerProxy<'static>,
) -> (
    tokio::sync::mpsc::Sender<HashSet<String>>,
    tokio::task::JoinHandle<()>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<HashSet<String>>(1);
    let task = tokio::spawn(async move {
        while let Some(names) = rx.recv().await {
            let units = summaries_for(&conn, &mgr, names, scope).await;
            if !units.is_empty() {
                // 没有订阅者时 send 返回 Err，无所谓。
                let _ = shared.events.send(ServiceEvent { units });
            }
        }
    });
    (tx, task)
}

/// 退订，带超时。
///
/// 失败记 warn 而不是 debug：漏掉的退订曾经意味着本频道永久死亡
/// （AlreadySubscribed 被当失败），现在 setup 已兼容，但它仍是值得在
/// 日志里看得见的异常。超时兜底与其余总线调用同一档（15 秒）。
async fn unsubscribe_with_timeout(conn: &Connection, scope: UnitScope) {
    let done = with_timeout("Unsubscribe", async {
        let mgr = SystemdBus::manager(conn).await?;
        mgr.unsubscribe()
            .await
            .map_err(|e| map_zbus_error(e, "Unsubscribe"))
    })
    .await;
    if let Err(e) = done {
        tracing::warn!(?scope, error = %e, "退订失败（下次 Subscribe 按已订阅兼容）");
    }
}

/// 为一批 unit 名取当前摘要。批量大（daemon-reload 时几百个）就整表拉一次，小批量逐个查。
///
/// **两条路都套 `with_timeout`**：`summary_no_load` 与 `list_units_raw` 曾是
/// 本文件仅有的两个不带超时的总线调用，而它们恰好在 2026-09-25 死锁被无限
/// 等下去的那条路径上。根因已由「冲刷移出监听循环」修掉，这里是纵深——
/// 即便将来某个调用又挂住，挂住的也只是冲刷任务，且 15 秒后自己解开。
///
/// **超时（或整表失败）时丢弃条目而不是伪造结论**：`summary_for_vanished`
/// 序列化成 `load_state = not_found`，前端按频道契约会**删行**
/// （`ws/channels/services_changed.rs`）。一次 15 秒的总线抖动不该在界面上
/// 表现成「这个服务被删了」；漏掉的这次刷新由窗口重获焦点的整表刷新兜底。
async fn summaries_for(
    conn: &Connection,
    mgr: &ManagerProxy<'_>,
    names: HashSet<String>,
    scope: UnitScope,
) -> Vec<UnitSummary> {
    const PER_UNIT_LIMIT: usize = 16;
    if names.len() > PER_UNIT_LIMIT {
        let all = with_timeout("事件刷新 ListUnits", SystemdBus::list_units_raw(conn, scope)).await;
        return match all {
            Ok(all) => {
                let mut by_name: HashMap<String, UnitSummary> =
                    all.into_iter().map(|u| (u.name.clone(), u)).collect();
                names
                    .into_iter()
                    .map(|n| {
                        // 整表是刚拉的权威快照：不在表里 = 真的没了，
                        // 这里的 vanished 是事实而不是猜测。
                        by_name
                            .remove(&n)
                            .unwrap_or_else(|| summary_for_vanished(&n, scope))
                    })
                    .collect()
            }
            Err(e) => {
                tracing::warn!(error = %e, dropped = names.len(), "事件刷新时 ListUnits 失败，这批更新已丢弃");
                Vec::new()
            }
        };
    }
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        match with_timeout(
            "事件刷新 unit 摘要",
            async { Ok::<_, ApiError>(SystemdBus::summary_no_load(conn, mgr, &name, scope).await) },
        )
        .await
        {
            Ok(s) => out.push(s),
            // 超时 ≠ 消失：跳过这一条，见函数文档。
            Err(e) => tracing::warn!(unit = %name, error = %e, "事件刷新时取 unit 摘要超时，已跳过"),
        }
    }
    out
}

#[async_trait]
impl Provider for SystemdBus {
    fn id(&self) -> &'static str {
        "systemd"
    }

    async fn probe(&self) -> Probe {
        let fut = async {
            let dbus = zbus::fdo::DBusProxy::new(&self.system).await?;
            let name = BusName::try_from(SYSTEMD_DEST)?;
            dbus.name_has_owner(name).await.map_err(zbus::Error::from)
        };
        match tokio::time::timeout(super::CALL_TIMEOUT, fut).await {
            Ok(Ok(true)) => Probe::Available,
            Ok(Ok(false)) => Probe::unavailable("org.freedesktop.systemd1 在 bus 上没有 owner"),
            Ok(Err(e)) => Probe::unavailable(format!("查询 bus 失败: {e}")),
            Err(_) => Probe::unavailable("查询 bus 超时"),
        }
    }
}

#[async_trait]
impl ServiceProvider for SystemdBus {
    async fn list_units(&self, query: &UnitListQuery) -> ApiResult<Vec<UnitSummary>> {
        let scope = query.scope.unwrap_or_default();
        with_timeout("ListUnits", async {
            let conn = self.conn(scope).await?;
            let units = Self::list_units_raw(&conn, scope).await?;
            Ok(apply_list_query(units, query))
        })
        .await
    }

    async fn unit_detail(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDetail> {
        with_timeout("unit 详情", async {
            let (conn, path, mut u) = self.load_unit_props(scope, unit).await?;
            let summary = Self::summary_from_props(unit, &mut u, scope);

            let mut t = match type_interface(&summary.unit_type) {
                Some(iface) => Self::get_all(&conn, &path, iface).await?,
                None => Props::default(),
            };

            let cgroup = t.opt_string("ControlGroup").map(|cg| {
                let mut usage = self.shared.cgroup.read(&cg).unwrap_or_default();
                // 直读不到的字段回落 systemd 自己的记账（u64::MAX = 未设置）。
                fill_from_props(&mut usage, &mut t);
                usage.path = Some(cg);
                usage
            });

            Ok(UnitDetail {
                fragment_path: u.opt_string("FragmentPath"),
                drop_in_paths: u.strings("DropInPaths"),
                main_pid: t.u32("MainPID").filter(|p| *p != 0),
                active_enter_ts: u.u64("ActiveEnterTimestamp").and_then(usec_to_ts),
                state_change_ts: u.u64("StateChangeTimestamp").and_then(usec_to_ts),
                n_restarts: t.u32("NRestarts"),
                result: t.opt_string("Result"),
                exit_code: t.i32("ExecMainStatus"),
                documentation: u.strings("Documentation"),
                user: t.opt_string("User"),
                cgroup,
                summary,
            })
        })
        .await
    }

    async fn unit_file(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitFile> {
        with_timeout("unit 文件", async {
            let (_, _, mut u) = self.load_unit_props(scope, unit).await?;
            let fragment = match u.opt_string("FragmentPath") {
                Some(p) => Some(read_unit_fragment(&p).await?),
                None => None,
            };
            let mut drop_ins = Vec::new();
            for p in u.strings("DropInPaths") {
                drop_ins.push(read_unit_fragment(&p).await?);
            }
            Ok(UnitFile {
                unit: unit.to_owned(),
                fragment,
                drop_ins,
            })
        })
        .await
    }

    async fn unit_deps(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDeps> {
        with_timeout("unit 依赖", async {
            let (_, _, mut u) = self.load_unit_props(scope, unit).await?;
            Ok(UnitDeps {
                unit: unit.to_owned(),
                requires: u.strings("Requires"),
                requisite: u.strings("Requisite"),
                wants: u.strings("Wants"),
                binds_to: u.strings("BindsTo"),
                part_of: u.strings("PartOf"),
                required_by: u.strings("RequiredBy"),
                wanted_by: u.strings("WantedBy"),
                bound_by: u.strings("BoundBy"),
                conflicts: u.strings("Conflicts"),
                conflicted_by: u.strings("ConflictedBy"),
                before: u.strings("Before"),
                after: u.strings("After"),
                triggers: u.strings("Triggers"),
                triggered_by: u.strings("TriggeredBy"),
            })
        })
        .await
    }

    async fn unit_action(
        &self,
        scope: UnitScope,
        unit: &str,
        action: UnitAction,
    ) -> ApiResult<UnitActionResp> {
        validate_unit_name(unit)?;
        with_timeout("unit 操作", async {
            let conn = self.conn(scope).await?;
            let mgr = Self::manager(&conn).await?;
            let files = [unit];
            let job = match action {
                UnitAction::Start => Some(mgr.start_unit(unit, "replace").await),
                UnitAction::Stop => Some(mgr.stop_unit(unit, "replace").await),
                UnitAction::Restart => Some(mgr.restart_unit(unit, "replace").await),
                UnitAction::Reload => Some(mgr.reload_unit(unit, "replace").await),
                UnitAction::Enable => {
                    mgr.enable_unit_files(&files, false, false)
                        .await
                        .map_err(|e| map_zbus_error(e, unit))?;
                    None
                }
                UnitAction::Disable => {
                    mgr.disable_unit_files(&files, false)
                        .await
                        .map_err(|e| map_zbus_error(e, unit))?;
                    None
                }
                UnitAction::Mask => {
                    mgr.mask_unit_files(&files, false, false)
                        .await
                        .map_err(|e| map_zbus_error(e, unit))?;
                    None
                }
                UnitAction::Unmask => {
                    mgr.unmask_unit_files(&files, false)
                        .await
                        .map_err(|e| map_zbus_error(e, unit))?;
                    None
                }
            };
            let job = match job {
                Some(r) => Some(r.map_err(|e| map_zbus_error(e, unit))?.to_string()),
                None => None,
            };
            if action.is_persistent() {
                // 改了符号链接后要 Reload 才会反映到 UnitFileState；这一步同样受 polkit 裁决。
                mgr.reload().await.map_err(|e| map_zbus_error(e, unit))?;
            }
            Ok(UnitActionResp {
                unit: unit.to_owned(),
                action,
                job,
                active_state: Self::active_state_now(&conn, &mgr, unit).await,
            })
        })
        .await
    }

    async fn list_timers(&self, scope: UnitScope) -> ApiResult<Vec<TimerEntry>> {
        with_timeout("ListTimers", async {
            let conn = self.conn(scope).await?;
            let mgr = Self::manager(&conn).await?;
            let loaded = mgr
                .list_units()
                .await
                .map_err(|e| map_zbus_error(e, "list"))?;
            let mut out = Vec::new();
            for (name, _desc, _load, active, _sub, _following, path, ..) in loaded {
                if !name.ends_with(".timer") {
                    continue;
                }
                let mut t =
                    match Self::get_all(&conn, &path, "org.freedesktop.systemd1.Timer").await {
                        Ok(p) => p,
                        Err(e) => {
                            // 单个 timer 在查询间隙被 GC 不该让整个列表失败
                            tracing::debug!(unit = %name, error = %e, "读 Timer 属性失败，跳过");
                            continue;
                        }
                    };
                let mut schedule = Vec::new();
                if let Some(v) = t.value("TimersCalendar") {
                    schedule.extend(timer_spec_lines(&v));
                }
                if let Some(v) = t.value("TimersMonotonic") {
                    schedule.extend(timer_spec_lines(&v));
                }
                let next_ts = t
                    .u64("NextElapseUSecRealtime")
                    .and_then(opt_u64)
                    .and_then(usec_to_ts)
                    .or_else(|| {
                        t.u64("NextElapseUSecMonotonic")
                            .and_then(opt_u64)
                            .and_then(monotonic_usec_to_ts)
                    });
                out.push(TimerEntry {
                    source: TimerSource::SystemdTimer,
                    schedule,
                    next_ts,
                    last_ts: t.u64("LastTriggerUSec").and_then(usec_to_ts),
                    target: t.opt_string("Unit"),
                    active: Some(active == "active"),
                    scope,
                    name,
                });
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        })
        .await
    }

    async fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        // 先拿 receiver 再注册监听：注册期间到达的事件也不会丢。
        let rx = self.shared.events.subscribe();
        self.ensure_listener(UnitScope::System, self.system.clone())
            .await;
        if let Some(u) = self.user.get() {
            self.ensure_listener(UnitScope::User, u.clone()).await;
        }
        rx
    }
}

/// Timer 的 `TimersCalendar`（`a(sst)`）/ `TimersMonotonic`（`a(stt)`）→ `Base=spec` 行。
///
/// 第二个字段是日历表达式（字符串）或相对偏移（微秒）；第三个字段（下次触发）
/// 由 `NextElapseUSec*` 统一给出，不进调度行。手工拆 `Value` 而不是 `TryFrom` 成元组，
/// 属性形状对不上时得到的是空数组而不是错误——调度规则「拿不到」不该毁掉整条记录。
fn timer_spec_lines(v: &zbus::zvariant::Value<'_>) -> Vec<String> {
    use zbus::zvariant::Value;
    let Value::Array(arr) = v else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let Value::Structure(s) = item else {
                return None;
            };
            let f = s.fields();
            let Value::Str(base) = f.first()? else {
                return None;
            };
            match f.get(1)? {
                Value::Str(spec) => Some(format!("{base}={spec}")),
                Value::U64(usec) => Some(format!("{base}={}s", usec / 1_000_000)),
                _ => None,
            }
        })
        .collect()
}

/// `NextElapseUSecMonotonic`（CLOCK_MONOTONIC 微秒）→ unix 秒。
///
/// monotonic 时钟不含 epoch 信息，换算靠「现在的 monotonic 读数」对齐：
/// `unix_now + (next_mono - now_mono)`。已过期（差为负）视为「马上」，报当前时刻。
fn monotonic_usec_to_ts(next_mono_usec: u64) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 只写入栈上的 timespec。
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    let now_mono_usec = ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000;
    let delta_secs = (next_mono_usec as i64 - now_mono_usec) / 1_000_000;
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(unix_now + delta_secs.max(0))
}

/// 直读缺失的字段用 systemd 属性补齐。
fn fill_from_props(usage: &mut CgroupUsage, t: &mut Props) {
    if usage.cpu_usage_nsec.is_none() {
        usage.cpu_usage_nsec = t.u64("CPUUsageNSec").and_then(opt_u64);
    }
    if usage.memory_current_bytes.is_none() {
        usage.memory_current_bytes = t.u64("MemoryCurrent").and_then(opt_u64);
    }
    if usage.memory_peak_bytes.is_none() {
        usage.memory_peak_bytes = t.u64("MemoryPeak").and_then(opt_u64);
    }
    if usage.memory_limit_bytes.is_none() {
        usage.memory_limit_bytes = t.u64("MemoryMax").and_then(opt_u64);
    }
    if usage.tasks_current.is_none() {
        usage.tasks_current = t.u64("TasksCurrent").and_then(opt_u64);
    }
    if usage.tasks_limit.is_none() {
        usage.tasks_limit = t.u64("TasksMax").and_then(opt_u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use strixmaid_types::service::UnitLoadState;

    #[test]
    fn decodes_unit_object_paths() {
        assert_eq!(
            unit_name_from_path("/org/freedesktop/systemd1/unit/ssh_2eservice").as_deref(),
            Some("ssh.service")
        );
        assert_eq!(
            unit_name_from_path("/org/freedesktop/systemd1/unit/getty_40tty1_2eservice").as_deref(),
            Some("getty@tty1.service")
        );
        assert_eq!(
            unit_name_from_path(
                "/org/freedesktop/systemd1/unit/dev_2ddisk_2dby_5cx2dlabel_2droot_2edevice"
            )
            .as_deref(),
            Some("dev-disk-by\\x2dlabel-root.device")
        );
        assert_eq!(unit_name_from_path("/org/freedesktop/systemd1/job/1"), None);
    }

    #[test]
    fn maps_error_names() {
        let e = map_error_name(
            "org.freedesktop.systemd1.NoSuchUnit",
            "x".into(),
            "a.service",
        );
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = map_error_name(
            "org.freedesktop.DBus.Error.AccessDenied",
            "x".into(),
            "a.service",
        );
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);
        assert!(e.message.contains("需要管理访问"));
        let e = map_error_name(
            "org.freedesktop.DBus.Error.InteractiveAuthorizationRequired",
            String::new(),
            "a",
        );
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        let e = map_error_name("org.freedesktop.systemd1.UnitMasked", String::new(), "a");
        assert_eq!(e.code, ErrorCode::Conflict);
        let e = map_error_name("org.example.Whatever", String::new(), "a");
        assert_eq!(e.code, ErrorCode::Internal);
    }

    #[test]
    fn merges_loaded_and_files() {
        let path = OwnedObjectPath::try_from("/org/freedesktop/systemd1/unit/a_2eservice").unwrap();
        let loaded = vec![(
            "a.service".to_owned(),
            "A".to_owned(),
            "loaded".to_owned(),
            "active".to_owned(),
            "running".to_owned(),
            String::new(),
            path.clone(),
            0,
            String::new(),
            path,
        )];
        let files = vec![
            (
                "/usr/lib/systemd/system/a.service".to_owned(),
                "enabled".to_owned(),
            ),
            (
                "/usr/lib/systemd/system/b.service".to_owned(),
                "disabled".to_owned(),
            ),
            (
                "/usr/lib/systemd/system/c@.service".to_owned(),
                "static".to_owned(),
            ),
            (
                "/usr/lib/systemd/system/alias.service".to_owned(),
                "alias".to_owned(),
            ),
        ];
        let mut out = merge_lists(loaded, files, UnitScope::System);
        out.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<_> = out.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["a.service", "b.service"], "模板与 alias 被跳过");
        assert_eq!(
            out[0].enable_state,
            Some(strixmaid_types::service::UnitEnableState::Enabled)
        );
        assert_eq!(out[1].active_state, UnitActiveState::Inactive);
        assert_eq!(out[1].load_state, UnitLoadState::Loaded);
    }

    // ---- 以下需要真实 systemd；连不上 system bus 时静默跳过 ----

    async fn bus_or_skip() -> Option<SystemdBus> {
        let bus = SystemdBus::connect().await.ok()?;
        match bus.probe().await {
            Probe::Available => Some(bus),
            other => {
                eprintln!("跳过：systemd bus 不可用 {other:?}");
                None
            }
        }
    }

    #[tokio::test]
    async fn live_list_detail_file_deps() {
        let Some(bus) = bus_or_skip().await else {
            return;
        };

        let t0 = std::time::Instant::now();
        let all = bus.list_units(&UnitListQuery::default()).await.unwrap();
        eprintln!("[bus] {} units, {:?}", all.len(), t0.elapsed());
        assert!(!all.is_empty());
        let services = bus
            .list_units(&UnitListQuery {
                unit_type: Some("service".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(services.iter().all(|u| u.unit_type == "service"));

        // 挑一个正在运行的 service 看详情（本机通常有 ssh.service / dbus.service）。
        let running = services
            .iter()
            .find(|u| u.active_state == UnitActiveState::Active && u.sub_state == "running")
            .expect("至少有一个 running service");
        let d = bus
            .unit_detail(UnitScope::System, &running.name)
            .await
            .unwrap();
        assert_eq!(d.summary.name, running.name);
        assert!(d.main_pid.is_some(), "running service 应有 MainPID");
        let cg = d.cgroup.as_ref().expect("running service 应有 cgroup");
        assert!(cg.path.as_deref().unwrap_or("").ends_with(&running.name));
        eprintln!("[bus] {} cgroup: {cg:?}", running.name);

        let f = bus
            .unit_file(UnitScope::System, &running.name)
            .await
            .unwrap();
        assert!(
            f.fragment
                .as_ref()
                .is_some_and(|fr| fr.content.contains("[Service]"))
        );

        let deps = bus
            .unit_deps(UnitScope::System, &running.name)
            .await
            .unwrap();
        assert!(!deps.after.is_empty() || !deps.requires.is_empty());
    }

    #[tokio::test]
    async fn live_errors_for_missing_unit() {
        let Some(bus) = bus_or_skip().await else {
            return;
        };
        let e = bus
            .unit_detail(UnitScope::System, "strixmaid-does-not-exist.service")
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = bus
            .unit_action(
                UnitScope::System,
                "strixmaid-does-not-exist.service",
                UnitAction::Start,
            )
            .await
            .unwrap_err();
        // 非 root：polkit 先拒绝；root：systemd 报 NoSuchUnit。两者都是正确的错误路径。
        assert!(
            matches!(e.code, ErrorCode::NotFound | ErrorCode::PermissionDenied),
            "{e:?}"
        );
        let e = bus
            .unit_detail(UnitScope::System, "bad name")
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);
    }

    /// 在当前用户的 user manager 里起一个 transient unit，验证 user 作用域与事件流。
    #[tokio::test]
    async fn live_user_scope_transient_unit_events() {
        let Some(bus) = bus_or_skip().await else {
            return;
        };
        // 先确认 user bus 可用，不可用（无 loginctl 会话）就跳过。
        let q = UnitListQuery {
            scope: Some(UnitScope::User),
            ..Default::default()
        };
        if let Err(e) = bus.list_units(&q).await {
            eprintln!("跳过：user bus 不可用 {e}");
            return;
        }
        let mut rx = bus.subscribe().await;

        let unit = format!("strixmaid-test-{}.service", std::process::id());
        let status = tokio::process::Command::new("systemd-run")
            .args(["--user", "--collect", "--unit", &unit, "/bin/sleep", "1"])
            .status()
            .await;
        let Ok(status) = status else {
            eprintln!("跳过：没有 systemd-run");
            return;
        };
        assert!(status.success());

        // 起来后应能查到详情。
        let d = bus.unit_detail(UnitScope::User, &unit).await.unwrap();
        assert_eq!(d.summary.scope, UnitScope::User);

        // 事件流里应出现该 unit（activating/active），随后 sleep 结束 → inactive / 消失。
        let mut seen_states = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let ev = tokio::time::timeout_at(deadline, rx.recv()).await;
            let Ok(Ok(ev)) = ev else { break };
            for u in ev.units.into_iter().filter(|u| u.name == unit) {
                assert_eq!(u.scope, UnitScope::User);
                seen_states.push((u.active_state, u.load_state));
            }
            if seen_states
                .iter()
                .any(|(a, l)| *a == UnitActiveState::Inactive || *l == UnitLoadState::NotFound)
            {
                break;
            }
        }
        eprintln!("[bus] events for {unit}: {seen_states:?}");
        assert!(!seen_states.is_empty(), "应收到 services.changed 事件");
        assert!(
            seen_states
                .iter()
                .any(|(a, _)| matches!(a, UnitActiveState::Active | UnitActiveState::Activating)),
            "subscribe() 返回后立即启动的 unit，其启动态不应漏掉: {seen_states:?}"
        );
        assert!(
            seen_states
                .iter()
                .any(|(a, l)| *a == UnitActiveState::Inactive || *l == UnitLoadState::NotFound)
        );

        // stop 一个已经结束的 transient unit：要么 NotFound（已 GC），要么成功。
        match bus
            .unit_action(UnitScope::User, &unit, UnitAction::Stop)
            .await
        {
            Ok(r) => assert_eq!(r.action, UnitAction::Stop),
            Err(e) => assert_eq!(e.code, ErrorCode::NotFound, "{e:?}"),
        }
    }
}
