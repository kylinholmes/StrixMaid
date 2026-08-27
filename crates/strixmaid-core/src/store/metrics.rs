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
