//! 事件日志的**纯函数层**：游标、时间窗口、XPath 拼接、级别映射、事件 XML 解析。
//!
//! 这一层不碰任何 Win32 调用，因此可以逐条用固定输入测；带 `unsafe` 的部分全在
//! [`super::evt`] 里。划这条线的理由和 `providers/log/parse.rs` 对 journalctl 一样：
//! 日志的**正确性风险几乎全在拼字符串与解析字符串上**（XPath 注入、翻页边界、
//! 时间戳换算），把它们从 FFI 里摘出来才测得动。
//!
//! # 游标
//!
//! 照搬 macOS 侧 [`crate::providers::log::oslog`] 的设计：`<unix 微秒>:<FNV-1a 64 位哈希>`。
//! 时间戳负责排序与翻页（EvtQuery 的时间窗口只认时刻），哈希负责在同一微秒内区分条目。
//! 与 oslog 的差异只有一处：哈希的输入是**整条事件 XML**（`EvtRender` 的
//! `EvtRenderEventXml` 结果），而不是一行 ndjson。
//!
//! ## 为什么不用 `EventRecordID`
//!
//! 它看起来是天然主键，实际上不是：
//!
//! 1. **只在单个通道内唯一**。本实现把 `System` / `Application`（以及有权限时的
//!    `Security`）归并成一条流，两个通道各自的 1 号记录会撞在一起。撞了之后翻页按
//!    「严格早于游标」裁剪，同键的条目会被**整组丢掉**——静默漏日志，正是 oslog
//!    模块文档里记的那个坑。
//! 2. **通道被清空后从头开始**。`EvtClearLog` 之后新事件的 RecordID 回到 1，
//!    比清空前的游标都小，翻页会直接翻到「没有更多」。
//!
//! 整条 XML 的哈希则天然把 `<Channel>` 与 `<EventRecordID>` 都算进去了，所以在
//! 跨通道归并之后仍然唯一——这一点比 oslog 那边还稳（macOS 的统一日志确实会把
//! 同一条事件吐两次，Windows 不会）。
//!
//! # 时间窗口
//!
//! `EvtQuery` 的 XPath 必须带时间窗口，否则一条 `*` 查询会从通道最老的记录开始扫。
//! 调用方没给 `since` 时按 [`DEFAULT_WINDOW_SECS`] 兜底，取 **1 小时**——
//! 比 oslog 的 5 分钟宽一个数量级，因为两边的日志量级差着一个数量级：
//!
//! | 平台 | 实测量级 | 默认窗口 |
//! |---|---|---|
//! | macOS 统一日志 | 约 250 行/秒（5 分钟 6.2 万行） | 5 分钟 |
//! | Windows 事件日志 | 本机 1 小时窗口 `System` + `Application` 合计 **5 条** | 1 小时 |
//!
//! 事件日志是「值得记一笔的事情」而不是「程序的调试输出」，一台普通机器一天几千条，
//! 1 小时的窗口既不会拖慢查询，又不至于让默认视图空空如也。
//!
//! # XPath 的两处硬事实（都是实测出来的，不是笔误）
//!
//! 1. `EvtQuery` 在 `EvtQueryChannelPath` 下收的是**裸 XPath**，不是 XML 查询文档。
//!    因此比较运算符要写成 `>=` / `<=`，写成 XML 实体 `&gt;=` 会直接得到
//!    `ERROR_EVT_INVALID_QUERY`（「指定的查询无效」）。
//! 2. 这个 XPath 子集**不支持 `concat()`**（同样报 `ERROR_EVT_INVALID_QUERY`），
//!    而 XPath 1.0 的字符串字面量本身没有转义机制。所以含单引号的 provider 名
//!    没有任何办法安全地拼进字面量——见 [`quote_xpath_literal`]。

use std::collections::BTreeMap;

use strixmaid_types::log::{LogEntry, LogPriority, LogQuery};
use strixmaid_types::{ApiError, ApiResult};

/// 调用方没给 `since` 时的默认回看窗口，见模块文档的量级对照表。
pub const DEFAULT_WINDOW_SECS: i64 = 3600;

// ---------------------------------------------------------------------------
// 游标
// ---------------------------------------------------------------------------

/// FNV-1a 64 位哈希。
///
/// 抄自 `providers/log/oslog.rs` 的同名函数（macOS 专属编译，Windows 上取不到，
/// 只能复制这几行）。选它的理由不变：实现三行、无依赖、结果与平台和进程无关——
/// 游标要能跨请求复现，不能用 `DefaultHasher`（种子与版本都不保证稳定）。
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// 游标的两个组成部分。字段顺序即比较顺序，构成日志的全序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CursorKey {
    /// unix 微秒。
    pub micros: i64,
    /// 整条事件 XML 的 FNV-1a 64 位哈希，用于在同一微秒内区分条目。见模块文档。
    pub hash: u64,
}

impl CursorKey {
    /// 从条目取键。游标解析不出来时退回时间戳（`hash = 0`），保证排序仍然可用。
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

/// 条目的 unix 微秒。
pub fn entry_micros(e: &LogEntry) -> i64 {
    e.ts.saturating_mul(1_000_000) + i64::from(e.us)
}

// ---------------------------------------------------------------------------
// 时间窗口
// ---------------------------------------------------------------------------

/// 一次查询的时间窗口（unix 微秒）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// 左端（含）。
    pub start_us: i64,
    /// 右端（含）。
    pub end_us: i64,
    /// 游标：只要严格早于它的条目。`None` 表示不按游标裁剪。
    pub before: Option<CursorKey>,
}

