//! 采集调度：按 `interval_secs` 跑全部采集器 → 写环 → 广播快照 → 每分钟落盘。
//!
//! # 一轮的步骤（[`Scheduler::tick`]）
//!
//! 1. 在 `spawn_blocking` 里依次跑所有采集器。整轮带超时；上一轮还没结束
//!    （某个 `statvfs` 挂在坏掉的挂载上）时**跳过本轮**而不是排队，
//!    这样调度循环永远不会被采集拖死。单个采集器失败只记 warn。
//! 2. 样本写入 [`RingSet`]，同时生成本轮 [`MetricSnapshot`] 通过 `broadcast` 发出去
//!    （WS `metrics.live` 频道从这里取数据）。
//! 3. 新出现的 series 到 `series` 表注册拿 id。
//! 4. 分钟切换时，把上一整分钟的环数据算成 [`MetricRow`]（cnt / min / max / sum / med）
//!    写 `m_1m`，再调 [`Store::maintain`] 做逐级聚合与保留期清理。
//!
//! 落盘失败不推进「已落盘分钟」，下一轮重试；环至少覆盖 1 分钟
//! （配置校验保证 `ring_secs >= 60`），所以短暂的数据库故障不会丢数据。

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::{Duration, Instant};

use strixmaid_types::metrics::{MetricSnapshot, MetricValue};
use tokio::sync::{broadcast, watch};

use super::catalog;
use super::collect::{CollectError, Collector, Sample};
use super::ring::{Point, RingSet, SeriesKey, summarize_points};
use crate::config::MetricsConfig;
use crate::store::{MetricRow, Store, now_unix};

/// 整轮采集的超时下限。
const MIN_COLLECT_TIMEOUT: Duration = Duration::from_secs(10);

/// 调度参数。从 [`MetricsConfig`] 派生，测试里也可以手工构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// 采集间隔。
    pub interval: Duration,
    /// 环形缓冲覆盖时长（秒）。
    pub ring_secs: u64,
    /// 本机节点 id，写进 `series.node`。
    pub node: String,
    /// 整轮采集的超时；超时后本轮作废、采集线程留在后台自生自灭。
    pub collect_timeout: Duration,
}

impl SchedulerConfig {
    /// 从配置派生：节点固定为 [`super::LOCAL_NODE`]，超时取 `max(10s, 3 × 间隔)`。
    pub fn from_metrics(cfg: &MetricsConfig) -> Self {
        let interval = cfg.interval();
        SchedulerConfig {
            interval,
            ring_secs: cfg.ring_secs,
            node: super::LOCAL_NODE.to_owned(),
            collect_timeout: MIN_COLLECT_TIMEOUT.max(interval * 3),
        }
    }

    /// 采集间隔的整数秒，至少 1。
    pub fn interval_secs(&self) -> i64 {
        self.interval.as_secs().max(1) as i64
    }

    /// 每个环的容量 = `ring_secs / interval_secs`（向上取整）。
    pub fn ring_capacity(&self) -> usize {
        self.ring_secs.div_ceil(self.interval.as_secs().max(1)) as usize
    }
}

impl From<&MetricsConfig> for SchedulerConfig {
    fn from(cfg: &MetricsConfig) -> Self {
        SchedulerConfig::from_metrics(cfg)
    }
}

/// 最近一轮采集的统计，供调试页与报告使用。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct RoundInfo {
    /// 采样时刻。
    pub ts: i64,
    /// 样本数。
    pub samples: usize,
    /// 全部采集器耗时（毫秒）。
    pub duration_ms: f64,
    /// 失败的采集器及原因。
    pub errors: Vec<String>,
}

/// 调度器与 [`super::MetricsEngine`] 之间共享的状态。
pub(crate) struct Shared {
    pub cfg: SchedulerConfig,
    pub rings: RwLock<RingSet>,
    pub snapshot: RwLock<Arc<MetricSnapshot>>,
    pub last_round: RwLock<RoundInfo>,
    pub rounds: AtomicU64,
    /// 环里最早一个点的时间戳；还没有任何数据时为 `i64::MAX`。
    /// 查询据此判断「跨度是否落在环的覆盖范围内」。
    pub first_ts: AtomicI64,
}

