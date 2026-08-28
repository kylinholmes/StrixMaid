//! 时序指标的写入、分层聚合、保留期清理与自动选层查询。
//!
//! 聚合规则严格照 design.md §7.2：
//!
//! ```text
//! min = MIN(min)
//! max = MAX(max)
//! sum = SUM(sum)          精确，无浮点累积误差
//! cnt = SUM(cnt)
//! med = MEDIAN(med)       子桶中位数的中位数，近似
//! ```
//!
//! 展示时 `avg = sum / cnt`。存 sum 而非 avg，使逐级聚合完全精确。

use sqlx::Row;

use super::{MetricLayer, Result, RetentionPreset, Store, StoreError, TIERS, TierSpec};

/// 一个桶。既是写入的输入，也是查询的输出。
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MetricRow {
    /// 所属 series。
    pub series_id: i64,
    /// 桶起始时间，unix 秒。
    pub ts: i64,
    /// 实际采样点数，用于加权聚合与缺失检测。
    pub cnt: i64,
    /// 桶内最小值。
    pub min: f64,
    /// 桶内最大值。
    pub max: f64,
    /// 桶内累加值；`avg = sum / cnt`。
    pub sum: f64,
    /// 桶内中位数；粗粒度层为 median of medians。
    pub med: f64,
}

impl MetricRow {
    /// `avg = sum / cnt`；`cnt` 为 0 时返回 0。
    pub fn avg(&self) -> f64 {
        if self.cnt == 0 {
            0.0
        } else {
            self.sum / self.cnt as f64
        }
    }
}

/// 一行桶数据连同其 series 元数据，供 Agent 导出（[`Store::export_after`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct ExportRow {
    /// 指标名。
    pub metric: String,
    /// 规范化标签串。
    pub labels: String,
    /// 单位。
    pub unit: Option<String>,
    /// 桶数据；`row.series_id` 是**本地**的 series id，键集分页用，
    /// 不应发给对端（对端按 `metric + labels` 重新映射）。
    pub row: MetricRow,
}

/// 单次查询返回的点数上限。超过就自动升到更粗的分层。
///
/// design.md §7.5 用 uPlot 画图，几万点也不掉帧；这里限制的是 HTTP 响应体大小，
/// 4000 点足以铺满任何屏幕宽度。
pub const MAX_QUERY_POINTS: i64 = 4000;

/// 自动选层查询的结果。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QueryResult {
    /// 实际读取的分层。恒为落盘层。
    pub layer: MetricLayer,
    /// 实际桶宽（秒），等于该层的 [`TierSpec::width`]。
    pub step: i64,
    /// 结果按 `(series_id, ts)` 升序排列。
    pub rows: Vec<MetricRow>,
}

/// 按时间跨度、期望步长与保留期预设挑选最合适的分层。纯函数，便于单测。
///
/// 三条规则依次生效：
///
/// 1. 在「桶宽 ≤ step」的分层里取最粗的一层——不牺牲请求分辨率的前提下行数最少；
///    step 比最细层还小时退化为 `m_1m`（库里没有更细的数据）。
/// 2. 若该层在此跨度下点数仍超过 [`MAX_QUERY_POINTS`]，继续逐级升粗。
/// 3. 若该层的保留期覆盖不住整个跨度，继续升粗——否则曲线前半段会因为
///    已被 [`Store::prune`] 清掉而凭空缺一大块。
///
/// 最粗层封顶：即使 `m_1d` 的保留期（1 年）也覆盖不住时，只能返回它。
pub fn select_tier(from: i64, to: i64, step: i64, preset: RetentionPreset) -> MetricLayer {
    let span = (to - from).max(0);
    let step = step.max(1);

    // TIERS 由细到粗，最后一个满足「桶宽 ≤ step」的即最粗的那层。
    let mut chosen = &TIERS[0];
    for spec in &TIERS {
        if spec.width <= step {
            chosen = spec;
        }
    }

    loop {
        let too_many = span / chosen.width > MAX_QUERY_POINTS;
        let out_of_retention = span > chosen.retention(preset);
        if !(too_many || out_of_retention) {
            break;
        }
        match chosen.coarser() {
            Some(c) => chosen = c,
            None => break,
        }
    }
    chosen.layer
}

