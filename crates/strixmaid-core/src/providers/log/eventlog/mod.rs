//! Windows 的 [`LogProvider`] 实现：事件日志（`wevtapi` 的 `EvtQuery` / `EvtSubscribe`）。
//!
//! - 查询：`EvtQuery` + `EvtNext`，反向读（最新在前），读够 `limit` 就停；
//! - follow：`EvtSubscribe(EvtSubscribeToFutureEvents)`，内核推送，同一过滤条件共享一个订阅；
//! - 正文：`EvtOpenPublisherMetadata` + `EvtFormatMessage`，发布者元数据按名字缓存。
//!
//! 不起 `wevtutil` 子进程的理由见 [`evt`] 的模块文档。
//!
//! # 与 Linux / macOS 最大的结构差异：**多通道**
//!
//! journald 是一条流，统一日志也是一条流；Windows 的事件日志是**一组互不相干的
//! 通道**，每个通道有自己的 `.evtx` 文件、自己的保留策略、自己的访问控制，
//! 连记录序号（`EventRecordID`）都是各数各的。
//!
//! 本实现查这几个通道，并把结果按时刻**归并**成一条流交给上层：
//!
//! | 通道 | 内容 | 权限 |
//! |---|---|---|
//! | `System` | 驱动、服务控制管理器、内核子系统 | 普通用户可读 |
//! | `Application` | 应用程序与大部分第三方服务 | 普通用户可读 |
//! | `Security` | 审计（登录、对象访问） | **需要管理员**，读不到就跳过 |
//!
//! `Security` 读不到是 Windows 的常态而不是故障，因此只在 `debug` 日志里记一笔、
//! 跳过它继续，**不**把整次查询报成失败（`docs/design.md` §1 原则 3：授权外包给
//! 操作系统）。反过来，若**一个通道都打不开**，那就是真的出问题了，如实报错——
//! 绝不把读取失败伪装成「这段时间没有日志」。
//!
//! 归并靠游标的全序（见 [`model`]），所以「不重不漏」的保证与单通道时完全一样。
//!
//! # 字段映射
//!
//! | journald | 事件日志 | 说明 |
//! |---|---|---|
//! | `PRIORITY` | `Level` | 六档映射见 [`model::priority_from_level`] |
//! | `MESSAGE` | `EvtFormatMessage` 的渲染结果 | 取不到时退回 `EventData` 拼接，见下 |
//! | `_SYSTEMD_UNIT` | `Provider@Name` | 查询时 `unit` 的 `.service` 后缀要剥掉，见 [`model::provider_from_unit`] |
//! | `SYSLOG_IDENTIFIER` | `Provider@Name` | 事件日志里两者是同一个东西 |
//! | `_PID` | `Execution@ProcessID` | `0` 记作 `None`（内核 / 早期用户态，不是「进程 0」） |
//! | `_UID` | `Security@UserID` 的 SID → RID | 口径与 `platform::windows::token` 一致 |
//! | `_BOOT_ID` | **没有** | 见下「boots」 |
//! | `_TRANSPORT` | 通道名（`System` / `Application` / `Security`） | 这是最贴近的对应物：都在说「这条记录从哪条路进来的」 |
//! | `_HOSTNAME` | `Computer` 元素 | |
//!
//! ## 正文为什么必须走 `EvtFormatMessage`
//!
//! 事件 XML 里的 `EventData` 是**没有上下文的占位参数**——
//! `<Data Name='param1'>Spooler</Data><Data Name='param2'>自动启动</Data>`
//! 这三个词凑不出一句话，真正的句子（「Print Spooler 服务的启动类型已从 X 改为 Y」）
//! 在发布者的消息资源 DLL 里。所以每条事件都要 `EvtOpenPublisherMetadata` +
//! `EvtFormatMessage`，元数据句柄按 provider 名缓存（见 [`evt::PublisherCache`]）。
//!
//! **渲染不出来时退化成 `EventData` 拼接**：发布者没在本机注册（软件被卸载但日志
//! 还在）、消息 DLL 缺失、当前语言没有对应消息，这三种情况都是正常的。退化后的正文
//! 只有裸参数，但参数本身是真的——比空白强，也不是编出来的。
//!
//! # boots
//!
//! 事件日志里**没有 boot id**：每条事件只知道自己的时刻，不知道自己属于哪次启动。
//! 因此与 macOS 侧一样，[`EventLog::boots`] 只报当前这一次启动（`index = 0`）：
//!
//! - `first_ts` 取开机时刻（`platform::windows::ntdll::boot_time_unix`）；
//! - `boot_id` 由开机时刻**派生**出一个 32 位 hex 串（[`derive_boot_id`]）。
//!   它不是 Windows 提供的标识，只是一个「同一次启动内稳定、重启后必然不同」的
//!   占位值，好让 API 契约里的 `boot_id` 字段有东西可填、前端的 boot 选择器能用。
//!   拿它去跨机器比对是没有意义的——这一点必须说在前面，而不是让人以为它是内核给的。
//!
//! 同理，`LogQuery::boot` 只接受 `"0"` 或当前这次启动的 id；要看上一次启动的日志
//! 在 Windows 上做不到（事件不带 boot 归属），于是如实报
//! [`ErrorCode::CapabilityUnavailable`]，而不是偷偷返回全部日志。
//!
//! 单条事件的 `boot_id` 按时刻判定：开机时刻之后的属于本次启动，更早的留 `None`
//! ——那些事件确实无从判断属于哪一次启动。
//!
//! # 清理
//!
//! Windows 可以按通道清空（`EvtClearLog`），但**没有**「收缩到 N 字节」或「只保留
//! 最近 N 天」（通道的 `MaxSize` / `Retention` 是**写入策略**，改了不会立刻回收空间，
//! 拿它冒充 vacuum 是谎报）。所以 `modes` 只有 [`VacuumMode::EraseAll`] 一档，
//! 与 macOS 一致。

pub mod evt;
pub mod model;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use strixmaid_types::log::{
    BootInfo, LogEntry, LogEntryDetail, LogPage, LogPriority, LogQuery, LogUsage, VacuumMode,
    VacuumReq, VacuumResp,
};
use strixmaid_types::{ApiError, ApiResult, ErrorCode};
use tokio::sync::{broadcast, mpsc};
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_EVT_CHANNEL_NOT_FOUND, ERROR_EVT_INVALID_CHANNEL_PATH,
    ERROR_EVT_INVALID_QUERY, ERROR_PRIVILEGE_NOT_HELD,
};
use windows_sys::Win32::System::EventLog::{
    EVT_HANDLE, EVT_SUBSCRIBE_NOTIFY_ACTION, EvtSubscribeActionDeliver,
};

use self::evt::{EvtHandle, PublisherCache};
use self::model::{
    CursorKey, EventXml, LevelFilter, Window, build_xpath, fnv1a64, parse_event_xml, priority_ok,
    provider_from_unit, rid_from_sid,
};
use super::{LogFollow, LogProvider, normalize_limit, validate_vacuum};
use crate::platform::windows::unix_now;
use crate::providers::{Probe, Provider};

/// 普通用户也读得到的通道。一个都打不开才算查询失败。
const CORE_CHANNELS: &[&str] = &["System", "Application"];

/// 需要管理员的通道。读不到就跳过，**不报错**——那是 Windows 的常态。
const ELEVATED_CHANNELS: &[&str] = &["Security"];

/// 单次查询的总超时。事件日志的量级远小于统一日志，30 秒是相当宽的余量。
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// 一次查询最多扫过的事件数。
///
/// 级别与 provider 都在 XPath 里由源头过滤，真正可能扫很多的是 `q` 全文关键字
/// ——XPath 表达不了任意子串匹配，只能拉回来逐条比（journalctl 在没有 `--grep`
/// 时也是这么降级的）。给它一个上限，免得一条冷门关键字把整个通道翻一遍。
const MAX_SCAN: usize = 20_000;

/// 结果多收这么多条再排序截断。
///
/// 同一个通道里 `EvtNext` 的反向顺序是**记录顺序**，而我们的全序是
/// `(微秒, 整条 XML 的哈希)`；同一微秒内两者不一定一致。多收一截再按游标排序，
/// 边界处就不会因为「取的那 limit 条不是游标序的前 limit 条」而漏掉几条。
/// 与 oslog 的 `TAIL_SLACK` 是同一个补丁，只是这边的乱序范围只有一微秒，
/// 64 条已经绰绰有余。
const CURSOR_SLACK: usize = 64;