impl Window {
    /// 由查询参数推出窗口。
    ///
    /// `cursor` 存在时窗口右端收到游标时刻——翻页就是「再往前看一段」；
    /// `since` 缺省时按 [`DEFAULT_WINDOW_SECS`] 兜底。
    pub fn from_query(q: &LogQuery, now: i64) -> ApiResult<Window> {
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
            (None, None) => now.saturating_mul(1_000_000),
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

    /// 把左端往后收（用于 `boot` 过滤：本次启动之前的事件不属于本次启动）。
    pub fn clamp_start(&mut self, at_least_us: i64) {
        self.start_us = self.start_us.max(at_least_us);
    }

    /// XPath 里 `@SystemTime>=` 的右值。毫秒精度**向下**取整，不会漏掉边界上的条目。
    pub fn start_arg(&self) -> String {
        format_iso8601_ms(self.start_us.div_euclid(1_000))
    }

    /// XPath 里 `@SystemTime<=` 的右值。毫秒精度**向上**取整；多出来的不到一毫秒
    /// 由 [`Self::accepts`] 裁掉。
    pub fn end_arg(&self) -> String {
        format_iso8601_ms(self.end_us.div_euclid(1_000) + 1)
    }

    /// 精确边界判定，补上 XPath 只到毫秒造成的误差，并排除游标本身那一条。
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

// ---------------------------------------------------------------------------
// 级别映射
// ---------------------------------------------------------------------------

/// 事件级别未知（发布者自定义级别，取值 >5）或缺失时的归类。
///
/// 与 oslog 同一取向：宁可让它出现在默认视图里被看见，也不要静默降级成 debug
/// 而永远看不到。
pub const UNKNOWN_LEVEL_PRIORITY: LogPriority = LogPriority::Notice;

/// 事件日志的 `Level` → syslog 优先级。
///
/// | Level | 事件查看器 | 本项目 |
/// |---|---|---|
/// | 0 | LogAlways（「信息」） | `Notice` |
/// | 1 | Critical | `Crit` |
/// | 2 | Error | `Err` |
/// | 3 | Warning | `Warning` |
/// | 4 | Information | `Info` |
/// | 5 | Verbose | `Debug` |
/// | 其它 | 发布者自定义 | [`UNKNOWN_LEVEL_PRIORITY`] |
///
/// 注意 syslog 的 `Emerg` / `Alert` 在 Windows 上**没有对应物**：事件日志最严重
/// 的一档就是 Critical。因此 `priority=emerg` / `priority=alert` 的查询必然为空，
/// 这由 [`LevelFilter::Impossible`] 如实表达，而不是偷偷放宽成 Critical。
pub fn priority_from_level(level: u32) -> LogPriority {
    match level {
        0 => LogPriority::Notice,
        1 => LogPriority::Crit,
        2 => LogPriority::Err,
        3 => LogPriority::Warning,
        4 => LogPriority::Info,
        5 => LogPriority::Debug,
        _ => UNKNOWN_LEVEL_PRIORITY,
    }
}

/// 级别下限翻译成的 XPath 条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LevelFilter {
    /// 所有级别都放行，XPath 里不加 `Level` 条件。
    All,
    /// 只放行列出的级别；`unknown` 为真时连带放行自定义级别（`Level>5`）。
    Only { levels: Vec<u32>, unknown: bool },
    /// 没有任何 Windows 级别满足这个下限，查询必然为空（`emerg` / `alert`）。
    Impossible,
}

impl LevelFilter {
    /// 由级别下限推出。下限为 `None`（不过滤）时是 [`LevelFilter::All`]。
    pub fn from_floor(floor: Option<LogPriority>) -> LevelFilter {
        let Some(floor) = floor.map(LogPriority::as_u8) else {
            return LevelFilter::All;
        };
        let levels: Vec<u32> = (0..=5)
            .filter(|l| priority_from_level(*l).as_u8() <= floor)
            .collect();
        let unknown = UNKNOWN_LEVEL_PRIORITY.as_u8() <= floor;
        if levels.len() == 6 && unknown {
            LevelFilter::All
        } else if levels.is_empty() && !unknown {
            LevelFilter::Impossible
        } else {
            LevelFilter::Only { levels, unknown }
        }
    }

