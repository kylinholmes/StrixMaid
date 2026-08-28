//! `Journalctl`：`journalctl -o json` 子进程实现。
//!
//! # 参数拼接
//!
//! 所有过滤条件走 `Command::arg`，且一律用 `--opt=value` 形式：值以 `-` 开头也不会被
//! 当成选项，更不存在 shell。`q` 全文关键字用 PCRE2 的 `\Q…\E` 包成字面量再交给 `--grep`，
//! 用户输入里的正则元字符不会改变语义。
//!
//! # 分页
//!
//! 实测（systemd 255）：`--after-cursor=C -r --lines=N` 给出**严格早于** C 的 N 条、由新到旧；
//! `--cursor=C --lines=1` 给出 C 本身。每页多要一条来判断还有没有更旧的（`next_cursor`）。
//! 游标不存在时 journalctl 不报错而是定位到最近的条目，所以详情接口要核对返回的 `__CURSOR`。
//!
//! # `--grep` 不可用时
//!
//! 老版本或未链接 pcre2 的 journalctl 没有 `-g`，此时不加 `--lines`，流式读并在进程内做
//! 子串匹配，凑够一页就 kill 子进程。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use strixmaid_types::log::{BootInfo, LogEntry, LogEntryDetail, LogPage, LogQuery};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{OnceCell, broadcast};

use super::parse::{boots_from_json, boots_from_text, detail_from_raw, entry_from_raw, parse_line};
use super::{LogFollow, LogProvider, normalize_limit};
use crate::providers::{Probe, Provider};

/// 单次查询的总超时（含子进程启动）。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
/// follow 批量推送：拿到第一条后最多再等这么久收集后续行。
const FOLLOW_BATCH_WINDOW: Duration = Duration::from_millis(30);
/// follow 单批最大条数。
const FOLLOW_BATCH_MAX: usize = 128;
/// follow 通道容量（批次数）。
const FOLLOW_CAPACITY: usize = 256;

/// 一个共享的 `journalctl -f` 子进程。最后一个 `Arc` drop 时 abort 读任务，
/// 任务持有的 `Child` 随之 drop → `kill_on_drop` 送 SIGKILL。
#[derive(Debug)]
pub struct FollowShared {
    tx: broadcast::Sender<Arc<Vec<LogEntry>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FollowShared {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// follow 的共享键：影响子进程参数的那部分过滤条件。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FollowKey {
    priority: Option<u8>,
    unit: Option<String>,
    boot: Option<String>,
    q: Option<String>,
}

impl FollowKey {
    fn from_query(q: &LogQuery) -> Self {
        Self {
            priority: q.priority.map(|p| p.as_u8()),
            unit: q.unit.clone(),
            boot: q.boot.clone(),
            q: q.q.clone(),
        }
    }
}

/// journalctl 子进程实现。
#[derive(Debug, Default)]
pub struct Journalctl {
    follows: Mutex<HashMap<FollowKey, Weak<FollowShared>>>,
    grep_supported: OnceCell<bool>,
}

impl Journalctl {
    pub fn new() -> Self {
        Self::default()
    }