impl Shared {
    /// `started_at` 只用作首轮采集前那个空快照的 `ts`。
    pub fn new(cfg: SchedulerConfig, started_at: i64) -> Shared {
        let cap = cfg.ring_capacity();
        Shared {
            cfg,
            rings: RwLock::new(RingSet::new(cap)),
            snapshot: RwLock::new(Arc::new(MetricSnapshot {
                ts: started_at,
                values: Vec::new(),
            })),
            last_round: RwLock::new(RoundInfo::default()),
            rounds: AtomicU64::new(0),
            first_ts: AtomicI64::new(i64::MAX),
        }
    }
}

/// 读锁；中毒（持锁线程 panic）时照常取出内容——环里只是数字，不会处于半更新状态。
pub(crate) fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

/// 写锁，同上。
pub(crate) fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// 采集调度器。由 [`super::MetricsEngine::start`] 创建并 spawn；测试里直接调 [`Scheduler::tick`]。
pub struct Scheduler {
    shared: Arc<Shared>,
    collectors: Arc<Mutex<Vec<Box<dyn Collector>>>>,
    store: Option<Store>,
    tx: broadcast::Sender<Arc<MetricSnapshot>>,
    /// 已经写进 `m_1m` 的最后一个分钟号（`ts / 60`）。`None` 表示还没跑过。
    last_flushed_minute: Option<i64>,
}

/// 一轮采集的原始产出。
struct Round {
    samples: Vec<Sample>,
    duration: Duration,
    errors: Vec<CollectError>,
}

impl Scheduler {
    pub(crate) fn new(
        shared: Arc<Shared>,
        collectors: Vec<Box<dyn Collector>>,
        store: Option<Store>,
        tx: broadcast::Sender<Arc<MetricSnapshot>>,
    ) -> Scheduler {
        Scheduler {
            shared,
            collectors: Arc::new(Mutex::new(collectors)),
            store,
            tx,
            last_flushed_minute: None,
        }
    }

