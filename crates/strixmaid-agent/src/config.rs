//! Agent 配置（roadmap/05 §3.1）。独立于 Server 的 `Config`。
//!
//! 加载顺序：内置默认 → TOML → 环境变量 `STRIXMAID_AGENT_*`（嵌套用 `__`）。
//! TOML 路径来自 `--config`；显式给出的文件必须存在，缺省路径
//! （`/etc/strixmaid/agent.toml`）不存在则静默跳过——与 Server 的行为一致。
//!
//! # 偏离记录（相对 roadmap/05 §3.1 的表）
//!
//! `tls.insecure` 未实现，`wss://` 暂不支持：TLS 栈（rustls 还是 native-tls、
//! 与 musl 静态链接的关系）该随 `06-packaging.md` 一并决策，先为一个开发用
//! 开关引入整套 TLS 依赖不划算。当前 `server_url` 只接受 `ws://`，配了
//! `wss://` 在校验时报错并说明原因。

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
use strixmaid_core::config::MetricsConfig;

/// 缺省配置文件路径。
pub const DEFAULT_CONFIG_PATH: &str = "/etc/strixmaid/agent.toml";

/// Agent 运行时配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Server 地址，如 `ws://server:9700`。必填。路径部分不用写，
    /// Agent 自己拼 `/ws/agent`。
    pub server_url: String,
    /// 节点稳定标识；必须与 Server 上 `POST /nodes` 登记的 id 一致。
    /// 缺省取 `/etc/machine-id`。
    pub node_id: Option<String>,
    /// 显示名；缺省取主机名。
    pub node_name: Option<String>,
    /// 预共享 token（`POST /nodes` 的响应）。与 `token_file` 二选一。
    pub token: Option<String>,
    /// 从文件读 token（首行，去空白）。适合不想把 token 写进配置文件的部署。
    pub token_file: Option<PathBuf>,
    /// 本地 SQLite 目录。
    pub data_dir: PathBuf,
    /// 采集配置，与 Server 的 `[metrics]` 完全同构。
    pub metrics: MetricsConfig,
    /// 常规推送节拍（秒）：每隔这么久把本地新落盘的 `m_1m` 行推给 Server。
    /// 落盘本身每分钟一次，节拍取它的零头即可；允许 5–300。
    ///
    /// roadmap/05 未给这个字段命名，属实现补充。
    pub sync_interval_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            server_url: String::new(),
            node_id: None,
            node_name: None,
            token: None,
            token_file: None,
            data_dir: PathBuf::from("/var/lib/strixmaid-agent"),
            metrics: MetricsConfig::default(),
            sync_interval_secs: 20,
        }
    }
}

impl AgentConfig {
    /// 按层加载。`path` 为 `--config` 的值；`None` 用缺省路径（可缺席）。
    pub fn load(path: Option<&Path>) -> anyhow::Result<AgentConfig> {
        let mut figment = Figment::from(Serialized::defaults(AgentConfig::default()));
        match path {
            Some(p) => {
                if !p.exists() {
                    bail!("配置文件 {} 不存在", p.display());
                }
                figment = figment.merge(Toml::file(p));
            }
            None => {
                let p = Path::new(DEFAULT_CONFIG_PATH);
                if p.exists() {
                    figment = figment.merge(Toml::file(p));
                }
            }
        }
        let cfg: AgentConfig = figment
            .merge(Env::prefixed("STRIXMAID_AGENT_").split("__"))
            .extract()
            .context("解析 Agent 配置失败")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验。错误信息面向改配置的人。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server_url.trim().is_empty() {
            bail!("server_url 必填，如 ws://server:9700");
        }
        if self.server_url.starts_with("wss://") {
            bail!(
                "暂不支持 wss://（TLS 栈随 06-packaging 一并决策）；\
                 开发与内网部署请用 ws://，公网请在 Server 前挂反代终结 TLS"
            );
        }
        if !self.server_url.starts_with("ws://") {
            bail!("server_url 必须以 ws:// 开头，收到 {}", self.server_url);
        }
        if self.token.is_none() && self.token_file.is_none() {
            bail!("token 与 token_file 必须配置其一（来自 Server 的 POST /nodes）");
        }
        if !(1..=60).contains(&self.metrics.interval_secs) {
            bail!(
                "metrics.interval_secs 允许 1–60，收到 {}",
                self.metrics.interval_secs
            );
        }
        if !(5..=300).contains(&self.sync_interval_secs) {
            bail!(
                "sync_interval_secs 允许 5–300，收到 {}",
                self.sync_interval_secs
            );
        }
        Ok(())
    }

    /// 本地数据库路径。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("strixmaid-agent.db")
    }

    /// 定下节点 id：配置优先，其次 `/etc/machine-id`。
    pub fn resolve_node_id(&self) -> anyhow::Result<String> {
        if let Some(id) = &self.node_id {
            return Ok(id.clone());
        }
        for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
            if let Ok(text) = std::fs::read_to_string(p) {
                let id = text.trim();
                if !id.is_empty() {
                    return Ok(id.to_string());
                }
            }
        }
        bail!("读不到 /etc/machine-id，请显式配置 node_id");
    }

    /// 定下 token：`token` 优先，其次读 `token_file` 首行。
    pub fn resolve_token(&self) -> anyhow::Result<String> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let path = self.token_file.as_ref().expect("validate 保证二者有一");
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 token_file {} 失败", path.display()))?;
        let token = text.lines().next().unwrap_or("").trim().to_string();
        if token.is_empty() {
            bail!("token_file {} 是空的", path.display());
        }
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_toml(toml: &str) -> anyhow::Result<AgentConfig> {
        let cfg: AgentConfig = Figment::from(Serialized::defaults(AgentConfig::default()))
            .merge(Toml::string(toml))
            .extract()?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn 最小配置与缺省值() {
        let cfg = from_toml(
            r#"
            server_url = "ws://server:9700"
            token = "abc"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from("/var/lib/strixmaid-agent"));
        assert_eq!(cfg.db_path(), PathBuf::from("/var/lib/strixmaid-agent/strixmaid-agent.db"));
        assert_eq!(cfg.sync_interval_secs, 20);
        assert_eq!(cfg.metrics.interval_secs, 2, "metrics 与 Server 同构同默认");
    }

    #[test]
    fn 必填与边界() {
        assert!(from_toml("").is_err(), "缺 server_url");
        assert!(
            from_toml(r#"server_url = "ws://s""#).is_err(),
            "缺 token / token_file"
        );
        assert!(
            from_toml(r#"server_url = "http://s"
token = "t""#)
            .is_err(),
            "只认 ws://"
        );
        let err = from_toml(r#"server_url = "wss://s"
token = "t""#)
        .unwrap_err();
        assert!(err.to_string().contains("wss"), "{err}");
        assert!(
            from_toml(r#"server_url = "ws://s"
token = "t"
sync_interval_secs = 1"#)
            .is_err()
        );
    }

    #[test]
    fn token_file_读首行() {
        let dir = std::env::temp_dir().join(format!("strixmaid-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("token");
        std::fs::write(&f, "  s3cret \n下一行不算\n").unwrap();
        let cfg = from_toml(&format!(
            "server_url = \"ws://s\"\ntoken_file = \"{}\"",
            f.display()
        ))
        .unwrap();
        assert_eq!(cfg.resolve_token().unwrap(), "s3cret");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