/// 写入语句。冲突时整行覆盖——重算同一个桶必须是幂等的。
fn insert_sql(tier: &TierSpec) -> String {
    format!(
        r#"
        INSERT INTO {table} (series_id, ts, cnt, "min", "max", "sum", med)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(series_id, ts) DO UPDATE SET
            cnt   = excluded.cnt,
            "min" = excluded."min",
            "max" = excluded."max",
            "sum" = excluded."sum",
            med   = excluded.med
        "#,
        table = tier.table()
    )
}

/// 聚合语句。表名来自 [`MetricLayer`] 枚举，其余全部走 bind。
///
/// `mid` 这个 CTE 用窗口函数算中位数：按 med 排序取中间一位（偶数个时取中间两位
/// 的平均）。SQLite 没有 MEDIAN 聚合函数，这是标准写法。
fn rollup_sql(from: &TierSpec, to: &TierSpec) -> String {
    format!(
        r#"
        WITH src AS (
            SELECT series_id,
                   (ts / ?) * ? AS bucket,
                   cnt,
                   "min" AS mn,
                   "max" AS mx,
                   "sum" AS sm,
                   med
            FROM {src}
            WHERE ts >= ? AND ts < ?
        ),
        agg AS (
            SELECT series_id,
                   bucket,
                   SUM(cnt) AS cnt,
                   MIN(mn)  AS mn,
                   MAX(mx)  AS mx,
                   SUM(sm)  AS sm
            FROM src
            GROUP BY series_id, bucket
        ),
        ranked AS (
            SELECT series_id,
                   bucket,
                   med,
                   ROW_NUMBER() OVER (PARTITION BY series_id, bucket ORDER BY med) AS rn,
                   COUNT(*)     OVER (PARTITION BY series_id, bucket)              AS n
            FROM src
        ),
        mid AS (
            SELECT series_id, bucket, AVG(med) AS med
            FROM ranked
            WHERE rn IN ((n + 1) / 2, (n + 2) / 2)
            GROUP BY series_id, bucket
        )
        INSERT INTO {dst} (series_id, ts, cnt, "min", "max", "sum", med)
        SELECT agg.series_id, agg.bucket, agg.cnt, agg.mn, agg.mx, agg.sm, mid.med
        FROM agg
        JOIN mid ON mid.series_id = agg.series_id AND mid.bucket = agg.bucket
        WHERE 1
        ON CONFLICT(series_id, ts) DO UPDATE SET
            cnt   = excluded.cnt,
            "min" = excluded."min",
            "max" = excluded."max",
            "sum" = excluded."sum",
            med   = excluded.med
        "#,
        src = from.table(),
        dst = to.table()
    )
}

impl Store {
    /// 批量写入 1m 层。一个事务一次提交。
    ///
    /// 返回受影响行数（新增 + 覆盖）。
    pub async fn insert_1m(&self, rows: &[MetricRow]) -> Result<u64> {
        self.insert_tier(MetricLayer::M1m, rows).await
    }

    /// 批量写入指定分层。用于 Agent 断连补发时直接回填粗粒度层。
    ///
    /// `layer` 必须是落盘层，否则返回 [`StoreError::NotPersisted`]。
    pub async fn insert_tier(&self, layer: MetricLayer, rows: &[MetricRow]) -> Result<u64> {
        let spec = TierSpec::require(layer)?;
        if rows.is_empty() {
            return Ok(0);
        }

        let sql = insert_sql(spec);
        let mut tx = self.write_pool().begin().await?;
        let mut affected = 0u64;

        for row in rows {
            affected += sqlx::query(sqlx::AssertSqlSafe(sql.as_str()))
                .bind(row.series_id)
                .bind(row.ts)
                .bind(row.cnt)
                .bind(row.min)
                .bind(row.max)
                .bind(row.sum)
                .bind(row.med)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }

        tx.commit().await?;
        Ok(affected)
    }