    /// 主循环：按间隔 tick，直到 `shutdown` 变为 `true` 或发送端被丢弃；
    /// 退出前把未满的分钟也落盘。
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(self.shared.cfg.interval);
        // 落盘慢了就跳过错过的 tick，不要连发一串补偿采集。
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tracing::info!(
            interval_secs = self.shared.cfg.interval.as_secs(),
            ring_secs = self.shared.cfg.ring_secs,
            ring_capacity = self.shared.cfg.ring_capacity(),
            persist = self.store.is_some(),
            "指标调度器启动"
        );
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.tick(now_unix(), Instant::now()).await;
                }
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() {
                        break;
                    }
                }
            }
        }
        let rows = self.flush(now_unix(), true).await;
        tracing::info!(rows, "指标调度器已停止");
    }

    /// 跑一轮。返回本轮快照；采集整轮失败（超时 / 上一轮未结束）时返回 `None`。
    pub async fn tick(&mut self, now_unix: i64, now_mono: Instant) -> Option<Arc<MetricSnapshot>> {
        let round = self.collect_round(now_mono).await?;
        for e in &round.errors {
            tracing::warn!(collector = e.collector, error = %e.message, "采集器失败，本轮跳过该项");
        }
        let snap = self.absorb(now_unix, round);
        // 没有订阅者时 send 返回 Err，是常态，不是错误。
        let _ = self.tx.send(Arc::clone(&snap));
        if self.store.is_some() {
            self.register_series().await;
            self.flush(now_unix, false).await;
        }
        Some(snap)
    }

    /// 在阻塞线程里跑全部采集器，带超时。
    async fn collect_round(&self, now_mono: Instant) -> Option<Round> {
        let collectors = Arc::clone(&self.collectors);
        let handle = tokio::task::spawn_blocking(move || {
            let mut guard = match collectors.try_lock() {
                Ok(g) => g,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => return None,
            };
            let started = Instant::now();
            let mut samples = Vec::with_capacity(256);
            let mut errors = Vec::new();
            for c in guard.iter_mut() {
                match c.collect(now_mono) {
                    Ok(s) => samples.extend(s),
                    Err(e) => errors.push(e),
                }
            }
            Some(Round {
                samples,
                duration: started.elapsed(),
                errors,
            })
        });
        match tokio::time::timeout(self.shared.cfg.collect_timeout, handle).await {
            Ok(Ok(Some(round))) => Some(round),
            Ok(Ok(None)) => {
                tracing::warn!("上一轮采集仍未结束，跳过本轮");
                None
            }
            Ok(Err(join)) => {
                tracing::warn!(error = %join, "采集线程异常退出，跳过本轮");
                None
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = self.shared.cfg.collect_timeout.as_secs(),
                    "整轮采集超时，跳过本轮（采集线程留在后台）"
                );
                None
            }
        }
    }

    /// 样本入环并生成快照。
    fn absorb(&self, now_unix: i64, round: Round) -> Arc<MetricSnapshot> {
        let mut values = Vec::with_capacity(round.samples.len());
        {
            let mut rings = write_lock(&self.shared.rings);
            for s in round.samples {
                let labels = s.canonical_labels();
                let unit = catalog::unit_of(s.metric);
                let point = Point {
                    ts: now_unix,
                    value: s.value,
                };
                if !rings.push(SeriesKey::new(s.metric, labels.clone()), unit, point) {
                    tracing::debug!(
                        metric = s.metric,
                        labels,
                        "时间戳早于环内最新点，丢弃（时钟被回拨？）"
                    );
                    continue;
                }
                values.push(MetricValue {
                    metric: s.metric.to_owned(),
                    labels,
                    value: s.value,
                    unit: unit.map(str::to_owned),
                });
            }
        }
        let snap = Arc::new(MetricSnapshot {
            ts: now_unix,
            values,
        });
        *write_lock(&self.shared.snapshot) = Arc::clone(&snap);
        *write_lock(&self.shared.last_round) = RoundInfo {
            ts: now_unix,
            samples: snap.values.len(),
            duration_ms: round.duration.as_secs_f64() * 1000.0,
            errors: round.errors.iter().map(ToString::to_string).collect(),
        };
        self.shared.rounds.fetch_add(1, Ordering::Relaxed);
        if !snap.values.is_empty() {
            self.shared.first_ts.fetch_min(now_unix, Ordering::Relaxed);
        }
        snap
    }

    /// 给还没有 id 的 series 到 `series` 表注册。失败只记 warn，下一轮再试。
    async fn register_series(&self) {
        let Some(store) = &self.store else { return };
        let pending: Vec<(SeriesKey, Option<&'static str>)> = read_lock(&self.shared.rings)
            .iter()
            .filter(|(_, e)| e.id.is_none())
            .map(|(k, e)| (k.clone(), e.unit))
            .collect();
        if pending.is_empty() {
            return;
        }
        let mut resolved = Vec::with_capacity(pending.len());
        for (key, unit) in pending {
            match store
                .get_or_create_series_raw(&self.shared.cfg.node, &key.metric, &key.labels, unit)
                .await
            {
                Ok(id) => resolved.push((key, id)),
                Err(e) => {
                    tracing::warn!(metric = %key.metric, labels = %key.labels, error = %e, "注册 series 失败，稍后重试");
                    break;
                }
            }
        }
        let mut rings = write_lock(&self.shared.rings);
        for (key, id) in resolved {
            if let Some(e) = rings.get_mut(&key) {
                e.id = Some(id);
            }
        }
    }

    /// 把已经完整结束的分钟落盘；`include_partial` 为真时连当前这个未满的分钟也写
    /// （进程退出前用）。返回写入的行数。
    pub async fn flush(&mut self, now_unix: i64, include_partial: bool) -> u64 {
        let Some(store) = &self.store else { return 0 };
        let cur_min = now_unix.div_euclid(60);
        // 第一次调用：从「启动所在的分钟」开始算，它虽然不满也要写（cnt 会如实反映）。
        let last = *self.last_flushed_minute.get_or_insert(cur_min - 1);
        let target = if include_partial {
            cur_min
        } else {
            cur_min - 1
        };
        if target <= last {
            return 0;
        }
        // 进程被挂起很久后，环里根本没有那么早的数据，别白跑几百次空查询。
        let ring_minutes = (self.shared.cfg.ring_secs / 60) as i64;
        let first = (last + 1).max(cur_min - ring_minutes);

        let mut written = 0;
        let mut any = false;
        for minute in first..=target {
            let rows = rows_for_minute(&read_lock(&self.shared.rings), minute * 60);
            if rows.is_empty() {
                continue;
            }
            match store.insert_1m(&rows).await {
                Ok(n) => {
                    written += n;
                    any = true;
                    tracing::debug!(minute_ts = minute * 60, rows = rows.len(), "m_1m 落盘");
                }
                Err(e) => {
                    // 不推进 last_flushed_minute，下一轮从这里重来。
                    tracing::warn!(minute_ts = minute * 60, error = %e, "m_1m 落盘失败，下一轮重试");
                    return written;
                }
            }
        }
        self.last_flushed_minute = Some(target);

        if any {
            match store.maintain(now_unix).await {
                Ok((rolled, pruned)) => {
                    tracing::debug!(rolled, pruned, "分层聚合与清理完成");
                }
                Err(e) => tracing::warn!(error = %e, "分层聚合 / 清理失败"),
            }
        }
        written
    }
}

