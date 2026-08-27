//! macOS 的 [`LogProvider`] 实现：`log(1)` 统一日志。
//!
//! - 查询：`log show --style ndjson`，逐行流式解析；
//! - follow：`log stream --style ndjson`，常驻子进程，同一过滤条件共享。
//!
//! # 字段映射
//!
//! | journald | 统一日志 | 说明 |
//! |---|---|---|
//! | `PRIORITY` | `messageType` | 五档映射见 [`priority_from_message_type`] |
//! | `MESSAGE` | `eventMessage` | |
//! | `_SYSTEMD_UNIT` | `subsystem` | 都是反向域名标识，是最贴近的对应物 |
//! | `SYSLOG_IDENTIFIER` | `processImagePath` 的 basename | |
//! | `_PID` / `_UID` | `processID` / `userID` | |
//! | `_BOOT_ID` | `bootUUID` | |
//! | `_TRANSPORT` | `eventType` | 区分 logEvent / activity / signpost |
//! | `_HOSTNAME` | **没有** | 统一日志不记录主机名（本机日志本机看） |
//!
//! # 游标
//!
//! journald 的游标是内核给的不透明串。统一日志没有等价物，这里自己造一个：
//! `<unix 微秒>:<整行的 FNV-1a 64 位哈希>`。时间戳负责排序与翻页
//! （`log show --end` 只认时间），哈希负责在**同一微秒内**区分多条日志。
//! 两部分都只依赖日志内容本身，不依赖任何服务端状态，因此游标可以跨请求使用。
//!
//! ## 为什么不用 `traceID`
//!
//! 它看起来像个天然的唯一标识，实际不是：实测两分钟的日志里，
//! `(timestamp, traceID)` 有 57 组重复（涉及 123 条），单个 traceID 最多重复 1698 次
//! ——同一个 activity 下的所有事件共享它。用它做游标的后果不是「排序不稳」这么轻：
//! 翻页时 `accepts` 按 `< 游标` 筛，与游标同键的那些条目会被**整组丢掉**，
//! 也就是**静默漏日志**。整行哈希则只在两条日志逐字节相同且落在同一微秒时才碰撞
//! ——`log show` 偶尔确实会把同一条事件吐两次。那两条彼此无从区分，
//! [`OsLog::show`] 直接去重，游标因此在结果里严格唯一。
//!
//! # 性能与默认窗口
//!
//! `log show` 每次都要扫日志归档，不接受「只要最后 N 条」这种指令，
//! 而统一日志的量级又远大于 journald——一台普通 Mac 约 **250 行/秒**。因此：
//!
//! - 查询必须有时间窗口，调用方没给 `since` 时按 [`DEFAULT_WINDOW_SECS`]（5 分钟）兜底；
//! - 输出按时间**升序**，环形缓冲里只留最新的一段**原始行**（[`TailBuffer`]），
//!   读完再解析那几十条。内存不随窗口增长，也不会为了丢弃而解析七十多万行；
//! - 默认不加 `--info` / `--debug`，与 journald 默认只显示 notice 及以上同理。
//!   调用方显式要求 `priority >= Info` 时才打开，避免平时白扫几十倍的数据量。
//!
//! 收窄查询最有效的手段是 `unit` / `q`——它们变成 `--predicate` 由 `log` 在源头过滤，
//! 比拉回来再筛快一个量级（实测 5 分钟窗口：不过滤 6.2 万行，按 subsystem 过滤 437 行）。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde_json::Value;
use strixmaid_types::log::{BootInfo, LogEntry, LogEntryDetail, LogPage, LogPriority, LogQuery};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::broadcast;

use super::super::{Probe, Provider};
use super::{LogFollow, LogProvider, normalize_limit};

/// `log` 的绝对路径。不靠 `PATH`——那上面可能有同名的东西。
const LOG_BIN: &str = "/usr/bin/log";

/// 调用方没给 `since` 时的默认回看窗口。
///
/// 统一日志的量级与 journald 完全不是一回事：一台普通 Mac 约 **250 行/秒**
/// （实测 5 分钟 6.2 万行），1 小时就是七十多万行、`log show` 本身要跑十几秒。
/// 5 分钟是「够看清刚才发生了什么」与「两秒内返回」之间的折中。
///
/// 要看更长的跨度，显式给 `since`，并尽量同时给 `unit` / `q` ——
/// 它们会变成 `--predicate` 交给 `log` 在源头收窄，比拉回来再过滤快一个量级。
pub const DEFAULT_WINDOW_SECS: i64 = 300;

/// 单次查询超时。`log show` 在大窗口上确实会慢，给得比 journalctl 宽。
const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// follow 单批最大条数。
const FOLLOW_BATCH_MAX: usize = 128;
/// follow 攒批窗口。
const FOLLOW_BATCH_WINDOW: std::time::Duration = std::time::Duration::from_millis(200);
/// follow 通道容量（批次数）。
const FOLLOW_CAPACITY: usize = 256;

/// 尾部缓冲在 `limit` 之外多留的行数。
///
/// `--end` 只精确到秒，最后一秒里的行会被 [`Window::accepts`] 按游标逐条筛掉。
/// 按 250 行/秒算，4096 行相当于十几秒的余量，足够覆盖这点误差。
const TAIL_SLACK: usize = 4096;

// ---------------------------------------------------------------------------
// provider
// ---------------------------------------------------------------------------

/// 一个共享的 `log stream` 子进程。最后一个 `Arc` drop 时 abort 读任务，
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
    q: Option<String>,
}

/// `log(1)` 实现。
#[derive(Debug, Default)]
pub struct OsLog {
    follows: Mutex<HashMap<FollowKey, Weak<FollowShared>>>,
}

