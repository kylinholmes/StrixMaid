//! 存储层：SQLite + sqlx，Agent 与 Server 共用（design.md §3 / §7 / §8）。
//!
//! 设计要点：
//!
//! * schema 严格照 design.md §8，字段名与顺序不作改动。`min` / `max` / `sum`
//!   与 SQLite 聚合函数同名，所有 SQL 中一律加双引号。
//! * 连接分两个池：`write` 池固定 1 条连接，把写入串行化到单个连接上，
//!   彻底避免 `SQLITE_BUSY`；`read` 池若干条连接，靠 WAL 与写入并发。
//! * 全部 SQL 走 **运行时查询**（[`sqlx::query`] / [`sqlx::query_as`]），
//!   不使用编译期宏 `query!`，因此不需要 `DATABASE_URL` 或 `.sqlx` 离线缓存。
//!
//! 少数 SQL 需要按分层拼接表名（表名来自 [`MetricLayer`] 枚举，不含任何外部输入），
//! 或按 series 数量拼接 `?` 占位符，这类语句用 [`sqlx::AssertSqlSafe`] 包装；
//! 所有真实数据一律走 bind 参数。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::SqlitePool;
use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

/// 指标层与保留期预设的**唯一权威定义**在 `strixmaid-types`（它们是 API 契约的一部分）。
/// 本模块只补上存储侧的元数据表，见 [`TierSpec`]。
pub use strixmaid_types::metrics::{MetricLayer, RetentionPreset};

mod audit;
mod metrics;
mod series;
mod session;
mod settings;

#[cfg(test)]
mod tests;

pub use audit::{AuditEntry, AuditFilter, AuditOutcome, AuditPage, NewAuditEntry};
pub use metrics::{MAX_QUERY_POINTS, MetricRow, QueryResult, select_tier};
pub use series::{Series, canonical_labels};
pub use session::{NodeKind, NodeRecord, NodeSession, SessionRecord};

/// 内嵌的 migration 集合（`crates/strixmaid-core/migrations/`）。
///
/// `sqlx::migrate!` 只是把 .sql 文件在编译期 include 进来，不连数据库，
/// 因此不引入 `DATABASE_URL` 依赖。
static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// 读连接池大小。时序写入是单写多读模型，读并发主要来自 HTTP 查询。
const READ_POOL_SIZE: u32 = 4;

/// 获取连接的超时。
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(15);

/// SQLite 忙等超时。写入已在进程内串行化，这里只兜底外部进程持锁的情况。
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

// ============================ 错误 ============================

/// 存储层错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// 数据库执行失败。
    #[error("数据库操作失败: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// migration 执行失败。
    #[error("数据库迁移失败: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// 数据目录创建失败等 IO 问题。
    #[error("数据目录不可用: {0}")]
    Io(#[from] std::io::Error),
    /// 聚合链路非法：`to` 必须是 `from` 的直接上一层。
    #[error("非法的聚合层级: {from} -> {to}（{to} 的聚合来源应为 {expected}）")]
    InvalidRollup {
        /// 源分层。
        from: MetricLayer,
        /// 目标分层。
        to: MetricLayer,
        /// 目标分层实际应有的来源，`m_1m` 无来源时为 "内存环形缓冲"。
        expected: &'static str,
    },
    /// 该层不落盘（[`MetricLayer::Live`] 只存在于内存环形缓冲），没有对应的表。
    #[error("{0} 层不落盘，没有对应的表")]
    NotPersisted(MetricLayer),
}

/// 存储层 Result 别名。
pub type Result<T> = std::result::Result<T, StoreError>;

// ============================ 分层与保留期 ============================

const MINUTE: i64 = 60;
const HOUR: i64 = 60 * MINUTE;
const DAY: i64 = 24 * HOUR;
const YEAR: i64 = 365 * DAY;