    /// 基础命令。`--all` 关掉 4096 字节截断（stack trace 常超过）；`-q` 去掉「看不到其他用户日志」的提示。
    fn base_command() -> Command {
        let mut cmd = Command::new("journalctl");
        cmd.args(["--output=json", "--no-pager", "--quiet", "--all"])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .env("SYSTEMD_PAGER", "")
            .env("SYSTEMD_COLORS", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// 探测 `--grep` 是否可用（只探一次）。
    async fn grep_supported(&self) -> bool {
        *self
            .grep_supported
            .get_or_init(|| async {
                let mut cmd = Self::base_command();
                cmd.args(["--grep=strixmaid-probe", "--lines=0"]);
                match tokio::time::timeout(QUERY_TIMEOUT, cmd.output()).await {
                    Ok(Ok(out)) => {
                        // 没匹配到任何条目时 journalctl 退出码是 1，不能拿退出码判断；
                        // 只看 stderr 有没有「不认识这个选项 / 没编译 pcre2」。
                        let err = String::from_utf8_lossy(&out.stderr).to_lowercase();
                        let ok = !err.contains("pattern matching")
                            && !err.contains("unrecognized option")
                            && !err.contains("unknown option");
                        if !ok {
                            tracing::info!(stderr = %err.trim(), "journalctl 不支持 --grep，全文过滤改为进程内匹配");
                        }
                        ok
                    }
                    _ => false,
                }
            })
            .await
    }

    /// 把过滤条件加到命令上。`grep=false` 时 `q` 不下发，由调用方在进程内匹配。
    fn apply_filters(cmd: &mut Command, q: &LogQuery, grep: bool) -> ApiResult<()> {
        if let (Some(s), Some(u)) = (q.since, q.until)
            && s > u
        {
            return Err(ApiError::invalid_request("since 晚于 until"));
        }
        if let Some(p) = q.priority {
            cmd.arg(format!("--priority={}", p.as_u8()));
        }
        if let Some(s) = q.since {
            cmd.arg(format!("--since=@{s}"));
        }
        if let Some(u) = q.until {
            cmd.arg(format!("--until=@{u}"));
        }
        if let Some(unit) = &q.unit {
            crate::providers::service::validate_unit_name(unit)?;
            cmd.arg(format!("--unit={unit}"));
        }
        if let Some(boot) = &q.boot {
            validate_boot(boot)?;
            cmd.arg(format!("--boot={boot}"));
        }
        if grep && let Some(pat) = &q.q {
            cmd.arg(format!("--grep={}", regex_literal(pat)));
            cmd.arg("--case-sensitive=false");
        }
        Ok(())
    }

    /// 起子进程并拿到 stdout；stderr 由一个任务异步收集，避免管道写满互相卡死。
    fn spawn(mut cmd: Command) -> ApiResult<(Child, ChildStdout, tokio::task::JoinHandle<String>)> {
        let mut child = cmd.spawn().map_err(|e| {
            ApiError::new(ErrorCode::Unavailable, "无法执行 journalctl").with_detail(e.to_string())
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ApiError::internal("journalctl stdout 未接管"))?;
        let stderr = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut s = String::new();
            if let Some(mut e) = stderr {
                let _ = e.read_to_string(&mut s).await;
            }
            s
        });
        Ok((child, stdout, stderr_task))
    }

    /// 查询主体（不含超时）。
    async fn query_inner(&self, q: &LogQuery) -> ApiResult<LogPage> {
        let limit = normalize_limit(q.limit)?;
        let grep = q.q.is_some() && self.grep_supported().await;
        let client_filter = if grep {
            None
        } else {
            q.q.as_deref().map(str::to_lowercase)
        };

        let mut cmd = Self::base_command();
        Self::apply_filters(&mut cmd, q, grep)?;
        if let Some(c) = &q.cursor {
            validate_cursor(c)?;
            cmd.arg(format!("--after-cursor={c}"));
        }
        cmd.arg("--reverse");
        if client_filter.is_none() {
            // 多要一条，用来判断还有没有更旧的。
            cmd.arg(format!("--lines={}", limit + 1));
        }

        let (mut child, stdout, stderr_task) = Self::spawn(cmd)?;
        let mut lines = BufReader::new(stdout).lines();
        let mut entries: Vec<LogEntry> = Vec::with_capacity(limit + 1);
        while entries.len() <= limit {
            let Some(line) = lines.next_line().await.map_err(|e| {
                ApiError::internal("读取 journalctl 输出失败").with_detail(e.to_string())
            })?
            else {
                break;
            };
            let Some(entry) = parse_line(&line).as_ref().and_then(entry_from_raw) else {
                continue;
            };
            if let Some(needle) = &client_filter
                && !entry.message.to_lowercase().contains(needle)
            {
                continue;
            }
            entries.push(entry);
        }
        // 读够了就结束子进程（进程内过滤时它可能还在往外吐）。
        drop(lines);
        let _ = child.start_kill();
        let status = child.wait().await.ok();
        let stderr = stderr_task.await.unwrap_or_default();

        // 被我们 kill 的不算失败；自己非零退出且一条都没读到，才把 stderr 当错误报出去。
        if entries.is_empty()
            && let Some(st) = status
            && !st.success()
            && st.signal_none()
            && !stderr.trim().is_empty()
        {
            return Err(map_journalctl_error(&stderr));
        }

        let has_more = entries.len() > limit;
        entries.truncate(limit);
        Ok(LogPage {
            prev_cursor: entries.first().map(|e| e.cursor.clone()),
            next_cursor: if has_more {
                entries.last().map(|e| e.cursor.clone())
            } else {
                None
            },
            entries,
        })
    }

    async fn entry_inner(&self, cursor: &str) -> ApiResult<LogEntryDetail> {
        validate_cursor(cursor)?;
        let mut cmd = Self::base_command();
        cmd.arg(format!("--cursor={cursor}")).arg("--lines=1");
        let out = tokio::time::timeout(QUERY_TIMEOUT, cmd.output())
            .await
            .map_err(|_| ApiError::new(ErrorCode::Timeout, "journalctl 超时"))?
            .map_err(|e| {
                ApiError::new(ErrorCode::Unavailable, "无法执行 journalctl")
                    .with_detail(e.to_string())
            })?;
        if !out.status.success() {
            return Err(map_journalctl_error(&String::from_utf8_lossy(&out.stderr)));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let detail = stdout
            .lines()
            .find_map(|l| parse_line(l).as_ref().and_then(detail_from_raw))
            .ok_or_else(|| ApiError::not_found("游标对应的日志不存在"))?;
        // journalctl 对不存在的游标会「就近」定位，必须核对。
        if detail.entry.cursor != cursor {
            return Err(ApiError::not_found(
                "游标对应的日志不存在（可能已被轮转淘汰）",
            ));
        }
        Ok(detail)
    }

    async fn boots_inner(&self) -> ApiResult<Vec<BootInfo>> {
        // 新版：JSON。
        let mut cmd = Self::base_command();
        cmd.arg("--list-boots");
        let out = cmd.output().await.map_err(|e| {
            ApiError::new(ErrorCode::Unavailable, "无法执行 journalctl").with_detail(e.to_string())
        })?;
        if out.status.success()
            && let Some(mut boots) = boots_from_json(&String::from_utf8_lossy(&out.stdout))
        {
            boots.sort_by_key(|b| b.index);
            return Ok(boots);
        }

        // 旧版：不认 `--output=json` 配 `--list-boots`，退回文本格式（TZ=UTC 已设）。
        let mut cmd = Command::new("journalctl");
        cmd.args(["--list-boots", "--no-pager", "--quiet"])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let out = cmd.output().await.map_err(|e| {
            ApiError::new(ErrorCode::Unavailable, "无法执行 journalctl").with_detail(e.to_string())
        })?;
        if !out.status.success() {
            return Err(map_journalctl_error(&String::from_utf8_lossy(&out.stderr)));
        }
        let mut boots = boots_from_text(&String::from_utf8_lossy(&out.stdout));
        boots.sort_by_key(|b| b.index);
        Ok(boots)
    }
}

/// follow 读任务：逐行读、小窗口攒批、广播。`child` 由本任务持有，任务被 abort 时随之 kill。
async fn follow_reader(
    child: Child,
    stdout: ChildStdout,
    tx: broadcast::Sender<Arc<Vec<LogEntry>>>,
    client_filter: Option<String>,
) {
    let _child = child;
    let mut lines = BufReader::new(stdout).lines();
    let accept = |line: &str| -> Option<LogEntry> {
        let entry = parse_line(line).as_ref().and_then(entry_from_raw)?;
        if let Some(needle) = &client_filter
            && !entry.message.to_lowercase().contains(needle)
        {
            return None;
        }
        Some(entry)
    };

    'outer: loop {
        let mut batch = Vec::new();
        match lines.next_line().await {
            Ok(Some(line)) => batch.extend(accept(&line)),
            _ => break,
        }
        // 第一条到手后，在小窗口内把紧随其后的行一起带上，减少 WS 帧数。
        let window = tokio::time::sleep(FOLLOW_BATCH_WINDOW);
        tokio::pin!(window);
        while batch.len() < FOLLOW_BATCH_MAX {
            tokio::select! {
                l = lines.next_line() => match l {
                    Ok(Some(line)) => batch.extend(accept(&line)),
                    _ => {
                        if !batch.is_empty() { let _ = tx.send(Arc::new(batch)); }
                        break 'outer;
                    }
                },
                _ = &mut window => break,
            }
        }
        if !batch.is_empty() {
            // 暂时没有订阅者也不退出：Arc 还活着说明马上会有人 subscribe。
            let _ = tx.send(Arc::new(batch));
        }
    }
    tracing::debug!("journalctl -f 结束");
}

/// boot 参数：相对偏移（`0` / `-1` / `+2`）或 32 位 hex boot id。
fn validate_boot(b: &str) -> ApiResult<()> {
    let is_offset = {
        let digits = b
            .strip_prefix('-')
            .or_else(|| b.strip_prefix('+'))
            .unwrap_or(b);
        !digits.is_empty() && digits.len() <= 6 && digits.bytes().all(|c| c.is_ascii_digit())
    };
    let is_id = b.len() == 32 && b.bytes().all(|c| c.is_ascii_hexdigit());
    if is_offset || is_id {
        Ok(())
    } else {
        Err(ApiError::invalid_request(format!("boot 参数非法: {b}")))
    }
}

/// 游标：journald 的形式是 `s=…;i=…;b=…;m=…;t=…;x=…`，只允许这些字符。
fn validate_cursor(c: &str) -> ApiResult<()> {
    if c.is_empty() || c.len() > 512 {
        return Err(ApiError::invalid_request("游标为空或过长"));
    }
    if !c
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b';' | b'=' | b'-' | b'+' | b'_'))
    {
        return Err(ApiError::invalid_request("游标含非法字符"));
    }
    Ok(())
}