/// `entry()` 在 ±1 秒窗口里最多看这么多条。
const ENTRY_SCAN: usize = 4096;

/// follow 单批最大条数。
const FOLLOW_BATCH_MAX: usize = 128;
/// follow 攒批窗口。
const FOLLOW_BATCH_WINDOW: Duration = Duration::from_millis(200);
/// follow 通道容量（批次数）。
const FOLLOW_CAPACITY: usize = 256;

/// follow 留底的容量。
///
/// 比 oslog 的 4096 小：这里留的是「条目 + 整条解析好的 XML」（详情要用），
/// 单条比 macOS 那边重；而事件日志一天也就几千条，1024 条足够覆盖用户在流里
/// 看到的任何一条。
const RECENT_CAP: usize = 1024;

// ---------------------------------------------------------------------------
// provider
// ---------------------------------------------------------------------------

/// 一条事件：对外的 [`LogEntry`] 加上解析好的 XML（详情接口要用）。
#[derive(Debug, Clone)]
struct Hit {
    entry: LogEntry,
    xml: EventXml,
}

/// 本次启动的时刻与派生出的 id，见模块文档「boots」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootStamp {
    /// 开机时刻，unix 秒。取不到时为 0。
    pub at: i64,
    /// 派生出的 32 位 hex 串。
    pub id: String,
}

/// 事件日志实现。
#[derive(Debug, Default)]
pub struct EventLog {
    follows: Mutex<HashMap<FollowKey, Weak<FollowShared>>>,
    /// 发布者元数据缓存，查询与 follow 共用。
    publishers: Arc<PublisherCache>,
    /// follow 流里最近经手的条目。
    ///
    /// 详情接口靠「按游标时刻开窗重查 + 游标串精确匹配」定位，但
    /// **`EvtSubscribe` 与 `EvtQuery` 渲染出的 XML 不保证逐字节相同**
    /// （订阅推上来的事件可能带 `RenderingInfo` 段、属性顺序也未必一致），
    /// 而游标里的哈希取自整条 XML——流里点开的条目重查时会 404。
    /// 流经手的条目在这里留底，详情先查这里、再退回重查。与 oslog 的 `recent`
    /// 同一个坑、同一个解法。
    recent: Arc<Mutex<VecDeque<Hit>>>,
}

impl EventLog {
    pub fn new() -> Self {
        Self::default()
    }

    /// 跑一次查询（含超时与线程切换）。
    ///
    /// 所有 `wevtapi` 调用都是**同步阻塞**的，必须放进 [`tokio::task::spawn_blocking`]，
    /// 否则一条慢查询会把整个运行时的一个 worker 线程钉住。
    async fn collect(&self, plan: QueryPlan) -> ApiResult<Vec<Hit>> {
        let publishers = Arc::clone(&self.publishers);
        let job = tokio::task::spawn_blocking(move || collect_blocking(&publishers, &plan));
        tokio::time::timeout(QUERY_TIMEOUT, job)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Timeout, "事件日志查询超时"))?
            .map_err(|e| ApiError::internal("事件日志查询任务失败").with_detail(e.to_string()))?
    }
}

/// 一次查询的全部参数。捆成一个结构体，方便整体搬进 `spawn_blocking`。
#[derive(Debug, Clone)]
struct QueryPlan {
    window: Window,
    floor: Option<LogPriority>,
    /// 已剥掉 `.service` 的 Provider 名。
    provider: Option<String>,
    /// 全文关键字，**已转小写**。
    needle: Option<String>,
    limit: usize,
    boot: BootStamp,
}

impl QueryPlan {
    /// 由查询参数构造。`limit` 由调用方先 [`normalize_limit`] 过。
    fn new(q: &LogQuery, window: Window, limit: usize) -> QueryPlan {
        QueryPlan {
            window,
            floor: q.priority,
            provider: q.unit.as_deref().map(|u| provider_from_unit(u).to_owned()),
            needle: q.q.as_deref().map(str::to_lowercase),
            limit,
            boot: boot_stamp().clone(),
        }
    }

    /// 进程内的复核与降级过滤。
    ///
    /// - 时间窗口与游标边界：XPath 只到毫秒，差的那一点在这里补齐；
    /// - 级别下限：`Level` 元素缺失的事件在 XPath 里筛不掉，这里按 Notice 复判；
    /// - Provider：含引号时进不了 XPath（见 [`model::quote_xpath_literal`]），
    ///   全靠这里；进了 XPath 的也再比一次，权当纵深防御；
    /// - `q` 全文关键字：XPath 不支持任意子串匹配，只能拉回来比。
    fn accepts(&self, hit: &Hit) -> bool {
        self.window.accepts(&hit.entry)
            && priority_ok(self.floor, &hit.entry)
            && self
                .provider
                .as_deref()
                .is_none_or(|p| hit.entry.unit.as_deref() == Some(p))
            && self
                .needle
                .as_deref()
                .is_none_or(|n| hit.entry.message.to_lowercase().contains(n))
    }
}

#[async_trait]
impl Provider for EventLog {
    fn id(&self) -> &'static str {
        // 与 Linux 的 "journald"、macOS 的 "oslog" 平级：这是日志后端的名字。
        "eventlog"
    }

    async fn probe(&self) -> Probe {
        let ok = tokio::task::spawn_blocking(|| {
            CORE_CHANNELS
                .iter()
                .map(|ch| (*ch, evt::open_query(ch, "*").err()))
                .collect::<Vec<_>>()
        })
        .await;
        let Ok(results) = ok else {
            return Probe::unavailable("探测事件日志的阻塞任务失败");
        };
        let failed: Vec<String> = results
            .iter()
            .filter_map(|(ch, e)| e.as_ref().map(|e| format!("{ch}：{e}")))
            .collect();
        match failed.len() {
            0 => Probe::Available,
            n if n < CORE_CHANNELS.len() => {
                Probe::degraded(format!("部分通道不可读（{}）", failed.join("；")))
            }
            _ => Probe::unavailable(format!("事件日志通道全部不可读（{}）", failed.join("；"))),
        }
    }
}

#[async_trait]
impl LogProvider for EventLog {
    async fn query(&self, q: &LogQuery) -> ApiResult<LogPage> {
        let limit = normalize_limit(q.limit)?;
        let mut window = Window::from_query(q, unix_now())?;
        apply_boot_filter(q, &mut window, boot_stamp())?;
        let entries: Vec<LogEntry> = self
            .collect(QueryPlan::new(q, window, limit))
            .await?
            .into_iter()
            .map(|h| h.entry)
            .collect();

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

        // 先查 follow 留底，理由见 `recent` 字段文档。
        if let Some(hit) = self
            .recent
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .rev()
            .find(|h| h.entry.cursor == cursor)
            .cloned()
        {
            return Ok(detail_of(hit));
        }

        // 事件日志没有「按 id 取一条」的接口（`EventRecordID` 只在单通道内唯一，
        // 本来就不是我们的游标），只能在游标时刻前后各一秒开窗重查、再按游标串
        // 精确匹配。与 oslog 的处理一致。
        let plan = QueryPlan {
            window: Window {
                start_us: key.micros - 1_000_000,
                end_us: key.micros + 1_000_000,
                before: None,
            },
            // 详情要能看到任何级别，这里绝不带级别下限
            floor: None,
            provider: None,
            needle: None,
            limit: ENTRY_SCAN,
            boot: boot_stamp().clone(),
        };
        let hit = self
            .collect(plan)
            .await?
            .into_iter()
            .find(|h| h.entry.cursor == cursor)
            .ok_or_else(|| ApiError::not_found(format!("游标 {cursor} 对应的日志不存在")))?;
        Ok(detail_of(hit))
    }