    /// XPath 谓词片段，如 `(Level=1 or Level=2)`。不需要条件时返回 `None`。
    pub fn predicate(&self) -> Option<String> {
        match self {
            LevelFilter::All | LevelFilter::Impossible => None,
            LevelFilter::Only { levels, unknown } => {
                let mut parts: Vec<String> =
                    levels.iter().map(|l| format!("Level={l}")).collect();
                if *unknown {
                    // 发布者自定义级别，实测 `Level>5` 能被 wevtapi 的 XPath 子集接受
                    parts.push("Level>5".to_owned());
                }
                Some(format!("({})", parts.join(" or ")))
            }
        }
    }
}

/// journald 语义的级别下限：严重程度 >= 要求（数字 <=）。
///
/// XPath 已经在源头筛过一遍，这里是**复核**：`Level` 元素缺失的事件在 XPath 里
/// 匹配不上任何 `Level=N`，却可能被 `Level>5` 之外的路径（例如 [`LevelFilter::All`]）
/// 放进来，仍要按 [`UNKNOWN_LEVEL_PRIORITY`] 复判一次。
pub fn priority_ok(floor: Option<LogPriority>, e: &LogEntry) -> bool {
    floor.is_none_or(|p| e.priority.as_u8() <= p.as_u8())
}

// ---------------------------------------------------------------------------
// XPath 拼接
// ---------------------------------------------------------------------------

/// API 契约里的 unit 名 → 事件的 Provider 名。
///
/// service provider 那边的 unit 名带 `.service` 后缀（Windows 服务的对外形态），
/// 而事件里的 `Provider@Name` 是裸名。不剥后缀的话「服务 → 它的日志」永远查不到
/// 东西——macOS 侧 `OsLog::apply_predicate` 栽过同一个坑，这里照抄它的处理。
pub fn provider_from_unit(unit: &str) -> &str {
    unit.strip_suffix(".service").unwrap_or(unit)
}

/// XPath 1.0 字符串字面量。
///
/// **XPath 没有转义机制**：`'` 在单引号字面量里无法表示，`"` 换成双引号字面量也只是
/// 把问题挪个地方；标准做法 `concat('a', "'", 'b')` 在 wevtapi 的 XPath 子集里
/// 不被支持（实测 `ERROR_EVT_INVALID_QUERY`）。
///
/// 因此含引号的值一律返回 `None`，**不拼进 XPath**——否则用户在 `?unit=` 里放一个
/// 单引号就能改写整条查询（和 oslog 的 `quote_predicate` 是同一类问题，只是那边
/// NSPredicate 有反斜杠转义，这边没有）。调用方收到 `None` 时退回进程内过滤，
/// 结果一样正确，只是慢一点。
pub fn quote_xpath_literal(s: &str) -> Option<String> {
    if s.contains('\'') || s.contains('"') {
        return None;
    }
    Some(format!("'{s}'"))
}

/// 拼出一次查询的 XPath。
///
/// `provider` 是已经剥过 `.service` 的裸 Provider 名；含引号无法安全表达时
/// 由调用方传 `None` 并在进程内复核。
///
/// 返回 `None` 表示级别下限在 Windows 上**必然无解**（`emerg` / `alert`），
/// 调用方应当直接给空页而不是白跑一次查询。
pub fn build_xpath(window: &Window, provider: Option<&str>, floor: Option<LogPriority>) -> Option<String> {
    let level = LevelFilter::from_floor(floor);
    if level == LevelFilter::Impossible {
        return None;
    }
    let mut conds = vec![format!(
        "TimeCreated[@SystemTime>='{}' and @SystemTime<='{}']",
        window.start_arg(),
        window.end_arg()
    )];
    if let Some(p) = level.predicate() {
        conds.push(p);
    }
    if let Some(lit) = provider.and_then(quote_xpath_literal) {
        conds.push(format!("Provider[@Name={lit}]"));
    }
    Some(format!("*[System[{}]]", conds.join(" and ")))
}

// ---------------------------------------------------------------------------
// 时间戳
// ---------------------------------------------------------------------------

/// 解析事件 XML 里的 `TimeCreated@SystemTime`，形如
/// `2026-09-15T08:27:01.1169070Z`（小数是 100 纳秒，7 位）。
///
/// 返回 `(unix 秒, 微秒余数)`。小数部分**截断**到 6 位：`LogEntry` 的时间精度是
/// 微秒（DTO 的既定契约），第 7 位没地方放。截断带来的唯一后果是同一微秒内的两条
/// 事件时间戳相同，而它们由游标里的哈希区分，排序仍然稳定。
///
/// 不引 chrono：格式是 schema 定死的，且整个 crate 里只有日志需要解析日期。
pub fn parse_system_time(s: &str) -> Option<(i64, u32)> {
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, mo, da) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<u32>().ok()?,
        d.next()?.parse::<u32>().ok()?,
    );
    if d.next().is_some() {
        return None;
    }

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
    if t.next().is_some() {
        return None;
    }
    if !(0..24).contains(&h) || !(0..60).contains(&mi) || !(0..=60).contains(&se) {
        return None;
    }

    // 小数部分补齐 / 截断到 6 位
    let mut micros = 0u32;
    for (i, c) in frac.chars().take(6).enumerate() {
        micros += c.to_digit(10)? * 10u32.pow(5 - i as u32);
    }

    let days = days_from_civil(y, mo, da)?;
    Some((days * 86400 + h * 3600 + mi * 60 + se - offset_secs, micros))
}

