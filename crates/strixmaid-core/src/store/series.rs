//! series 注册与查找（design.md §8 的 `series` 表）。
//!
//! 每条曲线由 `(node, metric, labels)` 唯一确定，`labels` 是把 k=v 按键排序后
//! 用逗号拼接的规范形式。写入热路径上不能每次都查库，因此这里带一层进程内缓存。

use sqlx::Row;

use super::{Result, SeriesKey, Store};

/// 一条已注册的 series。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Series {
    /// 主键。
    pub id: i64,
    /// 'local' 或节点 ID。
    pub node: String,
    /// 指标名，如 `cpu.usage`。
    pub metric: String,
    /// 规范化后的标签串，如 `dev=sda`；无标签时为空串。
    pub labels: String,
    /// 单位，如 `bytes` / `percent`。
    pub unit: Option<String>,
}

/// 把标签对拼成规范形式：按键升序排列，`k=v` 用逗号连接。
///
/// 规范化是 series 去重的基础——同一组标签无论以什么顺序传入都必须得到同一个串。
/// 键重复时保持稳定排序（相同键按传入顺序保留），不做去重也不做转义：
/// 标签键值由采集器生成，不含逗号与等号。
pub fn canonical_labels(labels: &[(&str, &str)]) -> String {
    if labels.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(&str, &str)> = labels.to_vec();
    pairs.sort_by(|a, b| a.0.cmp(b.0));

    let mut out = String::with_capacity(labels.len() * 16);
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

/// 查询单条 series 的完整 SQL 片段，避免各处重复。
const SERIES_COLUMNS: &str = "id, node, metric, labels, unit";

fn row_to_series(row: &sqlx::sqlite::SqliteRow) -> Series {
    Series {
        id: row.get("id"),
        node: row.get("node"),
        metric: row.get("metric"),
        labels: row.get("labels"),
        unit: row.get("unit"),
    }
}

impl Store {
    /// 取得（必要时创建）一条 series 的 id。
    ///
    /// `labels` 会先按键排序拼成规范形式；缓存命中时不触库。
    pub async fn get_or_create_series(
        &self,
        node: &str,
        metric: &str,
        labels: &[(&str, &str)],
        unit: Option<&str>,
    ) -> Result<i64> {
        let labels = canonical_labels(labels);
        self.get_or_create_series_raw(node, metric, &labels, unit)
            .await
    }

    /// 同上，但 `labels` 必须已经是规范形式（由 [`canonical_labels`] 产生）。
    pub async fn get_or_create_series_raw(
        &self,
        node: &str,
        metric: &str,
        labels: &str,
        unit: Option<&str>,
    ) -> Result<i64> {
        let key: SeriesKey = (node.to_string(), metric.to_string(), labels.to_string());

        // 先查缓存。注意不要跨 await 持锁。
        if let Some(id) = self.cached_series(&key) {
            return Ok(id);
        }

        // UPSERT + RETURNING 一次往返即可拿到 id，且天然抗并发竞态：
        // 冲突时走 DO UPDATE 分支（RETURNING 在 DO NOTHING 分支不返回行）。
        // unit 用 COALESCE 保留既有值，只在原先为 NULL 时回填。
        let id: i64 = sqlx::query(
            r#"
            INSERT INTO series (node, metric, labels, unit)
            VALUES (?, ?, ?, ?)
            ON CONFLICT(node, metric, labels)
                DO UPDATE SET unit = COALESCE(unit, excluded.unit)
            RETURNING id
            "#,
        )
        .bind(node)
        .bind(metric)
        .bind(labels)
        .bind(unit)
        .fetch_one(self.write_pool())
        .await?
        .get("id");

        self.cache_series(key, id);
        Ok(id)
    }

    /// 按 `(node, metric, 规范化 labels)` 查找，不存在时返回 `None`。
    pub async fn find_series(&self, node: &str, metric: &str, labels: &str) -> Result<Option<i64>> {
        let key: SeriesKey = (node.to_string(), metric.to_string(), labels.to_string());
        if let Some(id) = self.cached_series(&key) {
            return Ok(Some(id));
        }

        let row = sqlx::query("SELECT id FROM series WHERE node = ? AND metric = ? AND labels = ?")
            .bind(node)
            .bind(metric)
            .bind(labels)
            .fetch_optional(self.read_pool())
            .await?;

        let Some(row) = row else { return Ok(None) };
        let id: i64 = row.get("id");
        self.cache_series(key, id);
        Ok(Some(id))
    }

    /// 按 id 取完整记录。
    pub async fn get_series(&self, id: i64) -> Result<Option<Series>> {
        let sql = format!("SELECT {SERIES_COLUMNS} FROM series WHERE id = ?");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(self.read_pool())
            .await?;
        Ok(row.as_ref().map(row_to_series))
    }

    /// 列出全部 series；给定 `node` 时只列该节点的。
    ///
    /// 对应 `GET /api/v1/metrics/series`。
    pub async fn list_series(&self, node: Option<&str>) -> Result<Vec<Series>> {
        let rows = match node {
            Some(node) => {
                let sql = format!(
                    "SELECT {SERIES_COLUMNS} FROM series WHERE node = ? ORDER BY metric, labels"
                );
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .bind(node)
                    .fetch_all(self.read_pool())
                    .await?
            }
            None => {
                let sql =
                    format!("SELECT {SERIES_COLUMNS} FROM series ORDER BY node, metric, labels");
                sqlx::query(sqlx::AssertSqlSafe(sql))
                    .fetch_all(self.read_pool())
                    .await?
            }
        };
        Ok(rows.iter().map(row_to_series).collect())
    }

    /// 删除一条 series；五张时序表里的数据由外键 `ON DELETE CASCADE` 一并清掉。
    pub async fn delete_series(&self, id: i64) -> Result<bool> {
        // 先取回记录才能把缓存项摘掉。
        let existing = self.get_series(id).await?;

        let affected = sqlx::query("DELETE FROM series WHERE id = ?")
            .bind(id)
            .execute(self.write_pool())
            .await?
            .rows_affected();

        if let Some(s) = existing
            && let Ok(mut cache) = self.inner.series_cache.write()
        {
            cache.remove(&(s.node, s.metric, s.labels));
        }
        Ok(affected > 0)
    }

    /// 清空 series 缓存。库被外部改动后调用。
    pub fn clear_series_cache(&self) {
        if let Ok(mut cache) = self.inner.series_cache.write() {
            cache.clear();
        }
    }

    /// 当前缓存条目数，用于测试与指标暴露。
    pub fn series_cache_len(&self) -> usize {
        self.inner.series_cache.read().map(|c| c.len()).unwrap_or(0)
    }

    fn cached_series(&self, key: &SeriesKey) -> Option<i64> {
        self.inner.series_cache.read().ok()?.get(key).copied()
    }

    fn cache_series(&self, key: SeriesKey, id: i64) {
        if let Ok(mut cache) = self.inner.series_cache.write() {
            cache.insert(key, id);
        }
    }
}