    async fn boots(&self) -> ApiResult<Vec<BootInfo>> {
        let b = boot_stamp();
        Ok(vec![BootInfo {
            index: 0,
            boot_id: b.id.clone(),
            first_ts: b.at,
            last_ts: unix_now(),
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

        let provider = q.unit.as_deref().map(|u| provider_from_unit(u).to_owned());
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let template = SubContext {
            tx: event_tx,
            publishers: Arc::clone(&self.publishers),
            recent: Arc::clone(&self.recent),
            floor: q.priority,
            needle: q.q.as_deref().map(str::to_lowercase),
            provider: provider.clone(),
            boot: boot_stamp().clone(),
        };
        let subs = subscribe_all(&template, provider.as_deref(), q.priority);

        let (tx, rx) = broadcast::channel(FOLLOW_CAPACITY);
        let task = tokio::spawn(follow_batcher(event_rx, tx.clone()));
        let shared = Arc::new(FollowShared {
            tx,
            task,
            _subs: subs,
        });
        map.insert(key, Arc::downgrade(&shared));
        tracing::debug!(filter = ?q, "事件日志订阅已建立");
        Ok(LogFollow::new(rx, Box::new(shared)))
    }

    async fn usage(&self) -> ApiResult<LogUsage> {
        Ok(LogUsage {
            bytes: evtx_usage().await,
            // Windows 只能按通道清空，没有按期 / 按大小收缩，见模块文档「清理」
            modes: vec![VacuumMode::EraseAll],
        })
    }

    async fn vacuum(&self, req: &VacuumReq) -> ApiResult<VacuumResp> {
        match validate_vacuum(req)? {
            VacuumMode::EraseAll => {}
            VacuumMode::KeepDuration | VacuumMode::MaxSize => {
                return Err(ApiError::capability_unavailable(
                    "eventlog",
                    "事件日志不支持按保留期 / 目标大小收缩",
                )
                .with_detail(
                    "通道的 MaxSize / Retention 是写入策略，改了不会回收已占用的空间；\
                     Windows 能做的只有按通道清空（erase_all）",
                ));
            }
        }

        let before = evtx_usage().await;
        let mut cleared: Vec<&str> = Vec::new();
        for channel in CORE_CHANNELS {
            let ch = *channel;
            let r = tokio::task::spawn_blocking(move || evt::clear_log(ch))
                .await
                .map_err(|e| ApiError::internal("清空事件日志的任务失败").with_detail(e.to_string()))?;
            match r {
                Ok(()) => cleared.push(ch),
                Err(e) => {
                    let err = clear_error(ch, &e);
                    let err = if cleared.is_empty() {
                        err
                    } else {
                        let so_far = err.detail.clone().unwrap_or_default();
                        err.with_detail(format!("{so_far}（已清空：{}）", cleared.join("、")))
                    };
                    return Err(err);
                }
            }
        }
        Ok(VacuumResp {
            before_bytes: before,
            after_bytes: evtx_usage().await,
            detail: Some(format!("已清空通道：{}", cleared.join("、"))),
        })
    }
}

// ---------------------------------------------------------------------------
// 查询：多通道归并
// ---------------------------------------------------------------------------

/// 一个通道的读取状态。
struct ChannelStream {
    channel: &'static str,
    result: EvtHandle,
    buf: VecDeque<Hit>,
    drained: bool,
}

impl ChannelStream {
    /// 缓冲空了就再取一批，直到有东西或读完。
    ///
    /// 整批都解析不出来时（渲染失败、时刻缺失）要继续往下取，否则归并会误以为
    /// 这个通道已经读完，**静默少给一个通道的日志**。
    fn fill(&mut self, publishers: &PublisherCache, boot: &BootStamp) -> std::io::Result<()> {
        while self.buf.is_empty() && !self.drained {
            let batch = evt::next_batch(&self.result, evt::NEXT_BATCH)?;
            if batch.is_empty() {
                self.drained = true;
                break;
            }
            for h in &batch {
                let Ok(xml) = evt::render_xml(h.raw()) else {
                    continue;
                };
                if let Some(hit) = hit_from_xml(&xml, h.raw(), publishers, boot) {
                    self.buf.push_back(hit);
                }
            }
        }
        Ok(())
    }
}

/// 查询的阻塞主体：开通道、归并、筛选、排序。
fn collect_blocking(publishers: &PublisherCache, plan: &QueryPlan) -> ApiResult<Vec<Hit>> {
    let Some(xpath) = build_xpath(&plan.window, plan.provider.as_deref(), plan.floor) else {
        // 级别下限在 Windows 上无解（emerg / alert），直接给空页，不白跑一次查询
        tracing::debug!(floor = ?plan.floor, "事件日志没有对应级别，结果必然为空");
        return Ok(Vec::new());
    };

    let mut streams: Vec<ChannelStream> = Vec::new();
    let mut failures: Vec<(&str, std::io::Error)> = Vec::new();
    for channel in CORE_CHANNELS.iter().chain(ELEVATED_CHANNELS) {
        match evt::open_query(channel, &xpath) {
            Ok(result) => streams.push(ChannelStream {
                channel,
                result,
                buf: VecDeque::new(),
                drained: false,
            }),
            Err(e) => {
                // 我们自己拼的 XPath 被拒 → 是参数问题，对所有通道都一样，立刻报
                if e.raw_os_error() == Some(ERROR_EVT_INVALID_QUERY as i32) {
                    return Err(ApiError::invalid_request("日志过滤条件无法被事件日志解析")
                        .with_detail(format!("{xpath}（{e}）")));
                }
                if ELEVATED_CHANNELS.contains(channel) {
                    // 普通用户读不到 Security 是常态，不是故障
                    tracing::debug!(channel, error = %e, "跳过需要管理员的通道");
                } else {
                    tracing::warn!(channel, error = %e, "事件日志通道打不开");
                }
                failures.push((channel, e));
            }
        }
    }
    if streams.is_empty() {
        return Err(open_error(&failures));
    }

    let deadline = Instant::now() + QUERY_TIMEOUT;
    let target = plan.limit.saturating_add(CURSOR_SLACK);
    let mut out: Vec<Hit> = Vec::with_capacity(target.min(1024));
    let mut scanned = 0usize;
    loop {
        if Instant::now() >= deadline {
            return Err(ApiError::new(ErrorCode::Timeout, "事件日志查询超时"));
        }
        for s in &mut streams {
            // 读到一半失败绝不能当成「读完了」——那会把一次失败的查询报成
            // 「这段时间没有日志」，是最糟糕的一种谎报（oslog 模块文档里的同一条）。
            s.fill(publishers, &plan.boot).map_err(|e| {
                ApiError::internal(format!("读取 {} 通道失败", s.channel))
                    .with_detail(e.to_string())
            })?;
        }
        // 所有通道的队首里挑最新的一条，这就是归并
        let Some(idx) = streams
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.buf.is_empty())
            .max_by(|a, b| CursorKey::of(&a.1.buf[0].entry).cmp(&CursorKey::of(&b.1.buf[0].entry)))
            .map(|(i, _)| i)
        else {
            break;
        };
        let Some(hit) = streams[idx].buf.pop_front() else {
            break;
        };
        scanned += 1;
        if scanned > MAX_SCAN {
            tracing::warn!(scanned, "事件日志扫描量触顶，结果可能不完整");
            break;
        }
        if !plan.accepts(&hit) {
            continue;
        }
        out.push(hit);
        if out.len() >= target {
            break;
        }
    }

    finalize(out, plan.limit)
}

/// 排序 → 去重 → 截断。
///
/// 必须显式排序，不能默认「`EvtNext` 的反向顺序就是游标顺序」：同一微秒内
/// 记录顺序与我们按哈希定的全序不一定一致。翻页的下界是「上一页最后一条的游标」，
/// 若取的那 `limit` 条不是游标序的前 `limit` 条，边界处就会漏掉几条。
///
/// 去重是纵深防御：整条 XML 含 `<Channel>` 与 `<EventRecordID>`，跨通道归并后
/// 撞游标在事实上不可能；但**游标唯一是翻页正确的前提**，一旦撞了，下一页的
/// 「严格早于游标」边界会把它的孪生兄弟一起漏掉，代价远大于这一行 `dedup`。
fn finalize(mut out: Vec<Hit>, limit: usize) -> ApiResult<Vec<Hit>> {
    out.sort_unstable_by(|a, b| CursorKey::of(&b.entry).cmp(&CursorKey::of(&a.entry)));
    out.dedup_by(|a, b| a.entry.cursor == b.entry.cursor);
    out.truncate(limit);
    Ok(out)
}

/// 一个通道都打不开时的错误分类。
///
/// 全是「拒绝访问」就是权限问题——必须归成 `PermissionDenied` **并**打上
/// `can_retry_elevated`，否则 `auth::exec` 的提权重试整条路都被堵死
/// （journalctl 与 oslog 都在这一点上栽过，见它们的 `map_*_error`）。
fn open_error(failures: &[(&str, std::io::Error)]) -> ApiError {
    let detail = failures
        .iter()
        .map(|(ch, e)| format!("{ch}：{e}"))
        .collect::<Vec<_>>()
        .join("；");
    if failures
        .iter()
        .any(|(_, e)| is_access_denied(e))
    {
        return ApiError::permission_denied("没有读取事件日志的权限")
            .with_detail(detail)
            .retry_elevated();
    }
    if failures.iter().all(|(_, e)| is_missing_channel(e)) && !failures.is_empty() {
        return ApiError::capability_unavailable("eventlog", "本机没有可读的事件日志通道")
            .with_detail(detail);
    }
    ApiError::internal("事件日志通道全部打不开").with_detail(detail)
}

fn is_access_denied(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(c) if c == ERROR_ACCESS_DENIED as i32 || c == ERROR_PRIVILEGE_NOT_HELD as i32
    )
}

fn is_missing_channel(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(c) if c == ERROR_EVT_CHANNEL_NOT_FOUND as i32
            || c == ERROR_EVT_INVALID_CHANNEL_PATH as i32
    )
}

