//! 审计日志（design.md §8 的 `audit_log` 表，对应 `GET /api/v1/audit`）。
//!
//! 写入路径要求极低开销：一条 INSERT，不做任何同步等待之外的事。
//! 查询路径按 id 倒序做游标分页——`id` 是 AUTOINCREMENT，与写入顺序严格一致，
//! 比按 ts 分页稳定（同一秒内的多条记录不会漏也不会重）。

use sqlx::Row;

use super::{Result, Store, now_unix};

/// 操作结果，对应 `audit_log.result` 列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuditOutcome {
    /// 执行成功。
    #[default]
    Ok,
    /// 被拒绝（polkit / 权限 / 未提权）。
    Denied,
    /// 执行出错。
    Error,
}

impl AuditOutcome {
    /// 落库用的字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Error => "error",
        }
    }

    /// 从库里读回来。未知取值一律当作 `Error`，避免读日志时报错。
    //
    // 不实现 `std::str::FromStr`：这个映射永不失败（未知取值有兜底），
    // `Result` 会逼调用方处理一个不存在的错误分支。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> AuditOutcome {
        match s {
            "ok" => AuditOutcome::Ok,
            "denied" => AuditOutcome::Denied,
            _ => AuditOutcome::Error,
        }
    }
}

impl std::fmt::Display for AuditOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 待写入的审计记录。
#[derive(Debug, Clone, Default)]
pub struct NewAuditEntry {
    /// unix 秒；`None` 表示取当前时间。
    pub ts: Option<i64>,
    /// 'local' 或节点 ID。
    pub node_id: String,
    /// 执行者用户名。
    pub username: String,
    /// 执行者 uid。
    pub uid: Option<i64>,
    /// 是否处于提权状态。
    pub elevated: bool,
    /// 动作，如 `service.start`。
    pub action: String,
    /// 目标，如 `nginx.service`。
    pub target: Option<String>,
    /// 附加参数，JSON 串。**不得包含任何凭据**（design.md §5.3）。
    pub params: Option<String>,
    /// 结果。
    pub result: AuditOutcome,
    /// 补充说明，如错误信息。
    pub detail: Option<String>,
    /// 来源地址。
    pub remote_addr: Option<String>,
}

impl NewAuditEntry {
    /// 最小构造：节点、用户、动作、结果。其余字段用链式 setter 补。
    pub fn new(
        node_id: impl Into<String>,
        username: impl Into<String>,
        action: impl Into<String>,
        result: AuditOutcome,
    ) -> NewAuditEntry {
        NewAuditEntry {
            node_id: node_id.into(),
            username: username.into(),
            action: action.into(),
            result,
            ..NewAuditEntry::default()
        }
    }

    /// 设置操作目标。
    pub fn target(mut self, target: impl Into<String>) -> NewAuditEntry {
        self.target = Some(target.into());
        self
    }

    /// 设置执行者 uid 与提权状态。
    pub fn actor(mut self, uid: i64, elevated: bool) -> NewAuditEntry {
        self.uid = Some(uid);
        self.elevated = elevated;
        self
    }

    /// 设置 JSON 参数。
    pub fn params(mut self, params: impl Into<String>) -> NewAuditEntry {
        self.params = Some(params.into());
        self
    }

    /// 设置补充说明。
    pub fn detail(mut self, detail: impl Into<String>) -> NewAuditEntry {
        self.detail = Some(detail.into());
        self
    }

    /// 设置来源地址。
    pub fn remote_addr(mut self, addr: impl Into<String>) -> NewAuditEntry {
        self.remote_addr = Some(addr.into());
        self
    }
}

/// 已落库的审计记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    /// 自增主键，同时充当分页游标。
    pub id: i64,
    /// unix 秒。
    pub ts: i64,
    /// 节点 ID。
    pub node_id: String,
    /// 用户名。
    pub username: String,
    /// uid。
    pub uid: Option<i64>,
    /// 是否提权。
    pub elevated: bool,
    /// 动作。
    pub action: String,
    /// 目标。
    pub target: Option<String>,
    /// JSON 参数。
    pub params: Option<String>,
    /// 结果。
    pub result: AuditOutcome,
    /// 补充说明。
    pub detail: Option<String>,
    /// 来源地址。
    pub remote_addr: Option<String>,
}

/// 查询过滤条件。全部字段可选，`limit` 为 0 时取默认值。
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// 只看某个节点。
    pub node_id: Option<String>,
    /// 只看某个用户。
    pub username: Option<String>,
    /// 精确匹配动作。
    pub action: Option<String>,
    /// 只看某种结果。
    pub result: Option<AuditOutcome>,
    /// `ts >= since`。
    pub since: Option<i64>,
    /// `ts < until`。
    pub until: Option<i64>,
    /// 游标：只返回 `id < cursor` 的记录（上一页 `next_cursor` 原样传回）。
    pub cursor: Option<i64>,
    /// 每页条数，0 表示用 [`AuditFilter::DEFAULT_LIMIT`]。
    pub limit: i64,
}