/// 把用户输入包成 PCRE2 字面量：`\Q…\E`；输入里自带的 `\E` 要切开重新包。
fn regex_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for (i, part) in s.split("\\E").enumerate() {
        if i > 0 {
            out.push_str("\\\\E");
        }
        if !part.is_empty() {
            out.push_str("\\Q");
            out.push_str(part);
            out.push_str("\\E");
        }
    }
    out
}

/// journalctl 的 stderr → 错误码。
fn map_journalctl_error(stderr: &str) -> ApiError {
    let l = stderr.to_lowercase();
    let detail = stderr.trim().to_owned();
    if l.contains("failed to seek to cursor") || l.contains("invalid cursor") {
        ApiError::not_found("游标对应的日志不存在").with_detail(detail)
    } else if l.contains("failed to parse")
        || l.contains("invalid")
        || l.contains("unrecognized option")
        || l.contains("specifying boot id")
        || l.contains("no such boot")
        || l.contains("data from the specified boot")
    {
        ApiError::invalid_request("日志查询参数不合法").with_detail(detail)
    } else if l.contains("permission denied")
        || l.contains("access denied")
        // RHEL 系上非特权用户一个 journal 文件都打不开时的措辞，不含 "permission denied"。
        // 归错了不只是错误码难看：auth::exec::escalate **只在 PermissionDenied 时才升级**，
        // 落进 Internal 就等于把「提权本可以解决」这条路也堵死了。
        || l.contains("insufficient permissions")
    {
        ApiError::permission_denied("没有读取日志的权限")
            .with_detail(detail)
            .retry_elevated()
    } else {
        ApiError::internal("journalctl 失败").with_detail(detail)
    }
}