/// 单个落盘层的静态元数据：桶宽、聚合来源与两套保留期预设。表驱动，见 design.md §7.2。
///
/// 分工：枚举 [`MetricLayer`] / [`RetentionPreset`] 是 API 契约，唯一定义在
/// `strixmaid-types`；**哪一层桶多宽、从哪层聚合来、各预设留多久**是存储实现细节，
/// 只在本模块定义一次，并以 [`MetricLayer`] 为键。
///
/// 表名不是本结构的字段——它就是层名本身（[`MetricLayer::as_str`]），见 [`TierSpec::table`]。
#[derive(Debug, Clone, Copy)]
pub struct TierSpec {
    /// 本行的键。恒为落盘层，不会是 [`MetricLayer::Live`]。
    pub layer: MetricLayer,
    /// 桶宽，秒。
    pub width: i64,
    /// 聚合来源层；`m_1m` 的来源是内存环形缓冲，故为 `None`。
    pub source: Option<MetricLayer>,
    /// Less 预设保留时长，秒。
    pub retain_less: i64,
    /// Normal 预设（默认）保留时长，秒。
    pub retain_normal: i64,
}

/// 五个落盘层的完整定义，顺序与 [`MetricLayer::PERSISTED`] 一致（细 -> 粗）。
///
/// 数值逐格对应 design.md §7.2 的表格，由 `retention_table_matches_design` 用例守住。
pub const TIERS: [TierSpec; 5] = [
    TierSpec {
        layer: MetricLayer::M1m,
        width: MINUTE,
        source: None,
        retain_less: 6 * HOUR,
        retain_normal: DAY,
    },
    TierSpec {
        layer: MetricLayer::M5m,
        width: 5 * MINUTE,
        source: Some(MetricLayer::M1m),
        retain_less: 3 * DAY,
        retain_normal: 7 * DAY,
    },
    TierSpec {
        layer: MetricLayer::M15m,
        width: 15 * MINUTE,
        source: Some(MetricLayer::M5m),
        retain_less: 14 * DAY,
        retain_normal: 30 * DAY,
    },
    TierSpec {
        layer: MetricLayer::M12h,
        width: 12 * HOUR,
        source: Some(MetricLayer::M15m),
        retain_less: 90 * DAY,
        retain_normal: 90 * DAY,
    },
    TierSpec {
        layer: MetricLayer::M1d,
        width: DAY,
        source: Some(MetricLayer::M12h),
        retain_less: YEAR,
        retain_normal: YEAR,
    },
];

/// 层在 [`TIERS`] 中的下标。顺序与 [`MetricLayer::PERSISTED`] 一致；
/// [`MetricLayer::Live`] 不落盘，没有下标。
const fn tier_index(layer: MetricLayer) -> Option<usize> {
    Some(match layer {
        MetricLayer::Live => return None,
        MetricLayer::M1m => 0,
        MetricLayer::M5m => 1,
        MetricLayer::M15m => 2,
        MetricLayer::M12h => 3,
        MetricLayer::M1d => 4,
    })
}

impl TierSpec {
    /// 查表。[`MetricLayer::Live`] 不落盘，返回 `None`。
    pub const fn of(layer: MetricLayer) -> Option<&'static TierSpec> {
        match tier_index(layer) {
            Some(i) => Some(&TIERS[i]),
            None => None,
        }
    }

    /// 同 [`TierSpec::of`]，但把「该层不落盘」转成 [`StoreError::NotPersisted`]，便于 `?`。
    pub fn require(layer: MetricLayer) -> Result<&'static TierSpec> {
        TierSpec::of(layer).ok_or(StoreError::NotPersisted(layer))
    }

    /// SQLite 表名。落盘层的表名就是层名（design.md §8）。
    pub const fn table(&self) -> &'static str {
        self.layer.as_str()
    }

    /// 该预设下本层的保留时长（秒）。
    pub const fn retention(&self, preset: RetentionPreset) -> i64 {
        match preset {
            RetentionPreset::Less => self.retain_less,
            RetentionPreset::Normal => self.retain_normal,
        }
    }

    /// 把时间戳向下对齐到本层桶起点（UTC / unix epoch 对齐）。
    pub const fn align(&self, ts: i64) -> i64 {
        ts.div_euclid(self.width) * self.width
    }

    /// 更粗一层；最粗层为 `None`。
    pub const fn coarser(&self) -> Option<&'static TierSpec> {
        let i = match tier_index(self.layer) {
            Some(i) => i,
            None => return None,
        };
        if i + 1 < TIERS.len() {
            Some(&TIERS[i + 1])
        } else {
            None
        }
    }
}

// ============================ Store ============================

