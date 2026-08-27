//! 存储层单元测试。
//!
//! 大部分用例跑在内存库上（快、无需清理）；migration / WAL / 连接参数相关的用例
//! 必须用真实文件才有意义，用 [`TempDb`] 建临时文件并在 Drop 时删干净。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::Row;

use super::*;

// ============================ 测试脚手架 ============================

/// 临时数据库文件，Drop 时连同 -wal / -shm 一起删掉。
struct TempDb {
    path: PathBuf,
}

impl TempDb {
    fn new(tag: &str) -> TempDb {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "strixmaid-test-{}-{}-{}.db",
            tag,
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        TempDb { path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

/// 内存库 + 一条注册好的 series，返回 (store, series_id)。
async fn fixture() -> (Store, i64) {
    let store = Store::open_in_memory().await.expect("打开内存库");
    let id = store
        .get_or_create_series("local", "cpu.usage", &[], Some("percent"))
        .await
        .expect("注册 series");
    (store, id)
}

fn row(series_id: i64, ts: i64, cnt: i64, min: f64, max: f64, sum: f64, med: f64) -> MetricRow {
    MetricRow {
        series_id,
        ts,
        cnt,
        min,
        max,
        sum,
        med,
    }
}

fn assert_close(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{what}: 期望 {expected}，实际 {actual}"
    );
}

// ============================ migration ============================

#[tokio::test]
async fn migration_creates_all_tables() {
    let tmp = TempDb::new("migrate");
    let store = Store::open(&tmp.path).await.expect("打开文件库");

    let names: Vec<String> =
        sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .fetch_all(store.read_pool())
            .await
            .expect("列出表")
            .iter()
            .map(|r| r.get::<String, _>("name"))
            .collect();

    for expected in [
        "series",
        "m_1m",
        "m_5m",
        "m_15m",
        "m_12h",
        "m_1d",
        "nodes",
        "sessions",
        "node_sessions",
        "audit_log",
        "settings",
    ] {
        assert!(names.iter().any(|n| n == expected), "缺少表 {expected}");
    }

    // 五张时序表必须是 WITHOUT ROWID。
    for layer in MetricLayer::PERSISTED {
        let sql: String = sqlx::query("SELECT sql FROM sqlite_master WHERE name = ?")
            .bind(layer.as_str())
            .fetch_one(store.read_pool())
            .await
            .expect("取建表语句")
            .get("sql");
        assert!(
            sql.contains("WITHOUT ROWID"),
            "{layer} 不是 WITHOUT ROWID 表"
        );
    }

    store.close().await;
}

#[tokio::test]
async fn file_db_uses_wal() {
    let tmp = TempDb::new("wal");
    let store = Store::open(&tmp.path).await.expect("打开文件库");

    let mode: String = sqlx::query("PRAGMA journal_mode")
        .fetch_one(store.read_pool())
        .await
        .expect("读 journal_mode")
        .get(0);
    assert_eq!(mode.to_lowercase(), "wal");

    let sync: i64 = sqlx::query("PRAGMA synchronous")
        .fetch_one(store.read_pool())
        .await
        .expect("读 synchronous")
        .get(0);
    assert_eq!(sync, 1, "synchronous 应为 NORMAL(1)");

    store.close().await;
    assert!(store.is_closed());
}

#[tokio::test]
async fn migration_is_idempotent() {
    let tmp = TempDb::new("reopen");
    let a = Store::open(&tmp.path).await.expect("首次打开");
    a.close().await;
    // 重开同一个文件，migration 已记录过版本，不应重复执行。
    let b = Store::open(&tmp.path).await.expect("二次打开");
    b.close().await;
}

// ============================ series ============================

#[test]
fn canonical_labels_sorts_by_key() {
    assert_eq!(canonical_labels(&[]), "");
    assert_eq!(canonical_labels(&[("dev", "sda")]), "dev=sda");
    // 传入顺序不影响结果。
    assert_eq!(
        canonical_labels(&[("iface", "eth0"), ("dev", "sda")]),
        "dev=sda,iface=eth0"
    );
    assert_eq!(
        canonical_labels(&[("dev", "sda"), ("iface", "eth0")]),
        "dev=sda,iface=eth0"
    );
}

#[tokio::test]
async fn series_is_deduplicated() {
    let store = Store::open_in_memory().await.unwrap();

    let a = store
        .get_or_create_series("local", "disk.read_bytes", &[("dev", "sda")], Some("bytes"))
        .await
        .unwrap();
    // 完全相同 -> 命中缓存，同一个 id。
    let b = store
        .get_or_create_series("local", "disk.read_bytes", &[("dev", "sda")], Some("bytes"))
        .await
        .unwrap();
    assert_eq!(a, b);

    // 标签顺序不同、unit 不同都不产生新 series（规范化后键相同）。
    store.clear_series_cache();
    let c = store
        .get_or_create_series(
            "local",
            "disk.read_bytes",
            &[("dev", "sda"), ("x", "1")],
            None,
        )
        .await
        .unwrap();
    let d = store
        .get_or_create_series(
            "local",
            "disk.read_bytes",
            &[("x", "1"), ("dev", "sda")],
            None,
        )
        .await
        .unwrap();
    assert_eq!(c, d);
    assert_ne!(a, c, "标签不同应是不同 series");

    // node 不同 -> 不同 series。
    let e = store
        .get_or_create_series("agent-1", "disk.read_bytes", &[("dev", "sda")], None)
        .await
        .unwrap();
    assert_ne!(a, e);

    assert_eq!(store.list_series(None).await.unwrap().len(), 3);
    assert_eq!(store.list_series(Some("local")).await.unwrap().len(), 2);

    // 缓存生效：清空后 find_series 仍能查到并回填缓存。
    store.clear_series_cache();
    assert_eq!(store.series_cache_len(), 0);
    assert_eq!(
        store
            .find_series("local", "disk.read_bytes", "dev=sda")
            .await
            .unwrap(),
        Some(a)
    );
    assert_eq!(store.series_cache_len(), 1);

    // unit 首次为 None，后续带 unit 时回填。
    let s = store.get_series(c).await.unwrap().unwrap();
    assert_eq!(s.unit, None);
    store.clear_series_cache();
    store
        .get_or_create_series(
            "local",
            "disk.read_bytes",
            &[("dev", "sda"), ("x", "1")],
            Some("bytes"),
        )
        .await
        .unwrap();
    let s = store.get_series(c).await.unwrap().unwrap();
    assert_eq!(s.unit.as_deref(), Some("bytes"));
}

#[tokio::test]
async fn deleting_series_cascades_to_metrics() {
    let (store, sid) = fixture().await;
    store
        .insert_1m(&[row(sid, 0, 30, 1.0, 9.0, 150.0, 5.0)])
        .await
        .unwrap();
    assert_eq!(store.count_tier(MetricLayer::M1m, sid).await.unwrap(), 1);

    assert!(store.delete_series(sid).await.unwrap());
    assert_eq!(store.count_tier(MetricLayer::M1m, sid).await.unwrap(), 0);
    assert_eq!(store.series_cache_len(), 0);
}

// ============================ 写入与查询 ============================

#[tokio::test]
async fn insert_1m_is_batched_and_idempotent() {
    let (store, sid) = fixture().await;

    let rows = vec![
        row(sid, 0, 30, 1.0, 9.0, 150.0, 5.0),
        row(sid, 60, 30, 2.0, 8.0, 150.0, 5.0),
    ];
    assert_eq!(store.insert_1m(&rows).await.unwrap(), 2);
    assert_eq!(store.count_tier(MetricLayer::M1m, sid).await.unwrap(), 2);

    // 同一个桶重写 -> 覆盖，不新增。
    let updated = vec![row(sid, 0, 31, 0.5, 9.5, 155.0, 5.5)];
    store.insert_1m(&updated).await.unwrap();
    assert_eq!(store.count_tier(MetricLayer::M1m, sid).await.unwrap(), 2);

    let res = store.query(&[sid], 0, 120, 60).await.unwrap();
    assert_eq!(res.layer, MetricLayer::M1m);
    assert_eq!(res.step, 60);
    assert_eq!(res.rows.len(), 2);
    assert_eq!(res.rows[0].cnt, 31);
    assert_close(res.rows[0].min, 0.5, "覆盖后的 min");
    assert_close(res.rows[0].avg(), 155.0 / 31.0, "avg = sum / cnt");

    // 空切片是合法输入。
    assert_eq!(store.insert_1m(&[]).await.unwrap(), 0);
    // 区间左闭右开。
    assert_eq!(store.query(&[sid], 0, 60, 60).await.unwrap().rows.len(), 1);
    // 空 series 列表返回空结果但仍给出选中的层。
    let empty = store.query(&[], 0, 120, 60).await.unwrap();
    assert!(empty.rows.is_empty());
    assert_eq!(empty.layer, MetricLayer::M1m);
}

// ============================ 分层聚合 ============================

#[tokio::test]
async fn rollup_1m_to_5m_matches_spec() {
    let (store, sid) = fixture().await;

    // 一个 5m 桶 = 5 个 1m 桶。构造互不相同的值以便逐项校验。
    //   cnt: 10 20 30 40 50  -> SUM = 150
    //   sum: 100 200 300 400 500 -> SUM = 1500  (avg = 10，为加权平均而非算术平均)
    //   min: 5 4 3 2 1       -> MIN = 1
    //   max: 6 7 8 9 10      -> MAX = 10
    //   med: 5 1 4 2 3       -> 中位数 = 3
    let rows = vec![
        row(sid, 0, 10, 5.0, 6.0, 100.0, 5.0),
        row(sid, 60, 20, 4.0, 7.0, 200.0, 1.0),
        row(sid, 120, 30, 3.0, 8.0, 300.0, 4.0),
        row(sid, 180, 40, 2.0, 9.0, 400.0, 2.0),
        row(sid, 240, 50, 1.0, 10.0, 500.0, 3.0),
    ];
    store.insert_1m(&rows).await.unwrap();

    // until_ts = 300 -> 第一个 5m 桶已完整关闭。
    assert_eq!(store.rollup(MetricLayer::M1m, MetricLayer::M5m, 300).await.unwrap(), 1);

    let res = store.query(&[sid], 0, 300, 300).await.unwrap();
    assert_eq!(res.layer, MetricLayer::M5m);
    assert_eq!(res.rows.len(), 1);
    let b = res.rows[0];
    assert_eq!(b.ts, 0);
    assert_eq!(b.cnt, 150, "cnt = SUM(cnt)");
    assert_close(b.min, 1.0, "min = MIN(min)");
    assert_close(b.max, 10.0, "max = MAX(max)");
    assert_close(b.sum, 1500.0, "sum = SUM(sum)");
    assert_close(b.avg(), 10.0, "加权 avg = SUM(sum)/SUM(cnt)");
    assert_close(b.med, 3.0, "med = 子桶 med 的中位数（奇数个取正中）");

    // 幂等：再跑一次数值不变，也不新增行。
    store.rollup(MetricLayer::M1m, MetricLayer::M5m, 300).await.unwrap();
    let again = store.query(&[sid], 0, 300, 300).await.unwrap();
    assert_eq!(again.rows.len(), 1);
    assert_eq!(again.rows[0], b);
}

#[tokio::test]
async fn rollup_median_of_even_count_averages_middle_two() {
    let (store, sid) = fixture().await;

    // 只有 4 个 1m 桶（缺一个采样周期）：med 排序后为 2 4 6 8，
    // 取中间两位的平均 -> (4 + 6) / 2 = 5。
    store
        .insert_1m(&[
            row(sid, 0, 1, 0.0, 1.0, 1.0, 8.0),
            row(sid, 60, 1, 0.0, 1.0, 1.0, 2.0),
            row(sid, 120, 1, 0.0, 1.0, 1.0, 4.0),
            row(sid, 180, 1, 0.0, 1.0, 1.0, 6.0),
        ])
        .await
        .unwrap();

    store.rollup(MetricLayer::M1m, MetricLayer::M5m, 300).await.unwrap();
    let res = store.query(&[sid], 0, 300, 300).await.unwrap();
    assert_close(res.rows[0].med, 5.0, "偶数个子桶取中间两位的平均");
    assert_eq!(res.rows[0].cnt, 4, "cnt 反映实际采样数，可用于缺失检测");
}

#[tokio::test]
async fn rollup_skips_unclosed_buckets() {
    let (store, sid) = fixture().await;
    store
        .insert_1m(&[
            row(sid, 0, 1, 1.0, 1.0, 1.0, 1.0),
            row(sid, 300, 1, 2.0, 2.0, 2.0, 2.0),
        ])
        .await
        .unwrap();

    // until_ts = 420 落在第二个 5m 桶 [300,600) 内部，该桶还没关，不应写入。
    assert_eq!(store.rollup(MetricLayer::M1m, MetricLayer::M5m, 420).await.unwrap(), 1);
    assert_eq!(store.count_tier(MetricLayer::M5m, sid).await.unwrap(), 1);

    // 桶关闭后才补上。返回 2 是因为起点包含已有的最大桶（重算以吸收迟到数据），
    // 于是 [0,300) 与 [300,600) 两个桶都被写了一遍。
    assert_eq!(store.rollup(MetricLayer::M1m, MetricLayer::M5m, 600).await.unwrap(), 2);
    assert_eq!(store.count_tier(MetricLayer::M5m, sid).await.unwrap(), 2);
}

#[tokio::test]
async fn rollup_absorbs_late_data_in_last_bucket() {
    let (store, sid) = fixture().await;
    store
        .insert_1m(&[row(sid, 0, 10, 5.0, 5.0, 50.0, 5.0)])
        .await
        .unwrap();
    store.rollup(MetricLayer::M1m, MetricLayer::M5m, 300).await.unwrap();

    // 同一个 5m 桶内迟到了一条 1m 数据：起点包含目标表最大桶，会重算。
    store
        .insert_1m(&[row(sid, 120, 10, 1.0, 9.0, 30.0, 1.0)])
        .await
        .unwrap();
    store.rollup(MetricLayer::M1m, MetricLayer::M5m, 300).await.unwrap();

    let res = store.query(&[sid], 0, 300, 300).await.unwrap();
    assert_eq!(res.rows[0].cnt, 20);
    assert_close(res.rows[0].sum, 80.0, "迟到数据被吸收");
    assert_close(res.rows[0].min, 1.0, "min 重算");
    assert_close(res.rows[0].max, 9.0, "max 重算");
}

#[tokio::test]
async fn rollup_chain_reaches_1d() {
    let (store, sid) = fixture().await;

    // 覆盖两天，每 1m 一个桶太多，改用「每 15 分钟写一个 1m 桶」的稀疏数据：
    // 聚合规则与桶密度无关，链路连通性才是这里要验证的。
    let mut rows = Vec::new();
    let mut ts = 0;
    while ts < 2 * 86_400 {
        rows.push(row(sid, ts, 1, 1.0, 3.0, 2.0, 2.0));
        ts += 900;
    }
    store.insert_1m(&rows).await.unwrap();

    let total = store.rollup_all(2 * 86_400).await.unwrap();
    assert!(total > 0);

    // 两天 = 2 个 1d 桶。
    assert_eq!(store.count_tier(MetricLayer::M1d, sid).await.unwrap(), 2);
    assert_eq!(store.count_tier(MetricLayer::M12h, sid).await.unwrap(), 4);

    let day = store.query(&[sid], 0, 86_400, 86_400).await.unwrap();
    assert_eq!(day.layer, MetricLayer::M1d);
    assert_eq!(day.rows.len(), 1);
    // 一天 96 个 1m 桶，sum 精确逐级相加。
    assert_eq!(day.rows[0].cnt, 96);
    assert_close(day.rows[0].sum, 192.0, "sum 逐级精确");
    assert_close(day.rows[0].med, 2.0, "med of med 仍为 2");
    assert_close(day.rows[0].min, 1.0, "min 穿透五层");
    assert_close(day.rows[0].max, 3.0, "max 穿透五层");
}

#[tokio::test]
async fn rollup_rejects_non_adjacent_tiers() {
    let (store, _) = fixture().await;
    let err = store.rollup(MetricLayer::M1m, MetricLayer::M15m, 0).await.unwrap_err();
    assert!(matches!(err, StoreError::InvalidRollup { .. }));

    let err = store.rollup(MetricLayer::M5m, MetricLayer::M1m, 0).await.unwrap_err();
    assert!(matches!(err, StoreError::InvalidRollup { .. }));
}

#[tokio::test]
async fn rollup_on_empty_source_is_noop() {
    let (store, _) = fixture().await;
    assert_eq!(store.rollup(MetricLayer::M1m, MetricLayer::M5m, 10_000).await.unwrap(), 0);
    assert_eq!(store.rollup_all(10_000).await.unwrap(), 0);
}

// ============================ 保留期清理 ============================

#[tokio::test]
async fn prune_drops_only_expired_buckets() {
    let (store, sid) = fixture().await;
    store
        .insert_1m(&[
            row(sid, 0, 1, 1.0, 1.0, 1.0, 1.0),
            row(sid, 60, 1, 1.0, 1.0, 1.0, 1.0),
            row(sid, 120, 1, 1.0, 1.0, 1.0, 1.0),
        ])
        .await
        .unwrap();

    // ts < 120 的两条被删。
    assert_eq!(store.prune(MetricLayer::M1m, 120).await.unwrap(), 2);
    assert_eq!(store.count_tier(MetricLayer::M1m, sid).await.unwrap(), 1);
    let left = store.query(&[sid], 0, 1000, 60).await.unwrap();
    assert_eq!(left.rows[0].ts, 120);
}

#[tokio::test]
async fn prune_all_honours_retention_preset() {
    let store = Store::open_in_memory().await.unwrap();
    let sid = store
        .get_or_create_series("local", "cpu.usage", &[], None)
        .await
        .unwrap();

    // now 取一个足够大的值，避免 now - retention 变成负数。
    let now = 3 * YEAR;

    // 每层各放两条：一条刚过 Normal 保留期，一条还在期内。
    for layer in MetricLayer::PERSISTED {
        let spec = TierSpec::of(layer).unwrap();
        let keep = now - spec.retention(RetentionPreset::Normal) + spec.width;
        let drop = now - spec.retention(RetentionPreset::Normal) - spec.width;
        store
            .insert_tier(
                layer,
                &[
                    row(sid, spec.align(keep), 1, 1.0, 1.0, 1.0, 1.0),
                    row(sid, spec.align(drop), 1, 1.0, 1.0, 1.0, 1.0),
                ],
            )
            .await
            .unwrap();
    }

    assert_eq!(store.retention(), RetentionPreset::Normal);
    assert_eq!(store.prune_all(now).await.unwrap(), 5, "每层各删一条");
    for layer in MetricLayer::PERSISTED {
        assert_eq!(
            store.count_tier(layer, sid).await.unwrap(),
            1,
            "{layer} 应剩一条"
        );
    }

    // Less 预设更短，同样的数据会再删掉一批。
    assert!(
        store
            .prune_all_with(RetentionPreset::Less, now)
            .await
            .unwrap()
            > 0
    );
}

#[test]
fn retention_table_matches_design() {
    use MetricLayer::{M1d, M1m, M5m, M12h, M15m};

    // design.md §7.2 的表格，逐格对照：层 | 桶宽 | Less 保留 | Normal 保留 | 聚合来源。
    let cases = [
        (M1m, "m_1m", 60, 6 * HOUR, DAY, None),
        (M5m, "m_5m", 300, 3 * DAY, 7 * DAY, Some(M1m)),
        (M15m, "m_15m", 900, 14 * DAY, 30 * DAY, Some(M5m)),
        (M12h, "m_12h", 43_200, 90 * DAY, 90 * DAY, Some(M15m)),
        (M1d, "m_1d", 86_400, YEAR, YEAR, Some(M12h)),
    ];
    assert_eq!(cases.len(), MetricLayer::PERSISTED.len());

    for (i, (layer, table, width, less, normal, source)) in cases.into_iter().enumerate() {
        // 表的顺序必须与 types 的 PERSISTED 一致（由细到粗），否则 coarser() 会串层。
        assert_eq!(MetricLayer::PERSISTED[i], layer);
        assert_eq!(TIERS[i].layer, layer);

        let spec = TierSpec::of(layer).expect("落盘层必有元数据");
        assert_eq!(spec.table(), table, "{layer} 表名");
        assert_eq!(spec.width, width, "{layer} 桶宽");
        assert_eq!(spec.retention(RetentionPreset::Less), less, "{layer} Less");
        assert_eq!(
            spec.retention(RetentionPreset::Normal),
            normal,
            "{layer} Normal"
        );
        assert_eq!(spec.source, source, "{layer} 聚合来源");
        // 保留期必须是桶宽的整数倍，否则清理边界会切到半个桶。
        assert_eq!(less % width, 0);
        assert_eq!(normal % width, 0);
    }

    // live 层不落盘，没有表也没有元数据。
    assert!(TierSpec::of(MetricLayer::Live).is_none());
    assert!(matches!(
        TierSpec::require(MetricLayer::Live),
        Err(StoreError::NotPersisted(MetricLayer::Live))
    ));

    // 层名（即表名）的解析归 types，store 不再自带一份。
    assert_eq!("m_12h".parse::<MetricLayer>().unwrap(), M12h);
    assert!("30s".parse::<MetricLayer>().is_err());
}

// ============================ 自动选层 ============================

#[test]
fn select_tier_uses_step_span_and_retention() {
    let n = RetentionPreset::Normal;
    let day = 86_400;

    // step 比最细层还细 -> 退化为 1m。
    assert_eq!(select_tier(0, 3600, 2, n), MetricLayer::M1m);
    assert_eq!(select_tier(0, 3600, 60, n), MetricLayer::M1m);
    // step 落在两层之间取更细的那层（桶宽 ≤ step 中最粗的）。
    assert_eq!(select_tier(0, 3600, 299, n), MetricLayer::M1m);
    assert_eq!(select_tier(0, 3600, 300, n), MetricLayer::M5m);
    assert_eq!(select_tier(0, 3600, 900, n), MetricLayer::M15m);

    // 跨度过大时即使 step 很小也必须升粗，否则点数爆炸。
    assert_eq!(select_tier(0, 30 * day, 60, n), MetricLayer::M15m);
    assert_eq!(select_tier(0, 90 * day, 60, n), MetricLayer::M12h);
    // 一年跨度：m_12h 只留 90 天，必须升到 m_1d 才有完整曲线。
    assert_eq!(select_tier(0, 365 * day, 60, n), MetricLayer::M1d);
    // 最粗层封顶，不会无限升。
    assert_eq!(select_tier(0, 100 * 365 * day, 1, n), MetricLayer::M1d);

    // 保留期预设影响选层：Less 下 m_5m 只留 3 天，7 天跨度必须用 m_15m。
    assert_eq!(select_tier(0, 7 * day, 2, n), MetricLayer::M5m);
    assert_eq!(
        select_tier(0, 7 * day, 2, RetentionPreset::Less),
        MetricLayer::M15m
    );

    // 选中层的点数不超过上限、保留期也覆盖得住（除非已经到最粗层）。
    for (from, to, step) in [(0, 90 * day, 60), (0, 30 * day, 60), (0, 7 * day, 2)] {
        let layer = select_tier(from, to, step, n);
        let spec = TierSpec::of(layer).unwrap();
        if layer != MetricLayer::M1d {
            assert!((to - from) / spec.width <= MAX_QUERY_POINTS);
            assert!(to - from <= spec.retention(n));
        }
    }
}

#[tokio::test]
async fn query_reads_from_selected_tier() {
    let (store, sid) = fixture().await;
    let day = 86_400;

    store
        .insert_tier(MetricLayer::M1m, &[row(sid, 0, 1, 1.0, 1.0, 1.0, 1.0)])
        .await
        .unwrap();
    store
        .insert_tier(MetricLayer::M1d, &[row(sid, 0, 999, 7.0, 7.0, 7.0, 7.0)])
        .await
        .unwrap();

    // 一年跨度 -> 落到 1d 层，读到的是 1d 那条。
    let res = store.query(&[sid], 0, 365 * day, 60).await.unwrap();
    assert_eq!(res.layer, MetricLayer::M1d);
    assert_eq!(res.step, 86_400);
    assert_eq!(res.rows.len(), 1);
    assert_eq!(res.rows[0].cnt, 999);
}

// ============================ 审计 ============================

#[tokio::test]
async fn audit_write_and_paginate() {
    let store = Store::open_in_memory().await.unwrap();

    for i in 0..25 {
        store
            .audit_write(
                &NewAuditEntry::new("local", "alice", "service.start", AuditOutcome::Ok)
                    .target(format!("unit-{i}.service"))
                    .actor(1000, i % 2 == 0)
                    .params(r#"{"scope":"system"}"#)
                    .remote_addr("127.0.0.1"),
            )
            .await
            .unwrap();
    }
    store
        .audit_write(&NewAuditEntry::new(
            "local",
            "bob",
            "process.kill",
            AuditOutcome::Denied,
        ))
        .await
        .unwrap();

    // 第一页 10 条，倒序。
    let page1 = store
        .audit_query(&AuditFilter {
            limit: 10,
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(page1.entries.len(), 10);
    assert_eq!(page1.entries[0].username, "bob");
    assert_eq!(page1.entries[0].result, AuditOutcome::Denied);
    assert!(page1.entries[0].id > page1.entries[9].id, "按 id 倒序");
    let cursor = page1.next_cursor.expect("应有下一页");

    // 翻页不重不漏。
    let page2 = store
        .audit_query(&AuditFilter {
            limit: 10,
            cursor: Some(cursor),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(page2.entries.len(), 10);
    assert!(page2.entries[0].id < page1.entries[9].id);

    let page3 = store
        .audit_query(&AuditFilter {
            limit: 10,
            cursor: page2.next_cursor,
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(page3.entries.len(), 6);
    assert_eq!(page3.next_cursor, None, "最后一页无游标");

    // 过滤。
    let denied = store
        .audit_query(&AuditFilter {
            result: Some(AuditOutcome::Denied),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(denied.entries.len(), 1);
    assert_eq!(denied.entries[0].username, "bob");

    let by_user = store
        .audit_query(&AuditFilter {
            username: Some("alice".into()),
            action: Some("service.start".into()),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(by_user.entries.len(), 25);
    assert_eq!(by_user.entries[0].uid, Some(1000));
    assert_eq!(by_user.entries[0].remote_addr.as_deref(), Some("127.0.0.1"));

    // 单条读取与按时间清理。
    let id = denied.entries[0].id;
    assert_eq!(store.audit_get(id).await.unwrap().unwrap().id, id);
    assert_eq!(store.audit_get(9_999).await.unwrap(), None);

    let ts = denied.entries[0].ts;
    assert_eq!(store.audit_prune(ts + 1).await.unwrap(), 26);
    assert!(
        store
            .audit_query(&AuditFilter::default())
            .await
            .unwrap()
            .entries
            .is_empty()
    );
}

#[tokio::test]
async fn audit_time_range_filter() {
    let store = Store::open_in_memory().await.unwrap();
    for ts in [100, 200, 300, 400] {
        let mut e = NewAuditEntry::new("local", "alice", "file.write", AuditOutcome::Ok);
        e.ts = Some(ts);
        store.audit_write(&e).await.unwrap();
    }

    let page = store
        .audit_query(&AuditFilter {
            since: Some(200),
            until: Some(400),
            ..AuditFilter::default()
        })
        .await
        .unwrap();
    let times: Vec<i64> = page.entries.iter().map(|e| e.ts).collect();
    assert_eq!(times, vec![300, 200], "[since, until) 半开区间");
}

// ============================ 会话 ============================

#[tokio::test]
async fn session_lifecycle() {
    let store = Store::open_in_memory().await.unwrap();

    store
        .upsert_node("local", "本机", NodeKind::Local, None)
        .await
        .unwrap();
    store
        .upsert_node(
            "agent-1",
            "边缘节点",
            NodeKind::Agent,
            Some("hash-of-token"),
        )
        .await
        .unwrap();
    assert_eq!(store.list_nodes().await.unwrap().len(), 2);

    let node = store.get_node("agent-1").await.unwrap().unwrap();
    assert_eq!(node.kind, NodeKind::Agent);
    assert_eq!(node.token_hash.as_deref(), Some("hash-of-token"));
    assert_eq!(node.last_seen, None);
    assert!(store.touch_node("agent-1", 12_345).await.unwrap());
    assert_eq!(
        store.get_node("agent-1").await.unwrap().unwrap().last_seen,
        Some(12_345)
    );
    // upsert 改名不覆盖已有 token_hash。
    store
        .upsert_node("agent-1", "改名了", NodeKind::Agent, None)
        .await
        .unwrap();
    let node = store.get_node("agent-1").await.unwrap().unwrap();
    assert_eq!(node.name, "改名了");
    assert_eq!(node.token_hash.as_deref(), Some("hash-of-token"));

    // 会话 id 是 token 的 hash，本层不接触明文。
    let token_hash = "sha256:0123456789abcdef";
    let s = store
        .create_session(token_hash, Some("Firefox"), Some("10.0.0.1"))
        .await
        .unwrap();
    assert_eq!(s.id, token_hash);
    assert_eq!(store.get_session(token_hash).await.unwrap().unwrap(), s);
    assert_eq!(store.list_sessions().await.unwrap().len(), 1);
    assert!(
        store
            .touch_session(token_hash, s.last_active + 60)
            .await
            .unwrap()
    );
    assert!(!store.touch_session("不存在", 0).await.unwrap());

    // node_sessions：认证后默认未提权。
    let ns = store
        .upsert_node_session(token_hash, "local", 1000, "alice")
        .await
        .unwrap();
    assert!(!ns.elevated);
    assert_eq!(ns.elevated_at, None);

    assert!(
        store
            .set_elevated(token_hash, "local", true, 999)
            .await
            .unwrap()
    );
    let ns = store
        .get_node_session(token_hash, "local")
        .await
        .unwrap()
        .unwrap();
    assert!(ns.elevated);
    assert_eq!(ns.elevated_at, Some(999));

    // 重新认证会把提权状态清掉。
    store
        .upsert_node_session(token_hash, "local", 1000, "alice")
        .await
        .unwrap();
    let ns = store
        .get_node_session(token_hash, "local")
        .await
        .unwrap()
        .unwrap();
    assert!(!ns.elevated);
    assert_eq!(ns.elevated_at, None);

    store
        .upsert_node_session(token_hash, "agent-1", 0, "root")
        .await
        .unwrap();
    assert_eq!(store.list_node_sessions(token_hash).await.unwrap().len(), 2);
    assert!(
        store
            .delete_node_session(token_hash, "agent-1")
            .await
            .unwrap()
    );
    assert_eq!(store.list_node_sessions(token_hash).await.unwrap().len(), 1);

    assert!(
        store
            .touch_node_session(token_hash, "local", 4242)
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .get_node_session(token_hash, "local")
            .await
            .unwrap()
            .unwrap()
            .last_active,
        4242
    );

    // 登出：node_sessions 级联删除。
    assert!(store.delete_session(token_hash).await.unwrap());
    assert!(store.get_session(token_hash).await.unwrap().is_none());
    assert!(
        store
            .list_node_sessions(token_hash)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn prune_sessions_drops_idle() {
    let store = Store::open_in_memory().await.unwrap();
    store.create_session("h1", None, None).await.unwrap();
    store.create_session("h2", None, None).await.unwrap();
    store.touch_session("h1", 100).await.unwrap();
    store.touch_session("h2", 5_000).await.unwrap();

    assert_eq!(store.prune_sessions(1_000).await.unwrap(), 1);
    assert!(store.get_session("h1").await.unwrap().is_none());
    assert!(store.get_session("h2").await.unwrap().is_some());
}

#[tokio::test]
async fn node_session_requires_existing_node_and_session() {
    let store = Store::open_in_memory().await.unwrap();
    // 外键必须是开着的：没有 session / node 时插入应失败。
    let err = store
        .upsert_node_session("不存在的会话", "不存在的节点", 0, "root")
        .await;
    assert!(err.is_err(), "外键约束应生效");
}

// ============================ settings ============================

#[tokio::test]
async fn settings_kv() {
    let store = Store::open_in_memory().await.unwrap();
    assert_eq!(store.get_setting("retention").await.unwrap(), None);

    store.set_setting("retention", "normal").await.unwrap();
    store.set_setting("interval", "2").await.unwrap();
    assert_eq!(
        store.get_setting("retention").await.unwrap().as_deref(),
        Some("normal")
    );

    store.set_setting("retention", "less").await.unwrap();
    assert_eq!(
        store.get_setting("retention").await.unwrap().as_deref(),
        Some("less")
    );

    assert_eq!(
        store.list_settings().await.unwrap(),
        vec![
            ("interval".to_string(), "2".to_string()),
            ("retention".to_string(), "less".to_string()),
        ]
    );

    assert!(store.delete_setting("interval").await.unwrap());
    assert!(!store.delete_setting("interval").await.unwrap());
}

// ============================ 维护周期 ============================

#[tokio::test]
async fn maintain_rolls_up_then_prunes() {
    let tmp = TempDb::new("maintain");
    let store = Store::open_with(&tmp.path, RetentionPreset::Less)
        .await
        .unwrap();
    let sid = store
        .get_or_create_series("local", "mem.available", &[], Some("bytes"))
        .await
        .unwrap();

    let now = 3 * YEAR;
    // 12 小时的 1m 数据，其中前 6 小时已超出 Less 预设对 m_1m 的保留期。
    let start = now - 12 * HOUR;
    let rows: Vec<MetricRow> = (0..(12 * 60))
        .map(|i| row(sid, start + i * 60, 30, 1.0, 3.0, 60.0, 2.0))
        .collect();
    store.insert_1m(&rows).await.unwrap();

    let (rolled, pruned) = store.maintain(now).await.unwrap();
    assert!(rolled > 0, "应产生粗粒度桶");
    assert!(pruned > 0, "应清掉超期的 1m 桶");

    // m_1m 只剩最近 6 小时（Less）。
    let left = store.count_tier(MetricLayer::M1m, sid).await.unwrap();
    assert!(left <= 6 * 60 + 1, "m_1m 剩余 {left} 条，超出 Less 保留期");
    assert!(left > 0);

    // 5m 层保留 3 天，12 小时的数据一条不少。
    assert_eq!(store.count_tier(MetricLayer::M5m, sid).await.unwrap(), 12 * 12);

    store.close().await;
}
