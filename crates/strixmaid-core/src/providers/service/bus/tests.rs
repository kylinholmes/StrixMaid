use super::super::{ServiceProvider, UnitAction, UnitActiveState, UnitListQuery};
use super::error::map_error_name;
use super::properties::merge_lists;
use super::*;
use std::time::Duration;
use strixmaid_types::service::UnitLoadState;
use zbus::zvariant::OwnedObjectPath;

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
    let bus = match SystemdBus::connect().await {
        Ok(bus) => bus,
        Err(e) => {
            assert!(!require_live_bus(), "system bus 必须可用: {e}");
            eprintln!("跳过：system bus 不可用 {e}");
            return None;
        }
    };
    match bus.probe().await {
        Probe::Available => Some(bus),
        other => {
            assert!(!require_live_bus(), "systemd 必须可用: {other:?}");
            eprintln!("跳过：systemd bus 不可用 {other:?}");
            None
        }
    }
}

fn require_live_bus() -> bool {
    std::env::var_os("STRIXMAID_REQUIRE_LIVE_DBUS").is_some()
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
        assert!(!require_live_bus(), "user bus 必须可用: {e}");
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
        assert!(!require_live_bus(), "必须安装 systemd-run");
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