/// 把环里 `[minute_ts, minute_ts + 60)` 的点算成 `m_1m` 行。没有 id 的 series 跳过。
pub(crate) fn rows_for_minute(rings: &RingSet, minute_ts: i64) -> Vec<MetricRow> {
    let mut rows = Vec::with_capacity(rings.len());
    for (_, entry) in rings.iter() {
        let Some(id) = entry.id else { continue };
        let points = entry.ring.range(minute_ts, minute_ts + 60);
        if let Some(b) = summarize_points(&points) {
            rows.push(MetricRow {
                series_id: id,
                ts: minute_ts,
                cnt: i64::from(b.cnt),
                min: b.min,
                max: b.max,
                sum: b.sum,
                med: b.med,
            });
        }
    }
    rows
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::MetricLayer;
    use sqlx::Row as _;

    /// 每轮吐一个递增值的假采集器：`test.value` 从 1 开始，`test.labeled{k=v}` 是它的两倍。
    pub struct SeqCollector {
        next: f64,
    }

    impl SeqCollector {
        pub fn new() -> Self {
            SeqCollector { next: 1.0 }
        }
    }

    impl Collector for SeqCollector {
        fn name(&self) -> &'static str {
            "seq"
        }

        fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
            let v = self.next;
            self.next += 1.0;
            Ok(vec![
                Sample::new("test.value", v),
                Sample::labeled("test.labeled", "k", "v", v * 2.0),
            ])
        }
    }

    /// 永远失败的采集器。
    struct BrokenCollector;

    impl Collector for BrokenCollector {
        fn name(&self) -> &'static str {
            "broken"
        }

        fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
            Err(CollectError::new("broken", "故意失败"))
        }
    }

    pub fn test_config() -> SchedulerConfig {
        SchedulerConfig {
            interval: Duration::from_secs(2),
            ring_secs: 3600,
            node: "local".into(),
            collect_timeout: Duration::from_secs(5),
        }
    }

    fn scheduler(store: Option<Store>, collectors: Vec<Box<dyn Collector>>) -> Scheduler {
        let shared = Arc::new(Shared::new(test_config(), 0));
        let (tx, _rx) = broadcast::channel(8);
        Scheduler::new(shared, collectors, store, tx)
    }

    async fn m1m_row(store: &Store, metric: &str, labels: &str, ts: i64) -> Option<MetricRow> {
        let id = store.find_series("local", metric, labels).await.unwrap()?;
        let row = sqlx::query(
            r#"SELECT cnt, "min", "max", "sum", med FROM m_1m WHERE series_id = ? AND ts = ?"#,
        )
        .bind(id)
        .bind(ts)
        .fetch_optional(store.read_pool())
        .await
        .unwrap()?;
        Some(MetricRow {
            series_id: id,
            ts,
            cnt: row.get("cnt"),
            min: row.get("min"),
            max: row.get("max"),
            sum: row.get("sum"),
            med: row.get("med"),
        })
    }

    #[tokio::test]
    async fn 每分钟落盘_偶数个样本() {
        let store = Store::open_in_memory().await.unwrap();
        let mut s = scheduler(Some(store.clone()), vec![Box::new(SeqCollector::new())]);
        let t0 = Instant::now();
        // 第 0 分钟：ts = 0, 2, ..., 58 共 30 个样本，值 1..=30
        for i in 0..30 {
            let snap = s.tick(i * 2, t0).await.expect("采集成功");
            assert_eq!(snap.values.len(), 2);
        }
        assert!(
            m1m_row(&store, "test.value", "", 0).await.is_none(),
            "分钟未结束不落盘"
        );
        // 跨到第 1 分钟 → 第 0 分钟落盘
        s.tick(60, t0).await.unwrap();
        let row = m1m_row(&store, "test.value", "", 0)
            .await
            .expect("m_1m 应有行");
        assert_eq!(row.cnt, 30);
        assert_eq!(row.min, 1.0);
        assert_eq!(row.max, 30.0);
        assert_eq!(row.sum, 465.0);
        assert_eq!(row.med, 15.5, "偶数个：中间两位的平均");
        let labeled = m1m_row(&store, "test.labeled", "k=v", 0).await.unwrap();
        assert_eq!(labeled.med, 31.0);
        assert_eq!(labeled.cnt, 30);
        // 同一分钟内再 tick 不会重复写
        assert_eq!(s.flush(62, false).await, 0);
        assert_eq!(
            store
                .count_tier(MetricLayer::M1m, row.series_id)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn 每分钟落盘_奇数个样本() {
        let store = Store::open_in_memory().await.unwrap();
        let mut s = scheduler(Some(store.clone()), vec![Box::new(SeqCollector::new())]);
        let t0 = Instant::now();
        // ts = 2, 4, ..., 58 共 29 个样本，值 1..=29
        for i in 1..30 {
            s.tick(i * 2, t0).await.unwrap();
        }
        s.tick(60, t0).await.unwrap();
        let row = m1m_row(&store, "test.value", "", 0).await.unwrap();
        assert_eq!(row.cnt, 29);
        assert_eq!(row.med, 15.0, "奇数个：正中间那个");
        assert_eq!(row.sum, 435.0);
    }

    #[tokio::test]
    async fn 退出时未满分钟也落盘() {
        let store = Store::open_in_memory().await.unwrap();
        let mut s = scheduler(Some(store.clone()), vec![Box::new(SeqCollector::new())]);
        let t0 = Instant::now();
        for i in 0..5 {
            s.tick(120 + i * 2, t0).await.unwrap();
        }
        assert_eq!(s.flush(128, true).await, 2, "两条 series 各一行");
        let row = m1m_row(&store, "test.value", "", 120).await.unwrap();
        assert_eq!(row.cnt, 5);
        assert_eq!(row.med, 3.0);
    }

    #[tokio::test]
    async fn 采集器失败只记录不中断() {
        let mut s = scheduler(
            None,
            vec![Box::new(BrokenCollector), Box::new(SeqCollector::new())],
        );
        let snap = s.tick(10, Instant::now()).await.expect("整轮不应失败");
        assert_eq!(snap.values.len(), 2, "好的采集器照常产出");
        let info = read_lock(&s.shared.last_round).clone();
        assert_eq!(info.samples, 2);
        assert_eq!(info.errors.len(), 1);
        assert!(info.errors[0].contains("broken"));
        assert_eq!(s.shared.rounds.load(Ordering::Relaxed), 1);
        // 无 store 时 flush 是空操作
        assert_eq!(s.flush(70, true).await, 0);
    }

    #[tokio::test]
    async fn 广播本轮快照() {
        let mut s = scheduler(None, vec![Box::new(SeqCollector::new())]);
        let mut rx = s.tx.subscribe();
        s.tick(4, Instant::now()).await.unwrap();
        let snap = rx.recv().await.unwrap();
        assert_eq!(snap.ts, 4);
        assert_eq!(snap.values[0].metric, "test.value");
        assert_eq!(snap.values[1].labels, "k=v");
    }

    #[test]
    fn 配置派生() {
        let cfg = SchedulerConfig::from_metrics(&MetricsConfig::default());
        assert_eq!(cfg.interval_secs(), 2);
        assert_eq!(cfg.ring_capacity(), 1800);
        assert_eq!(cfg.node, "local");
        assert_eq!(cfg.collect_timeout, Duration::from_secs(10));
        let slow = SchedulerConfig::from_metrics(&MetricsConfig {
            interval_secs: 60,
            ring_secs: 60,
            ..MetricsConfig::default()
        });
        assert_eq!(slow.ring_capacity(), 1);
        assert_eq!(slow.collect_timeout, Duration::from_secs(180));
    }
}