    /// 分层聚合：把 `from` 层的数据汇总进 `to` 层。
    ///
    /// * `to` 必须是 `from` 的直接上一层，否则返回 [`StoreError::InvalidRollup`]。
    /// * 只处理在 `until_ts` 之前**已经完整关闭**的目标桶：上界取
    ///   `to.align(until_ts)`，正在填充中的桶不会被过早写死。
    /// * 起点取目标表已有的最大 ts（含该桶，重算一遍以吸收迟到数据）；
    ///   目标表为空时从源表最早的一条开始。整个语句是 UPSERT，重复执行幂等。
    ///
    /// 返回写入的目标桶数量。
    pub async fn rollup(&self, from: MetricLayer, to: MetricLayer, until_ts: i64) -> Result<u64> {
        let from_spec = TierSpec::require(from)?;
        let to_spec = TierSpec::require(to)?;

        if to_spec.source != Some(from) {
            return Err(StoreError::InvalidRollup {
                from,
                to,
                expected: match to_spec.source {
                    Some(l) => l.as_str(),
                    None => "内存环形缓冲",
                },
            });
        }

        let width = to_spec.width;
        let end = to_spec.align(until_ts);

        // 起点：目标表已有的最大桶（含），否则源表最早的一条对齐到目标桶。
        let dst_max: Option<i64> = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT MAX(ts) AS m FROM {}",
            to_spec.table()
        )))
        .fetch_one(self.read_pool())
        .await?
        .try_get("m")
        .unwrap_or(None);

        let start = match dst_max {
            Some(m) => m,
            None => {
                let src_min: Option<i64> = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SELECT MIN(ts) AS m FROM {}",
                    from_spec.table()
                )))
                .fetch_one(self.read_pool())
                .await?
                .try_get("m")
                .unwrap_or(None);

                match src_min {
                    Some(m) => to_spec.align(m),
                    None => return Ok(0),
                }
            }
        };

        if start >= end {
            return Ok(0);
        }

        let affected = sqlx::query(sqlx::AssertSqlSafe(rollup_sql(from_spec, to_spec)))
            .bind(width)
            .bind(width)
            .bind(start)
            .bind(end)
            .execute(self.write_pool())
            .await?
            .rows_affected();

        tracing::debug!(
            from = %from, to = %to, start, end, affected,
            "分层聚合完成"
        );
        Ok(affected)
    }

    /// 沿 `m_1m -> m_5m -> m_15m -> m_12h -> m_1d` 跑完整条聚合链。
    ///
    /// 由细到粗依次执行，因此同一次调用里新产生的 5m 桶能立刻参与 15m 聚合。
    /// 对应 design.md §7.2「后台任务每分钟运行一次」。
    pub async fn rollup_all(&self, until_ts: i64) -> Result<u64> {
        let mut total = 0;
        for layer in MetricLayer::PERSISTED {
            if let Some(src) = TierSpec::require(layer)?.source {
                total += self.rollup(src, layer, until_ts).await?;
            }
        }
        Ok(total)
    }

    /// 删除某层中 `ts < before_ts` 的桶。返回删除行数。
    pub async fn prune(&self, layer: MetricLayer, before_ts: i64) -> Result<u64> {
        let spec = TierSpec::require(layer)?;
        let affected = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {} WHERE ts < ?",
            spec.table()
        )))
        .bind(before_ts)
        .execute(self.write_pool())
        .await?
        .rows_affected();

        if affected > 0 {
            tracing::debug!(layer = %layer, before_ts, affected, "保留期清理完成");
        }
        Ok(affected)
    }

    /// 按当前保留期预设清理全部五层。`now` 为当前 unix 秒。
    pub async fn prune_all(&self, now: i64) -> Result<u64> {
        self.prune_all_with(self.retention(), now).await
    }

    /// 同上，但指定预设（便于在不重开库的情况下切换 Less / Normal）。
    pub async fn prune_all_with(&self, preset: RetentionPreset, now: i64) -> Result<u64> {
        let mut total = 0;
        for layer in MetricLayer::PERSISTED {
            let spec = TierSpec::require(layer)?;
            total += self.prune(layer, now - spec.retention(preset)).await?;
        }
        Ok(total)
    }

    /// 一次维护周期：先聚合，再清理。供后台任务每分钟调用。
    pub async fn maintain(&self, now: i64) -> Result<(u64, u64)> {
        let rolled = self.rollup_all(now).await?;
        let pruned = self.prune_all(now).await?;
        Ok((rolled, pruned))
    }

    /// 自动选层查询，对应 `GET /api/v1/metrics/query?series=&from=&to=&step=`。
    ///
    /// 时间区间为左闭右开 `[from, to)`——桶以起始时间标识，闭区间会把
    /// 恰好落在 `to` 上的下一个桶也带进来。
    pub async fn query(
        &self,
        series_ids: &[i64],
        from: i64,
        to: i64,
        step: i64,
    ) -> Result<QueryResult> {
        let layer = select_tier(from, to, step, self.retention());
        let spec = TierSpec::require(layer)?;
        let mut rows = Vec::new();

        if !series_ids.is_empty() {
            // SQLITE_MAX_VARIABLE_NUMBER 有上限，分批查询后合并。
            const CHUNK: usize = 400;
            for chunk in series_ids.chunks(CHUNK) {
                let placeholders = vec!["?"; chunk.len()].join(",");
                let sql = format!(
                    r#"
                    SELECT series_id, ts, cnt, "min", "max", "sum", med
                    FROM {table}
                    WHERE series_id IN ({placeholders}) AND ts >= ? AND ts < ?
                    ORDER BY series_id, ts
                    "#,
                    table = spec.table()
                );

                let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                for id in chunk {
                    q = q.bind(*id);
                }
                let fetched = q.bind(from).bind(to).fetch_all(self.read_pool()).await?;

                rows.reserve(fetched.len());
                for row in &fetched {
                    rows.push(MetricRow {
                        series_id: row.get("series_id"),
                        ts: row.get("ts"),
                        cnt: row.get("cnt"),
                        min: row.get("min"),
                        max: row.get("max"),
                        sum: row.get("sum"),
                        med: row.get("med"),
                    });
                }
            }

            // 分批查询破坏了全局顺序，这里重新排一次。
            if series_ids.len() > CHUNK {
                rows.sort_by_key(|r| (r.series_id, r.ts));
            }
        }

        Ok(QueryResult {
            layer,
            step: spec.width,
            rows,
        })
    }

    /// 某节点在某层的最大 `ts`；该节点无数据时 `None`。
    ///
    /// Agent 重连时 Server 以此构造 `agent.resume.since_ts`（roadmap/05 §3.2）。
    pub async fn tier_max_ts(&self, layer: MetricLayer, node: &str) -> Result<Option<i64>> {
        let spec = TierSpec::require(layer)?;
        let sql = format!(
            "SELECT MAX(m.ts) AS ts FROM {table} m JOIN series s ON s.id = m.series_id WHERE s.node = ?",
            table = spec.table()
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(node)
            .fetch_one(self.read_pool())
            .await?;
        Ok(row.get::<Option<i64>, _>("ts"))
    }

    /// 按 `(ts, series_id)` 键集分页导出一层的行，连同 series 元数据。
    ///
    /// 严格取 `(ts, series_id) > (after_ts, after_series)` 的行，按同序返回，
    /// 至多 `limit` 行。Agent 端的推送与补发都走它：要从 `ts >= T` 开始就传
    /// `(T - 1, i64::MAX)`。键集而不是 `ts >` 单键，是因为同一个 `ts` 的行数
    /// 可能超过一批的大小（series 很多时），单键分页会在批边界丢行或死循环。
    pub async fn export_after(
        &self,
        layer: MetricLayer,
        after_ts: i64,
        after_series: i64,
        limit: i64,
    ) -> Result<Vec<ExportRow>> {
        let spec = TierSpec::require(layer)?;
        let sql = format!(
            r#"
            SELECT s.metric, s.labels, s.unit,
                   m.series_id, m.ts, m.cnt, m."min", m."max", m."sum", m.med
            FROM {table} m JOIN series s ON s.id = m.series_id
            WHERE m.ts > ? OR (m.ts = ? AND m.series_id > ?)
            ORDER BY m.ts, m.series_id
            LIMIT ?
            "#,
            table = spec.table()
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(after_ts)
            .bind(after_ts)
            .bind(after_series)
            .bind(limit.max(0))
            .fetch_all(self.read_pool())
            .await?;
        Ok(rows
            .iter()
            .map(|row| ExportRow {
                metric: row.get("metric"),
                labels: row.get("labels"),
                unit: row.get("unit"),
                row: MetricRow {
                    series_id: row.get("series_id"),
                    ts: row.get("ts"),
                    cnt: row.get("cnt"),
                    min: row.get("min"),
                    max: row.get("max"),
                    sum: row.get("sum"),
                    med: row.get("med"),
                },
            })
            .collect())
    }

    /// 某条 series 在某层的桶数量，用于测试与容量统计。
    pub async fn count_tier(&self, layer: MetricLayer, series_id: i64) -> Result<i64> {
        let spec = TierSpec::require(layer)?;
        let n: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) AS n FROM {} WHERE series_id = ?",
            spec.table()
        )))
        .bind(series_id)
        .fetch_one(self.read_pool())
        .await?
        .get("n");
        Ok(n)
    }
}

