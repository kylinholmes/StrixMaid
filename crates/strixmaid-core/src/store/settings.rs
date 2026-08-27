//! 键值设置表（design.md §8 的 `settings`）。
//!
//! 存的是运行期可变的少量配置（例如保留期预设的当前取值），
//! 与 `/etc/strixmaid/config.toml` 里的启动配置分开——后者由 `config.rs` 负责。

use sqlx::Row;

use super::{Result, Store};

impl Store {
    /// 读取一个设置项。
    pub async fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
            .bind(key)
            .fetch_optional(self.read_pool())
            .await?;
        Ok(row.map(|r| r.get::<String, _>("value")))
    }

    /// 写入（覆盖）一个设置项。
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(self.write_pool())
        .await?;
        Ok(())
    }

    /// 删除一个设置项，返回是否存在过。
    pub async fn delete_setting(&self, key: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM settings WHERE key = ?")
            .bind(key)
            .execute(self.write_pool())
            .await?
            .rows_affected();
        Ok(affected > 0)
    }

    /// 列出全部设置项，按 key 升序。
    pub async fn list_settings(&self) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT key, value FROM settings ORDER BY key")
            .fetch_all(self.read_pool())
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get::<String, _>("key"), r.get::<String, _>("value")))
            .collect())
    }
}
