//! [`MetricsEngine`]：指标链路的对外句柄（`Clone`，内部 `Arc`）。
//!
//! 宿主（server / agent）用 [`MetricsEngine::start`] 起一个调度任务，然后把句柄
//! 交给 HTTP 路由与 WS 频道：
//!
//! - [`MetricsEngine::query`] —— `GET /api/v1/metrics/query`，跨度落在环内且 step 小时
//!   直接从环出（`live` 层），否则走 [`Store::query`] 自动选层；
//! - [`MetricsEngine::snapshot`] —— `GET /api/v1/metrics/current`；
//! - [`MetricsEngine::series_list`] —— `GET /api/v1/metrics/series`；
//! - [`MetricsEngine::subscribe`] —— WS `metrics.live` 的数据源（`broadcast`，慢消费者会 lag）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use strixmaid_types::ApiError;
use strixmaid_types::metrics::{
    MetricLayer, MetricPoint, MetricQuery, MetricQueryResp, MetricSeries, MetricSnapshot,
    SeriesMeta,
};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;

use super::catalog::{self, MetricDef};
use super::collect::{Collector, default_collectors};
use super::ring::{Point, RingStats, SeriesKey, summarize_points};
use super::scheduler::{RoundInfo, Scheduler, SchedulerConfig, Shared, read_lock};
use crate::config::MetricsConfig;
use crate::store::{MAX_QUERY_POINTS, Store, canonical_labels, now_unix};

/// 广播队列深度（快照个数）。WS 客户端落后超过这么多轮就会收到 lag 错误并丢帧。
const BROADCAST_CAPACITY: usize = 32;

/// `step` 缺省时的目标点数：`step = span / 1000`。
const DEFAULT_TARGET_POINTS: i64 = 1000;

/// 指标引擎句柄。
#[derive(Clone)]
pub struct MetricsEngine {
    inner: Arc<Inner>,
}

struct Inner {
    shared: Arc<Shared>,
    tx: broadcast::Sender<Arc<MetricSnapshot>>,
    store: Option<Store>,
    shutdown: watch::Sender<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // 最后一个句柄没了就让调度任务退出（它会把未满分钟落盘）。
        let _ = self.shutdown.send(true);
    }
}

/// `series` 参数里的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// 数字 id。
    Id(i64),
    /// `metric{labels}`，`labels` 已规范化。
    Key { metric: String, labels: String },
}