/// `EvtClearLog` 失败的分类。
///
/// 清空事件日志要管理员；非提权进程拿到的是 `ERROR_ACCESS_DENIED`。归错类不只是
/// 错误码难看——`auth::exec` **只在 `PermissionDenied` 时才走提权重试**，归成
/// Internal 等于把「提权本可以解决」这条路堵死。macOS 侧的 `erase_error` 有一条
/// 同样的教训，这里照它的风格写，并带单测钉住。
fn clear_error(channel: &str, e: &std::io::Error) -> ApiError {
    let detail = format!("{channel}：{e}");
    if is_access_denied(e) {
        return ApiError::permission_denied("清空事件日志需要管理访问")
            .with_detail(detail)
            .retry_elevated();
    }
    if is_missing_channel(e) {
        return ApiError::not_found(format!("事件日志通道 {channel} 不存在")).with_detail(detail);
    }
    ApiError::internal("清空事件日志失败").with_detail(detail)
}

// ---------------------------------------------------------------------------
// 条目构造
// ---------------------------------------------------------------------------

/// 渲染好的 XML + 事件句柄 → 一条 [`Hit`]。句柄只用来取人类可读正文。
fn hit_from_xml(
    xml: &str,
    event: EVT_HANDLE,
    publishers: &PublisherCache,
    boot: &BootStamp,
) -> Option<Hit> {
    let ev = parse_event_xml(xml)?;
    let message = publishers
        .message(ev.provider.as_deref().unwrap_or_default(), event)
        .unwrap_or_else(|| ev.fallback_message());
    Some(hit_of(ev, xml, message, boot))
}

/// 纯函数部分：把解析结果拼成 [`Hit`]。与 FFI 无关，可以直接测。
fn hit_of(ev: EventXml, xml: &str, message: String, boot: &BootStamp) -> Hit {
    let entry = LogEntry {
        cursor: CursorKey {
            micros: ev.ts.saturating_mul(1_000_000) + i64::from(ev.us),
            hash: fnv1a64(xml.as_bytes()),
        }
        .render(),
        ts: ev.ts,
        us: ev.us,
        priority: ev.priority(),
        message,
        // 事件日志里 unit 与 identifier 是同一个东西：写这条日志的发布者
        unit: ev.provider.clone(),
        identifier: ev.provider.clone(),
        pid: ev.pid,
        uid: ev.user_sid.as_deref().and_then(rid_from_sid),
        hostname: ev.computer.clone(),
        // 开机之前的事件无从判断属于哪一次启动，留 None，见模块文档
        boot_id: (boot.at > 0 && ev.ts >= boot.at).then(|| boot.id.clone()),
        transport: ev.channel.clone(),
    };
    Hit { entry, xml: ev }
}

/// [`Hit`] → 详情。`fields` 是事件 XML 的 `System` 段各字段 + `EventData` 的键值对。
///
/// `EventData` 的键统一加 `EventData.` 前缀：发布者可以把参数命名成 `Level`、
/// `Channel` 这类与 `System` 段重名的东西，不加前缀就会互相覆盖。
fn detail_of(hit: Hit) -> LogEntryDetail {
    let Hit { entry, xml } = hit;
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut put = |k: &str, v: Option<String>| {
        if let Some(v) = v.filter(|s| !s.is_empty()) {
            fields.insert(k.to_owned(), v);
        }
    };
    put("MESSAGE", Some(entry.message.clone()));
    put("PRIORITY", Some(entry.priority.as_u8().to_string()));
    put("Provider", xml.provider.clone());
    put("ProviderGuid", xml.provider_guid.clone());
    put("EventID", xml.event_id.clone());
    put("Level", xml.level.map(|l| l.to_string()));
    put("Channel", xml.channel.clone());
    put("Computer", xml.computer.clone());
    put("ProcessID", xml.pid.map(|v| v.to_string()));
    put("ThreadID", xml.tid.map(|v| v.to_string()));
    put("UserID", xml.user_sid.clone());
    put("EventRecordID", xml.record_id.clone());
    put(
        "TimeCreated",
        Some(model::format_iso8601_ms(
            entry.ts.saturating_mul(1_000) + i64::from(entry.us / 1_000),
        )),
    );
    for (k, v) in &xml.misc {
        fields.insert(k.clone(), v.clone());
    }
    for (k, v) in &xml.data {
        fields.insert(format!("EventData.{k}"), v.clone());
    }
    LogEntryDetail { entry, fields }
}

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

/// 本次启动的时刻与 id，进程内只算一次。
fn boot_stamp() -> &'static BootStamp {
    static STAMP: OnceLock<BootStamp> = OnceLock::new();
    STAMP.get_or_init(|| {
        let at = crate::platform::windows::ntdll::boot_time_unix().unwrap_or_else(|e| {
            tracing::warn!(error = %e, "读不到开机时刻，boot 信息退化");
            0
        });
        BootStamp {
            at,
            id: derive_boot_id(at),
        }
    })
}

/// 由开机时刻派生一个 32 位 hex 的 boot id。
///
/// **它不是 Windows 提供的标识**——事件日志里根本没有 boot id（见模块文档）。
/// 这里只保证两件事：同一次启动内稳定（开机时刻不变），重启后必然不同
/// （开机时刻变了）。形状对齐 journald 的 32 位 hex，好让前端不必分平台处理。
pub fn derive_boot_id(boot_at: i64) -> String {
    let a = fnv1a64(format!("strixmaid/eventlog/boot/{boot_at}").as_bytes());
    let b = fnv1a64(format!("{a:016x}/{boot_at}").as_bytes());
    format!("{a:016x}{b:016x}")
}

/// `LogQuery::boot` → 时间窗口的左端约束。
///
/// 只认「当前这次启动」：事件不带 boot 归属，历史 boot 在 Windows 上做不到，
/// 如实报 `CapabilityUnavailable`，不偷偷返回全部日志。
fn apply_boot_filter(q: &LogQuery, window: &mut Window, boot: &BootStamp) -> ApiResult<()> {
    let Some(b) = q.boot.as_deref() else {
        return Ok(());
    };
    if b == "0" || b == boot.id {
        window.clamp_start(boot.at.saturating_mul(1_000_000));
        return Ok(());
    }
    Err(
        ApiError::capability_unavailable("eventlog", "事件日志无法按历史 boot 过滤")
            .with_detail(format!(
                "事件不记录所属启动，只有当前这次（\"0\" 或 {}）可用",
                boot.id
            )),
    )
}

// ---------------------------------------------------------------------------
// 磁盘占用
// ---------------------------------------------------------------------------

/// 事件日志的存放目录：`%SystemRoot%\System32\winevt\Logs`。
fn logs_dir() -> PathBuf {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
    PathBuf::from(root)
        .join("System32")
        .join("winevt")
        .join("Logs")
}

