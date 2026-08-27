//! 节点、浏览器会话与「会话 × 节点」认证状态（design.md §8 的
//! `nodes` / `sessions` / `node_sessions` 三张表）。
//!
//! # 凭据处理硬约束（design.md §5.3）
//!
//! * `sessions.id` 存的是 **token 的 hash**，绝不存明文。本模块的所有入参都叫
//!   `token_hash`，本 crate 里不存在任何明文密码或明文 token 字段。
//! * 哈希在调用方完成（认证链路侧），存储层只负责搬运不可逆的摘要。
//! * 明文凭据不进日志、不进库、用完立即 zeroize。
//!
//! 会话分两层（`sessions` + `node_sessions`）是有意为之：MVP 中
//! `node_sessions` 永远只有 `local` 一行，但将来从「一会话一身份」改成
//! 「一会话多身份」会波及每一条鉴权路径。

use sqlx::Row;

use super::{Result, Store, now_unix};

// ============================ nodes ============================

/// 节点类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    /// Server 自带的本地 AgentCore。
    #[default]
    Local,
    /// 远程 Agent。
    Agent,
}

impl NodeKind {
    /// 落库用的字符串。
    pub const fn as_str(self) -> &'static str {
        match self {
            NodeKind::Local => "local",
            NodeKind::Agent => "agent",
        }
    }

    /// 从库里读回来。
    //
    // 不实现 `std::str::FromStr`：这个映射永不失败（非 "local" 一律视为 Agent），
    // `Result` 会逼调用方处理一个不存在的错误分支。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> NodeKind {
        match s {
            "local" => NodeKind::Local,
            _ => NodeKind::Agent,
        }
    }
}

/// 一个节点。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeRecord {
    /// 'local' 或 uuid。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 类型。
    pub kind: NodeKind,
    /// Agent 预共享 token 的 **hash**，绝不存明文。
    pub token_hash: Option<String>,
    /// 最近一次心跳，unix 秒。
    pub last_seen: Option<i64>,
    /// 创建时间，unix 秒。
    pub created_at: i64,
}

fn row_to_node(row: &sqlx::sqlite::SqliteRow) -> NodeRecord {
    NodeRecord {
        id: row.get("id"),
        name: row.get("name"),
        kind: NodeKind::from_str(&row.get::<String, _>("kind")),
        token_hash: row.get("token_hash"),
        last_seen: row.get("last_seen"),
        created_at: row.get("created_at"),
    }
}

// ============================ sessions ============================

/// 一个浏览器会话。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRecord {
    /// **token 的 hash**，绝不是明文 token。
    pub id: String,
    /// 创建时间，unix 秒。
    pub created_at: i64,
    /// 最近活跃时间，unix 秒。
    pub last_active: i64,
    /// User-Agent。
    pub user_agent: Option<String>,
    /// 来源地址。
    pub remote_addr: Option<String>,
}

fn row_to_session(row: &sqlx::sqlite::SqliteRow) -> SessionRecord {
    SessionRecord {
        id: row.get("id"),
        created_at: row.get("created_at"),
        last_active: row.get("last_active"),
        user_agent: row.get("user_agent"),
        remote_addr: row.get("remote_addr"),
    }
}

// ============================ node_sessions ============================

/// 某会话在某节点上的认证状态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NodeSession {
    /// 会话 id（token hash）。
    pub session_id: String,
    /// 节点 id。
    pub node_id: String,
    /// 该节点上的 uid。
    pub uid: i64,
    /// 该节点上的用户名。
    pub username: String,
    /// 是否已启用管理访问。
    pub elevated: bool,
    /// 提权时间，unix 秒。
    pub elevated_at: Option<i64>,
    /// 认证完成时间，unix 秒。
    pub authed_at: i64,
    /// 最近活跃时间，unix 秒。
    pub last_active: i64,
}

fn row_to_node_session(row: &sqlx::sqlite::SqliteRow) -> NodeSession {
    NodeSession {
        session_id: row.get("session_id"),
        node_id: row.get("node_id"),
        uid: row.get("uid"),
        username: row.get("username"),
        elevated: row.get("elevated"),
        elevated_at: row.get("elevated_at"),
        authed_at: row.get("authed_at"),
        last_active: row.get("last_active"),
    }
}

