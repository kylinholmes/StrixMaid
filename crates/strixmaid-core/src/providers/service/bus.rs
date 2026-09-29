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

mod error;
mod listener;
mod properties;
mod proxy;
#[cfg(test)]
mod tests;
mod units;

// Re-export the generated proxy API as well as the existing public helpers.
pub use error::map_zbus_error;
pub use properties::unit_name_from_path;
pub use proxy::*;

use super::cgroup::CgroupReader;
use super::{EVENT_CAPACITY, ServiceEvent};
use crate::providers::{Probe, Provider};
use async_trait::async_trait;
use listener::spawn_listener;
use std::sync::{Arc, Mutex};
use strixmaid_types::service::UnitScope;
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::sync::{OnceCell, broadcast};
use zbus::{Connection, names::BusName, proxy::CacheProperties};

/// systemd 在 bus 上的名字。
const SYSTEMD_DEST: &str = "org.freedesktop.systemd1";
/// unit 对象路径前缀。
const UNIT_PATH_PREFIX: &str = "/org/freedesktop/systemd1/unit/";
/// 所有 unit 共有的接口。
const IFACE_UNIT: &str = "org.freedesktop.systemd1.Unit";

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

    /// 确保某作用域的事件监听任务已启动（同一时刻只有一个）。
    ///
    /// `Subscribe()` 与 match rule 注册在**本调用内**完成后才返回，这样调用方
    /// `subscribe()` 一返回就立刻触发的操作也不会漏掉信号。
    async fn ensure_listener(&self, scope: UnitScope, conn: Connection) {
        spawn_listener(conn, scope, Arc::clone(&self.shared)).await;
    }
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
