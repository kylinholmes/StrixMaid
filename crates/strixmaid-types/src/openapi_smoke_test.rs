//! 冒烟测试：确认**每一个**对外类型都能进 OpenAPI spec。
//!
//! `docs/design.md` 把 OpenAPI 导出列为 P0 硬要求。`ToSchema` / `IntoParams` 的派生错误
//! 大多在**组装 spec 时**才暴露（重名、`flatten` 生成的 `allOf`、`untagged` 生成的 `oneOf`），
//! 单纯 `cargo check` 是看不出来的，所以这里把全部类型挂进一个 `OpenApi` 里跑一遍。
//!
//! 新增对外类型时**必须**同步登记到下面的 `schemas(...)` 列表。

use utoipa::OpenApi;

use crate::{
    agent, audit, auth, capability, error, file, log, metrics, process, service, system, terminal,
    ws,
};

#[derive(OpenApi)]
#[openapi(components(schemas(
    error::ApiError,
    error::ErrorCode,
    auth::PromptStyle,
    auth::Prompt,
    auth::AuthStartReq,
    auth::AuthStartResp,
    auth::PromptResponse,
    auth::AuthRespondReq,
    auth::AuthUser,
    auth::AuthOutcome,
    auth::SessionInfo,
    capability::Capabilities,
    capability::SystemCapabilities,
    capability::UserCapabilities,
    system::SystemInfo,
    system::OsInfo,
    system::HardwareInfo,
    system::CpuInfo,
    system::MemoryInfo,
    system::DiskInfo,
    system::FilesystemInfo,
    system::CpuPackage,
    system::GpuSource,
    system::GpuInfo,
    system::NetInfo,
    system::HealthSeverity,
    system::HealthReport,
    system::HealthItem,
    system::TimeInfo,
    system::SetHostnameReq,
    system::SetTimezoneReq,
    system::PowerAction,
    system::PowerReq,
    service::UnitScope,
    service::UnitLoadState,
    service::UnitActiveState,
    service::UnitEnableState,
    service::UnitSummary,
    service::UnitDetail,
    service::CgroupUsage,
    service::UnitFile,
    service::UnitFileFragment,
    service::UnitAction,
    service::UnitActionReq,
    service::UnitActionResp,
    service::UnitListQuery,
    log::LogPriority,
    log::LogQuery,
    log::LogEntry,
    log::LogEntryDetail,
    log::LogPage,
    log::BootInfo,
    process::ProcessState,
    process::ProcessSummary,
    process::ProcessDetail,
    process::FdInfo,
    process::SignalName,
    process::SignalReq,
    process::ReniceReq,
    process::ProcessSortKey,
    process::SortOrder,
    process::ProcessListQuery,
    metrics::MetricLayer,
    metrics::RetentionPreset,
    metrics::SeriesMeta,
    metrics::MetricPoint,
    metrics::MetricSeries,
    metrics::MetricQuery,
    metrics::MetricQueryResp,
    metrics::MetricSnapshot,
    metrics::MetricValue,
    metrics::SeriesListQuery,
    metrics::SnapshotQuery,
    terminal::CreateTerminalReq,
    terminal::CreateTerminalResp,
    terminal::TerminalInfo,
    terminal::ResizeReq,
    ws::WsMsgType,
    ws::WsEnvelope,
    ws::WsChannel,
    audit::AuditResult,
    audit::AuditEntry,
    audit::AuditQuery,
    audit::AuditPage,
    agent::NodeInfo,
    agent::CreateNodeReq,
    agent::CreateNodeResp,
    file::FileKind,
    file::FilePathQuery,
    file::DirEntryInfo,
    file::DirListing,
    file::FileContent,
)))]
struct AllSchemas;

/// spec 能组装出来，且每个登记的类型都真的生成了 schema。
#[test]
fn every_type_enters_the_spec() {
    let doc = AllSchemas::openapi();
    let components = doc.components.as_ref().expect("components 应存在");
    // 登记了 92 个类型；数量对不上说明有同名类型互相覆盖，会静默丢失 schema。
    assert_eq!(
        components.schemas.len(),
        92,
        "schema 数量与登记数量不一致，说明有重名覆盖：{:?}",
        components.schemas.keys().collect::<Vec<_>>()
    );
    // 能序列化成 JSON 才算真的能导出。
    let json = doc.to_json().expect("spec 应可序列化为 JSON");
    assert!(json.contains("\"ApiError\""));
}

