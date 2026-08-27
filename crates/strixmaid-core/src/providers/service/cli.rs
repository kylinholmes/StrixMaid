//! `SystemctlCli`：连不上 bus 时的降级路径，全部走 `systemctl` 子进程。
//!
//! 依赖 systemd ≥ 246（`list-units --output=json`）。所有参数走 `Command::arg`，
//! unit 名前加 `--`，不存在 shell 注入面。没有事件流：[`ServiceProvider::subscribe`]
//! 返回的 receiver 永远安静。
//!
//! 时间戳：给子进程设 `TZ=UTC`，`systemctl show` 输出的 `Thu 2026-08-27 06:54:25 UTC`
//! 用 [`crate::providers::log::parse::parse_utc_timestamp`] 解析，不依赖
//! `--timestamp=unix`（v249 才有）。

use std::collections::HashMap;
use std::process::Stdio;

use async_trait::async_trait;
use serde::Deserialize;
use strixmaid_types::service::{
    CgroupUsage, UnitAction, UnitActionResp, UnitDetail, UnitFile, UnitListQuery, UnitScope,
    UnitSummary,
};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::process::Command;
use tokio::sync::broadcast;

use super::cgroup::CgroupReader;
use super::{
    EVENT_CAPACITY, ServiceEvent, ServiceProvider, UnitDeps, apply_list_query, lookup_enable_state,
    parse_active_state, parse_enable_state, parse_load_state, read_unit_fragment,
    summary_for_unloaded_file, unit_file_basename, unit_type_of, validate_unit_name, with_timeout,
};
use crate::providers::log::parse::parse_utc_timestamp;
use crate::providers::{Probe, Provider};

/// `systemctl list-units --output=json` 的一行。
#[derive(Debug, Deserialize)]
struct ListUnitRow {
    unit: String,
    load: String,
    active: String,
    sub: String,
    #[serde(default)]
    description: String,
}

/// `systemctl list-unit-files --output=json` 的一行。
#[derive(Debug, Deserialize)]
struct UnitFileRow {
    unit_file: String,
    state: String,
}

/// 详情要取的属性。不存在于该类型的属性 systemctl 会直接省略。
const DETAIL_PROPS: &str = "Id,Description,LoadState,ActiveState,SubState,UnitFileState,\
FragmentPath,DropInPaths,MainPID,ActiveEnterTimestamp,StateChangeTimestamp,NRestarts,Result,\
ExecMainStatus,Documentation,User,ControlGroup,MemoryCurrent,MemoryPeak,MemoryMax,CPUUsageNSec,\
TasksCurrent,TasksMax";

/// 依赖要取的属性。
const DEPS_PROPS: &str = "Id,LoadState,Requires,Requisite,Wants,BindsTo,PartOf,RequiredBy,\
WantedBy,BoundBy,Conflicts,ConflictedBy,Before,After,Triggers,TriggeredBy";

/// systemctl 降级实现。
#[derive(Debug)]
pub struct SystemctlCli {
    user_uid: u32,
    cgroup: CgroupReader,
    /// 只为了让 `subscribe()` 有个永远不发消息、也永远不关闭的通道。
    idle: broadcast::Sender<ServiceEvent>,
}

impl Default for SystemctlCli {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemctlCli {
    /// user 作用域缺省指向本进程 uid。
    pub fn new() -> Self {
        let (idle, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            user_uid: nix::unistd::Uid::current().as_raw(),
            cgroup: CgroupReader::new(),
            idle,
        }
    }

    /// 指定 `scope=user` 对应的 uid，语义同 [`super::bus::SystemdBus::with_user_uid`]。
    #[must_use]
    pub fn with_user_uid(mut self, uid: u32) -> Self {
        self.user_uid = uid;
        self
    }

    /// 基础命令：无分页、C locale、UTC、不读 stdin。user 作用域补 `--user` 与 bus 地址。
    fn command(&self, scope: UnitScope) -> Command {
        let mut cmd = Command::new("systemctl");
        cmd.arg("--no-pager")
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("SYSTEMD_COLORS", "0")
            .env("SYSTEMD_PAGER", "")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if scope == UnitScope::User {
            let uid = self.user_uid;
            cmd.arg("--user")
                .env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"))
                .env(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path=/run/user/{uid}/bus"),
                );
        }
        cmd
    }