/// 存储句柄。内部是 `Arc`，克隆代价等同一次原子自增，可自由跨任务共享。
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    /// 读池：多连接，靠 WAL 与写入并发。
    read: SqlitePool,
    /// 写池：固定 1 条连接，所有写入在此串行。
    write: SqlitePool,
    /// series 注册缓存，避免每次落盘都查库。
    series_cache: RwLock<HashMap<SeriesKey, i64>>,
    /// 保留期预设。
    retention: RetentionPreset,
}

/// series 缓存键：(node, metric, 规范化 labels)。
type SeriesKey = (String, String, String);

impl Store {
    /// 打开（必要时创建）数据库，开 WAL 并跑完全部 migration。
    ///
    /// 保留期预设取默认值 [`RetentionPreset::Normal`]。
    pub async fn open(path: &Path) -> Result<Store> {
        Store::open_with(path, RetentionPreset::default()).await
    }

    /// 同 [`Store::open`]，但指定保留期预设。
    pub async fn open_with(path: &Path, retention: RetentionPreset) -> Result<Store> {
        // 数据目录默认 /var/lib/strixmaid/，首次启动时可能不存在。
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(dir).await?;
        }

        let options = file_connect_options(path);

        // 写池先建：它负责建库与跑 migration，读池随后连上已存在的库。
        let write = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options.clone())
            .await?;

        MIGRATOR.run(&write).await?;
        tracing::debug!(path = %path.display(), "数据库 migration 完成");

        let read = SqlitePoolOptions::new()
            .max_connections(READ_POOL_SIZE)
            .min_connections(1)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await?;

        Ok(Store::from_pools(read, write, retention))
    }

    /// 打开一个纯内存数据库，仅用于测试与临时用途。
    ///
    /// 内存库无法在多条连接间共享，因此读写共用同一条连接（池大小固定为 1），
    /// 且禁用空闲回收——连接一旦关闭，库的内容就没了。
    pub async fn open_in_memory() -> Result<Store> {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            // 内存库不支持 WAL，用 MEMORY journal。
            .journal_mode(SqliteJournalMode::Memory)
            .synchronous(SqliteSynchronous::Off)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);

        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .acquire_timeout(ACQUIRE_TIMEOUT)
            .connect_with(options)
            .await?;

        MIGRATOR.run(&pool).await?;

        Ok(Store::from_pools(
            pool.clone(),
            pool,
            RetentionPreset::default(),
        ))
    }

    fn from_pools(read: SqlitePool, write: SqlitePool, retention: RetentionPreset) -> Store {
        Store {
            inner: Arc::new(Inner {
                read,
                write,
                series_cache: RwLock::new(HashMap::new()),
                retention,
            }),
        }
    }

    /// 关闭全部连接。等待正在执行的语句结束，并把 WAL 检查点写回主库。
    pub async fn close(&self) {
        self.inner.write.close().await;
        // 内存库两个池是同一个，close 幂等。
        self.inner.read.close().await;
    }

    /// 是否已关闭。
    pub fn is_closed(&self) -> bool {
        self.inner.write.is_closed()
    }

    /// 当前保留期预设。
    pub fn retention(&self) -> RetentionPreset {
        self.inner.retention
    }

    /// 读连接池。仅用于只读查询——写入必须走 [`Store`] 提供的方法，
    /// 否则会绕过单写连接的串行化保证。
    pub fn read_pool(&self) -> &SqlitePool {
        &self.inner.read
    }

    /// 写连接池（固定 1 条连接）。
    pub(crate) fn write_pool(&self) -> &SqlitePool {
        &self.inner.write
    }
}

/// 文件库的连接参数：为「单写多读的时序写入」调优。
fn file_connect_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        // WAL：读不阻塞写、写不阻塞读，是单写多读的前提。
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL：WAL 下只在 checkpoint 时 fsync。掉电最多丢最近若干秒的指标，
        // 对观测数据是划算的取舍。
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .busy_timeout(BUSY_TIMEOUT)
        // 约 16MB 页缓存（负值表示 KiB）。
        .pragma("cache_size", "-16000")
        .pragma("temp_store", "MEMORY")
        // WAL 超过约 4MB（1000 页）自动 checkpoint，避免 -wal 无限增长。
        .pragma("wal_autocheckpoint", "1000")
}

/// 当前 unix 秒。
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