impl OsLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 基础命令。统一用 UTC 与 C locale，让时间戳格式稳定可解析。
    fn base_command(sub: &str) -> Command {
        let mut cmd = Command::new(LOG_BIN);
        cmd.arg(sub)
            .args(["--style", "ndjson"])
            .env("LC_ALL", "C")
            .env("TZ", "UTC")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    /// `--predicate`：两个子命令通用。
    ///
    /// NSPredicate 语法，字符串常量里的 `"` 与 `\` 必须转义，
    /// 否则用户在 `?q=` 里放一个引号就能改写谓词。
    fn apply_predicate(cmd: &mut Command, q: &LogQuery) {
        let mut preds: Vec<String> = Vec::new();
        if let Some(unit) = &q.unit {
            preds.push(format!("subsystem == {}", quote_predicate(unit)));
        }
        if let Some(needle) = &q.q {
            preds.push(format!(
                "eventMessage CONTAINS[c] {}",
                quote_predicate(needle)
            ));
        }
        if !preds.is_empty() {
            cmd.args(["--predicate", &preds.join(" AND ")]);
        }
    }

    /// 级别开关。
    ///
    /// **两个子命令的写法不一样，这是 `log(1)` 的既有事实，不是笔误**：
    ///
    /// | 子命令 | 写法 |
    /// |---|---|
    /// | `log show` | 布尔标志 `--info` / `--debug`（`--debug` 隐含 `--info`） |
    /// | `log stream` | `--level info` / `--level debug` |
    ///
    /// 给 `log show` 传 `--level` 会得到 `unrecognized option`、整条命令失败。
    /// journald 的 priority 语义是「最多这么不严重」，数值越大越啰嗦；
    /// 统一日志默认只出 Default 及以上，要看 Info / Debug 得显式打开。
    fn apply_level(cmd: &mut Command, q: &LogQuery, sub: Sub) {
        let Some(p) = q.priority.map(LogPriority::as_u8) else {
            return;
        };
        let want_debug = p >= LogPriority::Debug.as_u8();
        let want_info = p >= LogPriority::Info.as_u8();
        match (sub, want_debug, want_info) {
            (Sub::Show, true, _) => {
                cmd.arg("--debug");
            }
            (Sub::Show, false, true) => {
                cmd.arg("--info");
            }
            (Sub::Stream, true, _) => {
                cmd.args(["--level", "debug"]);
            }
            (Sub::Stream, false, true) => {
                cmd.args(["--level", "info"]);
            }
            _ => {}
        }
    }

    /// 跑一次 `log show` 并取**最新的 `limit` 条**。
    ///
    /// # 为什么先攒原始行、最后才解析
    ///
    /// 统一日志在一台普通 Mac 上约 250 行/秒——1 小时的窗口就是七十多万行。
    /// 逐行 `serde_json::from_str` 再丢掉其中 99.99% 是纯粹的浪费，
    /// 实测足以把整条查询拖过超时。因此环形缓冲里放的是**原始行**，
    /// 读完之后只解析留下的那几十条。
    ///
    /// 缓冲多留 [`TAIL_SLACK`] 行：`--end` 只精确到秒，最后一秒里的行可能被
    /// [`Window::accepts`] 按游标筛掉，没有余量就会少给几条。
    async fn show(&self, q: &LogQuery, window: &Window, limit: usize) -> ApiResult<Vec<LogEntry>> {
        let mut cmd = Self::base_command("show");
        cmd.args(["--start", &window.start_arg(), "--end", &window.end_arg()]);
        Self::apply_level(&mut cmd, q, Sub::Show);
        Self::apply_predicate(&mut cmd, q);

        let mut child = cmd.spawn().map_err(spawn_error)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ApiError::internal("log show 没有 stdout"))?;

        // stderr 必须**并发**抽干。留着不读，它写满一个管道缓冲（64 KiB）之后
        // 子进程就会阻塞在 write 上，stdout 再也不出新行——这正是经典的管道死锁，
        // 表现为查询一直挂到超时。
        let stderr = child.stderr.take();
        let stderr_task = tokio::spawn(async move {
            let mut buf = String::new();
            if let Some(mut s) = stderr {
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut s, &mut buf).await;
            }
            buf
        });

        let mut tail: TailBuffer<String> = TailBuffer::new(limit + TAIL_SLACK);
        let mut lines = BufReader::new(stdout).lines();
        loop {
            // 读失败绝不能静默当成「读完了」——那会把一次失败的查询报成
            // 「这段时间没有日志」，是最糟糕的一种谎报。
            match lines.next_line().await {
                Ok(Some(line)) => tail.push(line),
                Ok(None) => break,
                Err(e) => {
                    return Err(ApiError::internal("读取 log show 输出失败")
                        .with_detail(e.to_string()));
                }
            }
        }

        let status = child.wait().await.map_err(spawn_error)?;
        let stderr_text = stderr_task.await.unwrap_or_default();
        if !status.success() {
            return Err(map_log_error(&stderr_text));
        }

        // 缓冲里是最新的一段原始行。全部解析、筛选，**再按游标键排序**取前 limit 条。
        //
        // 必须显式排序，不能默认「`log show` 的输出顺序就是游标顺序」：同一微秒内
        // 它的排列与我们按哈希定的全序不一定一致。翻页的下界是「上一页最后一条的游标」，
        // 若取的那 limit 条不是按游标序的前 limit 条，边界处就会漏掉几条。
        // 这里参与排序的只有 limit + TAIL_SLACK 条，代价可忽略。
        let raw = tail.into_vec();
        let mut out: Vec<LogEntry> = raw
            .iter()
            .filter_map(|line| parse_line(line))
            .filter(|e| window.accepts(e))
            .collect();
        out.sort_unstable_by(|a, b| CursorKey::of(b).cmp(&CursorKey::of(a)));
        // 同一微秒里逐字节相同的记录会得到同一个游标。`log show` 偶尔真的会把
        // 同一条事件吐两次（实测数千次查询里出现过一次），而**游标唯一是翻页正确的
        // 前提**——重复游标会让下一页的 `< 游标` 边界把它的孪生兄弟一起漏掉。
        // 这种记录彼此无从区分，去重不丢信息。已排序，相邻去重即可。
        out.dedup_by(|a, b| a.cursor == b.cursor);
        out.truncate(limit);
        Ok(out)
    }
}