/// 所有 `.evtx` 的大小之和。
///
/// 目录本身读不到时返回 `None`（测不到，不编）。个别文件的属性读不到时跳过它——
/// 那样给出的是**下界**，比「不知道」有用，与 macOS 侧 `du` 部分可读时的处理一致。
async fn evtx_usage() -> Option<u64> {
    let mut rd = tokio::fs::read_dir(logs_dir()).await.ok()?;
    let mut total = 0u64;
    let mut seen = false;
    while let Ok(Some(e)) = rd.next_entry().await {
        let name = e.file_name();
        if !name.to_string_lossy().to_ascii_lowercase().ends_with(".evtx") {
            continue;
        }
        if let Ok(meta) = e.metadata().await {
            total += meta.len();
            seen = true;
        }
    }
    seen.then_some(total)
}

// ---------------------------------------------------------------------------
// follow
// ---------------------------------------------------------------------------

/// follow 的共享键：影响订阅参数的那部分过滤条件。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FollowKey {
    priority: Option<u8>,
    unit: Option<String>,
    q: Option<String>,
}

/// 回调要用的上下文。生命周期见 [`Subscription`]。
struct SubContext {
    tx: mpsc::UnboundedSender<LogEntry>,
    publishers: Arc<PublisherCache>,
    recent: Arc<Mutex<VecDeque<Hit>>>,
    floor: Option<LogPriority>,
    /// 已转小写的全文关键字。
    needle: Option<String>,
    provider: Option<String>,
    boot: BootStamp,
}

impl SubContext {
    fn clone_for_channel(&self) -> SubContext {
        SubContext {
            tx: self.tx.clone(),
            publishers: Arc::clone(&self.publishers),
            recent: Arc::clone(&self.recent),
            floor: self.floor,
            needle: self.needle.clone(),
            provider: self.provider.clone(),
            boot: self.boot.clone(),
        }
    }

    /// 订阅侧的过滤：XPath 已经筛过级别与 provider，这里补上 `q` 与级别复核。
    fn accepts(&self, entry: &LogEntry) -> bool {
        priority_ok(self.floor, entry)
            && self
                .provider
                .as_deref()
                .is_none_or(|p| entry.unit.as_deref() == Some(p))
            && self
                .needle
                .as_deref()
                .is_none_or(|n| entry.message.to_lowercase().contains(n))
    }
}

/// 一个通道的订阅。**字段的释放顺序是安全前提**，见 [`Drop`] 实现。
#[derive(Debug)]
struct Subscription {
    handle: Option<EvtHandle>,
    /// [`Arc::into_raw`] 出来的上下文。非空时本结构独占这一份强引用。
    ctx: *const SubContext,
}

// SAFETY: `ctx` 指向一块由 Arc 管理的堆内存，本结构只在 Drop 里动它（归还强引用），
// 不提供任何借出内部可变引用的方法；`EvtHandle` 是 isize，跨线程传递是 Win32 常规。
// 因此把 Subscription 移到别的线程、或从多个线程共享 `&Subscription` 都是安全的。
unsafe impl Send for Subscription {}
// SAFETY: 同上——`&Subscription` 拿不出任何可变状态。
unsafe impl Sync for Subscription {}

impl Drop for Subscription {
    fn drop(&mut self) {
        // SAFETY（顺序就是全部）：回调可能正跑在 wevtapi 自己的线程上，它持有的是
        // `ctx` 这个**裸指针**，不算强引用。`EvtClose` 是唯一能保证「此后回调不再被
        // 调用」的操作（MSDN 对 EvtSubscribe 的契约，微软示例也是这个释放顺序）。
        // 所以必须先关订阅、再归还 Arc：
        //   1. `self.handle.take()` 立刻 drop → EvtClose，订阅被取消；
        //   2. 之后才 `Arc::from_raw` 把强引用还回去，引用计数归零时释放上下文。
        // 顺序反过来就是 use-after-free：上下文没了，回调还可能读它。
        drop(self.handle.take());
        if !self.ctx.is_null() {
            // SAFETY: `ctx` 来自本结构构造时的 `Arc::into_raw`，只归还这一次
            // （take 之后置空），指针类型与当初一致。
            unsafe { drop(Arc::from_raw(self.ctx)) };
            self.ctx = std::ptr::null();
        }
    }
}

/// 一组共享的订阅。最后一个 `Arc` drop 时 abort 攒批任务，订阅随之关闭。
#[derive(Debug)]
pub struct FollowShared {
    tx: broadcast::Sender<Arc<Vec<LogEntry>>>,
    task: tokio::task::JoinHandle<()>,
    /// 字段顺序即释放顺序：`Drop` 的函数体先 abort 任务，之后才轮到这里关订阅。
    _subs: Vec<Subscription>,
}

impl Drop for FollowShared {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// 对每个可读通道各建一个订阅。打不开的通道跳过并记一笔——`Security` 对普通用户
/// 打不开是常态。
fn subscribe_all(
    template: &SubContext,
    provider: Option<&str>,
    floor: Option<LogPriority>,
) -> Vec<Subscription> {
    let level = LevelFilter::from_floor(floor);
    if level == LevelFilter::Impossible {
        // Windows 没有 emerg / alert 级别，这条 follow 永远不会有条目。
        // 如实地不订阅任何通道，而不是放宽成 Critical 去冒充。
        tracing::info!(?floor, "事件日志没有对应级别，follow 不订阅任何通道");
        return Vec::new();
    }
    let xpath = follow_xpath(&level, provider);

    let mut subs = Vec::new();
    for channel in CORE_CHANNELS.iter().chain(ELEVATED_CHANNELS) {
        let ctx = Arc::new(template.clone_for_channel());
        let raw = Arc::into_raw(ctx);
        // SAFETY: `raw` 是刚由 Arc::into_raw 交出的强引用，成功时由返回的
        // Subscription 独占并按 Drop 里的顺序释放；失败时在本分支立刻归还。
        // 回调 `deliver` 能在任意线程上跑，且用 catch_unwind 挡住了 unwind。
        match unsafe { evt::subscribe(channel, &xpath, raw.cast(), Some(deliver)) } {
            Ok(handle) => subs.push(Subscription {
                handle: Some(handle),
                ctx: raw,
            }),
            Err(e) => {
                // SAFETY: 订阅没建立成功，wevtapi 不会再碰这个指针，安全归还。
                unsafe { drop(Arc::from_raw(raw)) };
                if ELEVATED_CHANNELS.contains(channel) {
                    tracing::debug!(channel, error = %e, "跳过需要管理员的通道");
                } else {
                    tracing::warn!(channel, error = %e, "事件日志通道订阅失败");
                }
            }
        }
    }
    subs
}

/// follow 用的 XPath。**不带时间窗口**——`EvtSubscribeToFutureEvents` 本来就只给
/// 订阅之后产生的事件，再加时间条件只会把刚发生的事件误筛掉。
fn follow_xpath(level: &LevelFilter, provider: Option<&str>) -> String {
    let mut conds: Vec<String> = Vec::new();
    if let Some(p) = level.predicate() {
        conds.push(p);
    }
    if let Some(lit) = provider.and_then(model::quote_xpath_literal) {
        conds.push(format!("Provider[@Name={lit}]"));
    }
    if conds.is_empty() {
        "*".to_owned()
    } else {
        format!("*[System[{}]]", conds.join(" and "))
    }
}

/// `EvtSubscribe` 的回调。
///
/// # 在任意线程上被调用
///
/// wevtapi 用它自己的线程池投递事件，这里**不能有任何 async**：解析完就把条目塞进
/// [`mpsc::UnboundedSender`]（`send` 是同步的、可在任意线程调），攒批与广播交给
/// tokio 任务 [`follow_batcher`]。
///
/// # Safety
///
/// 由 [`evt::subscribe`] 的契约保证：`usercontext` 是 [`subscribe_all`] 传进去的
/// `Arc<SubContext>` 裸指针，在对应的 `EvtClose` 返回之前一直有效。
unsafe extern "system" fn deliver(
    action: EVT_SUBSCRIBE_NOTIFY_ACTION,
    usercontext: *const std::ffi::c_void,
    event: EVT_HANDLE,
) -> u32 {
    if action != EvtSubscribeActionDeliver || usercontext.is_null() {
        return 0;
    }
    // unwind 穿过 `extern "system"` 边界会直接 abort 掉整个进程。解析的是外部数据，
    // 再小心也不能赌它不 panic，这里兜住。
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: 见本函数的 Safety 段——指针在订阅存活期间有效，且这里只借用不释放。
        let ctx = unsafe { &*(usercontext.cast::<SubContext>()) };
        let Ok(xml) = evt::render_xml(event) else {
            return;
        };
        // 注意：`event` 的所有权属于订阅服务，回调返回后由它释放，**不要 EvtClose**。
        let Some(hit) = hit_from_xml(&xml, event, &ctx.publishers, &ctx.boot) else {
            return;
        };
        {
            let mut r = ctx.recent.lock().unwrap_or_else(|p| p.into_inner());
            if r.len() >= RECENT_CAP {
                r.pop_front();
            }
            r.push_back(hit.clone());
        }
        if ctx.accepts(&hit.entry) {
            // 送不进去只说明订阅者已经走了，不是错误
            let _ = ctx.tx.send(hit.entry);
        }
    }));
    0
}

