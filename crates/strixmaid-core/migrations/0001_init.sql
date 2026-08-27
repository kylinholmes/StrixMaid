-- StrixMaid Phase 0 初始 schema
-- 严格对应 docs/design.md §8「数据模型（SQLite）」，字段名与顺序不作任何改动。
-- 注意：min / max / sum 是 SQLite 的聚合函数名，作为列名使用时一律加双引号，
--       建表、写入、查询、UPSERT 的 excluded 引用处都要保持一致。

-- ================= 时序 =================

CREATE TABLE series (
  id      INTEGER PRIMARY KEY,
  node    TEXT    NOT NULL,               -- 'local' 或节点 ID
  metric  TEXT    NOT NULL,               -- 'cpu.usage' / 'mem.available' / 'disk.read_bytes'
  labels  TEXT    NOT NULL DEFAULT '',    -- 'dev=sda' / 'iface=eth0'，k=v 按键排序后拼接
  unit    TEXT,
  UNIQUE(node, metric, labels)
);

-- m_1m / m_5m / m_15m / m_12h / m_1d 五张同构表。
-- WITHOUT ROWID + 复合主键 (series_id, ts) 使数据按 series 聚簇，
-- 查询单条曲线的时间范围是一次顺序扫描（design.md §8）。

CREATE TABLE m_1m (
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  ts        INTEGER NOT NULL,             -- 桶起始时间，unix 秒
  cnt       INTEGER NOT NULL,             -- 实际采样点数，用于加权聚合与缺失检测
  "min"     REAL    NOT NULL,
  "max"     REAL    NOT NULL,
  "sum"     REAL    NOT NULL,             -- avg = sum / cnt；存 sum 使逐级聚合精确无累积误差
  med       REAL    NOT NULL,             -- 1m 层为真中位数，粗粒度层为 median of medians
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;

CREATE TABLE m_5m (
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  ts        INTEGER NOT NULL,
  cnt       INTEGER NOT NULL,
  "min"     REAL    NOT NULL,
  "max"     REAL    NOT NULL,
  "sum"     REAL    NOT NULL,
  med       REAL    NOT NULL,
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;

CREATE TABLE m_15m (
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  ts        INTEGER NOT NULL,
  cnt       INTEGER NOT NULL,
  "min"     REAL    NOT NULL,
  "max"     REAL    NOT NULL,
  "sum"     REAL    NOT NULL,
  med       REAL    NOT NULL,
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;

CREATE TABLE m_12h (
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  ts        INTEGER NOT NULL,
  cnt       INTEGER NOT NULL,
  "min"     REAL    NOT NULL,
  "max"     REAL    NOT NULL,
  "sum"     REAL    NOT NULL,
  med       REAL    NOT NULL,
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;

CREATE TABLE m_1d (
  series_id INTEGER NOT NULL REFERENCES series(id) ON DELETE CASCADE,
  ts        INTEGER NOT NULL,
  cnt       INTEGER NOT NULL,
  "min"     REAL    NOT NULL,
  "max"     REAL    NOT NULL,
  "sum"     REAL    NOT NULL,
  med       REAL    NOT NULL,
  PRIMARY KEY (series_id, ts)
) WITHOUT ROWID;

-- ================= 节点与会话（仅 Server） =================

CREATE TABLE nodes (
  id         TEXT PRIMARY KEY,            -- 'local' 或 uuid
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL,               -- 'local' | 'agent'
  token_hash TEXT,                        -- Agent 预共享 token 的 hash
  last_seen  INTEGER,
  created_at INTEGER NOT NULL
);

-- 浏览器会话。id 存的是 token 的 hash，绝不存明文（design.md §5.3）。
CREATE TABLE sessions (
  id          TEXT PRIMARY KEY,
  created_at  INTEGER NOT NULL,
  last_active INTEGER NOT NULL,
  user_agent  TEXT,
  remote_addr TEXT
);

-- 某会话在某节点上的认证状态。
CREATE TABLE node_sessions (
  session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  node_id     TEXT NOT NULL REFERENCES nodes(id)    ON DELETE CASCADE,
  uid         INTEGER NOT NULL,
  username    TEXT NOT NULL,
  elevated    INTEGER NOT NULL DEFAULT 0,
  elevated_at INTEGER,
  authed_at   INTEGER NOT NULL,
  last_active INTEGER NOT NULL,
  PRIMARY KEY (session_id, node_id)
);

CREATE TABLE audit_log (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          INTEGER NOT NULL,
  node_id     TEXT    NOT NULL,
  username    TEXT    NOT NULL,
  uid         INTEGER,
  elevated    INTEGER NOT NULL,
  action      TEXT    NOT NULL,           -- 'service.start' / 'process.kill' / 'file.write'
  target      TEXT,                       -- 'nginx.service' / '1234' / '/etc/hosts'
  params      TEXT,                       -- JSON
  result      TEXT    NOT NULL,           -- 'ok' | 'denied' | 'error'
  detail      TEXT,
  remote_addr TEXT
);
CREATE INDEX idx_audit_ts ON audit_log(ts DESC);

CREATE TABLE settings (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
