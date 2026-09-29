//! 配置加载层 —— 见 `docs/design.md` §12。
//!
//! # 四层优先级（从低到高）
//!
//! 1. 内置默认值 —— [`Config::default`]
//! 2. `/etc/strixmaid/config.toml` —— 路径可用 `STRIXMAID_CONFIG` 覆盖
//! 3. 环境变量 `STRIXMAID_*` —— 嵌套用双下划线，如 `STRIXMAID_METRICS__INTERVAL_SECS`
//! 4. 命令行参数 —— 由调用方（server / agent 的 clap）以 [`figment::Figment`] provider 传入
//!
//! 本模块刻意不引入 clap：命令行解析是宿主二进制的职责，core 只提供接入点。
//! 宿主的典型用法：
//!
//! ```no_run
//! # use strixmaid_core::config::{self, Config};
//! # use serde::Serialize;
//! #[derive(Serialize)]
//! struct Cli {
//!     listen: Option<String>,
//!     data_dir: Option<std::path::PathBuf>,
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let cli = Cli { listen: Some("0.0.0.0:9700".into()), data_dir: None };
//! // `cli_layer` 会剔除所有 None，避免「未指定的命令行参数」把低优先级来源清空。
//! let _cfg = Config::load(Some(config::cli_layer(&cli)?))?;
//! # Ok(())
//! # }
//! ```
//!
//! # 时长字段一律用「整数秒」
//!
//! 所有时长字段统一为 `u64` 秒，字段名带 `_secs` 后缀（`interval_secs`、`ring_secs`、
//! `idle_timeout_secs` …）。理由：
//!
//! * 环境变量层必须能表达同样的值。`STRIXMAID_METRICS__INTERVAL_SECS=5` 一目了然，
//!   而人类可读形式（`"2s"` / `"1h"`）在 env 里要额外处理引号与 figment 的宽松解析
//!   （`Value` 会把 `2` 猜成数字、把 `2s` 留成字符串），两层表示不一致是运维事故的温床；
//! * 当前依赖里没有 `humantime` / `humantime-serde`，自己写 duration 解析器等于
//!   凭空引入一处需要单独测试的解析逻辑，收益不足；
//! * 字段名里的 `_secs` 后缀让单位随字段名一起出现在报错、日志和示例配置中，
//!   不存在「这个 60 是秒还是毫秒」的歧义。
//!
//! 代价是 `ring_secs = 3600` 不如 `"1h"` 直观，因此 [`Config::example_toml`] 的注释中
//! 对每个时长都标注了等价的人类可读时间。

mod defaults;
mod error;
mod example;
mod file;
mod load;
mod schema;
mod validate;

pub use defaults::*;
pub use error::{ConfigError, FieldError, Result};
pub use file::ConfigFile;
pub use load::cli_layer;
pub use schema::{
    AuditConfig, Config, FilesConfig, LogConfig, LogLevel, MetricsConfig, SessionConfig,
    TerminalConfig,
};
pub use validate::is_absolute_root;

/// 保留期与层级契约仍由 types 定义，配置层只重导出。
pub use strixmaid_types::metrics::{MetricLayer, RetentionPreset};

use std::{net::SocketAddr, path::PathBuf};

impl Config {
    // ---------------------------------------------------------------- 派生值

    /// 解析后的监听地址。[`Config::validate`] 已保证可解析，但此处仍返回 `Result`，
    /// 以免手工构造的 `Config` 绕过校验后在这里 panic。
    pub fn listen_addr(&self) -> Result<SocketAddr> {
        self.listen.trim().parse::<SocketAddr>().map_err(|_| {
            ConfigError::Invalid(vec![FieldError::new(
                "listen",
                &self.listen,
                "必须是可解析的 `IP:端口`",
            )])
        })
    }

    /// SQLite 数据库文件路径。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join(DB_FILE_NAME)
    }

    /// helper 的 Unix socket 路径（§10，权限 0600）。
    pub fn helper_socket_path(&self) -> PathBuf {
        self.run_dir.join(HELPER_SOCKET_NAME)
    }
}

#[cfg(test)]
mod tests;