#[cfg(test)]
mod trim_migration_tests {
    use sqlx::Row as _;

    use super::Store;

    /// `0002_metrics_trim.sql` 的原文。测试直接重放它：sqlx 的迁移在 open 时只跑
    /// 一次，无法对「迁移前就存在的老 series」建立前置状态，这里手工造出该状态
    /// 再执行同一份 SQL，顺带验证幂等（roadmap/08 §10）。
    const TRIM_SQL: &str = include_str!("../../migrations/0002_metrics_trim.sql");

    async fn insert_series(store: &Store, metric: &str) -> i64 {
        sqlx::query("INSERT INTO series (node, metric, labels) VALUES ('local', ?, '') RETURNING id")
            .bind(metric)
            .fetch_one(store.write_pool())
            .await
            .unwrap()
            .get("id")
    }

    async fn count(store: &Store, sql: &str) -> i64 {
        sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(store.write_pool())
            .await
            .unwrap()
            .get(0)
    }

    #[tokio::test]
    async fn 裁剪迁移删除老_series_及其桶数据_且幂等() {
        let store = Store::open_in_memory().await.unwrap();
        // 模拟迁移前的老库：一条被裁的 series（cpu.idle）与一条保留的（cpu.usage），
        // 各带一行 m_1m 桶数据。
        let retired = insert_series(&store, "cpu.idle").await;
        let kept = insert_series(&store, "cpu.usage").await;
        for id in [retired, kept] {
            sqlx::query(
                r#"INSERT INTO m_1m (series_id, ts, cnt, "min", "max", "sum", med)
                   VALUES (?, 60, 1, 1.0, 1.0, 1.0, 1.0)"#,
            )
            .bind(id)
            .execute(store.write_pool())
            .await
            .unwrap();
        }

        sqlx::raw_sql(TRIM_SQL).execute(store.write_pool()).await.unwrap();

        assert_eq!(count(&store, "SELECT COUNT(*) FROM series").await, 1);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM series WHERE metric = 'cpu.idle'").await,
            0,
            "被裁名单中的 series 必须删除"
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM m_1m").await,
            1,
            "外键级联必须把老 series 的桶数据一并清掉"
        );