    /// 跑一条命令，非零退出码按 stderr 分类成 [`ApiError`]。
    async fn run(mut cmd: Command, unit: &str) -> ApiResult<String> {
        let out = cmd.output().await.map_err(|e| {
            ApiError::new(ErrorCode::Unavailable, "无法执行 systemctl").with_detail(e.to_string())
        })?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !out.status.success() {
            return Err(map_cli_error(&stderr, unit));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// `systemctl show -- <unit> -p <props>` → k=v 表。`LoadState=not-found` 归一成 404。
    async fn show(
        &self,
        scope: UnitScope,
        unit: &str,
        props: &str,
    ) -> ApiResult<HashMap<String, String>> {
        validate_unit_name(unit)?;
        let mut cmd = self.command(scope);
        cmd.arg("show").arg("-p").arg(props).arg("--").arg(unit);
        let out = Self::run(cmd, unit).await?;
        let map = parse_show(&out);
        if map.get("LoadState").map(String::as_str) == Some("not-found") {
            return Err(ApiError::not_found(format!("unit {unit} 不存在")));
        }
        Ok(map)
    }

    async fn list_raw(&self, scope: UnitScope) -> ApiResult<Vec<UnitSummary>> {
        let mut units_cmd = self.command(scope);
        units_cmd.args(["list-units", "--all", "--output=json"]);
        let mut files_cmd = self.command(scope);
        files_cmd.args(["list-unit-files", "--output=json"]);

        let (units_out, files_out) = tokio::try_join!(
            Self::run(units_cmd, "list-units"),
            Self::run(files_cmd, "list-unit-files")
        )?;

        let rows: Vec<ListUnitRow> = serde_json::from_str(&units_out).map_err(|e| {
            ApiError::internal("解析 systemctl list-units 输出失败").with_detail(e.to_string())
        })?;
        let files: Vec<UnitFileRow> = serde_json::from_str(&files_out).map_err(|e| {
            ApiError::internal("解析 systemctl list-unit-files 输出失败").with_detail(e.to_string())
        })?;

        let file_states: HashMap<String, String> = files
            .into_iter()
            .filter_map(|f| unit_file_basename(&f.unit_file).map(|n| (n.to_owned(), f.state)))
            .collect();

        let mut seen = std::collections::HashSet::with_capacity(rows.len());
        let mut out = Vec::with_capacity(rows.len() + file_states.len());
        for r in rows {
            seen.insert(r.unit.clone());
            out.push(UnitSummary {
                unit_type: unit_type_of(&r.unit).to_owned(),
                description: if r.description.is_empty() {
                    r.unit.clone()
                } else {
                    r.description
                },
                load_state: parse_load_state(&r.load),
                active_state: parse_active_state(&r.active),
                sub_state: r.sub,
                enable_state: lookup_enable_state(&file_states, &r.unit),
                scope,
                name: r.unit,
            });
        }
        for (name, state) in &file_states {
            if !seen.contains(name) && state != "alias" {
                out.push(summary_for_unloaded_file(name, state, scope));
            }
        }
        Ok(out)
    }
}

/// 按 stderr 内容给 systemctl 的失败分类。
fn map_cli_error(stderr: &str, unit: &str) -> ApiError {
    let l = stderr.to_lowercase();
    let detail = stderr.trim().to_owned();
    if l.contains("failed to connect to bus") || l.contains("failed to connect to user scope bus") {
        ApiError::new(ErrorCode::Unavailable, "systemctl 连不上 systemd").with_detail(detail)
    } else if l.contains("access denied")
        || l.contains("interactive authentication required")
        || l.contains("authentication is required")
        || l.contains("not authorized")
        || l.contains("permission denied")
    {
        ApiError::permission_denied(format!("需要管理访问：系统拒绝了对 {unit} 的操作"))
            .with_detail(detail)
            .retry_elevated()
    } else if l.contains("not found")
        || l.contains("could not be found")
        || l.contains("no such file")
        || l.contains("does not exist")
        || l.contains("not loaded")
    {
        ApiError::not_found(format!("unit {unit} 不存在")).with_detail(detail)
    } else {
        ApiError::internal(format!("systemctl 失败（{unit}）")).with_detail(detail)
    }
}

/// `Key=Value` 行 → 表。值里可以含 `=`，只按第一个分。
fn parse_show(out: &str) -> HashMap<String, String> {
    out.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// 空格分隔的列表（`Requires=` / `DropInPaths=`），`Documentation=` 的每项带双引号，去掉。
fn split_words(v: &str) -> Vec<String> {
    v.split_whitespace()
        .map(|w| w.trim_matches('"').to_owned())
        .filter(|w| !w.is_empty())
        .collect()
}

/// systemd 的记账值：`[not set]` / `infinity` 都表示没有。
fn parse_num(v: &str) -> Option<u64> {
    v.trim().parse().ok()
}

fn get<'a>(m: &'a HashMap<String, String>, k: &str) -> Option<&'a str> {
    m.get(k).map(String::as_str).filter(|s| !s.is_empty())
}

fn summary_from_show(fallback: &str, m: &HashMap<String, String>, scope: UnitScope) -> UnitSummary {
    let name = get(m, "Id").unwrap_or(fallback).to_owned();
    UnitSummary {
        unit_type: unit_type_of(&name).to_owned(),
        description: get(m, "Description").unwrap_or(&name).to_owned(),
        load_state: parse_load_state(get(m, "LoadState").unwrap_or("")),
        active_state: parse_active_state(get(m, "ActiveState").unwrap_or("")),
        sub_state: get(m, "SubState").unwrap_or("").to_owned(),
        enable_state: parse_enable_state(get(m, "UnitFileState").unwrap_or("")),
        scope,
        name,
    }
}

#[async_trait]
impl Provider for SystemctlCli {
    fn id(&self) -> &'static str {
        "systemd"
    }