/// `ExitStatus` 是否**不是**被信号杀掉的。
trait SignalNone {
    fn signal_none(&self) -> bool;
}

impl SignalNone for std::process::ExitStatus {
    fn signal_none(&self) -> bool {
        use std::os::unix::process::ExitStatusExt as _;
        self.signal().is_none()
    }
}

#[async_trait]
impl Provider for Journalctl {
    fn id(&self) -> &'static str {
        "journald"
    }

    async fn probe(&self) -> Probe {
        let mut cmd = Command::new("journalctl");
        cmd.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match tokio::time::timeout(QUERY_TIMEOUT, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => Probe::Available,
            Ok(Ok(out)) => Probe::unavailable(format!(
                "journalctl --version 退出码 {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(Err(e)) => Probe::unavailable(format!("无法执行 journalctl: {e}")),
            Err(_) => Probe::unavailable("journalctl --version 超时"),
        }
    }
}

#[async_trait]
impl LogProvider for Journalctl {
    async fn query(&self, q: &LogQuery) -> ApiResult<LogPage> {
        tokio::time::timeout(QUERY_TIMEOUT, self.query_inner(q))
            .await
            .map_err(|_| ApiError::new(ErrorCode::Timeout, "journalctl 查询超时"))?
    }

    async fn entry(&self, cursor: &str) -> ApiResult<LogEntryDetail> {
        self.entry_inner(cursor).await
    }

    async fn boots(&self) -> ApiResult<Vec<BootInfo>> {
        tokio::time::timeout(QUERY_TIMEOUT, self.boots_inner())
            .await
            .map_err(|_| ApiError::new(ErrorCode::Timeout, "journalctl --list-boots 超时"))?
    }

    async fn follow(&self, q: &LogQuery) -> ApiResult<LogFollow> {
        let grep = q.q.is_some() && self.grep_supported().await;
        let key = FollowKey::from_query(q);

        let mut cmd = Self::base_command();
        Self::apply_filters(&mut cmd, q, grep)?;
        cmd.args(["--follow", "--lines=0"]);

        let mut map = self.follows.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, w| w.strong_count() > 0);
        if let Some(shared) = map.get(&key).and_then(Weak::upgrade) {
            return Ok(LogFollow::new(shared.tx.subscribe(), Box::new(shared)));
        }

        let (child, stdout, _stderr_task) = Self::spawn(cmd)?;
        let (tx, rx) = broadcast::channel(FOLLOW_CAPACITY);
        let client_filter = if grep {
            None
        } else {
            q.q.as_deref().map(str::to_lowercase)
        };
        let task = tokio::spawn(follow_reader(child, stdout, tx.clone(), client_filter));
        let shared = Arc::new(FollowShared { tx, task });
        map.insert(key, Arc::downgrade(&shared));
        tracing::debug!(filter = ?q, "journalctl -f 已启动");
        Ok(LogFollow::new(rx, Box::new(shared)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::log::LogPriority;

    #[test]
    fn validators() {
        assert!(validate_boot("0").is_ok());
        assert!(validate_boot("-1").is_ok());
        assert!(validate_boot("5d6eb52cf50c4b1cb5950accac688272").is_ok());
        assert!(validate_boot("--help").is_err());
        assert!(validate_boot("abc").is_err());
        assert!(validate_cursor("s=f978c38c82b9455fa2ad7e5edd8f0a7e;i=452ef3a;b=5d6eb52c;m=14480bd4b04b;t=65a0426b2f897;x=84acd4b9a874bd7e").is_ok());
        assert!(validate_cursor("").is_err());
        assert!(validate_cursor("s=1; rm -rf /").is_err());
    }

    #[test]
    fn regex_literal_quoting() {
        assert_eq!(regex_literal("a.b*c"), "\\Qa.b*c\\E");
        assert_eq!(regex_literal("x\\Ey"), "\\Qx\\E\\\\E\\Qy\\E");
        assert_eq!(regex_literal(""), "");
    }

    #[test]
    fn filters_are_separate_args() {
        let q = LogQuery {
            priority: Some(LogPriority::Warning),
            since: Some(100),
            until: Some(200),
            unit: Some("nginx.service".into()),
            boot: Some("-1".into()),
            q: Some("-rf /".into()),
            ..Default::default()
        };
        let mut cmd = Journalctl::base_command();
        Journalctl::apply_filters(&mut cmd, &q, true).unwrap();
        let args: Vec<String> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--priority=4".to_owned()));
        assert!(args.contains(&"--since=@100".to_owned()));
        assert!(args.contains(&"--until=@200".to_owned()));
        assert!(args.contains(&"--unit=nginx.service".to_owned()));
        assert!(args.contains(&"--boot=-1".to_owned()));
        assert!(args.contains(&"--grep=\\Q-rf /\\E".to_owned()), "{args:?}");

        let bad = LogQuery {
            since: Some(5),
            until: Some(1),
            ..Default::default()
        };
        assert!(Journalctl::apply_filters(&mut Journalctl::base_command(), &bad, true).is_err());
        let bad = LogQuery {
            unit: Some("nginx; true".into()),
            ..Default::default()
        };
        assert!(Journalctl::apply_filters(&mut Journalctl::base_command(), &bad, true).is_err());
    }

    #[test]
    fn stderr_mapping() {
        assert_eq!(
            map_journalctl_error("Failed to seek to cursor: Invalid argument").code,
            ErrorCode::NotFound
        );
        assert_eq!(
            map_journalctl_error("Failed to parse timestamp: xx").code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(map_journalctl_error("boom").code, ErrorCode::Internal);

        // journalctl 说「权限不足」有不止一种措辞。RHEL 系上非特权用户一个 journal
        // 文件都打不开时说的是 "insufficient permissions"，不含 "permission denied"。
        // roadmap/07 在 Rocky 9 上实地撞到过：它落进了兜底的 Internal，于是一个普通
        // 用户打开日志页就得到 500。Ubuntu 上不会复现——那里 journald 的 ACL 让他
        // 至少打得开自己的用户日志，请求正常返回 200。
        assert_eq!(
            map_journalctl_error("No journal files were opened due to insufficient permissions.")
                .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            map_journalctl_error("Permission denied").code,
            ErrorCode::PermissionDenied
        );
    }

    // ---- 以下需要真实 journalctl；不可用时静默跳过 ----

    async fn journal_or_skip() -> Option<Journalctl> {
        let j = Journalctl::new();
        match j.probe().await {
            Probe::Available => Some(j),
            other => {
                eprintln!("跳过：{other:?}");
                None
            }
        }
    }

    #[tokio::test]
    async fn live_paging_is_gapless() {
        let Some(j) = journal_or_skip().await else {
            return;
        };
        let t0 = std::time::Instant::now();
        let base = j
            .query(&LogQuery {
                limit: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        eprintln!(
            "[journal] limit=10 → {} entries, {:?}",
            base.entries.len(),
            t0.elapsed()
        );
        if base.entries.len() < 10 {
            eprintln!("跳过：日志不足 10 条");
            return;
        }
        assert!(base.next_cursor.is_some());
        assert_eq!(
            base.prev_cursor.as_deref(),
            Some(base.entries[0].cursor.as_str())
        );
        // 由新到旧。
        assert!(
            base.entries
                .windows(2)
                .all(|w| (w[0].ts, w[0].us) >= (w[1].ts, w[1].us))
        );

        // 以第 1 条为锚往旧翻：4 条 + 5 条，应与 base[1..5]、base[5..10] 严格一致，不重不漏。
        let p1 = j
            .query(&LogQuery {
                cursor: Some(base.entries[0].cursor.clone()),
                limit: Some(4),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(p1.entries, base.entries[1..5].to_vec());
        let p2 = j
            .query(&LogQuery {
                cursor: p1.next_cursor.clone(),
                limit: Some(5),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(p2.entries, base.entries[5..10].to_vec());

        // 单条详情：全字段里必有 __CURSOR 与 MESSAGE 之外的元数据。
        let d = j.entry(&base.entries[3].cursor).await.unwrap();
        assert_eq!(d.entry, base.entries[3]);
        assert!(d.fields.contains_key("__REALTIME_TIMESTAMP"));

        // 伪造游标 → 404。
        let e = j
            .entry("s=00000000000000000000000000000000;i=1;b=00000000000000000000000000000000;m=1;t=1;x=1")
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);

        // 过滤：优先级 + 本次 boot。
        let f = j
            .query(&LogQuery {
                priority: Some(LogPriority::Warning),
                boot: Some("0".into()),
                limit: Some(20),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(f.entries.iter().all(|e| e.priority <= LogPriority::Warning));

        // 关键字：拿最新一条消息里的一个词回查，必须命中（走 --grep 或进程内匹配都一样）。
        if let Some(word) = base.entries[0]
            .message
            .split_whitespace()
            .find(|w| w.len() >= 4)
        {
            let g = j
                .query(&LogQuery {
                    q: Some(word.to_uppercase()),
                    limit: Some(5),
                    ..Default::default()
                })
                .await
                .unwrap();
            eprintln!(
                "[journal] q={word:?} (upper) → {} entries, grep={}",
                g.entries.len(),
                j.grep_supported().await
            );
            assert!(!g.entries.is_empty(), "关键字 {word:?} 应命中");
            assert!(
                g.entries
                    .iter()
                    .all(|e| e.message.to_lowercase().contains(&word.to_lowercase()))
            );
        }

        assert_eq!(
            j.query(&LogQuery {
                limit: Some(0),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .code,
            ErrorCode::InvalidRequest
        );
    }

    #[tokio::test]
    async fn live_boots() {
        let Some(j) = journal_or_skip().await else {
            return;
        };
        let boots = j.boots().await.unwrap();
        eprintln!("[journal] boots: {}", boots.len());
        assert!(boots.iter().any(|b| b.index == 0));
        assert!(
            boots
                .iter()
                .all(|b| b.boot_id.len() == 32 && b.first_ts <= b.last_ts)
        );
        // 文本回退路径也要能跑通（本机新版会输出空格分隔的格式）。
        let mut cmd = Command::new("journalctl");
        cmd.args(["--list-boots", "--no-pager", "--quiet"])
            .env("TZ", "UTC")
            .env("LC_ALL", "C");
        if let Ok(out) = cmd.output().await {
            let text = boots_from_text(&String::from_utf8_lossy(&out.stdout));
            assert_eq!(text.len(), boots.len());
            let cur = text.iter().find(|b| b.index == 0).unwrap();
            let cur_json = boots.iter().find(|b| b.index == 0).unwrap();
            assert_eq!(cur.boot_id, cur_json.boot_id);
            assert!((cur.first_ts - cur_json.first_ts).abs() <= 1);
        }
    }

    #[tokio::test]
    async fn live_follow_receives_logger_line() {
        let Some(j) = journal_or_skip().await else {
            return;
        };
        let marker = format!(
            "strixmaid-follow-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );

        // 两个订阅：一个不带过滤，一个用 q 过滤（走 --grep 或进程内匹配）；不带过滤的两个订阅共享子进程。
        let mut plain = j.follow(&LogQuery::default()).await.unwrap();
        let _plain2 = j.follow(&LogQuery::default()).await.unwrap();
        let mut filtered = j
            .follow(&LogQuery {
                q: Some(marker.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        eprintln!("[follow] --grep supported: {}", j.grep_supported().await);
        assert_eq!(
            j.follows.lock().unwrap().len(),
            2,
            "同一过滤条件共享一个子进程"
        );

        // 给 journalctl -f 一点时间起来。
        tokio::time::sleep(Duration::from_millis(300)).await;
        let ok = Command::new("logger")
            .args(["-t", "strixmaid-test", &marker])
            .status()
            .await;
        let Ok(st) = ok else {
            eprintln!("跳过：没有 logger");
            return;
        };
        assert!(st.success());

        async fn wait(f: &mut LogFollow, marker: &str, name: &str) -> bool {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
            loop {
                match tokio::time::timeout_at(deadline, f.next()).await {
                    Ok(Some(batch)) => {
                        if let Some(e) = batch.iter().find(|e| e.message.contains(marker)) {
                            eprintln!(
                                "[follow:{name}] got {} (ident={:?})",
                                e.message, e.identifier
                            );
                            return true;
                        }
                    }
                    Ok(None) => panic!("{name}: 流提前结束"),
                    Err(_) => return false,
                }
            }
        }
        assert!(
            wait(&mut plain, &marker, "plain").await,
            "无过滤 follow 应收到 logger 的行"
        );
        assert!(
            wait(&mut filtered, &marker, "filtered").await,
            "带 q 过滤的 follow 应收到 logger 的行"
        );

        // 订阅者归零后子进程被 kill。
        drop(plain);
        drop(_plain2);
        drop(filtered);
        tokio::time::sleep(Duration::from_millis(100)).await;
        j.follows
            .lock()
            .unwrap()
            .retain(|_, w| w.strong_count() > 0);
        assert!(j.follows.lock().unwrap().is_empty());
    }
}