/// 从时间串尾部切出 UTC 偏移（秒）。事件 XML 一律是 `Z`，但 `+08:00` 形式也一并认了。
fn split_offset(s: &str) -> Option<(&str, i64)> {
    if let Some(head) = s.strip_suffix('Z') {
        return Some((head, 0));
    }
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

/// unix 毫秒 → XPath 认的时刻串 `YYYY-MM-DDTHH:MM:SS.mmmZ`（UTC）。
pub fn format_iso8601_ms(millis: i64) -> String {
    let secs = millis.div_euclid(1_000);
    let ms = millis.rem_euclid(1_000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 民用日期 → 自 1970-01-01 起的天数（Howard Hinnant 的 `days_from_civil`）。
///
/// 与 oslog 里同名函数逐字相同；那份是 macOS 专属编译，Windows 上取不到。
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

// ---------------------------------------------------------------------------
// SID
// ---------------------------------------------------------------------------

/// `Security@UserID` 的 SID 串 → uid。
///
/// 口径与 [`crate::platform::windows::token`] 完全一致：取最后一段子权威（RID），
/// 并把 LocalSystem（`S-1-5-18`）映射成 0。
///
/// 这里不调 `token::rid_of_sid`：那个函数要的是 `PSID`（内存里的变长结构），而事件
/// XML 给的是字符串。为了复用它得先 `ConvertStringSidToSidW` 再 `LocalFree`——
/// 每条事件两次 advapi32 往返，换来的结果与直接取最后一段完全相同。
pub fn rid_from_sid(sid: &str) -> Option<u32> {
    if sid == crate::platform::windows::token::SID_LOCAL_SYSTEM {
        return Some(0);
    }
    let rest = sid.strip_prefix("S-")?;
    // 至少要有「修订号-权威-子权威」三段，否则不是一个能取出 RID 的 SID
    if rest.split('-').count() < 3 {
        return None;
    }
    rest.rsplit('-').next()?.parse::<u32>().ok()
}

// ---------------------------------------------------------------------------
// 事件 XML 解析
// ---------------------------------------------------------------------------

/// 一条事件 XML 里我们要的全部内容。
///
/// # 为什么解析 XML 而不是用 `EvtCreateRenderContext` 取值数组
///
/// 渲染上下文（`EvtRenderContextSystem`）确实能直接给出 `EVT_VARIANT` 数组，省一次
/// 解析。但本实现**无论如何都要拿到整条 XML**：游标里的哈希就是它的哈希（见模块
/// 文档），而且 `LogEntryDetail::fields` 要求把 `EventData` 的键值对原样带出来——
/// 值数组给不了键名。既然 XML 必取，再多渲染一次值数组就是白花一次 `EvtRender`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventXml {
    /// 记录时刻，unix 秒。
    pub ts: i64,
    /// 秒内微秒偏移。
    pub us: u32,
    /// `Level` 元素。缺失为 `None`，按 [`UNKNOWN_LEVEL_PRIORITY`] 归类。
    pub level: Option<u32>,
    /// `Provider@Name`。
    pub provider: Option<String>,
    /// `Provider@Guid`。
    pub provider_guid: Option<String>,
    /// `Channel` 元素（`System` / `Application` / …）。
    pub channel: Option<String>,
    /// `Computer` 元素。
    pub computer: Option<String>,
    /// `Execution@ProcessID`。
    pub pid: Option<u32>,
    /// `Execution@ThreadID`。
    pub tid: Option<u32>,
    /// `Security@UserID`，SID 串。
    pub user_sid: Option<String>,
    /// `EventID` 元素。
    pub event_id: Option<String>,
    /// `EventRecordID` 元素。**只在单个通道内唯一**，不做游标，见模块文档。
    pub record_id: Option<String>,
    /// `Version` / `Task` / `Opcode` / `Keywords` 四个元素，缺的不进表。
    pub misc: BTreeMap<String, String>,
    /// `EventData` / `UserData` 的键值对，按 XML 里的出现顺序。
    pub data: Vec<(String, String)>,
}

impl EventXml {
    /// 归一后的优先级。
    pub fn priority(&self) -> LogPriority {
        self.level.map_or(UNKNOWN_LEVEL_PRIORITY, priority_from_level)
    }

    /// 取不到渲染消息时的退化正文：把 `EventData` 的值按顺序拼起来。
    ///
    /// 发布者未注册、消息 DLL 缺失、事件定义被删掉时都会走到这里。拼出来的东西
    /// 没有上下文（原本是 `%1 已启动` 里的 `%1`），但比空字符串强——**至少参数还在**。
    /// 连参数都没有时给一个明确的占位，而不是让前端显示一条空白日志。
    pub fn fallback_message(&self) -> String {
        let joined = self
            .data
            .iter()
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        if !joined.is_empty() {
            return joined;
        }
        let id = self.event_id.as_deref().unwrap_or("?");
        let p = self.provider.as_deref().unwrap_or("?");
        format!("[事件 {p}/{id}：发布者未提供可读消息]")
    }
}

/// 解析一条事件 XML。`TimeCreated` 解析不出来时返回 `None`——没有时刻的条目
/// 既不能排序也不能翻页，放进结果里只会污染游标序。
pub fn parse_event_xml(xml: &str) -> Option<EventXml> {
    let system = element_content(xml, "System")?;
    let (ts, us) = parse_system_time(&tag_attr(system, "TimeCreated", "SystemTime")?)?;

    let mut misc = BTreeMap::new();
    for name in ["Version", "Task", "Opcode", "Keywords"] {
        if let Some(v) = element_content(system, name).map(xml_unescape)
            && !v.is_empty()
        {
            misc.insert(name.to_owned(), v);
        }
    }

    let mut data = Vec::new();
    if let Some(section) = element_content(xml, "EventData") {
        collect_event_data(section, &mut data);
    }
    // 新式发布者用 UserData 放自定义 schema 的负载，取其中的叶子元素
    if let Some(section) = element_content(xml, "UserData") {
        collect_leaf_elements(section, &mut data);
    }

    Some(EventXml {
        ts,
        us,
        level: element_content(system, "Level").and_then(|v| v.trim().parse().ok()),
        provider: tag_attr(system, "Provider", "Name"),
        provider_guid: tag_attr(system, "Provider", "Guid"),
        channel: element_content(system, "Channel").map(xml_unescape),
        computer: element_content(system, "Computer").map(xml_unescape),
        // ProcessID 为 0 的事件是内核 / 早期用户态写的，那不是「进程 0」而是「不知道」。
        // journald 对内核消息同样不给 `_PID`，这里保持一致：0 记作 None，不冒充。
        pid: tag_attr(system, "Execution", "ProcessID")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|p| *p != 0),
        tid: tag_attr(system, "Execution", "ThreadID").and_then(|v| v.parse::<u32>().ok()),
        user_sid: tag_attr(system, "Security", "UserID"),
        event_id: element_content(system, "EventID").map(xml_unescape),
        record_id: element_content(system, "EventRecordID").map(xml_unescape),
        misc,
        data,
    })
}

/// `EventData` 段：`<Data Name='x'>v</Data>`，也可能是没有 `Name` 的位置参数。
fn collect_event_data(section: &str, out: &mut Vec<(String, String)>) {
    let mut rest = section;
    let mut positional = 0usize;
    while let Some((tag, content, tail)) = next_element(rest, "Data") {
        let key = attr_in_tag(tag, "Name").unwrap_or_else(|| {
            positional += 1;
            format!("Data{positional}")
        });
        out.push((key, xml_unescape(content)));
        rest = tail;
    }
}

/// `UserData` 段：schema 由发布者自定，只能泛泛地把**叶子元素**收下来。
///
/// 递归深度不设限会被畸形 XML 拖住，这里只走两层（`<UserData><Foo><a>1</a></Foo></UserData>`
/// 是绝大多数发布者的形状），再深的整段作为一个值收下，不丢信息也不失控。
fn collect_leaf_elements(section: &str, out: &mut Vec<(String, String)>) {
    let mut rest = section;
    while let Some((tag, content, tail)) = next_any_element(rest) {
        let name = tag_name(tag);
        if content.contains('<') {
            let mut inner = rest;
            // 进到子层再扫一遍
            if let Some(start) = inner.find(content) {
                inner = &inner[start..start + content.len()];
                let before = out.len();
                let mut sub = inner;
                while let Some((t2, c2, tail2)) = next_any_element(sub) {
                    if !c2.contains('<') {
                        out.push((tag_name(t2).to_owned(), xml_unescape(c2)));
                    }
                    sub = tail2;
                }
                if out.len() > before {
                    rest = tail;
                    continue;
                }
            }
            out.push((name.to_owned(), xml_unescape(content)));
        } else if !content.is_empty() {
            out.push((name.to_owned(), xml_unescape(content)));
        }
        rest = tail;
    }
}

/// 元素的内容（`<name …>` 与 `</name>` 之间）。自闭合元素返回 `None`。
fn element_content<'a>(hay: &'a str, name: &str) -> Option<&'a str> {
    next_element(hay, name).map(|(_, content, _)| content)
}