        // 幂等：再执行一遍，行数不变。
        sqlx::raw_sql(TRIM_SQL).execute(store.write_pool()).await.unwrap();
        assert_eq!(count(&store, "SELECT COUNT(*) FROM series").await, 1);
        assert_eq!(count(&store, "SELECT COUNT(*) FROM m_1m").await, 1);
    }
}

#[cfg(test)]
mod export_tests {
    use crate::store::{MetricLayer, MetricRow, Store};

    fn row(series_id: i64, ts: i64) -> MetricRow {
        MetricRow {
            series_id,
            ts,
            cnt: 1,
            min: 1.0,
            max: 1.0,
            sum: 1.0,
            med: 1.0,
        }
    }

    async fn series(store: &Store, n: usize) -> Vec<i64> {
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(
                store
                    .get_or_create_series("local", &format!("t.m{i}"), &[], Some("count"))
                    .await
                    .unwrap(),
            );
        }
        ids
    }

    #[tokio::test]
    async fn 键集分页不重不漏_同_ts_跨批也正确() {
        let store = Store::open_in_memory().await.unwrap();
        let ids = series(&store, 3).await;
        // 3 series × 4 个 ts = 12 行；每批 4 行，批边界必然落在同一个 ts 中间。
        let mut rows = Vec::new();
        for ts in [60, 120, 180, 240] {
            for id in &ids {
                rows.push(row(*id, ts));
            }
        }
        store.insert_1m(&rows).await.unwrap();

        let mut got = Vec::new();
        let (mut ts, mut sid) = (-1i64, i64::MAX);
        loop {
            let batch = store
                .export_after(MetricLayer::M1m, ts, sid, 4)
                .await
                .unwrap();
            if batch.is_empty() {
                break;
            }
            let last = batch.last().unwrap().row;
            (ts, sid) = (last.ts, last.series_id);
            got.extend(batch.into_iter().map(|e| (e.row.ts, e.row.series_id)));
        }
        assert_eq!(got.len(), 12, "不重不漏");
        let mut expect: Vec<(i64, i64)> = Vec::new();
        for ts in [60, 120, 180, 240] {
            let mut s = ids.clone();
            s.sort_unstable();
            for id in s {
                expect.push((ts, id));
            }
        }
        assert_eq!(got, expect, "按 (ts, series_id) 全序返回");
    }

    #[tokio::test]
    async fn 一万行按一千切十批_最后一批不足时如实() {
        let store = Store::open_in_memory().await.unwrap();
        let ids = series(&store, 1).await;
        let rows: Vec<MetricRow> = (0..10_500).map(|i| row(ids[0], 60 * (i + 1))).collect();
        // insert_1m 单事务逐行，10500 行一次写入太慢；分块。
        for chunk in rows.chunks(2000) {
            store.insert_1m(chunk).await.unwrap();
        }
        let mut batches = Vec::new();
        let (mut ts, mut sid) = (-1i64, i64::MAX);
        loop {
            let batch = store
                .export_after(MetricLayer::M1m, ts, sid, 1000)
                .await
                .unwrap();
            if batch.is_empty() {
                break;
            }
            let last = batch.last().unwrap().row;
            (ts, sid) = (last.ts, last.series_id);
            batches.push(batch.len());
        }
        assert_eq!(batches.len(), 11);
        assert!(batches[..10].iter().all(|n| *n == 1000));
        assert_eq!(batches[10], 500, "最后一批不足 1000 时如实返回");
    }

    #[tokio::test]
    async fn 起点语义与元数据() {
        let store = Store::open_in_memory().await.unwrap();
        let id = store
            .get_or_create_series("local", "cpu.usage", &[("core", "1")], Some("percent"))
            .await
            .unwrap();
        store
            .insert_1m(&[row(id, 60), row(id, 120)])
            .await
            .unwrap();

        // 从 ts >= 60 开始：传 (59, MAX)。
        let all = store
            .export_after(MetricLayer::M1m, 59, i64::MAX, 100)
            .await
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].metric, "cpu.usage");
        assert_eq!(all[0].labels, "core=1");
        assert_eq!(all[0].unit.as_deref(), Some("percent"));

        // (60, MAX) 是严格键集：ts == 60 被跳过。
        let after = store
            .export_after(MetricLayer::M1m, 60, i64::MAX, 100)
            .await
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].row.ts, 120);
    }

    #[tokio::test]
    async fn 按节点取最大_ts() {
        let store = Store::open_in_memory().await.unwrap();
        assert_eq!(
            store.tier_max_ts(MetricLayer::M1m, "web-01").await.unwrap(),
            None
        );
        let local = store
            .get_or_create_series("local", "cpu.usage", &[], None)
            .await
            .unwrap();
        let remote = store
            .get_or_create_series("web-01", "cpu.usage", &[], None)
            .await
            .unwrap();
        store
            .insert_1m(&[row(local, 300), row(remote, 120), row(remote, 180)])
            .await
            .unwrap();
        assert_eq!(
            store.tier_max_ts(MetricLayer::M1m, "web-01").await.unwrap(),
            Some(180),
            "只看该节点自己的行"
        );
        assert_eq!(
            store.tier_max_ts(MetricLayer::M1m, "local").await.unwrap(),
            Some(300)
        );
    }
}