/// `log` 的子命令。级别开关的写法随它变，见 [`OsLog::apply_level`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    Show,
    Stream,
}

#[async_trait]
impl Provider for OsLog {
    fn id(&self) -> &'static str {
        // 与 Linux 侧的 "journald" 平级：这是日志后端的名字，不是「日志能力」本身。
        "oslog"
    }

    async fn probe(&self) -> Probe {
        let mut cmd = Self::base_command("show");
        cmd.args(["--last", "1m"]);
        match cmd.output().await {
            Ok(out) if out.status.success() => Probe::Available,
            Ok(out) => Probe::unavailable(format!(
                "log show 失败：{}",
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Err(e) => Probe::unavailable(format!("无法执行 {LOG_BIN}：{e}")),
        }
    }
}

#[async_trait]
impl LogProvider for OsLog {
    async fn query(&self, q: &LogQuery) -> ApiResult<LogPage> {
        let limit = normalize_limit(q.limit)?;
        let window = Window::from_query(q)?;
        let fut = self.show(q, &window, limit);
        let entries = tokio::time::timeout(QUERY_TIMEOUT, fut)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Timeout, "log show 超时"))??;

        // entries 已是由新到旧；下一页从最旧的那条继续往前翻。
        let next_cursor = (entries.len() == limit)
            .then(|| entries.last().map(|e| e.cursor.clone()))
            .flatten();
        let prev_cursor = entries.first().map(|e| e.cursor.clone());

        Ok(LogPage {
            entries,
            next_cursor,
            prev_cursor,
        })
    }

    async fn entry(&self, cursor: &str) -> ApiResult<LogEntryDetail> {
        let key = CursorKey::parse(cursor)
            .ok_or_else(|| ApiError::invalid_request(format!("游标格式不正确：{cursor}")))?;

        // 在游标时刻前后各一秒的窗口里找。统一日志没有「按 ID 取一条」的接口，
        // 只能用时间窗口逼近，再按游标串精确匹配。
        let window = Window {
            start_us: key.micros - 1_000_000,
            end_us: key.micros + 1_000_000,
            before: None,
        };
        let q = LogQuery {
            // 单条详情要能看到任何级别的日志，这里必须打开 debug
            priority: Some(LogPriority::Debug),
            ..Default::default()
        };
        let fut = self.show(&q, &window, 4096);
        let entries = tokio::time::timeout(QUERY_TIMEOUT, fut)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Timeout, "log show 超时"))??;

        let entry = entries
            .into_iter()
            .find(|e| e.cursor == cursor)
            .ok_or_else(|| ApiError::not_found(format!("游标 {cursor} 对应的日志不存在")))?;

        // 统一日志的原始 JSON 字段远多于 LogEntry；再查一次太贵，
        // 这里只把 LogEntry 已有的东西摊平。fields 的契约是「全字段详情」，
        // 在 macOS 上退化成「结构化字段」，不影响前端展示。
        let mut fields = BTreeMap::new();
        fields.insert("MESSAGE".to_owned(), entry.message.clone());
        fields.insert("PRIORITY".to_owned(), entry.priority.as_u8().to_string());
        if let Some(v) = &entry.unit {
            fields.insert("SUBSYSTEM".to_owned(), v.clone());
        }
        if let Some(v) = &entry.identifier {
            fields.insert("PROCESS".to_owned(), v.clone());
        }
        if let Some(v) = entry.pid {
            fields.insert("PID".to_owned(), v.to_string());
        }
        if let Some(v) = entry.uid {
            fields.insert("UID".to_owned(), v.to_string());
        }
        if let Some(v) = &entry.boot_id {
            fields.insert("BOOT_UUID".to_owned(), v.clone());
        }
        if let Some(v) = &entry.transport {
            fields.insert("EVENT_TYPE".to_owned(), v.clone());
        }
        Ok(LogEntryDetail { entry, fields })
    }

    async fn boots(&self) -> ApiResult<Vec<BootInfo>> {
        // 统一日志的归档里确实保留着历史 boot 的日志，但 `log` 没有
        // `journalctl --list-boots` 那样的枚举接口：要拿到历史 bootUUID 只能
        // 全量扫一遍归档去 distinct，代价与收益完全不成比例。
        // 因此只报当前这一次启动，index = 0，与 journald 里「0 表示本次启动」一致。
        let boot_id = crate::platform::macos::sysctl_str("kern.bootsessionuuid")
            .ok_or_else(|| ApiError::internal("读不到 kern.bootsessionuuid"))?;
        let first_ts = crate::platform::macos::sysctl_scalar::<libc::timeval>("kern.boottime")
            .map(|tv| tv.tv_sec)
            .unwrap_or(0);
        Ok(vec![BootInfo {
            index: 0,
            boot_id,
            first_ts,
            last_ts: now_unix(),
        }])
    }

    async fn follow(&self, q: &LogQuery) -> ApiResult<LogFollow> {
        let key = FollowKey {
            priority: q.priority.map(LogPriority::as_u8),
            unit: q.unit.clone(),
            q: q.q.clone(),
        };

        let mut map = self.follows.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, w| w.strong_count() > 0);
        if let Some(shared) = map.get(&key).and_then(Weak::upgrade) {
            return Ok(LogFollow::new(shared.tx.subscribe(), Box::new(shared)));
        }

        let mut cmd = Self::base_command("stream");
        Self::apply_level(&mut cmd, q, Sub::Stream);
        Self::apply_predicate(&mut cmd, q);
        let mut child = cmd.spawn().map_err(spawn_error)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ApiError::internal("log stream 没有 stdout"))?;

        let (tx, rx) = broadcast::channel(FOLLOW_CAPACITY);
        let task = tokio::spawn(follow_reader(child, stdout, tx.clone()));
        let shared = Arc::new(FollowShared { tx, task });
        map.insert(key, Arc::downgrade(&shared));
        tracing::debug!(filter = ?q, "log stream 已启动");
        Ok(LogFollow::new(rx, Box::new(shared)))
    }
}