    async fn probe(&self) -> Probe {
        let mut cmd = Command::new("systemctl");
        cmd.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(super::CALL_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => {
                Probe::degraded("systemd bus 不可用，走 systemctl 子进程（无实时事件）")
            }
            Ok(Ok(out)) => Probe::unavailable(format!(
                "systemctl --version 退出码 {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(Err(e)) => Probe::unavailable(format!("无法执行 systemctl: {e}")),
            Err(_) => Probe::unavailable("systemctl --version 超时"),
        }
    }
}

#[async_trait]
impl ServiceProvider for SystemctlCli {
    async fn list_units(&self, query: &UnitListQuery) -> ApiResult<Vec<UnitSummary>> {
        let scope = query.scope.unwrap_or_default();
        with_timeout("systemctl list-units", async {
            let units = self.list_raw(scope).await?;
            Ok(apply_list_query(units, query))
        })
        .await
    }

    async fn unit_detail(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitDetail> {
        with_timeout("systemctl show", async {
            let m = self.show(scope, unit, DETAIL_PROPS).await?;
            let summary = summary_from_show(unit, &m, scope);

            let cgroup = get(&m, "ControlGroup").map(|cg| {
                let mut usage = self.cgroup.read(cg).unwrap_or_default();
                fill_from_show(&mut usage, &m);
                usage.path = Some(cg.to_owned());
                usage
            });

            Ok(UnitDetail {
                fragment_path: get(&m, "FragmentPath").map(str::to_owned),
                drop_in_paths: split_words(get(&m, "DropInPaths").unwrap_or("")),
                main_pid: get(&m, "MainPID")
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|p| *p != 0),
                active_enter_ts: get(&m, "ActiveEnterTimestamp").and_then(parse_utc_timestamp),
                state_change_ts: get(&m, "StateChangeTimestamp").and_then(parse_utc_timestamp),
                n_restarts: get(&m, "NRestarts").and_then(|v| v.parse().ok()),
                result: get(&m, "Result").map(str::to_owned),
                exit_code: get(&m, "ExecMainStatus").and_then(|v| v.parse().ok()),
                documentation: split_words(get(&m, "Documentation").unwrap_or("")),
                user: get(&m, "User").map(str::to_owned),
                cgroup,
                summary,
            })
        })
        .await
    }

    async fn unit_file(&self, scope: UnitScope, unit: &str) -> ApiResult<UnitFile> {
        with_timeout("systemctl show", async {
            let m = self
                .show(scope, unit, "Id,LoadState,FragmentPath,DropInPaths")
                .await?;
            let fragment = match get(&m, "FragmentPath") {
                Some(p) => Some(read_unit_fragment(p).await?),
                None => None,
            };
            let mut drop_ins = Vec::new();
            for p in split_words(get(&m, "DropInPaths").unwrap_or("")) {
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
        with_timeout("systemctl show", async {
            let m = self.show(scope, unit, DEPS_PROPS).await?;
            let list = |k: &str| split_words(get(&m, k).unwrap_or(""));
            Ok(UnitDeps {
                unit: unit.to_owned(),
                requires: list("Requires"),
                requisite: list("Requisite"),
                wants: list("Wants"),
                binds_to: list("BindsTo"),
                part_of: list("PartOf"),
                required_by: list("RequiredBy"),
                wanted_by: list("WantedBy"),
                bound_by: list("BoundBy"),
                conflicts: list("Conflicts"),
                conflicted_by: list("ConflictedBy"),
                before: list("Before"),
                after: list("After"),
                triggers: list("Triggers"),
                triggered_by: list("TriggeredBy"),
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
        with_timeout("systemctl 操作", async {
            let verb = match action {
                UnitAction::Start => "start",
                UnitAction::Stop => "stop",
                UnitAction::Restart => "restart",
                UnitAction::Reload => "reload",
                UnitAction::Enable => "enable",
                UnitAction::Disable => "disable",
                UnitAction::Mask => "mask",
                UnitAction::Unmask => "unmask",
            };
            let mut cmd = self.command(scope);
            cmd.arg(verb).arg("--").arg(unit);
            Self::run(cmd, unit).await?;

            // systemctl 是同步的：返回时 job 已经完成，读到的状态就是结果。
            let active_state = self
                .show(scope, unit, "Id,LoadState,ActiveState")
                .await
                .ok()
                .and_then(|m| get(&m, "ActiveState").map(parse_active_state));
            Ok(UnitActionResp {
                unit: unit.to_owned(),
                action,
                job: None,
                active_state,
            })
        })
        .await
    }

    async fn subscribe(&self) -> broadcast::Receiver<ServiceEvent> {
        self.idle.subscribe()
    }
}

/// 直读缺失的字段用 `systemctl show` 的记账值补齐。
fn fill_from_show(usage: &mut CgroupUsage, m: &HashMap<String, String>) {
    let num = |k: &str| get(m, k).and_then(parse_num);
    if usage.cpu_usage_nsec.is_none() {
        usage.cpu_usage_nsec = num("CPUUsageNSec");
    }
    if usage.memory_current_bytes.is_none() {
        usage.memory_current_bytes = num("MemoryCurrent");
    }
    if usage.memory_peak_bytes.is_none() {
        usage.memory_peak_bytes = num("MemoryPeak");
    }
    if usage.memory_limit_bytes.is_none() {
        usage.memory_limit_bytes = num("MemoryMax");
    }
    if usage.tasks_current.is_none() {
        usage.tasks_current = num("TasksCurrent");
    }
    if usage.tasks_limit.is_none() {
        usage.tasks_limit = num("TasksMax");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use strixmaid_types::service::UnitActiveState;

    #[test]
    fn parses_show_output() {
        let out = "Id=ssh.service\nMemoryMax=infinity\nMemoryCurrent=[not set]\nTasksMax=308853\n\
                   Documentation=\"man:sshd(8)\" \"man:sshd_config(5)\"\nDropInPaths=\n\
                   ActiveEnterTimestamp=Thu 2026-08-27 06:54:25 UTC\nExecStart={ path=/usr/sbin/sshd ; argv[]=... }\n";
        let m = parse_show(out);
        assert_eq!(get(&m, "Id"), Some("ssh.service"));
        assert_eq!(get(&m, "MemoryMax").and_then(parse_num), None);
        assert_eq!(get(&m, "MemoryCurrent").and_then(parse_num), None);
        assert_eq!(get(&m, "TasksMax").and_then(parse_num), Some(308_853));
        assert_eq!(
            split_words(get(&m, "Documentation").unwrap()),
            ["man:sshd(8)", "man:sshd_config(5)"]
        );
        assert!(split_words(get(&m, "DropInPaths").unwrap_or("")).is_empty());
        assert_eq!(
            get(&m, "ActiveEnterTimestamp").and_then(parse_utc_timestamp),
            Some(1_787_813_665)
        );
        assert!(get(&m, "ExecStart").unwrap().contains("argv[]"));
    }

    #[test]
    fn classifies_stderr() {
        assert_eq!(
            map_cli_error("Failed to start x.service: Access denied", "x").code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            map_cli_error(
                "Failed to start x.service: Interactive authentication required.",
                "x"
            )
            .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            map_cli_error("Failed to start x.service: Unit x.service not found.", "x").code,
            ErrorCode::NotFound
        );
        assert_eq!(
            map_cli_error("Failed to connect to bus: No such file or directory", "x").code,
            ErrorCode::Unavailable
        );
        assert_eq!(
            map_cli_error("something else", "x").code,
            ErrorCode::Internal
        );
    }

    async fn cli_or_skip() -> Option<SystemctlCli> {
        let cli = SystemctlCli::new();
        match cli.probe().await {
            Probe::Unavailable { reason } => {
                eprintln!("跳过：{reason}");
                None
            }
            _ => Some(cli),
        }
    }

    #[tokio::test]
    async fn live_cli_matches_bus() {
        let Some(cli) = cli_or_skip().await else {
            return;
        };
        let t0 = std::time::Instant::now();
        let cli_units = match cli.list_units(&UnitListQuery::default()).await {
            Ok(u) => u,
            Err(e) if e.code == ErrorCode::Unavailable => {
                eprintln!("跳过：{e}");
                return;
            }
            Err(e) => panic!("{e:?}"),
        };
        eprintln!("[cli] {} units, {:?}", cli_units.len(), t0.elapsed());
        assert!(!cli_units.is_empty());

        // 与 bus 路径对比：unit 集合应一致（中间可能有 transient unit 生灭，允许极少量差异）。
        let Ok(bus) = super::super::bus::SystemdBus::connect().await else {
            return;
        };
        if bus.probe().await != Probe::Available {
            return;
        }
        let bus_units = bus.list_units(&UnitListQuery::default()).await.unwrap();
        let a: HashSet<&str> = cli_units.iter().map(|u| u.name.as_str()).collect();
        let b: HashSet<&str> = bus_units.iter().map(|u| u.name.as_str()).collect();
        let diff: Vec<_> = a.symmetric_difference(&b).collect();
        eprintln!("[cli vs bus] cli={} bus={} diff={diff:?}", a.len(), b.len());
        assert!(diff.len() <= 3, "两条路径的 unit 集合差异过大: {diff:?}");

        // 同一个 unit 的详情：两条路径的关键字段一致。
        let name = cli_units
            .iter()
            .find(|u| {
                u.unit_type == "service"
                    && u.active_state == UnitActiveState::Active
                    && u.sub_state == "running"
            })
            .map(|u| u.name.clone())
            .expect("有 running service");
        let dc = cli.unit_detail(UnitScope::System, &name).await.unwrap();
        let db = bus.unit_detail(UnitScope::System, &name).await.unwrap();
        assert_eq!(dc.summary.name, db.summary.name);
        assert_eq!(dc.main_pid, db.main_pid);
        assert_eq!(dc.fragment_path, db.fragment_path);
        assert_eq!(dc.active_enter_ts, db.active_enter_ts);
        assert_eq!(
            dc.cgroup.as_ref().and_then(|c| c.path.clone()),
            db.cgroup.as_ref().and_then(|c| c.path.clone())
        );

        let fc = cli.unit_file(UnitScope::System, &name).await.unwrap();
        let fb = bus.unit_file(UnitScope::System, &name).await.unwrap();
        assert_eq!(fc, fb);

        let e = cli
            .unit_detail(UnitScope::System, "strixmaid-does-not-exist.service")
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = cli
            .unit_action(
                UnitScope::System,
                "strixmaid-does-not-exist.service",
                UnitAction::Start,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(e.code, ErrorCode::NotFound | ErrorCode::PermissionDenied),
            "{e:?}"
        );
    }
}