/// 解析 `GET /metrics/query` 的 `series` 参数：逗号分隔，每项为 id 或 `metric{k=v,k2=v2}`。
/// 花括号内的逗号不作分隔。
pub fn parse_selectors(input: &str) -> Result<Vec<Selector>, ApiError> {
    let mut items = Vec::new();
    let mut depth = 0u32;
    let mut cur = String::new();
    for c in input.chars() {
        match c {
            '{' => {
                depth += 1;
                cur.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if depth == 0 => items.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    items.push(cur);

    let mut out = Vec::new();
    for raw in items {
        let item = raw.trim();
        if item.is_empty() {
            continue;
        }
        if let Ok(id) = item.parse::<i64>() {
            out.push(Selector::Id(id));
            continue;
        }
        let (metric, labels) = match item.split_once('{') {
            None => (item, String::new()),
            Some((metric, rest)) => {
                let inner = rest.strip_suffix('}').ok_or_else(|| {
                    ApiError::invalid_request(format!("series 项 `{item}` 缺少右花括号"))
                })?;
                let mut pairs = Vec::new();
                for kv in inner.split(',') {
                    let kv = kv.trim();
                    if kv.is_empty() {
                        continue;
                    }
                    let (k, v) = kv.split_once('=').ok_or_else(|| {
                        ApiError::invalid_request(format!("标签 `{kv}` 应为 k=v 形式"))
                    })?;
                    pairs.push((k.trim(), v.trim()));
                }
                (metric.trim(), canonical_labels(&pairs))
            }
        };
        if metric.is_empty() {
            return Err(ApiError::invalid_request(format!(
                "series 项 `{item}` 缺少指标名"
            )));
        }
        out.push(Selector::Key {
            metric: metric.to_owned(),
            labels,
        });
    }
    Ok(out)
}

impl MetricsEngine {
    /// 用 design.md §7.1 的全部默认采集器启动调度任务。**必须在 tokio 运行时内调用。**
    ///
    /// `store` 为 `None` 时只保留内存环，不落盘（测试或无盘场景）。
    pub fn start(cfg: &MetricsConfig, store: Option<Store>) -> MetricsEngine {
        Self::start_with(SchedulerConfig::from_metrics(cfg), store, default_collectors())
    }

    /// 同上，但自定义调度参数与采集器集合。
    pub fn start_with(
        cfg: SchedulerConfig,
        store: Option<Store>,
        collectors: Vec<Box<dyn Collector>>,
    ) -> MetricsEngine {
        let (engine, scheduler, rx) = Self::build(cfg, store, collectors);
        let task = tokio::spawn(scheduler.run(rx));
        *engine.inner.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
        engine
    }

    /// 只组装、不 spawn。测试里手工 `tick` 调度器。
    pub(crate) fn build(
        cfg: SchedulerConfig,
        store: Option<Store>,
        collectors: Vec<Box<dyn Collector>>,
    ) -> (MetricsEngine, Scheduler, watch::Receiver<bool>) {
        let shared = Arc::new(Shared::new(cfg, now_unix()));
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        let (shutdown, rx) = watch::channel(false);
        let scheduler = Scheduler::new(Arc::clone(&shared), collectors, store.clone(), tx.clone());
        let engine = MetricsEngine {
            inner: Arc::new(Inner {
                shared,
                tx,
                store,
                shutdown,
                task: Mutex::new(None),
            }),
        };
        (engine, scheduler, rx)
    }

    /// 调度参数。
    pub fn config(&self) -> &SchedulerConfig {
        &self.inner.shared.cfg
    }

    /// 是否配置了落盘。
    pub fn has_store(&self) -> bool {
        self.inner.store.is_some()
    }

    /// 指标常量表。
    pub fn catalog() -> &'static [MetricDef] {
        catalog::CATALOG
    }

    /// 订阅每轮快照。返回的 receiver 落后超过 [`BROADCAST_CAPACITY`] 轮会收到
    /// `RecvError::Lagged(n)`，调用方应把它翻译成一条 `err` 帧而不是断开。
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<MetricSnapshot>> {
        self.inner.tx.subscribe()
    }

    /// 最新一轮的全部瞬时值。启动后第一轮之前是空的（`values` 为空、`ts` 为启动时刻）。
    pub fn snapshot(&self) -> Arc<MetricSnapshot> {
        Arc::clone(&read_lock(&self.inner.shared.snapshot))
    }

    /// 最近一轮采集的统计。
    pub fn last_round(&self) -> RoundInfo {
        read_lock(&self.inner.shared.last_round).clone()
    }

    /// 环形缓冲的内存占用。
    pub fn ring_stats(&self) -> RingStats {
        read_lock(&self.inner.shared.rings).stats()
    }

    /// 请求调度任务退出（不等待）。
    pub fn shutdown(&self) {
        let _ = self.inner.shutdown.send(true);
    }

    /// 请求退出并等待调度任务结束（它会把未满的分钟落盘）。
    pub async fn stop(&self) {
        self.shutdown();
        let task = self
            .inner
            .task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    // ------------------------------------------------------------ 查询

    /// 可用 series 列表。有 store 时以 `series` 表为准（含历史上出现过、现在不再采的），
    /// 否则来自内存环（此时 `id` 为 0）。
    pub async fn series_list(
        &self,
        node: Option<&str>,
        prefix: Option<&str>,
    ) -> Result<Vec<SeriesMeta>, ApiError> {
        let mut list = match &self.inner.store {
            Some(store) => store
                .list_series(node)
                .await
                .map_err(|e| ApiError::internal("读取 series 列表失败").with_detail(e.to_string()))?
                .into_iter()
                .map(|s| SeriesMeta {
                    id: s.id,
                    unit: s
                        .unit
                        .or_else(|| catalog::unit_of(&s.metric).map(str::to_owned)),
                    node: s.node,
                    metric: s.metric,
                    labels: s.labels,
                })
                .collect::<Vec<_>>(),
            None => {
                let own = self.inner.shared.cfg.node.as_str();
                if node.is_some_and(|n| n != own) {
                    Vec::new()
                } else {
                    let rings = read_lock(&self.inner.shared.rings);
                    let mut v: Vec<SeriesMeta> = rings
                        .iter()
                        .map(|(k, e)| SeriesMeta {
                            id: e.id.unwrap_or(0),
                            node: own.to_owned(),
                            metric: k.metric.clone(),
                            labels: k.labels.clone(),
                            unit: e.unit.map(str::to_owned),
                        })
                        .collect();
                    v.sort_by(|a, b| (&a.metric, &a.labels).cmp(&(&b.metric, &b.labels)));
                    v
                }
            }
        };
        if let Some(p) = prefix {
            list.retain(|s| s.metric.starts_with(p));
        }
        Ok(list)
    }

    /// `GET /api/v1/metrics/query`：自动选层查询。
    pub async fn query(&self, q: &MetricQuery) -> Result<MetricQueryResp, ApiError> {
        if q.to < q.from {
            return Err(ApiError::invalid_request(format!(
                "to ({}) 必须不小于 from ({})",
                q.to, q.from
            )));
        }
        let selectors = parse_selectors(&q.series)?;
        if selectors.is_empty() {
            return Err(ApiError::invalid_request("series 不能为空"));
        }
        let span = q.to - q.from;
        let step = match q.step {
            Some(s) if s > 0 => i64::from(s),
            _ => div_ceil(span, DEFAULT_TARGET_POINTS).max(1),
        };

        // 非本机节点没有内存环（roadmap/05 §3.3），一律走落盘层——`live` 在
        // 选层时被跳过，step 再小也如此。
        if let Some(node) = q.node.as_deref()
            && node != self.inner.shared.cfg.node
        {
            let store = self.inner.store.as_ref().ok_or_else(|| {
                ApiError::invalid_request(format!(
                    "节点 {node} 的数据只在落盘层，当前实例未配置落盘"
                ))
            })?;
            return self
                .query_store(store, node, &selectors, q.from, q.to, step)
                .await;
        }

        let shared = &self.inner.shared;
        let now = now_unix();
        // 环覆盖 from：环里确实有这么早的数据，且没被覆盖掉。
        let first_ts = shared.first_ts.load(std::sync::atomic::Ordering::Relaxed);
        let ring_covers_from = q.from >= first_ts && q.from >= now - shared.cfg.ring_secs as i64;
        let use_live = match &self.inner.store {
            None => true,
            Some(_) => step < 60 && ring_covers_from,
        };
        if use_live {
            return Ok(self.query_live(&selectors, q.from, q.to, step));
        }
        let store = self
            .inner
            .store
            .as_ref()
            .expect("use_live 为假时必有 store");
        let node = self.inner.shared.cfg.node.clone();
        self.query_store(store, &node, &selectors, q.from, q.to, step)
            .await
    }

    /// 从环出数据。`step` 取整到采集间隔的倍数；点数超过 [`MAX_QUERY_POINTS`] 时再放粗。
    fn query_live(&self, selectors: &[Selector], from: i64, to: i64, step: i64) -> MetricQueryResp {
        let shared = &self.inner.shared;
        let interval = shared.cfg.interval_secs();
        let span = to - from;
        let mut eff = div_ceil(step.max(interval), interval) * interval;
        if span / eff > MAX_QUERY_POINTS {
            eff = div_ceil(div_ceil(span, MAX_QUERY_POINTS), interval) * interval;
        }

        let rings = read_lock(&shared.rings);
        let node = shared.cfg.node.as_str();
        let mut series = Vec::with_capacity(selectors.len());
        for sel in selectors {
            let found = match sel {
                Selector::Id(id) => rings.find_by_id(*id),
                Selector::Key { metric, labels } => {
                    let key = SeriesKey::new(metric.as_str(), labels.as_str());
                    rings.get(&key).map(|e| {
                        // 借 key 的生命周期，避免再分配：这里只是用来构造 meta。
                        let stored = rings.iter().find(|(k, _)| **k == key).map(|(k, _)| k);
                        (stored.expect("刚查到的键一定存在"), e)
                    })
                }
            };
            let Some((key, entry)) = found else { continue };
            let points = entry.ring.range(from, to);
            let points = if eff == interval {
                points
                    .into_iter()
                    .map(|p| MetricPoint {
                        ts: p.ts,
                        cnt: 1,
                        min: p.value,
                        max: p.value,
                        sum: p.value,
                        med: p.value,
                    })
                    .collect()
            } else {
                downsample(&points, eff)
            };
            series.push(MetricSeries {
                meta: SeriesMeta {
                    id: entry.id.unwrap_or(0),
                    node: node.to_owned(),
                    metric: key.metric.clone(),
                    labels: key.labels.clone(),
                    unit: entry.unit.map(str::to_owned),
                },
                points,
            });
        }
        MetricQueryResp {
            from,
            to,
            layer: MetricLayer::Live,
            step: u32::try_from(eff).unwrap_or(u32::MAX),
            series,
        }
    }

    /// 走落盘数据，[`Store::query`] 自动选层。`node` 决定名字型选择器解析到
    /// 哪个节点的 series；数字 id 型选择器不经解析、按调用方给的用。
    async fn query_store(
        &self,
        store: &Store,
        node: &str,
        selectors: &[Selector],
        from: i64,
        to: i64,
        step: i64,
    ) -> Result<MetricQueryResp, ApiError> {
        let internal = |what: &str, e: crate::store::StoreError| {
            ApiError::internal(what.to_owned()).with_detail(e.to_string())
        };
        let mut ids: Vec<i64> = Vec::with_capacity(selectors.len());
        for sel in selectors {
            let id = match sel {
                Selector::Id(id) => Some(*id),
                Selector::Key { metric, labels } => store
                    .find_series(node, metric, labels)
                    .await
                    .map_err(|e| internal("查找 series 失败", e))?,
            };
            if let Some(id) = id
                && !ids.contains(&id)
            {
                ids.push(id);
            }
        }
        let result = store
            .query(&ids, from, to, step)
            .await
            .map_err(|e| internal("查询指标失败", e))?;

        let mut grouped: HashMap<i64, Vec<MetricPoint>> = HashMap::new();
        for r in result.rows {
            grouped.entry(r.series_id).or_default().push(MetricPoint {
                ts: r.ts,
                cnt: u32::try_from(r.cnt).unwrap_or(u32::MAX),
                min: r.min,
                max: r.max,
                sum: r.sum,
                med: r.med,
            });
        }
        let mut series = Vec::with_capacity(grouped.len());
        for id in ids {
            let Some(points) = grouped.remove(&id) else {
                continue;
            };
            let Some(s) = store
                .get_series(id)
                .await
                .map_err(|e| internal("读取 series 失败", e))?
            else {
                continue;
            };
            series.push(MetricSeries {
                meta: SeriesMeta {
                    id: s.id,
                    unit: s
                        .unit
                        .or_else(|| catalog::unit_of(&s.metric).map(str::to_owned)),
                    node: s.node,
                    metric: s.metric,
                    labels: s.labels,
                },
                points,
            });
        }
        Ok(MetricQueryResp {
            from,
            to,
            layer: result.layer,
            step: u32::try_from(result.step).unwrap_or(u32::MAX),
            series,
        })
    }
}

/// 非负数的向上取整除法（`i64::div_ceil` 尚未稳定）。`b` 必须为正。
fn div_ceil(a: i64, b: i64) -> i64 {
    (a + b - 1).div_euclid(b)
}

/// 把点按 `step` 宽的桶（按 epoch 对齐）聚成 cnt/min/max/sum/med。输入须按时间升序。
fn downsample(points: &[Point], step: i64) -> Vec<MetricPoint> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < points.len() {
        let bucket = points[i].ts.div_euclid(step) * step;
        let end = bucket + step;
        let mut j = i;
        while j < points.len() && points[j].ts < end {
            j += 1;
        }
        if let Some(b) = summarize_points(&points[i..j]) {
            out.push(MetricPoint {
                ts: bucket,
                cnt: b.cnt,
                min: b.min,
                max: b.max,
                sum: b.sum,
                med: b.med,
            });
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::scheduler::tests::{SeqCollector, test_config};
    use super::*;
    use std::time::Instant;

    #[test]
    fn 选择器解析() {
        let v = parse_selectors("cpu.usage, 42 ,disk.read_bytes{dev=sda},fs.used{mount=/,x=1}")
            .unwrap();
        assert_eq!(
            v,
            vec![
                Selector::Key {
                    metric: "cpu.usage".into(),
                    labels: String::new()
                },
                Selector::Id(42),
                Selector::Key {
                    metric: "disk.read_bytes".into(),
                    labels: "dev=sda".into()
                },
                Selector::Key {
                    metric: "fs.used".into(),
                    labels: "mount=/,x=1".into()
                },
            ]
        );
        // 标签按键排序
        let v = parse_selectors("m{b=2,a=1}").unwrap();
        assert_eq!(
            v[0],
            Selector::Key {
                metric: "m".into(),
                labels: "a=1,b=2".into()
            }
        );
        assert!(parse_selectors("m{a=1").is_err());
        assert!(parse_selectors("m{a}").is_err());
        assert!(parse_selectors("{a=1}").is_err());
        assert!(parse_selectors(" , ").unwrap().is_empty());
    }

    #[test]
    fn 降采样() {
        let pts: Vec<Point> = (0..10)
            .map(|i| Point {
                ts: 100 + i * 2,
                value: i as f64,
            })
            .collect();
        // step 6，桶按 epoch 对齐：[96,102) [102,108) [108,114) [114,120)
        let out = downsample(&pts, 6);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].ts, 96);
        assert_eq!(out[0].cnt, 1);
        assert_eq!(out[1].ts, 102);
        assert_eq!(out[1].cnt, 3);
        assert_eq!(out[1].med, 2.0);
        assert_eq!(out[1].sum, 1.0 + 2.0 + 3.0);
        assert!(downsample(&[], 6).is_empty());
    }

    /// 让调度器在 `[start, end)` 内按 2s 一轮跑满。
    async fn fill(scheduler: &mut Scheduler, start: i64, end: i64) {
        let t0 = Instant::now();
        let mut ts = start;
        while ts < end {
            scheduler.tick(ts, t0).await.unwrap();
            ts += 2;
        }
    }

    #[tokio::test]
    async fn 无_store_时始终走_live() {
        let (engine, mut s, _rx) =
            MetricsEngine::build(test_config(), None, vec![Box::new(SeqCollector::new())]);
        let now = now_unix();
        let start = now - 120;
        fill(&mut s, start, now).await;
        assert!(!engine.has_store());

        let resp = engine
            .query(&MetricQuery {
                series: "test.value,test.labeled{k=v},nope".into(),
                from: start,
                to: now,
                step: Some(2),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.layer, MetricLayer::Live);
        assert_eq!(resp.step, 2);
        assert_eq!(resp.series.len(), 2, "查无此序列的不出现");
        assert_eq!(resp.series[0].points.len(), 60);
        assert_eq!(resp.series[0].points[0].cnt, 1);
        assert_eq!(resp.series[1].meta.labels, "k=v");

        // step 10 → 每桶 5 点
        let resp = engine
            .query(&MetricQuery {
                series: "test.value".into(),
                from: start,
                to: now,
                step: Some(10),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.step, 10);
        assert!(resp.series[0].points.iter().all(|p| p.cnt <= 5));
        let total: u32 = resp.series[0].points.iter().map(|p| p.cnt).sum();
        assert_eq!(total, 60);

        // step 3 取整到间隔的倍数 4
        let resp = engine
            .query(&MetricQuery {
                series: "test.value".into(),
                from: start,
                to: now,
                step: Some(3),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.step, 4);

        // 快照与 series 列表
        assert_eq!(engine.snapshot().values.len(), 2);
        let list = engine.series_list(None, Some("test.l")).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].metric, "test.labeled");
        assert!(
            engine
                .series_list(Some("other"), None)
                .await
                .unwrap()
                .is_empty()
        );
        let stats = engine.ring_stats();
        assert_eq!(stats.series, 2);
        assert_eq!(stats.points, 120);
        assert_eq!(stats.bytes, 2 * 1800 * 16);
    }

    #[tokio::test]
    async fn 有_store_时按跨度与_step_选路() {
        let store = Store::open_in_memory().await.unwrap();
        let (engine, mut s, _rx) = MetricsEngine::build(
            test_config(),
            Some(store.clone()),
            vec![Box::new(SeqCollector::new())],
        );
        // 数据从 5 分钟前开始，跑到现在；经过 4 次分钟切换 → m_1m 里有 4 行。
        //
        // `now` **必须**对齐到整分：不对齐时落进 m_1m 的行数会随当前墙上时刻变化。
        // 实测 60 个偏移里恰好有一个（start ≡ 1 mod 60）只得到 3 行，
        // 于是这个测试会以 1/60 的概率无故失败。对齐掉这个自由度，它才是在
        // 测「选路」，而不是在测「今天几点跑的 CI」。
        let now = now_unix();
        let now = now - now.rem_euclid(60);
        let start = now - 300;
        fill(&mut s, start, now).await;

        // 近 2 分钟、step 2：跨度在环内且 step 小 → live
        let resp = engine
            .query(&MetricQuery {
                series: "test.value".into(),
                from: now - 120,
                to: now,
                step: Some(2),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.layer, MetricLayer::Live);
        assert!(!resp.series[0].points.is_empty());
        assert_eq!(
            resp.series[0].meta.id,
            store
                .find_series("local", "test.value", "")
                .await
                .unwrap()
                .unwrap()
        );

        // step 60 → 走 store 的 m_1m
        let resp = engine
            .query(&MetricQuery {
                series: "test.value".into(),
                from: start,
                to: now,
                step: Some(60),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.layer, MetricLayer::M1m);
        assert_eq!(resp.step, 60);
        assert!(
            resp.series[0].points.len() >= 4,
            "实际 {}",
            resp.series[0].points.len()
        );
        assert!(
            resp.series[0]
                .points
                .iter()
                .all(|p| p.cnt >= 1 && p.cnt <= 30)
        );

        // 早于环里最早数据的 from → store，即使 step 小
        let resp = engine
            .query(&MetricQuery {
                series: "test.value".into(),
                from: now - 7200,
                to: now,
                step: Some(2),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(resp.layer, MetricLayer::M1m);

        // 按 id 查
        let id = resp.series[0].meta.id;
        let by_id = engine
            .query(&MetricQuery {
                series: id.to_string(),
                from: start,
                to: now,
                step: Some(60),
                node: None,
            })
            .await
            .unwrap();
        assert_eq!(by_id.series[0].meta.metric, "test.value");

        // series 列表来自 series 表，带 id
        let list = engine.series_list(Some("local"), None).await.unwrap();
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|m| m.id > 0));

        // 参数错误
        assert!(
            engine
                .query(&MetricQuery {
                    series: "test.value".into(),
                    from: 10,
                    to: 5,
                    step: None,
                    node: None,
                })
                .await
                .is_err()
        );
        assert!(
            engine
                .query(&MetricQuery {
                    series: String::new(),
                    from: 0,
                    to: 5,
                    step: None,
                    node: None,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn 非本机节点的查询走落盘层() {
        let store = Store::open_in_memory().await.unwrap();
        let id = store
            .get_or_create_series("web-01", "cpu.usage", &[], Some("percent"))
            .await
            .unwrap();
        store
            .insert_1m(&[crate::store::MetricRow {
                series_id: id,
                ts: 60,
                cnt: 1,
                min: 1.0,
                max: 1.0,
                sum: 1.0,
                med: 1.0,
            }])
            .await
            .unwrap();
        let (engine, _s, _rx) = MetricsEngine::build(
            test_config(),
            Some(store),
            vec![Box::new(SeqCollector::new())],
        );

        // step 再小也不走 live：非本机节点没有环。
        let resp = engine
            .query(&MetricQuery {
                series: "cpu.usage".into(),
                from: 0,
                to: 300,
                step: Some(2),
                node: Some("web-01".into()),
            })
            .await
            .unwrap();
        assert_eq!(resp.layer, MetricLayer::M1m);
        assert_eq!(resp.series.len(), 1);
        assert_eq!(resp.series[0].meta.node, "web-01");
        assert_eq!(resp.series[0].points.len(), 1);

        // 名字解析按节点隔离：local 没有这条 series。
        let resp = engine
            .query(&MetricQuery {
                series: "cpu.usage".into(),
                from: 0,
                to: 300,
                step: Some(60),
                node: Some("local".into()),
            })
            .await
            .unwrap();
        assert!(resp.series.is_empty());
    }

    /// 本机真实采集两轮，打印样本数 / 耗时 / 环内存（`--nocapture` 可见），并做基本断言。
    #[tokio::test]
    async fn 本机一轮采集统计() {
        let (engine, mut s, _rx) = MetricsEngine::build(test_config(), None, default_collectors());
        let now = now_unix();
        let t0 = Instant::now();
        s.tick(now - 2, t0).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        s.tick(now, Instant::now()).await.unwrap();
        let round = engine.last_round();
        let stats = engine.ring_stats();
        eprintln!(
            "本机第二轮：{} 个样本，采集耗时 {:.2} ms，失败 {:?}；环：{} series / {} 点 / {} 字节（容量 {} 点/环）",
            round.samples,
            round.duration_ms,
            round.errors,
            stats.series,
            stats.points,
            stats.bytes,
            engine.config().ring_capacity()
        );
        assert!(round.samples >= 20, "至少有 cpu/mem/load 这些基础项");
        assert!(
            round.errors.is_empty(),
            "本机采集不应失败: {:?}",
            round.errors
        );
        assert_eq!(stats.bytes, stats.series * 1800 * 16);
        assert!(round.duration_ms < 1000.0);
    }


    #[tokio::test]
    async fn start_与_stop() {
        let engine = MetricsEngine::start_with(
            SchedulerConfig {
                interval: std::time::Duration::from_millis(50),
                ..test_config()
            },
            None,
            vec![Box::new(SeqCollector::new())],
        );
        let mut rx = engine.subscribe();
        let snap = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("5s 内应收到一轮")
            .unwrap();
        assert_eq!(snap.values.len(), 2);
        engine.stop().await;
        assert!(engine.last_round().samples >= 2);
    }
}