/// follow 读任务：逐行读、小窗口攒批、广播。
///
/// `child` 由本任务持有，任务被 abort 时随之 drop → `kill_on_drop` 生效。
async fn follow_reader(
    child: Child,
    stdout: ChildStdout,
    tx: broadcast::Sender<Arc<Vec<LogEntry>>>,
) {
    let _child = child;
    let mut lines = BufReader::new(stdout).lines();
    'outer: loop {
        let mut batch = Vec::new();
        match lines.next_line().await {
            Ok(Some(line)) => batch.extend(parse_line(&line)),
            _ => break,
        }
        // 第一条到手后，在小窗口内把紧随其后的行一起带上，减少 WS 帧数。
        let window = tokio::time::sleep(FOLLOW_BATCH_WINDOW);
        tokio::pin!(window);
        while batch.len() < FOLLOW_BATCH_MAX {
            tokio::select! {
                l = lines.next_line() => match l {
                    Ok(Some(line)) => batch.extend(parse_line(&line)),
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
    tracing::debug!("log stream 结束");
}

// ---------------------------------------------------------------------------
// 时间窗口与游标
// ---------------------------------------------------------------------------

/// 一次查询的时间窗口（unix 微秒）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub start_us: i64,
    pub end_us: i64,
    /// 游标：只要严格早于它的条目。`None` 表示不按游标裁剪。
    pub before: Option<CursorKey>,
}

impl Window {
    /// 由查询参数推出窗口。
    ///
    /// `cursor` 存在时，窗口右端收到游标时刻——翻页就是「再往前看一段」。
    pub fn from_query(q: &LogQuery) -> ApiResult<Window> {
        let before = match &q.cursor {
            Some(c) => Some(
                CursorKey::parse(c)
                    .ok_or_else(|| ApiError::invalid_request(format!("游标格式不正确：{c}")))?,
            ),
            None => None,
        };
        let end_us = match (&before, q.until) {
            (Some(k), _) => k.micros,
            (None, Some(until)) => until.saturating_mul(1_000_000),
            (None, None) => now_unix().saturating_mul(1_000_000),
        };
        let start_us = match q.since {
            Some(since) => since.saturating_mul(1_000_000),
            None => end_us - DEFAULT_WINDOW_SECS * 1_000_000,
        };
        if start_us >= end_us {
            return Err(ApiError::invalid_request("since 必须早于 until"));
        }
        Ok(Window {
            start_us,
            end_us,
            before,
        })
    }

    /// `--start` 参数。`log` 只接受到秒的时间串，向下取整不会漏掉边界上的条目。
    pub fn start_arg(&self) -> String {
        format_utc(self.start_us.div_euclid(1_000_000))
    }

    /// `--end` 参数。向上取整同理，多出来的那不到一秒由 [`Self::accepts`] 裁掉。
    pub fn end_arg(&self) -> String {
        format_utc(self.end_us.div_euclid(1_000_000) + 1)
    }

    /// 精确边界判定，补上 `--start` / `--end` 只到秒级造成的误差。
    pub fn accepts(&self, e: &LogEntry) -> bool {
        let us = entry_micros(e);
        if us < self.start_us || us > self.end_us {
            return false;
        }
        match &self.before {
            // 游标本身那条不能重复出现在下一页
            Some(k) => CursorKey::of(e) < *k,
            None => true,
        }
    }
}

/// 游标的两个组成部分。字段顺序即比较顺序，构成日志的全序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CursorKey {
    pub micros: i64,
    /// 整行的 FNV-1a 64 位哈希，用于在同一微秒内区分条目。见模块文档。
    pub hash: u64,
}

impl CursorKey {
    /// 从条目取键。
    pub fn of(e: &LogEntry) -> CursorKey {
        CursorKey::parse(&e.cursor).unwrap_or(CursorKey {
            micros: entry_micros(e),
            hash: 0,
        })
    }

    /// 解析 `<micros>:<hash>`。
    pub fn parse(s: &str) -> Option<CursorKey> {
        let (micros, hash) = s.split_once(':')?;
        Some(CursorKey {
            micros: micros.parse().ok()?,
            hash: hash.parse().ok()?,
        })
    }

    /// 渲染成游标串。
    pub fn render(&self) -> String {
        format!("{}:{}", self.micros, self.hash)
    }
}

/// FNV-1a 64 位哈希。
///
/// 选它是因为实现只有三行、无依赖、结果与平台无关——游标要能跨进程复现，
/// 不能用 `DefaultHasher`（其种子与版本都不保证稳定）。
/// 这里不是密码学用途，只需要「不同的行几乎必然得到不同的值」。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 条目的 unix 微秒。
fn entry_micros(e: &LogEntry) -> i64 {
    e.ts.saturating_mul(1_000_000) + i64::from(e.us)
}

/// 只保留最新 `cap` 项的环形缓冲。输入必须按时间升序。
///
/// 泛型是为了装**原始行**而非解析好的条目——解析七十多万行再丢掉绝大多数
/// 是查询最大的一笔浪费，见 [`OsLog::show`]。
#[derive(Debug)]
pub struct TailBuffer<T> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T> TailBuffer<T> {
    pub fn new(cap: usize) -> Self {
        TailBuffer {
            buf: VecDeque::with_capacity(cap.min(1024)),
            cap: cap.max(1),
        }
    }

    pub fn push(&mut self, item: T) {
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(item);
    }

    /// 取出内容，仍为升序。
    pub fn into_vec(self) -> Vec<T> {
        self.buf.into()
    }
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

/// 解析一行 ndjson。解析不出来的行（`log` 偶尔会插入非 JSON 的提示行）跳过。
///
/// 游标里的哈希取自**去掉首尾空白后的整行**，与这里传给 `serde_json` 的是同一份字节，
/// 因此同一条日志在任何一次查询里都得到同一个游标。
pub fn parse_line(line: &str) -> Option<LogEntry> {
    let trimmed = line.trim();
    let v: Value = serde_json::from_str(trimmed).ok()?;
    entry_from_json(&v, trimmed)
}

/// 把统一日志的一条 JSON 转成 [`LogEntry`]。
///
/// `raw` 是这条 JSON 的原始文本，仅用于算游标里的哈希。
pub fn entry_from_json(v: &Value, raw: &str) -> Option<LogEntry> {
    let (ts, us) = parse_timestamp(v.get("timestamp")?.as_str()?)?;
    let str_field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .filter(|s| !s.is_empty())
    };

    Some(LogEntry {
        cursor: CursorKey {
            micros: ts.saturating_mul(1_000_000) + i64::from(us),
            hash: fnv1a64(raw.as_bytes()),
        }
        .render(),
        ts,
        us,
        priority: priority_from_message_type(
            v.get("messageType").and_then(Value::as_str).unwrap_or(""),
        ),
        message: str_field("eventMessage").unwrap_or_default(),
        unit: str_field("subsystem"),
        identifier: str_field("processImagePath")
            .as_deref()
            .and_then(basename)
            .map(str::to_owned),
        pid: v
            .get("processID")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        uid: v
            .get("userID")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok()),
        // 统一日志不记录主机名
        hostname: None,
        boot_id: str_field("bootUUID"),
        transport: str_field("eventType"),
    })
}