impl AuditFilter {
    /// 默认每页条数。
    pub const DEFAULT_LIMIT: i64 = 100;
    /// 每页条数上限。
    pub const MAX_LIMIT: i64 = 1000;

    fn effective_limit(&self) -> i64 {
        if self.limit <= 0 {
            AuditFilter::DEFAULT_LIMIT
        } else {
            self.limit.min(AuditFilter::MAX_LIMIT)
        }
    }
}

/// 一页审计记录。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditPage {
    /// 按 id 倒序（即时间倒序）。
    pub entries: Vec<AuditEntry>,
    /// 下一页游标；`None` 表示已到末页。
    pub next_cursor: Option<i64>,
}

/// 动态 WHERE 的绑定值。条件是代码里写死的，只有值来自外部。
enum Bind {
    Int(i64),
    Text(String),
}

impl Store {
    /// 写入一条审计记录，返回其 id。
    pub async fn audit_write(&self, entry: &NewAuditEntry) -> Result<i64> {
        let ts = entry.ts.unwrap_or_else(now_unix);

        let id: i64 = sqlx::query(
            r#"
            INSERT INTO audit_log
                (ts, node_id, username, uid, elevated, action, target, params, result, detail, remote_addr)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            RETURNING id
            "#,
        )
        .bind(ts)
        .bind(&entry.node_id)
        .bind(&entry.username)
        .bind(entry.uid)
        .bind(entry.elevated)
        .bind(&entry.action)
        .bind(entry.target.as_deref())
        .bind(entry.params.as_deref())
        .bind(entry.result.as_str())
        .bind(entry.detail.as_deref())
        .bind(entry.remote_addr.as_deref())
        .fetch_one(self.write_pool())
        .await?
        .get("id");

        Ok(id)
    }

    /// 分页查询审计日志，按 id 倒序。
    pub async fn audit_query(&self, filter: &AuditFilter) -> Result<AuditPage> {
        let mut conds: Vec<&'static str> = Vec::new();
        let mut binds: Vec<Bind> = Vec::new();

        if let Some(v) = &filter.node_id {
            conds.push("node_id = ?");
            binds.push(Bind::Text(v.clone()));
        }
        if let Some(v) = &filter.username {
            conds.push("username = ?");
            binds.push(Bind::Text(v.clone()));
        }
        if let Some(v) = &filter.action {
            conds.push("action = ?");
            binds.push(Bind::Text(v.clone()));
        }
        if let Some(v) = filter.result {
            conds.push("result = ?");
            binds.push(Bind::Text(v.as_str().to_string()));
        }
        if let Some(v) = filter.since {
            conds.push("ts >= ?");
            binds.push(Bind::Int(v));
        }
        if let Some(v) = filter.until {
            conds.push("ts < ?");
            binds.push(Bind::Int(v));
        }
        if let Some(v) = filter.cursor {
            conds.push("id < ?");
            binds.push(Bind::Int(v));
        }

        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conds.join(" AND "))
        };

        // 多取一条用来判断是否还有下一页。
        let limit = filter.effective_limit();
        let sql = format!(
            r#"
            SELECT id, ts, node_id, username, uid, elevated,
                   action, target, params, result, detail, remote_addr
            FROM audit_log
            {where_clause}
            ORDER BY id DESC
            LIMIT ?
            "#
        );

        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for b in binds {
            q = match b {
                Bind::Int(v) => q.bind(v),
                Bind::Text(v) => q.bind(v),
            };
        }
        let rows = q.bind(limit + 1).fetch_all(self.read_pool()).await?;

        let has_more = rows.len() as i64 > limit;
        let mut entries: Vec<AuditEntry> = rows
            .iter()
            .take(limit as usize)
            .map(|row| AuditEntry {
                id: row.get("id"),
                ts: row.get("ts"),
                node_id: row.get("node_id"),
                username: row.get("username"),
                uid: row.get("uid"),
                elevated: row.get("elevated"),
                action: row.get("action"),
                target: row.get("target"),
                params: row.get("params"),
                result: AuditOutcome::from_str(&row.get::<String, _>("result")),
                detail: row.get("detail"),
                remote_addr: row.get("remote_addr"),
            })
            .collect();

        let next_cursor = if has_more {
            entries.last().map(|e| e.id)
        } else {
            None
        };
        entries.shrink_to_fit();

        Ok(AuditPage {
            entries,
            next_cursor,
        })
    }

    /// 按 id 取单条。
    pub async fn audit_get(&self, id: i64) -> Result<Option<AuditEntry>> {
        let page = self
            .audit_query(&AuditFilter {
                cursor: Some(id + 1),
                limit: 1,
                ..AuditFilter::default()
            })
            .await?;
        Ok(page.entries.into_iter().find(|e| e.id == id))
    }

    /// 删除 `ts < before_ts` 的审计记录，返回删除条数。
    pub async fn audit_prune(&self, before_ts: i64) -> Result<u64> {
        let affected = sqlx::query("DELETE FROM audit_log WHERE ts < ?")
            .bind(before_ts)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected)
    }
}
