//! 单元查询/操作与 ServiceProvider 实现；所有请求入口保留超时。
use super::super::{
    ServiceEvent, ServiceProvider, UnitDeps, apply_list_query, opt_u64, parse_active_state,
    parse_enable_state, parse_load_state, read_unit_fragment, summary_for_unloaded_file,
    summary_for_vanished, unit_type_of, usec_to_ts, validate_unit_name, with_timeout,
};
use super::properties::*;
use super::{IFACE_UNIT, ManagerProxy, SYSTEMD_DEST, SystemdBus, map_zbus_error};
use async_trait::async_trait;
use strixmaid_types::service::{
    TimerEntry, TimerSource, UnitAction, UnitActionResp, UnitActiveState, UnitDetail, UnitFile,
    UnitListQuery, UnitScope, UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::broadcast;
use zbus::{Connection, names::InterfaceName, proxy::CacheProperties, zvariant::OwnedObjectPath};

impl SystemdBus {
    /// 对某个对象路径的某个接口做 `GetAll`。
    pub(super) async fn get_all(
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
    pub(super) async fn load_unit_props(
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
    pub(super) fn summary_from_props(
        fallback_name: &str,
        p: &mut Props,
        scope: UnitScope,
    ) -> UnitSummary {
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
    pub(super) async fn summary_no_load(
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
    pub(super) async fn list_units_raw(
        conn: &Connection,
        scope: UnitScope,
    ) -> ApiResult<Vec<UnitSummary>> {
        let mgr = Self::manager(conn).await?;
        let (loaded, files) = tokio::try_join!(mgr.list_units(), mgr.list_unit_files())
            .map_err(|e| map_zbus_error(e, "list"))?;
        Ok(merge_lists(loaded, files, scope))
    }

    /// 操作后立刻读一次活动状态，失败就不给。
    pub(super) async fn active_state_now(
        conn: &Connection,
        mgr: &ManagerProxy<'_>,
        unit: &str,
    ) -> Option<UnitActiveState> {
        let path = mgr.get_unit(unit).await.ok()?;
        let mut props = Self::get_all(conn, &path, IFACE_UNIT).await.ok()?;
        Some(parse_active_state(&props.string("ActiveState")))
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