/// `messageType` → [`LogPriority`]。
///
/// 统一日志只有五档，syslog 有八档，映射取语义最近的：
/// `Fault` 是「子系统级故障」，比单次 `Error` 严重，对应 `Critical`。
/// 未知取值按 `Notice` 处理——宁可让它出现在默认视图里被看见，也不要静默降级成 debug。
pub fn priority_from_message_type(t: &str) -> LogPriority {
    match t {
        "Fault" => LogPriority::Crit,
        "Error" => LogPriority::Err,
        "Default" => LogPriority::Notice,
        "Info" => LogPriority::Info,
        "Debug" => LogPriority::Debug,
        _ => LogPriority::Notice,
    }
}

/// 路径的最后一段。
fn basename(path: &str) -> Option<&str> {
    path.rsplit('/').next().filter(|s| !s.is_empty())
}

/// 解析统一日志的时间戳：`2026-08-27 22:19:30.810528+0800`。
///
/// 返回 `(unix 秒, 微秒余数)`。不引 chrono——格式是固定的，
/// 且这是整个 crate 里唯一需要解析日期的地方。
pub fn parse_timestamp(s: &str) -> Option<(i64, u32)> {
    let (date, rest) = s.split_once(' ')?;
    let mut d = date.split('-');
    let (y, mo, da) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<u32>().ok()?,
        d.next()?.parse::<u32>().ok()?,
    );

    // 时间部分形如 22:19:30.810528+0800 / 22:19:30.810528Z / 22:19:30+08:00
    let (time_frac, offset_secs) = split_offset(rest)?;
    let (hms, frac) = match time_frac.split_once('.') {
        Some((h, f)) => (h, f),
        None => (time_frac, ""),
    };
    let mut t = hms.split(':');
    let (h, mi, se) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
    );
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..=60).contains(&se) {
        return None;
    }

    // 小数部分补齐 / 截断到 6 位
    let mut micros = 0u32;
    for (i, c) in frac.chars().take(6).enumerate() {
        let digit = c.to_digit(10)?;
        micros += digit * 10u32.pow(5 - i as u32);
    }

    let days = days_from_civil(y, mo, da)?;
    let secs = days * 86400 + h * 3600 + mi * 60 + se - offset_secs;
    Some((secs, micros))
}

/// 从时间串尾部切出 UTC 偏移（秒）。支持 `Z`、`+0800`、`+08:00`。
fn split_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(head) = s.strip_suffix('Z') {
        return Some((head, 0));
    }
    // 从后往前找符号，跳过时间本身的冒号
    let idx = s.rfind(['+', '-'])?;
    let (head, tail) = s.split_at(idx);
    let sign = if tail.starts_with('-') { -1 } else { 1 };
    let digits: String = tail[1..].chars().filter(char::is_ascii_digit).collect();
    if digits.len() != 4 {
        return None;
    }
    let hh: i64 = digits[..2].parse().ok()?;
    let mm: i64 = digits[2..].parse().ok()?;
    Some((head, sign * (hh * 3600 + mm * 60)))
}

/// 民用日期 → 自 1970-01-01 起的天数（Howard Hinnant 的 `days_from_civil`）。
///
/// 对 1970 之前的日期也正确（返回负数）。月 / 日越界时返回 `None`。
pub fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = i64::from(m);
    let d = i64::from(d);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

/// unix 秒 → `log` 认的时间串 `YYYY-MM-DD HH:MM:SS`（UTC）。
///
/// 子进程的 `TZ=UTC` 保证 `log` 按 UTC 解释它。
pub fn format_utc(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// [`days_from_civil`] 的逆运算。
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// NSPredicate 字符串常量的转义。
fn quote_predicate(s: &str) -> String {
    let escaped: String = s
        .chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            other => vec![other],
        })
        .collect();
    format!("\"{escaped}\"")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn spawn_error(e: std::io::Error) -> ApiError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ApiError::capability_unavailable("oslog", format!("找不到 {LOG_BIN}"))
    } else {
        ApiError::new(ErrorCode::Unavailable, "无法执行 log").with_detail(e.to_string())
    }
}