/// 查询参数类型必须实现 `IntoParams`，否则无法出现在 spec 的 `parameters` 里。
#[test]
fn query_types_are_into_params() {
    fn assert_into_params<T: utoipa::IntoParams>() {
        assert!(!T::into_params(|| None).is_empty());
    }
    assert_into_params::<service::UnitListQuery>();
    assert_into_params::<log::LogQuery>();
    assert_into_params::<process::ProcessListQuery>();
    assert_into_params::<metrics::MetricQuery>();
    assert_into_params::<metrics::SeriesListQuery>();
    assert_into_params::<metrics::SnapshotQuery>();
    assert_into_params::<audit::AuditQuery>();
    assert_into_params::<file::FilePathQuery>();
}

/// 明文凭据绝不能被 `Debug` 打出来（`docs/design.md` §5.3 第 2 条）。
///
/// 注意 [`zeroize::Zeroizing`] 自己的 `Debug` 会原样打印内层明文，
/// 所以外层 `PromptResponse` 的 `Debug` 必须手写——这个测试守的就是那个手写实现。
#[test]
fn credentials_are_redacted_in_debug() {
    let req: auth::AuthRespondReq =
        serde_json::from_str(r#"{"session":"s1","responses":[{"id":0,"value":"hunter2"}]}"#)
            .expect("应能反序列化");
    let dbg = format!("{req:?}");
    assert!(!dbg.contains("hunter2"), "Debug 泄漏了明文凭据：{dbg}");
    assert!(dbg.contains("<redacted>"));
    // 内容本身仍然可用，只是不出现在 Debug 里。
    assert_eq!(req.responses[0].value.as_str(), "hunter2");
}

/// `value` 必须是 `Zeroizing<String>`，drop 时自动擦除——由类型强制，不靠 code review。
///
/// 「drop 后内存已清零」无法在安全 Rust 里断言（读已释放的内存是 UB），
/// 因此这里退而求其次做两件能确定的事：
/// 1. 字段类型确实是 `Zeroizing<String>`（下面的赋值编译不过就说明类型被改回了裸 `String`）；
/// 2. `Zeroizing` 的擦除语义本身可观测——手动调 `zeroize()` 后缓冲区被清空。
#[test]
fn credentials_are_wrapped_in_zeroizing() {
    use zeroize::{Zeroize, Zeroizing};

    let req: auth::AuthRespondReq =
        serde_json::from_str(r#"{"session":"s1","responses":[{"id":0,"value":"hunter2"}]}"#)
            .unwrap();
    // 类型断言：只有 `value: Zeroizing<String>` 时这行才编译得过。
    let _typed: &Zeroizing<String> = &req.responses[0].value;

    let mut plaintext = String::from("hunter2");
    plaintext.zeroize();
    assert!(plaintext.is_empty(), "zeroize 后缓冲区应被清空");
}

/// 未认证时 `capabilities.user` 为 `null`，接口仍返回 200——
/// helper 不可用时登录页得靠它显示「无法登录」。
#[test]
fn capabilities_user_is_nullable() {
    let caps: capability::Capabilities = serde_json::from_str(
        r#"{"system":{"systemd":true,"journal":true,"helper":false,"polkit":true,"user_units":false,"podman":false}}"#,
    )
    .expect("缺 user 字段也应能反序列化");
    assert!(caps.user.is_none());
    assert!(!caps.system.helper);
}

/// 日志的亚秒精度：`ts` 秒 + `us` 秒内微秒偏移。
#[test]
fn log_entry_carries_sub_second_precision() {
    let e: log::LogEntry = serde_json::from_str(
        r#"{"cursor":"c1","ts":1756252800,"us":481237,"priority":"err","message":"boom"}"#,
    )
    .unwrap();
    assert_eq!(e.ts, 1_756_252_800);
    assert_eq!(e.us, 481_237);
}

/// `MetricLayer` / `RetentionPreset` 是 core 直接复用的契约，往返转换必须稳。
#[test]
fn metric_layer_and_preset_round_trip() {
    use std::str::FromStr;

    for layer in metrics::MetricLayer::PERSISTED {
        assert_eq!(
            metrics::MetricLayer::from_str(layer.as_str()).unwrap(),
            layer
        );
        assert_eq!(layer.table_name(), Some(layer.as_str()));
        assert!(layer.bucket_secs().is_some());
        // 线格式与 as_str 一致，core 拿 as_str 当表名才成立。
        assert_eq!(serde_json::to_value(layer).unwrap(), layer.as_str());
    }
    assert_eq!(metrics::MetricLayer::Live.table_name(), None);
    assert!(metrics::MetricLayer::from_str("m_2m").is_err());

    assert_eq!(
        metrics::RetentionPreset::default(),
        metrics::RetentionPreset::Normal
    );
    assert_eq!(
        metrics::RetentionPreset::from_str("Normal").unwrap(),
        metrics::RetentionPreset::Normal
    );
    assert!(metrics::RetentionPreset::from_str("more").is_err());
}

/// [`auth::AuthOutcome`] 必须带 `status` 判别字段，且失败不是它的分支。
#[test]
fn auth_outcome_is_internally_tagged() {
    let more = auth::AuthOutcome::More {
        session: "s1".into(),
        prompts: vec![auth::Prompt {
            id: 0,
            style: auth::PromptStyle::Prompt,
            text: "Password: ".into(),
        }],
    };
    let v = serde_json::to_value(&more).unwrap();
    assert_eq!(v["status"], "more");
    assert_eq!(v["session"], "s1");
    assert_eq!(v["prompts"][0]["style"], "prompt");

    let done: auth::AuthOutcome = serde_json::from_str(
        r#"{"status":"complete","token":"t","user":{"uid":1000,"gid":1000,"username":"alice","groups":[]}}"#,
    )
    .unwrap();
    assert!(matches!(done, auth::AuthOutcome::Complete { .. }));

    // 失败走 HTTP 401 + ApiError，不在这个 union 里。
    assert!(serde_json::from_str::<auth::AuthOutcome>(r#"{"reason":"认证失败"}"#).is_err());
}

/// spec 里 `AuthOutcome` 的每个 `oneOf` 分支都必须把 `status` 声明为**必填的单值枚举**，
/// 代码生成器据此生成可判别的 tagged union。
///
/// 这里不断言 OpenAPI 的 `discriminator` 关键字：utoipa 5.5 只在
/// `#[serde(untagged)]` + 单字段 `$ref` 变体上才生成它，而 untagged 正是我们要避开的
/// （serde 侧仍按字段形状猜分支）。单值枚举属性是 JSON Schema 原生的判别方式，
/// 各主流生成器都认，且它比 `discriminator` 更严格——校验器会真的拒绝错配的 `status`。
#[test]
fn auth_outcome_branches_are_discriminable() {
    let json = AllSchemas::openapi().to_json().unwrap();
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    let branches = doc["components"]["schemas"]["AuthOutcome"]["oneOf"]
        .as_array()
        .expect("应为 oneOf");
    assert_eq!(branches.len(), 2, "只有 complete / more 两支，失败走 401");

    let tags: Vec<&str> = branches
        .iter()
        .map(|b| {
            assert!(
                b["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r == "status"),
                "每支都必须把 status 列为必填：{b}"
            );
            let values = b["properties"]["status"]["enum"].as_array().unwrap();
            assert_eq!(values.len(), 1, "status 必须是单值枚举，才能唯一判别");
            values[0].as_str().unwrap()
        })
        .collect();
    assert_eq!(tags, ["complete", "more"]);
}

/// `flatten` 的字段必须平铺，而不是嵌套成子对象。
#[test]
fn detail_types_flatten_their_summary() {
    let detail = service::UnitDetail {
        summary: service::UnitSummary {
            name: "nginx.service".into(),
            unit_type: "service".into(),
            description: "web".into(),
            load_state: service::UnitLoadState::Loaded,
            active_state: service::UnitActiveState::Active,
            sub_state: "running".into(),
            enable_state: Some(service::UnitEnableState::Enabled),
            scope: service::UnitScope::System,
        },
        fragment_path: None,
        drop_in_paths: vec![],
        main_pid: Some(1234),
        active_enter_ts: None,
        state_change_ts: None,
        n_restarts: None,
        result: None,
        exit_code: None,
        documentation: vec![],
        user: None,
        cgroup: None,
    };
    let v = serde_json::to_value(&detail).unwrap();
    assert_eq!(v["name"], "nginx.service");
    assert_eq!(v["active_state"], "active");
    assert!(v.get("summary").is_none(), "summary 不应嵌套");
}

/// 信号既接受 design.md 里的大写写法，也接受全局约定的 snake_case。
#[test]
fn signal_accepts_both_spellings() {
    let upper: process::SignalReq = serde_json::from_str(r#"{"signal":"TERM"}"#).unwrap();
    let lower: process::SignalReq = serde_json::from_str(r#"{"signal":"term"}"#).unwrap();
    assert_eq!(upper.signal, lower.signal);
    assert_eq!(
        serde_json::to_value(upper).unwrap()["signal"],
        "term",
        "输出侧统一 snake_case"
    );
}

/// 未来版本 systemd 报出新状态时，反序列化不能整体失败。
#[test]
fn unknown_enum_values_degrade_gracefully() {
    let s: service::UnitActiveState = serde_json::from_str(r#""some-future-state""#).unwrap();
    assert_eq!(s, service::UnitActiveState::Unknown);
}

#[test]
#[ignore]
fn dump_spec() {
    println!("{}", AllSchemas::openapi().to_pretty_json().unwrap());
}