/// 找下一个名为 `name` 的元素，返回 `(开标签, 内容, 该元素之后的剩余文本)`。
/// 自闭合元素的内容为空串。
fn next_element<'a>(hay: &'a str, name: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let mut from = 0usize;
    loop {
        let rel = hay[from..].find('<')?;
        let at = from + rel;
        let after = &hay[at + 1..];
        if after.starts_with(name)
            && after[name.len()..]
                .starts_with([' ', '\t', '\r', '\n', '>', '/'])
        {
            return split_element(hay, at, name);
        }
        from = at + 1;
    }
}

/// 找下一个元素，不限名字。跳过 `</…>`、`<?…?>`、`<!…>`。
fn next_any_element(hay: &str) -> Option<(&str, &str, &str)> {
    let mut from = 0usize;
    loop {
        let rel = hay[from..].find('<')?;
        let at = from + rel;
        let after = &hay[at + 1..];
        let first = after.chars().next()?;
        if first.is_alphabetic() || first == '_' {
            let name_len = after
                .find([' ', '\t', '\r', '\n', '>', '/'])
                .unwrap_or(after.len());
            return split_element(hay, at, &after[..name_len]);
        }
        from = at + 1;
    }
}

/// 从 `hay[at]` 处的 `<name` 开标签切出 `(开标签, 内容, 尾部)`。
fn split_element<'a>(hay: &'a str, at: usize, name: &str) -> Option<(&'a str, &'a str, &'a str)> {
    let gt = hay[at..].find('>')? + at;
    let tag = &hay[at..=gt];
    if tag.ends_with("/>") {
        return Some((tag, "", &hay[gt + 1..]));
    }
    let close = format!("</{name}>");
    let end = hay[gt + 1..].find(&close)? + gt + 1;
    Some((tag, &hay[gt + 1..end], &hay[end + close.len()..]))
}

/// 元素名（从开标签里切）。
fn tag_name(tag: &str) -> &str {
    let body = tag.trim_start_matches('<');
    let len = body
        .find([' ', '\t', '\r', '\n', '>', '/'])
        .unwrap_or(body.len());
    &body[..len]
}

/// 取 `<elem … attr='值' …>` 里的属性值，已做实体反转义。
fn tag_attr(hay: &str, elem: &str, attr: &str) -> Option<String> {
    let (tag, _, _) = next_element(hay, elem)?;
    attr_in_tag(tag, attr)
}

/// 在一个开标签里取属性值。
///
/// 属性名必须落在**词边界**上：`EventSourceName='x'` 里含子串 `Name=`，
/// 按子串找会把它错当成 `Name` 属性（`Provider` 标签上两者并存，实测就会取错）。
fn attr_in_tag(tag: &str, attr: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut from = 0usize;
    loop {
        let rel = tag[from..].find(attr)?;
        let at = from + rel;
        let after = at + attr.len();
        let boundary_ok = at > 0 && (bytes[at - 1] as char).is_whitespace();
        let rest = tag[after..].trim_start();
        if boundary_ok && let Some(v) = rest.strip_prefix('=') {
            let v = v.trim_start();
            let quote = v.chars().next()?;
            if quote == '\'' || quote == '"' {
                let end = v[1..].find(quote)? + 1;
                return Some(xml_unescape(&v[1..end]));
            }
        }
        from = at + 1;
    }
}