/// `log` 的 stderr → API 错误。
fn map_log_error(stderr: &str) -> ApiError {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not permitted") || lower.contains("permission") {
        return ApiError::permission_denied("系统拒绝读取统一日志")
            .with_detail(stderr.trim())
            .retry_elevated();
    }
    if lower.contains("invalid predicate") || lower.contains("unable to parse") {
        return ApiError::invalid_request("过滤条件无法被 log 解析").with_detail(stderr.trim());
    }
    ApiError::internal("log show 执行失败").with_detail(stderr.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析时间戳() {
        // 2026-08-27 22:19:30.810528+0800 = 2026-08-27 14:19:30.810528 UTC
        let (ts, us) = parse_timestamp("2026-08-27 22:19:30.810528+0800").unwrap();
        assert_eq!(us, 810_528);
        assert_eq!(format_utc(ts), "2026-08-27 14:19:30");

        // Z 与带冒号的偏移
        let (a, _) = parse_timestamp("2026-08-27 14:19:30.000000Z").unwrap();
        let (b, _) = parse_timestamp("2026-08-27 22:19:30.000000+08:00").unwrap();
        assert_eq!(a, b);
        assert_eq!(a, ts);

        // 负偏移
        let (c, _) = parse_timestamp("2026-08-27 09:19:30.000000-0500").unwrap();
        assert_eq!(c, ts);

        // 小数位数不是 6 位
        assert_eq!(
            parse_timestamp("2026-01-01 00:00:00.5+0000").unwrap().1,
            500_000
        );
        assert_eq!(parse_timestamp("2026-01-01 00:00:00+0000").unwrap().1, 0);

        // 坏输入
        assert!(parse_timestamp("").is_none());
        assert!(parse_timestamp("not a timestamp").is_none());
        assert!(
            parse_timestamp("2026-08-27 25:00:00+0000").is_none(),
            "小时越界"
        );
        assert!(
            parse_timestamp("2026-13-01 00:00:00+0000").is_none(),
            "月份越界"
        );
    }

    #[test]
    fn 日期与天数互为逆运算() {
        for (y, m, d) in [
            (1970, 1, 1),
            (2000, 2, 29),
            (2026, 8, 27),
            (2100, 3, 1),
            (1969, 12, 31),
        ] {
            let days = days_from_civil(y, m, d).unwrap();
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(1970, 1, 2), Some(1));
        assert_eq!(days_from_civil(1969, 12, 31), Some(-1));
    }

    #[test]
    fn 优先级映射() {
        assert_eq!(priority_from_message_type("Fault"), LogPriority::Crit);
        assert_eq!(priority_from_message_type("Error"), LogPriority::Err);
        assert_eq!(priority_from_message_type("Default"), LogPriority::Notice);
        assert_eq!(priority_from_message_type("Info"), LogPriority::Info);
        assert_eq!(priority_from_message_type("Debug"), LogPriority::Debug);
        assert_eq!(
            priority_from_message_type("SomethingNew"),
            LogPriority::Notice,
            "未知取值不能被静默降级成 debug 而看不见"
        );
    }

    #[test]
    fn 解析一行真实输出() {
        let line = r#"{"messageType":"Default","eventType":"logEvent","userID":24,"subsystem":"com.apple.symptomsd","threadID":3927377,"bootUUID":"D06FC2F9-7873-451F-910A-C09816584969","processImagePath":"\/usr\/libexec\/symptomsd","timestamp":"2026-08-27 22:19:30.831980+0800","eventMessage":"TCP metrics iteration:4","traceID":8198813747412992004,"processID":444}"#;
        let e = parse_line(line).unwrap();
        assert_eq!(e.message, "TCP metrics iteration:4");
        assert_eq!(e.priority, LogPriority::Notice);
        assert_eq!(e.unit.as_deref(), Some("com.apple.symptomsd"));
        assert_eq!(e.identifier.as_deref(), Some("symptomsd"), "取 basename");
        assert_eq!(e.pid, Some(444));
        assert_eq!(e.uid, Some(24));
        assert_eq!(e.transport.as_deref(), Some("logEvent"));
        assert_eq!(e.hostname, None, "统一日志不记录主机名");
        assert_eq!(e.us, 831_980);
        assert_eq!(
            e.cursor,
            format!("{}:{}", entry_micros(&e), fnv1a64(line.trim().as_bytes())),
            "游标 = 微秒:整行哈希"
        );
        // 同一行两次解析必须得到同一个游标，否则翻页会错位
        assert_eq!(parse_line(line).unwrap().cursor, e.cursor);
        assert!(CursorKey::parse(&e.cursor).is_some());
    }

    #[test]
    fn 跳过解析不出来的行() {
        assert!(parse_line("").is_none());
        assert!(parse_line("Filtering the log data using ...").is_none());
        assert!(parse_line(r#"{"no":"timestamp"}"#).is_none());
    }

    #[test]
    fn 游标排序即时间顺序() {
        let a = CursorKey {
            micros: 100,
            hash: 5,
        };
        let b = CursorKey {
            micros: 100,
            hash: 9,
        };
        let c = CursorKey {
            micros: 101,
            hash: 1,
        };
        assert!(a < b && b < c);
        assert_eq!(CursorKey::parse(&a.render()), Some(a));
        assert_eq!(CursorKey::parse("garbage"), None);
        assert_eq!(CursorKey::parse("12:notanumber"), None);
    }

    #[test]
    fn 窗口推导() {
        // 只给 since/until
        let w = Window::from_query(&LogQuery {
            since: Some(1_000),
            until: Some(2_000),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(w.start_us, 1_000_000_000);
        assert_eq!(w.end_us, 2_000_000_000);
        assert_eq!(w.start_arg(), format_utc(1_000));
        assert_eq!(w.end_arg(), format_utc(2_001), "end 向上取整一秒");

        // 什么都不给：默认窗口
        let w = Window::from_query(&LogQuery::default()).unwrap();
        assert_eq!(w.end_us - w.start_us, DEFAULT_WINDOW_SECS * 1_000_000);

        // 游标覆盖 until
        let w = Window::from_query(&LogQuery {
            cursor: Some("1500000000:7".into()),
            until: Some(9_999),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(w.end_us, 1_500_000_000, "游标优先于 until");
        assert_eq!(
            w.before,
            Some(CursorKey {
                micros: 1_500_000_000,
                hash: 7
            })
        );

        // 非法区间与坏游标
        assert!(
            Window::from_query(&LogQuery {
                since: Some(2_000),
                until: Some(1_000),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            Window::from_query(&LogQuery {
                cursor: Some("bad".into()),
                ..Default::default()
            })
            .is_err()
        );
    }

    fn entry_at(micros: i64, hash: u64) -> LogEntry {
        LogEntry {
            cursor: CursorKey { micros, hash }.render(),
            ts: micros.div_euclid(1_000_000),
            us: micros.rem_euclid(1_000_000) as u32,
            priority: LogPriority::Notice,
            message: String::new(),
            unit: None,
            identifier: None,
            pid: None,
            uid: None,
            hostname: None,
            boot_id: None,
            transport: None,
        }
    }

    #[test]
    fn 窗口边界裁剪() {
        let w = Window {
            start_us: 1_000,
            end_us: 2_000,
            before: Some(CursorKey {
                micros: 1_500,
                hash: 10,
            }),
        };
        assert!(w.accepts(&entry_at(1_400, 1)));
        assert!(w.accepts(&entry_at(1_500, 9)), "同一微秒、哈希更小即更旧");
        assert!(!w.accepts(&entry_at(1_500, 10)), "游标那条本身不能重复出现");
        assert!(!w.accepts(&entry_at(1_500, 11)));
        assert!(!w.accepts(&entry_at(999, 1)), "早于 start");
        assert!(!w.accepts(&entry_at(2_001, 1)), "晚于 end");
    }

    #[test]
    fn 尾部缓冲只留最新的() {
        let mut t = TailBuffer::new(3);
        for i in 1..=10 {
            t.push(entry_at(i * 100, i as u64));
        }
        let out = t.into_vec();
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].cursor,
            CursorKey {
                micros: 800,
                hash: 8
            }
            .render()
        );
        assert_eq!(
            out[2].cursor,
            CursorKey {
                micros: 1000,
                hash: 10
            }
            .render()
        );
        // cap 为 0 时退化成 1，不能除零 / 空转
        let mut t = TailBuffer::new(0);
        t.push(entry_at(1, 1));
        assert_eq!(t.into_vec().len(), 1);
    }

    #[test]
    fn 谓词转义() {
        assert_eq!(quote_predicate("com.apple.x"), "\"com.apple.x\"");
        assert_eq!(
            quote_predicate(r#"a" OR 1==1 OR "b"#),
            r#""a\" OR 1==1 OR \"b""#,
            "引号必须转义，否则用户能改写谓词"
        );
        assert_eq!(quote_predicate(r"back\slash"), r#""back\\slash""#);
    }

    #[test]
    fn 错误映射() {
        assert_eq!(
            map_log_error("log: Operation not permitted").code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            map_log_error("Invalid predicate: xyz").code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(map_log_error("something else").code, ErrorCode::Internal);
    }

    #[tokio::test]
    async fn 本机探测与查询() {
        let p = OsLog::new();
        assert_eq!(p.id(), "oslog");
        if !p.probe().await.is_available() {
            eprintln!("本机 log show 不可用，跳过");
            return;
        }

        let page = p
            .query(&LogQuery {
                limit: Some(20),
                since: Some(now_unix() - 120),
                ..Default::default()
            })
            .await
            .unwrap();
        // 统一日志约 250 行/秒，两分钟窗口不可能是空的。真空了说明读取路径
        // 出了问题却被当成「没有日志」——那正是本实现最该防住的谎报。
        assert!(!page.entries.is_empty(), "最近两分钟不可能没有日志");
        assert!(page.entries.len() <= 20);
        // 由新到旧
        for w in page.entries.windows(2) {
            assert!(
                CursorKey::of(&w[0]) > CursorKey::of(&w[1]),
                "结果必须严格由新到旧"
            );
        }
        eprintln!(
            "最近一条：{} {}",
            page.entries[0].identifier.as_deref().unwrap_or("?"),
            page.entries[0].message.chars().take(60).collect::<String>()
        );
    }

    #[tokio::test]
    async fn 本机翻页不重不漏() {
        let p = OsLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let since = now_unix() - 120;
        let first = p
            .query(&LogQuery {
                limit: Some(10),
                since: Some(since),
                ..Default::default()
            })
            .await
            .unwrap();
        let Some(cursor) = first.next_cursor.clone() else {
            eprintln!("本机日志不足 10 条，跳过翻页断言");
            return;
        };
        let second = p
            .query(&LogQuery {
                limit: Some(10),
                since: Some(since),
                cursor: Some(cursor),
                ..Default::default()
            })
            .await
            .unwrap();

        let ids: std::collections::HashSet<&str> =
            first.entries.iter().map(|e| e.cursor.as_str()).collect();
        for e in &second.entries {
            assert!(!ids.contains(e.cursor.as_str()), "第二页出现了第一页的条目");
        }
        if let (Some(last_of_first), Some(first_of_second)) =
            (first.entries.last(), second.entries.first())
        {
            assert!(
                CursorKey::of(last_of_first) > CursorKey::of(first_of_second),
                "第二页必须严格更旧"
            );
        }
    }

    /// 每个级别都真的把命令跑一遍。
    ///
    /// 这条用例是补票：`log show` 不认 `--level`（那是 `log stream` 的写法），
    /// 之前把两者写成了同一套，于是任何 `priority >= info` 的查询——包括
    /// `entry()` 内部固定用的 Debug——都会直接失败，而单测全都没覆盖到。
    /// **内容不同的记录必须得到不同的游标。**
    ///
    /// 这是游标唯一性的根不变量，直接测 [`parse_line`] 而**不经过 [`OsLog::show`]**
    /// ——后者会按游标去重，反而把「两条不同的日志撞成同一个游标」这件事盖住，
    /// 让测试看到一份永远唯一的结果。早先的用例正是栽在这里：把游标退回
    /// `traceID` 的变异跑下来居然全绿。
    ///
    /// 背景：traceID 在同一 activity 下的所有事件间共享，实测两分钟日志里
    /// `(timestamp, traceID)` 有 57 组重复。用它做游标，翻页时同键的条目会被
    /// 整组丢掉——静默漏日志。
    #[tokio::test]
    async fn 内容不同的日志必须得到不同的游标() {
        let out = tokio::process::Command::new(LOG_BIN)
            .args(["show", "--style", "ndjson", "--last", "2m"])
            .env("TZ", "UTC")
            .env("LC_ALL", "C")
            .output()
            .await;
        let Ok(out) = out else {
            eprintln!("本机 log 不可用，跳过");
            return;
        };
        if !out.status.success() {
            eprintln!("log show 失败，跳过");
            return;
        }
        let text = String::from_utf8_lossy(&out.stdout);

        // 行 → 游标。同一条原始行出现多次时只算一次：那种记录彼此无从区分，
        // 共用一个游标是本实现的既定行为（show() 会去重）。
        let mut by_cursor: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut checked = 0usize;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Some(entry) = parse_line(line) else { continue };
            checked += 1;
            if let Some(prev) = by_cursor.insert(entry.cursor.clone(), line.to_owned())
                && prev != line
            {
                panic!(
                    "两条内容不同的日志撞到同一个游标 {}：\n  A: {}\n  B: {}",
                    entry.cursor,
                    prev.chars().take(160).collect::<String>(),
                    line.chars().take(160).collect::<String>()
                );
            }
        }
        assert!(checked > 1000, "样本只有 {checked} 条，说明不了问题");
        eprintln!("{checked} 条日志，内容不同者游标互不相同");
    }

    /// `show()` 的输出必须唯一且严格递减（去重 + 排序都生效）。
    #[tokio::test]
    async fn 查询结果的游标唯一且严格有序() {
        let p = OsLog::new();
        if !p.probe().await.is_available() {
            eprintln!("本机 log show 不可用，跳过");
            return;
        }
        let page = p
            .query(&LogQuery {
                limit: Some(1000),
                since: Some(now_unix() - 120),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.entries.len() > 100, "样本太小，说明不了问题");

        let mut seen = std::collections::HashSet::new();
        for e in &page.entries {
            assert!(seen.insert(e.cursor.clone()), "游标重复：{}", e.cursor);
        }
        for w in page.entries.windows(2) {
            assert!(
                CursorKey::of(&w[0]) > CursorKey::of(&w[1]),
                "必须严格递减：{} 之后是 {}",
                w[0].cursor,
                w[1].cursor
            );
        }
    }

    #[test]
    fn 哈希稳定且区分相邻输入() {
        // 跨进程可复现：写死一个已知值，换实现就会失败
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert_ne!(fnv1a64(b"ab"), fnv1a64(b"ba"));
    }

    #[tokio::test]
    async fn 三个级别的查询都能跑通() {
        let p = OsLog::new();
        if !p.probe().await.is_available() {
            eprintln!("本机 log show 不可用，跳过");
            return;
        }
        for priority in [None, Some(LogPriority::Info), Some(LogPriority::Debug)] {
            let r = p
                .query(&LogQuery {
                    limit: Some(3),
                    since: Some(now_unix() - 60),
                    priority,
                    ..Default::default()
                })
                .await;
            assert!(r.is_ok(), "priority={priority:?} 查询失败：{:?}", r.err());
        }
    }

    /// 级别开关的写法必须随子命令而变，见 [`OsLog::apply_level`]。
    #[test]
    fn 级别开关按子命令取不同写法() {
        let args = |priority, sub| {
            let mut cmd = OsLog::base_command(match sub {
                Sub::Show => "show",
                Sub::Stream => "stream",
            });
            OsLog::apply_level(&mut cmd, &LogQuery { priority, ..Default::default() }, sub);
            cmd.as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };

        // show 用布尔标志，绝不能出现 --level
        let show_debug = args(Some(LogPriority::Debug), Sub::Show);
        assert!(show_debug.contains(&"--debug".to_owned()), "{show_debug:?}");
        assert!(!show_debug.contains(&"--level".to_owned()), "log show 不认 --level");
        let show_info = args(Some(LogPriority::Info), Sub::Show);
        assert!(show_info.contains(&"--info".to_owned()), "{show_info:?}");
        assert!(!show_info.contains(&"--level".to_owned()));

        // stream 用 --level
        let stream_debug = args(Some(LogPriority::Debug), Sub::Stream);
        assert!(stream_debug.windows(2).any(|w| w == ["--level", "debug"]), "{stream_debug:?}");
        let stream_info = args(Some(LogPriority::Info), Sub::Stream);
        assert!(stream_info.windows(2).any(|w| w == ["--level", "info"]), "{stream_info:?}");

        // 比 Info 更严重的级别不需要打开任何开关（默认就出 Default 及以上）
        for sub in [Sub::Show, Sub::Stream] {
            let a = args(Some(LogPriority::Err), sub);
            assert!(!a.contains(&"--info".to_owned()) && !a.contains(&"--level".to_owned()), "{a:?}");
            let none = args(None, sub);
            assert!(!none.contains(&"--info".to_owned()) && !none.contains(&"--level".to_owned()));
        }
    }

    #[tokio::test]
    async fn 本机_boots_只报当前启动() {
        let p = OsLog::new();
        let boots = p.boots().await.unwrap();
        assert_eq!(boots.len(), 1);
        assert_eq!(boots[0].index, 0);
        assert!(!boots[0].boot_id.is_empty());
        assert!(boots[0].first_ts > 0);
        assert!(boots[0].last_ts >= boots[0].first_ts);
    }
}