const NODE_COLUMNS: &str = "id, name, kind, token_hash, last_seen, created_at";
const SESSION_COLUMNS: &str = "id, created_at, last_active, user_agent, remote_addr";
const NODE_SESSION_COLUMNS: &str =
    "session_id, node_id, uid, username, elevated, elevated_at, authed_at, last_active";

impl Store {
    // -------------------- nodes --------------------

    /// 新增或更新一个节点。`created_at` 只在首次插入时写入。
    pub async fn upsert_node(
        &self,
        id: &str,
        name: &str,
        kind: NodeKind,
        token_hash: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO nodes (id, name, kind, token_hash, last_seen, created_at)
            VALUES (?, ?, ?, ?, NULL, ?)
            ON CONFLICT(id) DO UPDATE SET
                name       = excluded.name,
                kind       = excluded.kind,
                token_hash = COALESCE(excluded.token_hash, token_hash)
            "#,
        )
        .bind(id)
        .bind(name)
        .bind(kind.as_str())
        .bind(token_hash)
        .bind(now_unix())
        .execute(self.write_pool())
        .await?;
        Ok(())
    }

    /// 按 id 取节点。
    pub async fn get_node(&self, id: &str) -> Result<Option<NodeRecord>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE id = ?");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(self.read_pool())
            .await?;
        Ok(row.as_ref().map(row_to_node))
    }

    /// 列出全部节点。
    pub async fn list_nodes(&self) -> Result<Vec<NodeRecord>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes ORDER BY created_at, id");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.read_pool())
            .await?;
        Ok(rows.iter().map(row_to_node).collect())
    }

    /// 更新节点心跳时间。
    pub async fn touch_node(&self, id: &str, ts: i64) -> Result<bool> {
        let affected = sqlx::query("UPDATE nodes SET last_seen = ? WHERE id = ?")
            .bind(ts)
            .bind(id)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 删除节点；其 `node_sessions` 由外键级联删除。
    pub async fn delete_node(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM nodes WHERE id = ?")
            .bind(id)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    // -------------------- sessions --------------------

    /// 创建一个浏览器会话。
    ///
    /// `token_hash` 必须是 token 的哈希摘要——存储层不接受、也不应看到明文。
    pub async fn create_session(
        &self,
        token_hash: &str,
        user_agent: Option<&str>,
        remote_addr: Option<&str>,
    ) -> Result<SessionRecord> {
        let now = now_unix();
        sqlx::query(
            r#"
            INSERT INTO sessions (id, created_at, last_active, user_agent, remote_addr)
            VALUES (?, ?, ?, ?, ?)
            ON CONFLICT(id) DO UPDATE SET last_active = excluded.last_active
            "#,
        )
        .bind(token_hash)
        .bind(now)
        .bind(now)
        .bind(user_agent)
        .bind(remote_addr)
        .execute(self.write_pool())
        .await?;

        Ok(SessionRecord {
            id: token_hash.to_string(),
            created_at: now,
            last_active: now,
            user_agent: user_agent.map(str::to_string),
            remote_addr: remote_addr.map(str::to_string),
        })
    }

    /// 按 token hash 取会话。
    pub async fn get_session(&self, token_hash: &str) -> Result<Option<SessionRecord>> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = ?");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(token_hash)
            .fetch_optional(self.read_pool())
            .await?;
        Ok(row.as_ref().map(row_to_session))
    }

    /// 列出全部会话，按最近活跃倒序。
    pub async fn list_sessions(&self) -> Result<Vec<SessionRecord>> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions ORDER BY last_active DESC");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.read_pool())
            .await?;
        Ok(rows.iter().map(row_to_session).collect())
    }

    /// 刷新会话活跃时间。返回会话是否存在。
    pub async fn touch_session(&self, token_hash: &str, ts: i64) -> Result<bool> {
        let affected = sqlx::query("UPDATE sessions SET last_active = ? WHERE id = ?")
            .bind(ts)
            .bind(token_hash)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 删除会话（登出）；其 `node_sessions` 由外键级联删除。
    pub async fn delete_session(&self, token_hash: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM sessions WHERE id = ?")
            .bind(token_hash)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 清理 `last_active < idle_before` 的会话，返回清理条数。
    pub async fn prune_sessions(&self, idle_before: i64) -> Result<u64> {
        let affected = sqlx::query("DELETE FROM sessions WHERE last_active < ?")
            .bind(idle_before)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected)
    }

    // -------------------- node_sessions --------------------

    /// 记录（或刷新）某会话在某节点上的认证结果。
    ///
    /// 重新认证同一节点时保留原 `elevated` 状态由调用方决定：这里按
    /// 「重新认证 = 重置为未提权」处理，提权必须显式再走一次
    /// `/api/v1/auth/elevate/*`。
    pub async fn upsert_node_session(
        &self,
        session_id: &str,
        node_id: &str,
        uid: i64,
        username: &str,
    ) -> Result<NodeSession> {
        let now = now_unix();
        sqlx::query(
            r#"
            INSERT INTO node_sessions
                (session_id, node_id, uid, username, elevated, elevated_at, authed_at, last_active)
            VALUES (?, ?, ?, ?, 0, NULL, ?, ?)
            ON CONFLICT(session_id, node_id) DO UPDATE SET
                uid         = excluded.uid,
                username    = excluded.username,
                elevated    = 0,
                elevated_at = NULL,
                authed_at   = excluded.authed_at,
                last_active = excluded.last_active
            "#,
        )
        .bind(session_id)
        .bind(node_id)
        .bind(uid)
        .bind(username)
        .bind(now)
        .bind(now)
        .execute(self.write_pool())
        .await?;

        Ok(NodeSession {
            session_id: session_id.to_string(),
            node_id: node_id.to_string(),
            uid,
            username: username.to_string(),
            elevated: false,
            elevated_at: None,
            authed_at: now,
            last_active: now,
        })
    }

    /// 取某会话在某节点上的认证状态。
    pub async fn get_node_session(
        &self,
        session_id: &str,
        node_id: &str,
    ) -> Result<Option<NodeSession>> {
        let sql = format!(
            "SELECT {NODE_SESSION_COLUMNS} FROM node_sessions \
             WHERE session_id = ? AND node_id = ?"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(session_id)
            .bind(node_id)
            .fetch_optional(self.read_pool())
            .await?;
        Ok(row.as_ref().map(row_to_node_session))
    }

    /// 列出某会话已认证的全部节点。
    pub async fn list_node_sessions(&self, session_id: &str) -> Result<Vec<NodeSession>> {
        let sql = format!(
            "SELECT {NODE_SESSION_COLUMNS} FROM node_sessions \
             WHERE session_id = ? ORDER BY node_id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(session_id)
            .fetch_all(self.read_pool())
            .await?;
        Ok(rows.iter().map(row_to_node_session).collect())
    }

    /// 设置提权状态。开启时写入 `elevated_at`，关闭时清空。
    pub async fn set_elevated(
        &self,
        session_id: &str,
        node_id: &str,
        elevated: bool,
        ts: i64,
    ) -> Result<bool> {
        let affected = sqlx::query(
            r#"
            UPDATE node_sessions
               SET elevated    = ?,
                   elevated_at = CASE WHEN ? THEN ? ELSE NULL END,
                   last_active = ?
             WHERE session_id = ? AND node_id = ?
            "#,
        )
        .bind(elevated)
        .bind(elevated)
        .bind(ts)
        .bind(ts)
        .bind(session_id)
        .bind(node_id)
        .execute(self.write_pool())
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// 刷新「会话 × 节点」的活跃时间。
    pub async fn touch_node_session(
        &self,
        session_id: &str,
        node_id: &str,
        ts: i64,
    ) -> Result<bool> {
        let affected = sqlx::query(
            "UPDATE node_sessions SET last_active = ? WHERE session_id = ? AND node_id = ?",
        )
        .bind(ts)
        .bind(session_id)
        .bind(node_id)
        .execute(self.write_pool())
        .await?
        .rows_affected();
        Ok(affected > 0)
    }

    /// 注销某会话在某节点上的认证。
    pub async fn delete_node_session(&self, session_id: &str, node_id: &str) -> Result<bool> {
        let affected =
            sqlx::query("DELETE FROM node_sessions WHERE session_id = ? AND node_id = ?")
                .bind(session_id)
                .bind(node_id)
                .execute(self.write_pool())
                .await?
                .rows_affected();
        Ok(affected > 0)
    }
}