/// XML 实体反转义。只认 XML 1.0 的五个预定义实体与数字引用——事件 XML 里不会有
/// DTD 自定义实体。认不出的 `&…;` 原样保留，不丢字符。
pub fn xml_unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let Some(semi) = tail.find(';').filter(|i| *i <= 10) else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semi];
        let decoded = match name {
            "lt" => Some('<'),
            "gt" => Some('>'),
            "amp" => Some('&'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix('#')
                .and_then(|n| match n.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => n.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='Service Control Manager' Guid='{555908d1-a6d7-4695-8e1e-26931d2012f4}' EventSourceName='Service Control Manager'/><EventID Qualifiers='16384'>7040</EventID><Version>0</Version><Level>4</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8080000000000000</Keywords><TimeCreated SystemTime='2026-09-15T08:27:01.1169070Z'/><EventRecordID>28671</EventRecordID><Correlation/><Execution ProcessID='140' ThreadID='25224'/><Channel>System</Channel><Computer>DESKTOP-06GFNSU</Computer><Security UserID='S-1-5-18'/></System><EventData><Data Name='param1'>Background Intelligent Transfer Service</Data><Data Name='param2'>&#33258;&#21160;&#21551;&#21160;</Data><Data Name='param4'>BITS</Data></EventData></Event>";

    #[test]
    fn 解析一条真实事件() {
        let e = parse_event_xml(SAMPLE).expect("样本必须能解析");
        assert_eq!(e.provider.as_deref(), Some("Service Control Manager"));
        assert_eq!(
            e.provider_guid.as_deref(),
            Some("{555908d1-a6d7-4695-8e1e-26931d2012f4}"),
            "Guid 属性也要取到"
        );
        assert_eq!(e.level, Some(4));
        assert_eq!(e.priority(), LogPriority::Info);
        assert_eq!(e.channel.as_deref(), Some("System"));
        assert_eq!(e.computer.as_deref(), Some("DESKTOP-06GFNSU"));
        assert_eq!(e.pid, Some(140));
        assert_eq!(e.tid, Some(25224));
        assert_eq!(e.user_sid.as_deref(), Some("S-1-5-18"));
        assert_eq!(e.event_id.as_deref(), Some("7040"));
        assert_eq!(e.record_id.as_deref(), Some("28671"));
        assert_eq!(e.misc.get("Keywords").map(String::as_str), Some("0x8080000000000000"));
        assert_eq!(e.us, 116_907, "7 位小数截断到微秒");
        assert_eq!(format_iso8601_ms(e.ts * 1000), "2026-09-15T08:27:01.000Z");
        assert_eq!(
            e.data,
            vec![
                ("param1".to_owned(), "Background Intelligent Transfer Service".to_owned()),
                ("param2".to_owned(), "自动启动".to_owned()),
                ("param4".to_owned(), "BITS".to_owned()),
            ],
            "数字实体要反转义"
        );
    }

    /// `Provider` 标签上 `EventSourceName=` 含子串 `Name=`，按子串找会取错。
    #[test]
    fn 属性名必须落在词边界上() {
        let tag = "<Provider Name='A' Guid='{g}' EventSourceName='B'/>";
        assert_eq!(attr_in_tag(tag, "Name").as_deref(), Some("A"));
        assert_eq!(attr_in_tag(tag, "EventSourceName").as_deref(), Some("B"));
        assert_eq!(attr_in_tag(tag, "Missing"), None);
        // 双引号与等号两侧的空白
        assert_eq!(
            attr_in_tag("<X Name = \"v\"/>", "Name").as_deref(),
            Some("v")
        );
    }

    #[test]
    fn 没有时刻的事件被跳过() {
        assert!(parse_event_xml("").is_none());
        assert!(parse_event_xml("<Event><System></System></Event>").is_none());
        assert!(parse_event_xml("not xml at all").is_none());
        // 时刻格式不对
        assert!(
            parse_event_xml(
                "<Event><System><TimeCreated SystemTime='昨天'/></System></Event>"
            )
            .is_none()
        );
    }

    #[test]
    fn 级别缺失与自定义级别都归到_notice() {
        let xml = "<Event><System><TimeCreated SystemTime='2026-01-01T00:00:00.000Z'/></System></Event>";
        let e = parse_event_xml(xml).unwrap();
        assert_eq!(e.level, None);
        assert_eq!(e.priority(), LogPriority::Notice, "缺级别不能静默降成 debug");
        assert_eq!(priority_from_level(17), UNKNOWN_LEVEL_PRIORITY);
    }

    #[test]
    fn 没有_name_的_data_按位置编号() {
        let xml = "<Event><System><TimeCreated SystemTime='2026-01-01T00:00:00.000Z'/></System><EventData><Data>a</Data><Data>b</Data></EventData></Event>";
        let e = parse_event_xml(xml).unwrap();
        assert_eq!(
            e.data,
            vec![("Data1".to_owned(), "a".to_owned()), ("Data2".to_owned(), "b".to_owned())]
        );
    }

    #[test]
    fn userdata_取叶子元素() {
        let xml = "<Event><System><TimeCreated SystemTime='2026-01-01T00:00:00.000Z'/></System><UserData><RuleInfo xmlns='x'><Name>abc</Name><Id>7</Id></RuleInfo></UserData></Event>";
        let e = parse_event_xml(xml).unwrap();
        assert_eq!(
            e.data,
            vec![("Name".to_owned(), "abc".to_owned()), ("Id".to_owned(), "7".to_owned())]
        );
    }

    #[test]
    fn 退化正文() {
        let e = parse_event_xml(SAMPLE).unwrap();
        assert!(e.fallback_message().contains("BITS"));
        // 连参数都没有时给占位，而不是空白
        let empty = parse_event_xml("<Event><System><TimeCreated SystemTime='2026-01-01T00:00:00.000Z'/><EventID>99</EventID><Provider Name='P'/></System><EventData></EventData></Event>").unwrap();
        assert_eq!(empty.fallback_message(), "[事件 P/99：发布者未提供可读消息]");
    }

    #[test]
    fn 实体反转义() {
        assert_eq!(xml_unescape("a&lt;b&gt;c"), "a<b>c");
        assert_eq!(xml_unescape("&amp;&quot;&apos;"), "&\"'");
        assert_eq!(xml_unescape("&#33258;&#x52A8;"), "自动");
        assert_eq!(xml_unescape("no entities"), "no entities");
        // 认不出的原样保留，绝不吞字符
        assert_eq!(xml_unescape("a & b"), "a & b");
        assert_eq!(xml_unescape("&unknown;"), "&unknown;");
        assert_eq!(xml_unescape("&"), "&");
    }

    #[test]
    fn 级别映射() {
        assert_eq!(priority_from_level(0), LogPriority::Notice);
        assert_eq!(priority_from_level(1), LogPriority::Crit);
        assert_eq!(priority_from_level(2), LogPriority::Err);
        assert_eq!(priority_from_level(3), LogPriority::Warning);
        assert_eq!(priority_from_level(4), LogPriority::Info);
        assert_eq!(priority_from_level(5), LogPriority::Debug);
        assert_eq!(priority_from_level(255), LogPriority::Notice);
    }

    #[test]
    fn 级别下限翻译成_xpath() {
        // 不过滤 / info 以下：所有级别放行，不加条件
        assert_eq!(LevelFilter::from_floor(None), LevelFilter::All);
        assert_eq!(LevelFilter::from_floor(Some(LogPriority::Debug)), LevelFilter::All);

        // 错误及以上：只有 Critical(1) 与 Error(2)
        let f = LevelFilter::from_floor(Some(LogPriority::Err));
        assert_eq!(f.predicate().as_deref(), Some("(Level=1 or Level=2)"));

        // 警告及以上：再加 Warning(3)
        let f = LevelFilter::from_floor(Some(LogPriority::Warning));
        assert_eq!(f.predicate().as_deref(), Some("(Level=1 or Level=2 or Level=3)"));

        // notice 及以上：LogAlways(0) 与自定义级别一并放行
        let f = LevelFilter::from_floor(Some(LogPriority::Notice));
        assert_eq!(
            f.predicate().as_deref(),
            Some("(Level=0 or Level=1 or Level=2 or Level=3 or Level>5)")
        );

        // info 及以上：Verbose(5) 被挡在外面，自定义级别仍放行——
        // 它们按 `UNKNOWN_LEVEL_PRIORITY`（= Notice）归类，比 info 更严重。
        let f = LevelFilter::from_floor(Some(LogPriority::Info));
        assert_eq!(
            f.predicate().as_deref(),
            Some("(Level=0 or Level=1 or Level=2 or Level=3 or Level=4 or Level>5)")
        );

        // Windows 没有 emerg / alert，如实表达成「必然为空」
        assert_eq!(LevelFilter::from_floor(Some(LogPriority::Alert)), LevelFilter::Impossible);
        assert_eq!(LevelFilter::from_floor(Some(LogPriority::Emerg)), LevelFilter::Impossible);
    }

    #[test]
    fn xpath_拼接() {
        let w = Window {
            start_us: 1_756_252_800_000_000,
            end_us: 1_756_256_400_000_000,
            before: None,
        };
        let x = build_xpath(&w, None, Some(LogPriority::Err)).unwrap();
        // 1 756 252 800 秒 = 2025-08-27T00:00:00Z，窗口长一小时。
        assert_eq!(
            x,
            "*[System[TimeCreated[@SystemTime>='2025-08-27T00:00:00.000Z' and @SystemTime<='2025-08-27T01:00:00.001Z'] and (Level=1 or Level=2)]]"
        );
        // 比较运算符必须是裸的 >= / <=：写成 &gt;= 会被 wevtapi 拒掉（实测）
        assert!(!x.contains("&gt;") && !x.contains("&lt;"), "{x}");

        // 带 provider
        let x = build_xpath(&w, Some("nginx"), None).unwrap();
        assert!(x.contains("Provider[@Name='nginx']"), "{x}");
        assert!(!x.contains("Level="), "不过滤级别时不加 Level 条件：{x}");

        // 无解的级别下限不拼查询
        assert!(build_xpath(&w, None, Some(LogPriority::Emerg)).is_none());
    }

    /// XPath 没有转义机制，含引号的值一律不许进字面量——否则用户能改写整条查询。
    #[test]
    fn xpath_字面量拒绝引号() {
        assert_eq!(quote_xpath_literal("nginx").as_deref(), Some("'nginx'"));
        assert_eq!(quote_xpath_literal("Microsoft-Windows-Kernel-Power").as_deref(), Some("'Microsoft-Windows-Kernel-Power'"));
        assert_eq!(quote_xpath_literal("x' or Level=1 or @Name='y"), None);
        assert_eq!(quote_xpath_literal("a\"b"), None);

        // 注入式输入不会出现在 XPath 里
        let w = Window { start_us: 0, end_us: 1_000_000, before: None };
        let x = build_xpath(&w, Some("x' or '1'='1"), None).unwrap();
        assert!(!x.contains("Provider"), "含引号的 provider 不进 XPath：{x}");
    }

    #[test]
    fn unit_剥掉_service_后缀() {
        assert_eq!(provider_from_unit("Spooler.service"), "Spooler");
        assert_eq!(provider_from_unit("Spooler"), "Spooler");
        assert_eq!(provider_from_unit("a.service.service"), "a.service");
    }

    #[test]
    fn 解析事件时刻() {
        // 7 位小数（100ns）截断到微秒
        let (ts, us) = parse_system_time("2026-09-15T08:27:01.1169070Z").unwrap();
        assert_eq!(us, 116_907);
        assert_eq!(format_iso8601_ms(ts * 1000 + 116), "2026-09-15T08:27:01.116Z");

        // 无小数
        assert_eq!(parse_system_time("2026-09-15T08:27:01Z").unwrap(), (ts, 0));
        // 带偏移的形式也认
        let (a, _) = parse_system_time("2026-09-15T16:27:01.000+08:00").unwrap();
        assert_eq!(a, ts);

        // 坏输入
        assert!(parse_system_time("").is_none());
        assert!(parse_system_time("2026-09-15").is_none());
        assert!(parse_system_time("2026-09-15T25:00:00Z").is_none(), "小时越界");
        assert!(parse_system_time("2026-13-01T00:00:00Z").is_none(), "月份越界");
        assert!(parse_system_time("2026-09-15T08:27:01.11690xZ").is_none());
    }

    #[test]
    fn 日期与天数互为逆运算() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 9, 15), (2100, 3, 1), (1969, 12, 31)] {
            let days = days_from_civil(y, m, d).unwrap();
            assert_eq!(civil_from_days(days), (y, m, d), "{y}-{m}-{d}");
        }
        assert_eq!(format_iso8601_ms(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_iso8601_ms(-1), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn 游标解析与排序() {
        let a = CursorKey { micros: 100, hash: 5 };
        let b = CursorKey { micros: 100, hash: 9 };
        let c = CursorKey { micros: 101, hash: 1 };
        assert!(a < b && b < c, "先按时刻、同一微秒内按哈希");
        assert_eq!(CursorKey::parse(&a.render()), Some(a));
        assert_eq!(CursorKey::parse("garbage"), None);
        assert_eq!(CursorKey::parse("12:notanumber"), None);
        assert_eq!(CursorKey::parse("notanumber:12"), None);
    }

    #[test]
    fn 哈希稳定且区分相邻输入() {
        // 跨进程可复现：写死已知值，换实现就会失败
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_ne!(fnv1a64(b"abc"), fnv1a64(b"abd"));
        assert_ne!(fnv1a64(b"ab"), fnv1a64(b"ba"));
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
    fn 窗口推导() {
        let now = 2_000_000i64;
        // 只给 since / until
        let w = Window::from_query(
            &LogQuery { since: Some(1_000), until: Some(2_000), ..Default::default() },
            now,
        )
        .unwrap();
        assert_eq!(w.start_us, 1_000_000_000);
        assert_eq!(w.end_us, 2_000_000_000);

        // 什么都不给：默认窗口，右端是「现在」
        let w = Window::from_query(&LogQuery::default(), now).unwrap();
        assert_eq!(w.end_us, now * 1_000_000);
        assert_eq!(w.end_us - w.start_us, DEFAULT_WINDOW_SECS * 1_000_000);

        // 游标优先于 until
        let w = Window::from_query(
            &LogQuery { cursor: Some("1500000000:7".into()), until: Some(9_999), ..Default::default() },
            now,
        )
        .unwrap();
        assert_eq!(w.end_us, 1_500_000_000);
        assert_eq!(w.before, Some(CursorKey { micros: 1_500_000_000, hash: 7 }));

        // 非法区间与坏游标
        assert!(
            Window::from_query(
                &LogQuery { since: Some(2_000), until: Some(1_000), ..Default::default() },
                now
            )
            .is_err()
        );
        assert!(
            Window::from_query(&LogQuery { cursor: Some("bad".into()), ..Default::default() }, now)
                .is_err()
        );
    }

    #[test]
    fn 窗口边界裁剪() {
        let w = Window {
            start_us: 1_000,
            end_us: 2_000,
            before: Some(CursorKey { micros: 1_500, hash: 10 }),
        };
        assert!(w.accepts(&entry_at(1_400, 1)));
        assert!(w.accepts(&entry_at(1_500, 9)), "同一微秒、哈希更小即更旧");
        assert!(!w.accepts(&entry_at(1_500, 10)), "游标那条本身不能重复出现");
        assert!(!w.accepts(&entry_at(1_500, 11)));
        assert!(!w.accepts(&entry_at(999, 1)), "早于 start");
        assert!(!w.accepts(&entry_at(2_001, 1)), "晚于 end");

        // XPath 只到毫秒，窗口左右各向外取整一点，边界条目不会被 XPath 漏掉
        let w = Window { start_us: 1_500_000, end_us: 2_500_000, before: None };
        assert_eq!(w.start_arg(), format_iso8601_ms(1_500));
        assert_eq!(w.end_arg(), format_iso8601_ms(2_501));
    }

    #[test]
    fn 级别下限复核() {
        let mut e = entry_at(1, 1);
        e.priority = LogPriority::Warning;
        assert!(priority_ok(Some(LogPriority::Warning), &e));
        assert!(!priority_ok(Some(LogPriority::Err), &e));
        assert!(priority_ok(None, &e));
    }

    #[test]
    fn sid_取_rid() {
        assert_eq!(rid_from_sid("S-1-5-18"), Some(0), "LocalSystem 映射成 0");
        assert_eq!(rid_from_sid("S-1-5-21-1234567890-987654321-111111111-1001"), Some(1001));
        assert_eq!(rid_from_sid("S-1-5-32-544"), Some(544));
        assert_eq!(rid_from_sid(""), None);
        assert_eq!(rid_from_sid("S-1-5"), None, "段数不够不硬凑");
        assert_eq!(rid_from_sid("not-a-sid"), None);
    }

    #[test]
    fn 窗口左端可以被收紧() {
        let mut w = Window { start_us: 1_000, end_us: 9_000, before: None };
        w.clamp_start(5_000);
        assert_eq!(w.start_us, 5_000);
        // 已经更晚时不放宽
        w.clamp_start(1);
        assert_eq!(w.start_us, 5_000);
    }
}