/// 攒批并广播。
///
/// 多个通道的回调是并发投递的，到达顺序不保证有序，所以每批**按游标升序**排一次
/// ——`LogFollow::next` 的契约是「按时间先后」。
async fn follow_batcher(
    mut rx: mpsc::UnboundedReceiver<LogEntry>,
    tx: broadcast::Sender<Arc<Vec<LogEntry>>>,
) {
    loop {
        let Some(first) = rx.recv().await else {
            break;
        };
        let mut batch = vec![first];
        let mut closed = false;
        let window = tokio::time::sleep(FOLLOW_BATCH_WINDOW);
        tokio::pin!(window);
        while batch.len() < FOLLOW_BATCH_MAX {
            tokio::select! {
                m = rx.recv() => match m {
                    Some(e) => batch.push(e),
                    None => { closed = true; break; }
                },
                _ = &mut window => break,
            }
        }
        batch.sort_unstable_by(|a, b| CursorKey::of(a).cmp(&CursorKey::of(b)));
        batch.dedup_by(|a, b| a.cursor == b.cursor);
        // 暂时没有订阅者也不退出：Arc 还活着说明马上会有人 subscribe
        let _ = tx.send(Arc::new(batch));
        if closed {
            break;
        }
    }
    tracing::debug!("事件日志 follow 攒批任务结束");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> BootStamp {
        BootStamp {
            at: 1_700_000_000,
            id: derive_boot_id(1_700_000_000),
        }
    }

    const SAMPLE: &str = "<Event xmlns='http://schemas.microsoft.com/win/2004/08/events/event'><System><Provider Name='Service Control Manager' Guid='{555908d1-a6d7-4695-8e1e-26931d2012f4}' EventSourceName='Service Control Manager'/><EventID Qualifiers='16384'>7040</EventID><Version>0</Version><Level>2</Level><Task>0</Task><Opcode>0</Opcode><Keywords>0x8080000000000000</Keywords><TimeCreated SystemTime='2026-09-15T08:27:01.1169070Z'/><EventRecordID>28671</EventRecordID><Correlation/><Execution ProcessID='140' ThreadID='25224'/><Channel>System</Channel><Computer>DESKTOP-06GFNSU</Computer><Security UserID='S-1-5-18'/></System><EventData><Data Name='param1'>BITS</Data></EventData></Event>";

    #[test]
    fn 字段映射() {
        let ev = parse_event_xml(SAMPLE).unwrap();
        let hit = hit_of(ev, SAMPLE, "服务已启动".to_owned(), &stamp());
        let e = &hit.entry;
        assert_eq!(e.priority, LogPriority::Err, "Level=2 → err");
        assert_eq!(e.message, "服务已启动");
        assert_eq!(e.unit.as_deref(), Some("Service Control Manager"));
        assert_eq!(e.identifier.as_deref(), Some("Service Control Manager"));
        assert_eq!(e.pid, Some(140));
        assert_eq!(e.uid, Some(0), "S-1-5-18 是 LocalSystem，映射成 0");
        assert_eq!(e.hostname.as_deref(), Some("DESKTOP-06GFNSU"));
        assert_eq!(e.transport.as_deref(), Some("System"), "通道名当 transport");
        assert_eq!(e.us, 116_907);
        // 游标 = 微秒:整条 XML 的哈希，且可重复计算
        assert_eq!(
            e.cursor,
            CursorKey {
                micros: e.ts * 1_000_000 + i64::from(e.us),
                hash: fnv1a64(SAMPLE.as_bytes())
            }
            .render()
        );
        assert!(CursorKey::parse(&e.cursor).is_some());
    }

    /// 开机之前的事件不许被贴上本次启动的 boot_id。
    #[test]
    fn boot_id_按时刻判定() {
        let ev = parse_event_xml(SAMPLE).unwrap();
        let after = BootStamp {
            at: ev.ts - 10,
            id: derive_boot_id(ev.ts - 10),
        };
        assert_eq!(
            hit_of(ev.clone(), SAMPLE, String::new(), &after).entry.boot_id,
            Some(after.id.clone())
        );
        let before = BootStamp {
            at: ev.ts + 10,
            id: derive_boot_id(ev.ts + 10),
        };
        assert_eq!(
            hit_of(ev, SAMPLE, String::new(), &before).entry.boot_id,
            None,
            "开机之前的事件无从判断属于哪次启动"
        );
    }

    #[test]
    fn 派生的_boot_id_形状与稳定性() {
        let a = derive_boot_id(1_700_000_000);
        assert_eq!(a.len(), 32, "对齐 journald 的 32 位 hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, derive_boot_id(1_700_000_000), "同一次启动必须稳定");
        assert_ne!(a, derive_boot_id(1_700_000_001), "换一次启动必须不同");
    }

    #[test]
    fn 详情字段() {
        let ev = parse_event_xml(SAMPLE).unwrap();
        let d = detail_of(hit_of(ev, SAMPLE, "正文".to_owned(), &stamp()));
        assert_eq!(d.fields.get("MESSAGE").unwrap(), "正文");
        assert_eq!(d.fields.get("PRIORITY").unwrap(), "3");
        assert_eq!(d.fields.get("Provider").unwrap(), "Service Control Manager");
        assert_eq!(d.fields.get("EventID").unwrap(), "7040");
        assert_eq!(d.fields.get("EventRecordID").unwrap(), "28671");
        assert_eq!(d.fields.get("Channel").unwrap(), "System");
        assert_eq!(d.fields.get("ProcessID").unwrap(), "140");
        assert_eq!(d.fields.get("UserID").unwrap(), "S-1-5-18");
        assert_eq!(d.fields.get("Keywords").unwrap(), "0x8080000000000000");
        // EventData 加前缀，免得与 System 段重名字段互相覆盖
        assert_eq!(d.fields.get("EventData.param1").unwrap(), "BITS");
        assert!(!d.fields.contains_key("param1"));
        assert!(d.fields.get("TimeCreated").unwrap().starts_with("2026-09-15T08:27:01."));
    }

    /// 权限不足必须归成 403 + 可提权重试，否则提权重试整条路被堵死。
    #[test]
    fn 清空失败的分类() {
        let e = clear_error("System", &evt::io_error(ERROR_ACCESS_DENIED));
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated, "不带这个标记，提权重试不会被触发");
        assert!(e.detail.as_deref().unwrap().contains("System"));

        let e = clear_error("System", &evt::io_error(ERROR_PRIVILEGE_NOT_HELD));
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);

        let e = clear_error("Nope", &evt::io_error(ERROR_EVT_CHANNEL_NOT_FOUND));
        assert_eq!(e.code, ErrorCode::NotFound);

        // 说不清的失败仍是 500，但必须把原话带上
        let e = clear_error("System", &evt::io_error(1117));
        assert_eq!(e.code, ErrorCode::Internal);
        assert!(e.detail.is_some());
    }

    #[test]
    fn 打不开通道的分类() {
        let denied = vec![
            ("System", evt::io_error(ERROR_ACCESS_DENIED)),
            ("Application", evt::io_error(ERROR_ACCESS_DENIED)),
        ];
        let e = open_error(&denied);
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.can_retry_elevated);

        let missing = vec![("System", evt::io_error(ERROR_EVT_CHANNEL_NOT_FOUND))];
        assert_eq!(
            open_error(&missing).code,
            ErrorCode::CapabilityUnavailable,
            "通道不存在是「本机没这能力」，不是 500"
        );

        let weird = vec![("System", evt::io_error(1117))];
        assert_eq!(open_error(&weird).code, ErrorCode::Internal);
    }

    #[test]
    fn boot_过滤只认当前启动() {
        let b = stamp();
        let mut w = Window {
            start_us: 0,
            end_us: 2_000_000_000_000_000,
            before: None,
        };
        // "0" = 本次启动，窗口左端被收到开机时刻
        apply_boot_filter(
            &LogQuery {
                boot: Some("0".into()),
                ..Default::default()
            },
            &mut w,
            &b,
        )
        .unwrap();
        assert_eq!(w.start_us, b.at * 1_000_000);

        // 当前 boot 的 id 也认
        let mut w2 = w.clone();
        assert!(
            apply_boot_filter(
                &LogQuery {
                    boot: Some(b.id.clone()),
                    ..Default::default()
                },
                &mut w2,
                &b
            )
            .is_ok()
        );

        // 历史 boot 做不到，如实报 CapabilityUnavailable 而不是悄悄给全部日志
        let e = apply_boot_filter(
            &LogQuery {
                boot: Some("-1".into()),
                ..Default::default()
            },
            &mut w,
            &b,
        )
        .unwrap_err();
        assert_eq!(e.code, ErrorCode::CapabilityUnavailable);
        assert_eq!(e.capability.as_deref(), Some("eventlog"));

        // 不给 boot 就不动窗口
        let mut w3 = Window {
            start_us: 1,
            end_us: 2,
            before: None,
        };
        apply_boot_filter(&LogQuery::default(), &mut w3, &b).unwrap();
        assert_eq!(w3.start_us, 1);
    }

    #[test]
    fn follow_的_xpath_不带时间窗口() {
        let x = follow_xpath(&LevelFilter::from_floor(Some(LogPriority::Err)), None);
        assert_eq!(x, "*[System[(Level=1 or Level=2)]]");
        assert!(!x.contains("SystemTime"), "订阅只给未来事件，加时间条件会误筛");

        // 什么都不过滤时是裸 `*`
        assert_eq!(follow_xpath(&LevelFilter::All, None), "*");
        let x = follow_xpath(&LevelFilter::All, Some("Spooler"));
        assert_eq!(x, "*[System[Provider[@Name='Spooler']]]");
        // 含引号的 provider 不进 XPath，交给进程内过滤
        assert_eq!(follow_xpath(&LevelFilter::All, Some("a'b")), "*");
    }

    #[test]
    fn 结果排序去重截断() {
        let ev = parse_event_xml(SAMPLE).unwrap();
        let mk = |micros: i64, hash: u64| {
            let mut h = hit_of(ev.clone(), SAMPLE, String::new(), &stamp());
            h.entry.cursor = CursorKey { micros, hash }.render();
            h
        };
        let out = finalize(
            vec![mk(100, 1), mk(300, 1), mk(200, 1), mk(300, 1)],
            10,
        )
        .unwrap();
        let cursors: Vec<&str> = out.iter().map(|h| h.entry.cursor.as_str()).collect();
        assert_eq!(cursors, ["300:1", "200:1", "100:1"], "严格由新到旧且去重");

        let out = finalize(vec![mk(100, 1), mk(300, 1), mk(200, 1)], 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].entry.cursor, "300:1");
    }

    #[test]
    fn 日志目录在_systemroot_下() {
        let d = logs_dir();
        assert!(d.ends_with("System32\\winevt\\Logs") || d.ends_with("System32/winevt/Logs"), "{d:?}");
    }

    // -----------------------------------------------------------------------
    // 本机实测
    // -----------------------------------------------------------------------

    /// 三十天窗口，本机不可能一条系统日志都没有。
    fn wide_since() -> i64 {
        unix_now() - 30 * 86_400
    }

    #[tokio::test]
    async fn 本机探测与查询() {
        let p = EventLog::new();
        assert_eq!(p.id(), "eventlog");
        let probe = p.probe().await;
        eprintln!("[eventlog] probe = {probe:?}");
        if !probe.is_available() {
            eprintln!("[eventlog] 本机事件日志不可用，跳过");
            return;
        }

        let page = p
            .query(&LogQuery {
                limit: Some(20),
                since: Some(wide_since()),
                ..Default::default()
            })
            .await
            .expect("查询不应失败");
        // 读取路径出问题却被当成「没有日志」是最糟糕的一种谎报，用一个三十天窗口钉死
        assert!(!page.entries.is_empty(), "最近三十天不可能一条事件都没有");
        assert!(page.entries.len() <= 20);
        for w in page.entries.windows(2) {
            assert!(
                CursorKey::of(&w[0]) > CursorKey::of(&w[1]),
                "结果必须严格由新到旧"
            );
        }
        let top = &page.entries[0];
        eprintln!(
            "[eventlog] 最近一条：[{}] {} {:?} {}",
            top.transport.as_deref().unwrap_or("?"),
            top.identifier.as_deref().unwrap_or("?"),
            top.priority,
            top.message.chars().take(70).collect::<String>()
        );
        assert!(
            !top.message.is_empty(),
            "正文不该是空的：渲染不出来也要退回 EventData"
        );
        assert!(top.hostname.is_some(), "Computer 元素一定有");
    }

    /// 多通道归并要真的归并：System 与 Application 都应出现在结果里。
    #[tokio::test]
    async fn 本机结果覆盖多个通道() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let page = p
            .query(&LogQuery {
                limit: Some(1000),
                since: Some(wide_since()),
                ..Default::default()
            })
            .await
            .unwrap();
        let channels: std::collections::BTreeSet<&str> = page
            .entries
            .iter()
            .filter_map(|e| e.transport.as_deref())
            .collect();
        eprintln!(
            "[eventlog] {} 条，覆盖通道 {:?}",
            page.entries.len(),
            channels
        );
        assert!(!channels.is_empty());
        for c in &channels {
            assert!(
                CORE_CHANNELS.contains(c) || ELEVATED_CHANNELS.contains(c),
                "出现了没查过的通道：{c}"
            );
        }
        // 游标在跨通道归并之后仍然唯一——这是翻页正确的前提
        let mut seen = std::collections::HashSet::new();
        for e in &page.entries {
            assert!(seen.insert(e.cursor.clone()), "游标重复：{}", e.cursor);
        }
    }

    #[tokio::test]
    async fn 本机翻页不重不漏() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let since = wide_since();
        let first = p
            .query(&LogQuery {
                limit: Some(10),
                since: Some(since),
                ..Default::default()
            })
            .await
            .unwrap();
        let Some(cursor) = first.next_cursor.clone() else {
            eprintln!("[eventlog] 本机日志不足 10 条，跳过翻页断言");
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
        if let (Some(a), Some(b)) = (first.entries.last(), second.entries.first()) {
            assert!(
                CursorKey::of(a) > CursorKey::of(b),
                "第二页必须严格更旧"
            );
        }
        eprintln!(
            "[eventlog] 翻页：第一页 {} 条，第二页 {} 条，无重叠",
            first.entries.len(),
            second.entries.len()
        );
    }

    /// 用户实测抓到过的同类 bug（见 oslog 的同名用例）：级别选「错误及以上」，
    /// Notice / Information 照样混进来。这里在真机上验证 XPath 的级别下限真的生效。
    #[tokio::test]
    async fn 查询结果遵守级别下限() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        for floor in [LogPriority::Crit, LogPriority::Err, LogPriority::Warning] {
            let page = p
                .query(&LogQuery {
                    limit: Some(200),
                    since: Some(wide_since()),
                    priority: Some(floor),
                    ..Default::default()
                })
                .await
                .unwrap();
            for e in &page.entries {
                assert!(
                    e.priority.as_u8() <= floor.as_u8(),
                    "下限 {floor:?} 却放进了 {:?}：{}",
                    e.priority,
                    e.message.chars().take(60).collect::<String>()
                );
            }
            eprintln!("[eventlog] {floor:?} 及以上：{} 条", page.entries.len());
        }
        // Windows 没有 emerg / alert，如实给空，不放宽成 Critical 冒充
        let page = p
            .query(&LogQuery {
                since: Some(wide_since()),
                priority: Some(LogPriority::Emerg),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page.entries.is_empty(), "Windows 上不存在 emerg 级别的事件");
    }

    #[tokio::test]
    async fn 本机详情能按游标取回() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let page = p
            .query(&LogQuery {
                limit: Some(1),
                since: Some(wide_since()),
                ..Default::default()
            })
            .await
            .unwrap();
        let Some(first) = page.entries.first().cloned() else {
            eprintln!("[eventlog] 本机没有日志，跳过详情断言");
            return;
        };
        let detail = p.entry(&first.cursor).await.expect("游标应当能取回详情");
        assert_eq!(detail.entry.cursor, first.cursor);
        assert!(!detail.fields.is_empty());
        eprintln!(
            "[eventlog] 详情字段 {} 个：{:?}",
            detail.fields.len(),
            detail.fields.keys().take(8).collect::<Vec<_>>()
        );

        // 不存在的游标要 404，不能返回「最接近的一条」
        let e = p.entry("1:2").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        // 格式不对要 400
        let e = p.entry("garbage").await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn 本机_unit_过滤() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let page = p
            .query(&LogQuery {
                limit: Some(200),
                since: Some(wide_since()),
                ..Default::default()
            })
            .await
            .unwrap();
        let Some(provider) = page.entries.iter().find_map(|e| e.unit.clone()) else {
            eprintln!("[eventlog] 没有带 Provider 的事件，跳过");
            return;
        };
        // 带 `.service` 后缀也要能查到：service provider 那边的 unit 名是这个形态
        for unit in [provider.clone(), format!("{provider}.service")] {
            let filtered = p
                .query(&LogQuery {
                    limit: Some(20),
                    since: Some(wide_since()),
                    unit: Some(unit.clone()),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(
                !filtered.entries.is_empty(),
                "按 {unit} 过滤不该是空的（provider={provider}）"
            );
            for e in &filtered.entries {
                assert_eq!(e.unit.as_deref(), Some(provider.as_str()));
            }
            eprintln!("[eventlog] unit={unit} → {} 条", filtered.entries.len());
        }
    }

    /// XPath 注入：单引号进不了查询，也不能让查询报错。
    #[tokio::test]
    async fn 含引号的_unit_不会改写查询() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let page = p
            .query(&LogQuery {
                limit: Some(5),
                since: Some(wide_since()),
                unit: Some("x' or Level=1 or @Name='y".into()),
                ..Default::default()
            })
            .await
            .expect("含引号的 unit 不该让查询失败");
        assert!(page.entries.is_empty(), "没有这个 provider，结果就该是空的");
    }

    #[tokio::test]
    async fn 本机关键字过滤() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let page = p
            .query(&LogQuery {
                limit: Some(100),
                since: Some(wide_since()),
                ..Default::default()
            })
            .await
            .unwrap();
        // 从真实正文里取一个词当关键字，大小写故意改掉
        let Some(needle) = page
            .entries
            .iter()
            .flat_map(|e| e.message.split_whitespace())
            .find(|w| w.len() >= 4 && w.chars().all(|c| c.is_ascii_alphabetic()))
            .map(str::to_uppercase)
        else {
            eprintln!("[eventlog] 找不到合适的关键字，跳过");
            return;
        };
        let hit = p
            .query(&LogQuery {
                limit: Some(10),
                since: Some(wide_since()),
                q: Some(needle.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
        for e in &hit.entries {
            assert!(
                e.message.to_lowercase().contains(&needle.to_lowercase()),
                "关键字过滤没生效：{}",
                e.message
            );
        }
        eprintln!("[eventlog] q={needle} → {} 条（大小写不敏感）", hit.entries.len());
        assert!(!hit.entries.is_empty(), "关键字取自真实正文，不该一条都匹配不上");
    }

    #[tokio::test]
    async fn 本机_boots_只报当前启动() {
        let p = EventLog::new();
        let boots = p.boots().await.unwrap();
        assert_eq!(boots.len(), 1, "事件日志没有 boot 概念，只能报当前这次");
        assert_eq!(boots[0].index, 0);
        assert_eq!(boots[0].boot_id.len(), 32);
        assert!(boots[0].first_ts > 1_577_836_800, "开机时刻明显不对");
        assert!(boots[0].last_ts >= boots[0].first_ts);
        eprintln!(
            "[eventlog] boot {} 开机于 {}，已运行 {} 秒",
            boots[0].boot_id,
            boots[0].first_ts,
            boots[0].last_ts - boots[0].first_ts
        );
    }

    #[tokio::test]
    async fn 本机占用与清理方式() {
        let p = EventLog::new();
        let u = p.usage().await.unwrap();
        assert_eq!(
            u.modes,
            vec![VacuumMode::EraseAll],
            "Windows 没有按期 / 按大小收缩"
        );
        match u.bytes {
            Some(b) => {
                assert!(b > 0, "能读到目录就不该是 0 字节");
                eprintln!("[eventlog] .evtx 合计 {b} 字节（{:.1} MiB）", b as f64 / 1048576.0);
            }
            None => eprintln!("[eventlog] 读不到 winevt\\Logs，如实上报 None"),
        }
    }

    /// 不支持的清理方式必须在**动手之前**被挡下来。
    #[tokio::test]
    async fn 不支持的清理方式被拒() {
        let p = EventLog::new();
        for req in [
            VacuumReq {
                keep_secs: Some(86_400),
                ..Default::default()
            },
            VacuumReq {
                max_bytes: Some(1 << 20),
                ..Default::default()
            },
        ] {
            let e = p.vacuum(&req).await.unwrap_err();
            assert_eq!(e.code, ErrorCode::CapabilityUnavailable);
            assert_eq!(e.capability.as_deref(), Some("eventlog"));
        }
        // 三个字段都不给同样要 400
        assert_eq!(
            p.vacuum(&VacuumReq::default()).await.unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    /// follow 能建立、能 drop，且不同过滤条件各自独立、相同条件共享同一个订阅。
    #[tokio::test]
    async fn 本机_follow_能建立并共享() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        let q = LogQuery::default();
        let a = p.follow(&q).await.expect("follow 应当能建立");
        {
            let map = p.follows.lock().unwrap();
            assert_eq!(map.len(), 1);
        }
        let b = p.follow(&q).await.unwrap();
        {
            let map = p.follows.lock().unwrap();
            assert_eq!(map.len(), 1, "同一过滤条件必须共享一个订阅");
        }
        // 不同过滤条件各起一个
        let _c = p
            .follow(&LogQuery {
                priority: Some(LogPriority::Err),
                ..Default::default()
            })
            .await
            .unwrap();
        {
            let map = p.follows.lock().unwrap();
            assert_eq!(map.len(), 2);
        }

        // 最后一个订阅者走掉后，弱引用失效（下一次 follow 会重建）
        drop(a);
        drop(b);
        let q2 = LogQuery::default();
        let _d = p.follow(&q2).await.unwrap();
        eprintln!("[eventlog] follow 建立 / 共享 / 释放均正常");
    }

    /// 订阅句柄与回调上下文的释放顺序：反复建立再释放不应崩溃或泄漏。
    #[tokio::test]
    async fn follow_反复建立释放() {
        let p = EventLog::new();
        if !p.probe().await.is_available() {
            return;
        }
        for _ in 0..8 {
            let f = p.follow(&LogQuery::default()).await.unwrap();
            drop(f);
        }
        // 走到这里没崩就说明 EvtClose → Arc::from_raw 的顺序是对的
        tokio::time::sleep(Duration::from_millis(50)).await;
        eprintln!("[eventlog] 订阅反复建立释放 8 次，未崩溃");
    }

    /// `limit` 的归一与越界由公共函数管，这里只确认它真的被调用了。
    #[tokio::test]
    async fn limit_越界被拒() {
        let p = EventLog::new();
        for limit in [Some(0), Some(100_000)] {
            let e = p
                .query(&LogQuery {
                    limit,
                    ..Default::default()
                })
                .await
                .unwrap_err();
            assert_eq!(e.code, ErrorCode::InvalidRequest);
        }
    }
}
