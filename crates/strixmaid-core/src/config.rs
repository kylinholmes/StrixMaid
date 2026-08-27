//! 配置加载层 —— 见 `docs/design.md` §12。
//!
//! # 四层优先级（从低到高）
//!
//! 1. 内置默认值 —— [`Config::default`]
//! 2. `/etc/strixmaid/config.toml` —— 路径可用 `STRIXMAID_CONFIG` 覆盖
//! 3. 环境变量 `STRIXMAID_*` —— 嵌套用双下划线，如 `STRIXMAID_METRICS__INTERVAL_SECS`
//! 4. 命令行参数 —— 由调用方（server / agent 的 clap）以 [`Figment`] provider 传入
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

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use figment::value::{Dict, Value};
use serde::{Deserialize, Deserializer, Serialize};

/// 保留期预设是 API 契约的一部分，唯一定义在 `strixmaid-types`；
/// 「哪一层留多久」那张表则归 [`crate::store`]（见 [`crate::store::TierSpec`]）。
/// 本模块两者都不重复定义，只 import。
pub use strixmaid_types::metrics::{MetricLayer, RetentionPreset};

// ===========================================================================
// 常量
// ===========================================================================

/// 默认配置文件路径（§12）。
pub const DEFAULT_CONFIG_PATH: &str = "/etc/strixmaid/config.toml";
/// 环境变量前缀。
pub const ENV_PREFIX: &str = "STRIXMAID_";
/// 环境变量里表示「嵌套一层」的分隔符。
pub const ENV_NESTED_SEPARATOR: &str = "__";
/// 用于覆盖配置文件路径的环境变量。它本身不是配置项，会被 env provider 过滤掉。
pub const CONFIG_PATH_ENV: &str = "STRIXMAID_CONFIG";

/// 默认监听地址（§12）。
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9700";
/// 默认数据目录（§12）。
pub const DEFAULT_DATA_DIR: &str = "/var/lib/strixmaid";
/// 默认运行目录（§12）。
pub const DEFAULT_RUN_DIR: &str = "/run/strixmaid";
/// helper 二进制默认值：不含 `/`，交由 `Command::new` 按 `PATH` 查找。
pub const DEFAULT_HELPER_PATH: &str = "strixmaid-helper";
/// 默认 PAM 服务名（§5.4）。
pub const DEFAULT_PAM_SERVICE: &str = "strixmaid";

/// SQLite 数据库文件名，位于 `data_dir` 下。
pub const DB_FILE_NAME: &str = "strixmaid.db";
/// helper 的 Unix socket 文件名，位于 `run_dir` 下（§10）。
pub const HELPER_SOCKET_NAME: &str = "helper.sock";

/// 采集间隔下限（秒），§7.2 规定可配 1–60s。
pub const METRICS_INTERVAL_MIN_SECS: u64 = 1;
/// 采集间隔上限（秒）。
pub const METRICS_INTERVAL_MAX_SECS: u64 = 60;
/// 内存环形缓冲时长下限（秒）：低于 1 分钟连一个 `m_1m` 桶都凑不满。
pub const METRICS_RING_MIN_SECS: u64 = 60;
/// 内存环形缓冲时长上限（秒）：1 天。再长应该查落盘数据，而不是撑大常驻内存。
pub const METRICS_RING_MAX_SECS: u64 = 24 * 3600;
/// 会话空闲超时下限（秒）。
pub const SESSION_IDLE_MIN_SECS: u64 = 60;
/// 会话空闲超时上限（秒）：7 天。
pub const SESSION_IDLE_MAX_SECS: u64 = 7 * 24 * 3600;
/// 提权空闲超时下限（秒）。
pub const SESSION_ELEVATED_MIN_SECS: u64 = 30;
/// 提权空闲超时上限（秒）：1 天。
pub const SESSION_ELEVATED_MAX_SECS: u64 = 24 * 3600;

const HOUR: u64 = 3_600;

// ===========================================================================
// 错误
// ===========================================================================

/// 本模块的 `Result` 别名。
pub type Result<T, E = ConfigError> = std::result::Result<T, E>;

/// 单个配置项的校验错误。
///
/// 三要素齐全：**哪个字段**、**当前值是什么**、**合法范围是什么**——
/// 这是运维工具，配置报错必须直接可操作。同时附带对应的环境变量名，
/// 方便排查「明明改了配置文件却不生效」这类被高优先级来源覆盖的情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// 配置项的完整路径，如 `metrics.interval_secs`。
    pub field: String,
    /// 当前值（已转成可打印形式）。
    pub value: String,
    /// 期望：合法范围或格式说明。
    pub expected: String,
}

impl FieldError {
    /// 构造一条字段错误。
    pub fn new(
        field: impl Into<String>,
        value: impl fmt::Display,
        expected: impl Into<String>,
    ) -> Self {
        FieldError {
            field: field.into(),
            value: value.to_string(),
            expected: expected.into(),
        }
    }

    /// 该字段对应的环境变量名，如 `metrics.interval_secs` -> `STRIXMAID_METRICS__INTERVAL_SECS`。
    pub fn env_var(&self) -> String {
        format!(
            "{ENV_PREFIX}{}",
            self.field
                .replace('.', ENV_NESTED_SEPARATOR)
                .to_ascii_uppercase()
        )
    }
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "配置项 `{}`（环境变量 {}）当前值 `{}` 不合法：{}",
            self.field,
            self.env_var(),
            self.value,
            self.expected
        )
    }
}

/// 配置加载 / 校验失败。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// 读取或解析配置来源失败（TOML 语法错误、类型不匹配、未知字段等）。
    ///
    /// `figment::Error` 有 200 多字节，装箱以免把每个 `Result` 都撑大。
    #[error("读取配置失败：{0}")]
    Source(Box<figment::Error>),

    /// 配置值不合法。一次性报出全部问题，避免「改一个报一个」。
    #[error(
        "配置校验未通过（共 {} 项）：\n  - {}",
        .0.len(),
        .0.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n  - ")
    )]
    Invalid(Vec<FieldError>),
}

impl From<figment::Error> for ConfigError {
    fn from(error: figment::Error) -> Self {
        ConfigError::Source(Box::new(error))
    }
}

// ===========================================================================
// 枚举
// ===========================================================================

/// 日志级别（§12：日志写 stderr，交由 journald 收集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 关闭日志。
    #[serde(alias = "OFF", alias = "Off")]
    Off,
    /// 仅错误。
    #[serde(alias = "ERROR", alias = "Error")]
    Error,
    /// 警告及以上。
    #[serde(alias = "WARN", alias = "Warn", alias = "warning")]
    Warn,
    /// 默认级别。
    #[default]
    #[serde(alias = "INFO", alias = "Info")]
    Info,
    /// 调试。
    #[serde(alias = "DEBUG", alias = "Debug")]
    Debug,
    /// 全量跟踪。
    #[serde(alias = "TRACE", alias = "Trace")]
    Trace,
}

impl LogLevel {
    /// 全部取值，用于报错时列出候选。
    pub const ALL: [LogLevel; 6] = [
        LogLevel::Off,
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
        LogLevel::Trace,
    ];

    /// 小写字符串形式，可直接喂给 `tracing_subscriber` 的 `EnvFilter`。
    pub const fn as_str(self) -> &'static str {
        match self {
            LogLevel::Off => "off",
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 反序列化 `metrics.retention`。
///
/// [`RetentionPreset`] 的 serde 派生只认线格式（全小写），而运维手写 TOML 或
/// `STRIXMAID_METRICS__RETENTION` 时 `Normal` / `LESS` 都很常见。它的
/// [`FromStr`](std::str::FromStr) 正是大小写不敏感的那个入口，所以这里走它，
/// 而不是在 core 里另外抄一份带 alias 的枚举。
fn deserialize_retention<'de, D: Deserializer<'de>>(de: D) -> Result<RetentionPreset, D::Error> {
    let raw = String::deserialize(de)?;
    raw.parse::<RetentionPreset>()
        // ApiError 的 message 已经列出了候选值（less / normal）。
        .map_err(|e| serde::de::Error::custom(e.message))
}

// ===========================================================================
// 子配置
// ===========================================================================

/// 日志配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// 日志级别，默认 `info`。
    pub level: LogLevel,
}

/// 指标采集与存储配置（§7）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// 采集间隔（秒），默认 2，允许 1–60。
    pub interval_secs: u64,
    /// 内存环形缓冲保留时长（秒），默认 3600（1 小时，约 3MB）。
    pub ring_secs: u64,
    /// 落盘保留期预设，默认 `normal`。取值大小写不敏感。
    #[serde(deserialize_with = "deserialize_retention")]
    pub retention: RetentionPreset,
    /// 是否为每个 CPU 核采集全部 8 个状态（user/nice/system/…）。默认 `false`：
    /// 每核只保留 `cpu.core.usage` 一条曲线。128 核机器开启后每核 9 条 series，
    /// 环形缓冲会从约 7MB 涨到约 36MB，而面板上几乎没人看第 97 核的 softirq 历史。
    #[serde(default)]
    pub per_core_detail: bool,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        MetricsConfig {
            interval_secs: 2,
            ring_secs: HOUR,
            retention: RetentionPreset::default(),
            per_core_detail: false,
        }
    }
}

impl MetricsConfig {
    /// 采集间隔。
    pub const fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    /// 内存环形缓冲保留时长。
    pub const fn ring_duration(&self) -> Duration {
        Duration::from_secs(self.ring_secs)
    }

    /// 环形缓冲需要容纳的采样点数 = 缓冲时长 / 采集间隔（向上取整）。
    ///
    /// 校验保证 `interval_secs >= 1`，因此不会除零。
    pub const fn ring_capacity(&self) -> usize {
        if self.interval_secs == 0 {
            return 0;
        }
        self.ring_secs.div_ceil(self.interval_secs) as usize
    }

    /// 某一落盘层的保留时长（秒）。
    ///
    /// 表在 [`crate::store::TierSpec`]，本方法只做转发——「哪层留多久」（design.md §7.2）
    /// 全项目只定义一份。[`MetricLayer::Live`] 只在内存环形缓冲里，不落盘，返回 `None`。
    pub const fn retention_secs(&self, layer: MetricLayer) -> Option<u64> {
        match crate::store::TierSpec::of(layer) {
            Some(spec) => Some(spec.retention(self.retention) as u64),
            None => None,
        }
    }
}

/// 会话配置（§5 / §2.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionConfig {
    /// 会话空闲超时（秒）：超过该时长无任何请求则会话失效，需重新 PAM 认证。
    /// 默认 900（15 分钟）。
    pub idle_timeout_secs: u64,
    /// 提权状态的**独立**空闲超时（秒）：会话本身仍然有效，但超过该时长
    /// 没有管理操作，admin worker 回收、`elevated` 降回 false，需要重新提权。
    /// 默认 300（5 分钟），与 sudo 的 `timestamp_timeout` 一致。
    pub elevated_idle_timeout_secs: u64,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            idle_timeout_secs: 900,
            elevated_idle_timeout_secs: 300,
        }
    }
}

impl SessionConfig {
    /// 会话空闲超时。
    pub const fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.idle_timeout_secs)
    }

    /// 提权状态空闲超时。
    pub const fn elevated_idle_timeout(&self) -> Duration {
        Duration::from_secs(self.elevated_idle_timeout_secs)
    }
}

// ===========================================================================
// 顶层配置
// ===========================================================================

/// StrixMaid 运行时配置（§12）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// 监听地址，形如 `IP:端口`。默认 `127.0.0.1:9700`。
    /// MVP 不做 TLS，对外暴露走反向代理。
    pub listen: String,
    /// 数据目录，SQLite 数据库存放于此。默认 `/var/lib/strixmaid`。
    pub data_dir: PathBuf,
    /// 运行目录，helper 的 Unix socket 存放于此。默认 `/run/strixmaid`。
    pub run_dir: PathBuf,
    /// `strixmaid-helper` 二进制路径。默认 `strixmaid-helper`——
    /// 不含 `/` 的名字会被 `Command::new` 按 `PATH` 查找。
    pub helper_path: PathBuf,
    /// PAM 服务名，对应 `/etc/pam.d/<名字>`。默认 `strixmaid`。
    pub pam_service: String,
    /// 日志配置。
    pub log: LogConfig,
    /// 指标配置。
    pub metrics: MetricsConfig,
    /// 会话配置。
    pub session: SessionConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.to_string(),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
            helper_path: PathBuf::from(DEFAULT_HELPER_PATH),
            pam_service: DEFAULT_PAM_SERVICE.to_string(),
            log: LogConfig::default(),
            metrics: MetricsConfig::default(),
            session: SessionConfig::default(),
        }
    }
}

impl Config {
    /// 顶层配置键。用于把 `STRIXMAID_*` 里与配置无关的变量挡在外面
    /// （否则 `deny_unknown_fields` 会把 `STRIXMAID_CONFIG` 这类变量判成错误）。
    pub const TOP_LEVEL_KEYS: &'static [&'static str] = &[
        "listen",
        "data_dir",
        "run_dir",
        "helper_path",
        "pam_service",
        "log",
        "metrics",
        "session",
    ];

    // ---------------------------------------------------------------- 加载

    /// 实际生效的配置文件路径：`STRIXMAID_CONFIG` 优先，否则 [`DEFAULT_CONFIG_PATH`]。
    pub fn config_path() -> PathBuf {
        std::env::var_os(CONFIG_PATH_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_PATH))
    }

    /// 按四层优先级加载配置并校验。
    ///
    /// `extra` 是最高优先级的一层，通常由宿主用 [`cli_layer`] 从 clap 解析结果构造。
    /// 传 `None` 表示没有命令行覆盖。
    pub fn load(extra: Option<Figment>) -> Result<Config> {
        Config::load_from(Config::config_path(), extra)
    }

    /// 同 [`Config::load`]，但显式指定配置文件路径（用于 `--config` 与测试）。
    pub fn load_from(path: impl AsRef<Path>, extra: Option<Figment>) -> Result<Config> {
        Config::from_figment(Config::figment(path.as_ref(), extra))
    }

    /// 构造合并后的 [`Figment`]，但不 extract。
    ///
    /// 宿主如果想在解析前检查来源（`figment.metadata()`）、或者想把配置塞进
    /// 自己更大的结构里，可以从这里接入。
    pub fn figment(path: impl AsRef<Path>, extra: Option<Figment>) -> Figment {
        let path = path.as_ref();

        // merge 的顺序即优先级：后者覆盖前者。
        let mut figment = Figment::from(Serialized::defaults(Config::default()));

        // 配置文件层。两种「顺手」的写法都不对，所以先自己判断存在性：
        //
        // * `Toml::file()` 会**向上级目录逐层搜索**同名文件。守护进程的配置路径必须是
        //   确定的，否则从不同工作目录启动会读到不同的配置，是极难排查的事故；
        // * `Toml::file_exact()` 路径确定，但文件不存在时直接报 IO 错误，而首次启动时
        //   /etc/strixmaid/config.toml 通常还不存在（design.md §12：不需要任何安装物
        //   就能起服务、用系统账户登录）。
        //
        // 于是：不存在就整层跳过；存在则照常 merge——此时的语法错误、权限不足等
        // 仍会在 extract 时如实报错。静默吞掉一个写错的配置文件比直接失败危险得多。
        if path.is_file() {
            figment = figment.merge(Toml::file_exact(path));
        } else {
            tracing::debug!(
                path = %path.display(),
                "配置文件不存在，仅使用内置默认值 + 环境变量 + 命令行"
            );
        }

        figment = figment.merge(Config::env_provider());
        if let Some(extra) = extra {
            figment = figment.merge(extra);
        }
        figment
    }

    /// 环境变量层：前缀 `STRIXMAID_`，双下划线表示嵌套，且只接受已知的顶层键。
    pub fn env_provider() -> Env {
        Env::prefixed(ENV_PREFIX)
            .split(ENV_NESTED_SEPARATOR)
            .filter(|key| {
                // 此处 key 已剥掉前缀、已按 `__` 切分成点分路径，但尚未小写化。
                let root = key.as_str().split('.').next().unwrap_or_default();
                Config::TOP_LEVEL_KEYS
                    .iter()
                    .any(|known| root.eq_ignore_ascii_case(known))
            })
    }

    /// 从已构造好的 [`Figment`] 中提取并校验配置。
    pub fn from_figment(figment: Figment) -> Result<Config> {
        let config: Config = figment.extract()?;
        config.validate()?;
        Ok(config)
    }

    // ---------------------------------------------------------------- 校验

    /// 校验全部取值，一次性返回所有问题。
    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        // --- 监听地址 ---
        let listen = self.listen.trim();
        if listen.is_empty() {
            errors.push(FieldError::new(
                "listen",
                "<空>",
                format!("不能为空；应形如 `{DEFAULT_LISTEN}`"),
            ));
        } else if listen.parse::<SocketAddr>().is_err() {
            errors.push(FieldError::new(
                "listen",
                &self.listen,
                "必须是可解析的 `IP:端口`，如 `127.0.0.1:9700`、`0.0.0.0:9700`、`[::1]:9700`；\
                 不支持主机名，也不能省略端口",
            ));
        }

        // --- 目录与二进制路径 ---
        check_non_empty_path(
            "data_dir",
            &self.data_dir,
            "SQLite 数据库所在目录",
            &mut errors,
        );
        check_non_empty_path(
            "run_dir",
            &self.run_dir,
            "helper socket 所在目录",
            &mut errors,
        );
        check_non_empty_path(
            "helper_path",
            &self.helper_path,
            "strixmaid-helper 二进制路径；不含 `/` 时按 PATH 查找",
            &mut errors,
        );

        // --- PAM 服务名 ---
        if self.pam_service.is_empty() {
            errors.push(FieldError::new(
                "pam_service",
                "<空>",
                format!("不能为空；对应 /etc/pam.d/<名字>，默认 `{DEFAULT_PAM_SERVICE}`"),
            ));
        } else if self.pam_service.contains('/')
            || self.pam_service.contains('\0')
            || self.pam_service == "."
            || self.pam_service == ".."
        {
            errors.push(FieldError::new(
                "pam_service",
                &self.pam_service,
                "必须是一个合法文件名（对应 /etc/pam.d/<名字>），不能包含 `/` 或 NUL，也不能是 `.` / `..`",
            ));
        }

        // --- 指标 ---
        check_range(
            "metrics.interval_secs",
            self.metrics.interval_secs,
            METRICS_INTERVAL_MIN_SECS,
            METRICS_INTERVAL_MAX_SECS,
            "采集间隔",
            &mut errors,
        );
        check_range(
            "metrics.ring_secs",
            self.metrics.ring_secs,
            METRICS_RING_MIN_SECS,
            METRICS_RING_MAX_SECS,
            "内存环形缓冲时长",
            &mut errors,
        );
        if self.metrics.interval_secs > 0 && self.metrics.ring_secs < self.metrics.interval_secs {
            errors.push(FieldError::new(
                "metrics.ring_secs",
                self.metrics.ring_secs,
                format!(
                    "必须不小于 metrics.interval_secs（当前 {} 秒），否则环形缓冲连一个采样点都放不下",
                    self.metrics.interval_secs
                ),
            ));
        }

        // --- 会话 ---
        check_range(
            "session.idle_timeout_secs",
            self.session.idle_timeout_secs,
            SESSION_IDLE_MIN_SECS,
            SESSION_IDLE_MAX_SECS,
            "会话空闲超时",
            &mut errors,
        );
        check_range(
            "session.elevated_idle_timeout_secs",
            self.session.elevated_idle_timeout_secs,
            SESSION_ELEVATED_MIN_SECS,
            SESSION_ELEVATED_MAX_SECS,
            "提权状态空闲超时",
            &mut errors,
        );

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }

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

    // ---------------------------------------------------------------- 示例

    /// 一份带中文注释的完整示例配置，用于生成 `/etc/strixmaid/config.toml`。
    ///
    /// 其中所有取值均等于内置默认值（有单元测试保证），因此原样安装也不会改变行为。
    pub fn example_toml() -> &'static str {
        EXAMPLE_TOML
    }
}

// ===========================================================================
// 命令行接入点
// ===========================================================================

/// 把宿主解析好的命令行参数转换成可传给 [`Config::load`] 的最高优先级层。
///
/// **会递归剔除所有 `None` / 空值**：clap 的可选参数在未指定时序列化成 `null`，
/// 若直接交给 figment，会把配置文件与环境变量里的值覆盖成空——这是 figment
/// 分层配置里最常见的坑。
///
/// `args` 需要序列化成一张键值表，键名与 [`Config`] 的字段一一对应；嵌套项用
/// 嵌套结构（`metrics.interval_secs` 对应 `{ metrics: { interval_secs: .. } }`）。
pub fn cli_layer<T: Serialize>(args: T) -> Result<Figment> {
    let value = Value::serialize(args)?;
    let dict = value.into_dict().ok_or_else(|| {
        ConfigError::from(figment::Error::from(
            "命令行参数层必须序列化成键值表（struct 或 map）".to_string(),
        ))
    })?;
    Ok(Figment::from(Serialized::defaults(prune_empty_dict(dict))))
}

/// 递归剔除字典里的空值与空子字典。
fn prune_empty_dict(dict: Dict) -> Dict {
    dict.into_iter()
        .filter_map(|(key, value)| prune_empty_value(value).map(|v| (key, v)))
        .collect()
}

fn prune_empty_value(value: Value) -> Option<Value> {
    match value {
        Value::Empty(..) => None,
        Value::Dict(tag, dict) => {
            let dict = prune_empty_dict(dict);
            if dict.is_empty() {
                None
            } else {
                Some(Value::Dict(tag, dict))
            }
        }
        other => Some(other),
    }
}

// ===========================================================================
// 校验小工具
// ===========================================================================

fn check_non_empty_path(field: &str, path: &Path, what: &str, errors: &mut Vec<FieldError>) {
    if path.as_os_str().is_empty() {
        errors.push(FieldError::new(field, "<空>", format!("不能为空；{what}")));
    }
}

fn check_range(
    field: &str,
    value: u64,
    min: u64,
    max: u64,
    what: &str,
    errors: &mut Vec<FieldError>,
) {
    if value < min || value > max {
        errors.push(FieldError::new(
            field,
            value,
            format!("{what}必须在 {min} – {max} 秒之间（含两端）"),
        ));
    }
}

// ===========================================================================
// 示例配置
// ===========================================================================

const EXAMPLE_TOML: &str = r#"# StrixMaid 配置文件 —— /etc/strixmaid/config.toml
#
# 优先级（从低到高）：
#   内置默认值 < 本文件 < 环境变量 STRIXMAID_* < 命令行参数
#
# 环境变量命名：顶层字段直接大写加前缀（STRIXMAID_LISTEN、STRIXMAID_DATA_DIR），
# 嵌套字段用【双】下划线分隔（STRIXMAID_METRICS__INTERVAL_SECS、STRIXMAID_LOG__LEVEL）。
# 本文件的路径本身可用 STRIXMAID_CONFIG 覆盖。
#
# 所有时长字段统一以「秒」为单位，字段名带 _secs 后缀。
# 下面每一项的取值都等于内置默认值，可以按需修改或整行删除。

# 监听地址。只接受 `IP:端口`，不支持主机名。
# MVP 不提供 TLS：需要对外暴露时请放在 nginx / Caddy 等反向代理之后。
listen = "127.0.0.1:9700"

# 数据目录。SQLite 数据库（指标 / 会话 / 审计）存放于 <data_dir>/strixmaid.db。
data_dir = "/var/lib/strixmaid"

# 运行目录。helper 的 Unix socket（helper.sock，权限 0600）存放于此。
# 通常由 systemd unit 的 RuntimeDirectory=strixmaid 自动创建。
run_dir = "/run/strixmaid"

# strixmaid-helper 二进制路径。
# 不含 `/` 时按 PATH 查找；需要固定位置就写绝对路径，例如 "/usr/libexec/strixmaid-helper"。
helper_path = "strixmaid-helper"

# PAM 服务名，对应 /etc/pam.d/<名字>。安装包会按发行版写入对应模板。
pam_service = "strixmaid"

[log]
# 日志级别：off | error | warn | info | debug | trace
# 日志只写 stderr，交由 journald 收集，不自写日志文件。
level = "info"

[metrics]
# 采集间隔（秒），允许 1 – 60。默认 2 秒。
# 调小会线性增加 CPU 与内存占用，调大则丢失短时尖峰。
interval_secs = 2

# 内存环形缓冲保留时长（秒）。3600 = 1 小时，约 3MB。
# 这段数据只在内存里，用于实时曲线与 m_1m 层的聚合来源，进程重启即丢失。
# 允许 60 – 86400（1 分钟 – 1 天）。
ring_secs = 3600

# 落盘保留期预设，只有两档（不支持逐层自定义）：
#
#   层      桶宽     less     normal（默认）
#   m_1m    60s      6 小时   1 天
#   m_5m    300s     3 天     7 天
#   m_15m   900s     14 天    30 天
#   m_12h   43200s   90 天    90 天
#   m_1d    86400s   1 年     1 年
#
# 以一台 16 核 / 4 盘 / 2 网卡（约 200 条 series）的机器估算：
#   less   约 35MB，normal 约 100MB（含索引）。
retention = "normal"

# 是否为每个 CPU 核采集全部 8 个状态（user / nice / system / idle / iowait / irq / softirq / steal）。
# 关闭时每核只保留一条利用率曲线 `cpu.core.usage`。
# 128 核机器开启后环形缓冲约 36MB（关闭约 7MB），排查单核 softirq / steal 时再打开。
per_core_detail = false

[session]
# 会话空闲超时（秒）。900 = 15 分钟。
# 超过该时长没有任何请求，会话失效，需要重新用系统账户登录。
# 允许 60 – 604800（1 分钟 – 7 天）。
idle_timeout_secs = 900

# 提权状态的【独立】空闲超时（秒）。300 = 5 分钟，与 sudo 的 timestamp_timeout 一致。
# 会话本身仍然有效，但超过该时长没有管理操作，admin worker 会被回收、
# 权限降回普通用户，再做写操作需要重新提权。
# 允许 30 – 86400（30 秒 – 1 天）。
elevated_idle_timeout_secs = 300
"#;

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// 一天的秒数。只有 §7.2 保留期表的用例需要它。
    const DAY: u64 = 86_400;

    /// 只用「默认值 + TOML」两层构造配置，完全不碰进程环境，
    /// 因此可以和其它测试并行跑。
    fn from_toml(toml: &str) -> Result<Config> {
        Config::from_figment(
            Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(toml)),
        )
    }

    // ---------------------------------------------------------- 环境变量夹具

    /// 进程环境是全局状态，改它的测试必须互斥。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 在设置好指定环境变量的前提下执行 `f`，结束后清理。
    fn with_env<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
        // 中毒说明上一个测试 panic 了，环境已被 unset，继续用即可。
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: 所有会修改进程环境的测试都持有 ENV_LOCK，不存在并发读写。
        unsafe {
            for (k, v) in vars {
                std::env::set_var(k, v);
            }
        }
        let result = f();
        // SAFETY: 同上。
        unsafe {
            for (k, _) in vars {
                std::env::remove_var(k);
            }
        }
        result
    }

    /// 临时 TOML 文件，Drop 时删除。
    struct TempToml(PathBuf);

    impl TempToml {
        fn new(name: &str, content: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "strixmaid-config-test-{}-{name}.toml",
                std::process::id()
            ));
            std::fs::write(&path, content).expect("写入临时配置文件");
            TempToml(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempToml {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // ---------------------------------------------------------------- 默认值

    #[test]
    fn 默认值符合设计文档() {
        let c = Config::default();
        assert_eq!(c.listen, "127.0.0.1:9700");
        assert_eq!(c.data_dir, PathBuf::from("/var/lib/strixmaid"));
        assert_eq!(c.run_dir, PathBuf::from("/run/strixmaid"));
        assert_eq!(c.helper_path, PathBuf::from("strixmaid-helper"));
        assert_eq!(c.pam_service, "strixmaid");
        assert_eq!(c.log.level, LogLevel::Info);
        assert_eq!(c.metrics.interval_secs, 2);
        assert_eq!(c.metrics.ring_secs, 3600);
        assert_eq!(c.metrics.retention, RetentionPreset::Normal);
        assert_eq!(c.session.idle_timeout_secs, 900);
        assert_eq!(c.session.elevated_idle_timeout_secs, 300);
        c.validate().expect("默认值必须自洽");
    }

    #[test]
    fn 默认值的派生路径() {
        let c = Config::default();
        assert_eq!(
            c.db_path(),
            PathBuf::from("/var/lib/strixmaid/strixmaid.db")
        );
        assert_eq!(
            c.helper_socket_path(),
            PathBuf::from("/run/strixmaid/helper.sock")
        );
        assert_eq!(
            c.listen_addr().unwrap(),
            "127.0.0.1:9700".parse::<SocketAddr>().unwrap()
        );
        // 1 小时 / 2 秒 = 1800 个采样点
        assert_eq!(c.metrics.ring_capacity(), 1800);
        assert_eq!(c.metrics.interval(), Duration::from_secs(2));
    }

    #[test]
    fn 顶层键清单与结构体保持同步() {
        let dict = Value::serialize(Config::default())
            .unwrap()
            .into_dict()
            .unwrap();
        let mut actual: Vec<&str> = dict.keys().map(String::as_str).collect();
        actual.sort_unstable();
        let mut known: Vec<&str> = Config::TOP_LEVEL_KEYS.to_vec();
        known.sort_unstable();
        assert_eq!(actual, known);
    }

    // ------------------------------------------------------------- 示例配置

    #[test]
    fn 示例配置等于默认值且能通过校验() {
        let from_example = from_toml(Config::example_toml()).expect("示例配置必须合法");
        assert_eq!(from_example, Config::default());

        // 就算不叠默认值层，示例也应当能独立解析出完整配置（serde(default) 兜底）。
        let standalone =
            Config::from_figment(Figment::from(Toml::string(Config::example_toml()))).unwrap();
        assert_eq!(standalone, Config::default());
    }

    // ----------------------------------------------------- 第 2 层：TOML

    #[test]
    fn toml_覆盖默认值且未提及的字段保持默认() {
        let c = from_toml(
            r#"
            listen = "0.0.0.0:8080"
            data_dir = "/srv/strixmaid"

            [log]
            level = "debug"

            [metrics]
            interval_secs = 10
            retention = "less"
            "#,
        )
        .unwrap();

        // 被覆盖的
        assert_eq!(c.listen, "0.0.0.0:8080");
        assert_eq!(c.data_dir, PathBuf::from("/srv/strixmaid"));
        assert_eq!(c.log.level, LogLevel::Debug);
        assert_eq!(c.metrics.interval_secs, 10);
        assert_eq!(c.metrics.retention, RetentionPreset::Less);
        // 同一张表里没提到的字段必须保持默认（部分覆盖，不是整表替换）
        assert_eq!(c.metrics.ring_secs, 3600);
        // 完全没提到的表
        assert_eq!(c.run_dir, PathBuf::from(DEFAULT_RUN_DIR));
        assert_eq!(c.session, SessionConfig::default());
    }

    // --------------------------------------------------- 第 3 层：环境变量

    #[test]
    fn 环境变量覆盖_toml() {
        let file = TempToml::new(
            "env-over-toml",
            r#"
            listen = "0.0.0.0:8080"

            [metrics]
            interval_secs = 7
            ring_secs = 7200

            [session]
            idle_timeout_secs = 1200
            "#,
        );

        with_env(
            &[
                ("STRIXMAID_LISTEN", "127.0.0.1:19700"),
                ("STRIXMAID_METRICS__INTERVAL_SECS", "5"),
                ("STRIXMAID_LOG__LEVEL", "trace"),
                ("STRIXMAID_METRICS__RETENTION", "less"),
                // 与配置无关的 STRIXMAID_* 变量不应导致「未知字段」错误
                ("STRIXMAID_CONFIG", "/dev/null"),
            ],
            || {
                let c = Config::load_from(file.path(), None).expect("加载应成功");
                // env 覆盖 toml
                assert_eq!(c.listen, "127.0.0.1:19700");
                assert_eq!(c.metrics.interval_secs, 5);
                // env 覆盖默认值
                assert_eq!(c.log.level, LogLevel::Trace);
                assert_eq!(c.metrics.retention, RetentionPreset::Less);
                // env 没提到的字段，toml 值仍然生效
                assert_eq!(c.metrics.ring_secs, 7200);
                assert_eq!(c.session.idle_timeout_secs, 1200);
                // 三层都没提到的字段仍是默认值
                assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
            },
        );
    }

    #[test]
    fn 配置文件路径可用环境变量覆盖() {
        let file = TempToml::new("config-path", "pam_service = \"strixmaid-test\"\n");
        let path_str = file.path().to_str().unwrap().to_string();
        with_env(&[("STRIXMAID_CONFIG", &path_str)], || {
            assert_eq!(Config::config_path(), file.path());
            let c = Config::load(None).unwrap();
            assert_eq!(c.pam_service, "strixmaid-test");
        });
    }

    // ------------------------------------------------- 第 4 层：命令行参数

    #[derive(Serialize)]
    struct FakeCli {
        listen: Option<String>,
        data_dir: Option<PathBuf>,
        metrics: FakeCliMetrics,
    }

    #[derive(Serialize)]
    struct FakeCliMetrics {
        interval_secs: Option<u64>,
    }

    #[test]
    fn 命令行覆盖环境变量且_none_不清空低优先级来源() {
        let file = TempToml::new(
            "cli",
            r#"
            data_dir = "/srv/from-toml"

            [metrics]
            ring_secs = 1800
            "#,
        );

        let cli = cli_layer(FakeCli {
            listen: Some("127.0.0.1:29700".into()),
            data_dir: None,
            metrics: FakeCliMetrics {
                interval_secs: Some(30),
            },
        })
        .unwrap();

        with_env(
            &[
                ("STRIXMAID_LISTEN", "127.0.0.1:19700"),
                ("STRIXMAID_METRICS__INTERVAL_SECS", "5"),
            ],
            || {
                let c = Config::load_from(file.path(), Some(cli.clone())).unwrap();
                // 命令行 > 环境变量
                assert_eq!(c.listen, "127.0.0.1:29700");
                assert_eq!(c.metrics.interval_secs, 30);
                // 命令行里为 None 的项被剔除，TOML 的值不受影响
                assert_eq!(c.data_dir, PathBuf::from("/srv/from-toml"));
                assert_eq!(c.metrics.ring_secs, 1800);
            },
        );
    }

    #[test]
    fn cli_layer_剔除全部空值() {
        let layer = cli_layer(FakeCli {
            listen: None,
            data_dir: None,
            metrics: FakeCliMetrics {
                interval_secs: None,
            },
        })
        .unwrap();
        // 全空 -> 该层不产生任何键，配置等于默认值
        let c = Config::from_figment(
            Figment::from(Serialized::defaults(Config::default())).merge(layer),
        )
        .unwrap();
        assert_eq!(c, Config::default());
    }

    // ---------------------------------------------------------------- 校验

    #[test]
    fn 采集间隔越界被拒并给出可操作的错误() {
        for bad in [0_u64, 61, 3600] {
            let err = from_toml(&format!("[metrics]\ninterval_secs = {bad}\n")).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("metrics.interval_secs"), "缺字段名：{msg}");
            assert!(
                msg.contains("STRIXMAID_METRICS__INTERVAL_SECS"),
                "缺环境变量名：{msg}"
            );
            assert!(msg.contains(&format!("当前值 `{bad}`")), "缺当前值：{msg}");
            assert!(msg.contains("1 – 60 秒"), "缺合法范围：{msg}");
        }
        // 边界值合法
        for ok in [1_u64, 60] {
            from_toml(&format!("[metrics]\ninterval_secs = {ok}\n")).unwrap();
        }
    }

    #[test]
    fn 监听地址不可解析时被拒() {
        for bad in ["localhost:9700", "9700", "127.0.0.1", "not an addr", ""] {
            let err = from_toml(&format!("listen = \"{bad}\"\n")).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("listen"), "缺字段名：{msg}");
            assert!(msg.contains("STRIXMAID_LISTEN"), "缺环境变量名：{msg}");
        }
        for ok in ["127.0.0.1:9700", "0.0.0.0:80", "[::1]:9700", "[::]:9700"] {
            let c = from_toml(&format!("listen = \"{ok}\"\n")).unwrap();
            assert!(c.listen_addr().is_ok());
        }
    }

    #[test]
    fn 目录为空时被拒() {
        let err = from_toml("data_dir = \"\"\nrun_dir = \"\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("data_dir"), "{msg}");
        assert!(msg.contains("run_dir"), "{msg}");
        assert!(msg.contains("STRIXMAID_DATA_DIR"), "{msg}");
        assert!(msg.contains("共 2 项"), "{msg}");
    }

    #[test]
    fn pam_服务名必须是合法文件名() {
        for bad in ["", "../etc/shadow", "a/b"] {
            let err = from_toml(&format!("pam_service = \"{bad}\"\n")).unwrap_err();
            assert!(err.to_string().contains("pam_service"), "{err}");
        }
        from_toml("pam_service = \"strixmaid-agent\"\n").unwrap();
    }

    #[test]
    fn 会话超时越界被拒() {
        let err = from_toml("[session]\nidle_timeout_secs = 10\n").unwrap_err();
        assert!(
            err.to_string().contains("session.idle_timeout_secs"),
            "{err}"
        );

        let err = from_toml("[session]\nelevated_idle_timeout_secs = 0\n").unwrap_err();
        assert!(
            err.to_string()
                .contains("session.elevated_idle_timeout_secs"),
            "{err}"
        );
    }

    #[test]
    fn 环形缓冲必须能容纳至少一个采样点() {
        // 60s 缓冲 + 61s 间隔：两个字段各自都越界/临界，交叉约束必须报出来
        let err = from_toml("[metrics]\ninterval_secs = 60\nring_secs = 30\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("metrics.ring_secs"), "{msg}");
    }

    #[test]
    fn 一次性报出全部问题() {
        let err = from_toml(
            r#"
            listen = "nope"
            data_dir = ""

            [metrics]
            interval_secs = 0
            "#,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("共 3 项"), "{msg}");
        assert!(msg.contains("listen"), "{msg}");
        assert!(msg.contains("data_dir"), "{msg}");
        assert!(msg.contains("metrics.interval_secs"), "{msg}");
    }

    #[test]
    fn 拼错的配置项会被拒绝而不是被忽略() {
        let err = from_toml("lisen = \"127.0.0.1:9700\"\n").unwrap_err();
        assert!(err.to_string().contains("lisen"), "{err}");

        let err = from_toml("[metrics]\ninterval_sec = 5\n").unwrap_err();
        assert!(err.to_string().contains("interval_sec"), "{err}");
    }

    #[test]
    fn 非法枚举值给出候选列表() {
        let err = from_toml("[metrics]\nretention = \"lots\"\n").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("less") && msg.contains("normal"), "{msg}");

        let err = from_toml("[log]\nlevel = \"verbose\"\n").unwrap_err();
        assert!(err.to_string().contains("trace"), "{err}");
    }

    // ------------------------------------------------------- §7.2 保留期表

    #[test]
    fn 保留期预设与设计文档_7_2_一致() {
        use MetricLayer::{M1d, M1m, M5m, M12h, M15m};

        let less = MetricsConfig {
            retention: RetentionPreset::Less,
            ..MetricsConfig::default()
        };
        let normal = MetricsConfig::default();
        assert_eq!(normal.retention, RetentionPreset::Normal, "默认预设");

        let expect = [
            (M1m, 60, 6 * HOUR, DAY),
            (M5m, 300, 3 * DAY, 7 * DAY),
            (M15m, 900, 14 * DAY, 30 * DAY),
            (M12h, 43_200, 90 * DAY, 90 * DAY),
            (M1d, 86_400, 365 * DAY, 365 * DAY),
        ];
        for (layer, bucket, l, n) in expect {
            assert_eq!(layer.bucket_secs(), Some(bucket as u32), "{layer} 桶宽");
            assert_eq!(less.retention_secs(layer), Some(l), "{layer} less 保留期");
            assert_eq!(normal.retention_secs(layer), Some(n), "{layer} normal 保留期");
            // 保留期必须是桶宽的整数倍，否则清理边界会切到半个桶
            assert_eq!(l % bucket, 0);
            assert_eq!(n % bucket, 0);
        }
        assert_eq!(M15m.table_name(), Some("m_15m"));
        assert_eq!(MetricLayer::PERSISTED.len(), expect.len());

        // live 层不落盘，没有保留期可言。聚合链（谁从谁来）归 store 的
        // `retention_table_matches_design` 守，本用例只管 config 这一侧看到的数值。
        assert_eq!(normal.retention_secs(MetricLayer::Live), None);
    }

    #[test]
    fn 枚举的字符串形式可直接用于_serde_与日志() {
        for level in LogLevel::ALL {
            let toml = format!("[log]\nlevel = \"{}\"\n", level.as_str());
            assert_eq!(from_toml(&toml).unwrap().log.level, level);
        }
        for preset in [RetentionPreset::Less, RetentionPreset::Normal] {
            let toml = format!("[metrics]\nretention = \"{}\"\n", preset.as_str());
            assert_eq!(from_toml(&toml).unwrap().metrics.retention, preset);
        }
        // 大小写变体（运维手写配置时常见）
        assert_eq!(
            from_toml("[log]\nlevel = \"WARN\"\n").unwrap().log.level,
            LogLevel::Warn
        );
        assert_eq!(
            from_toml("[metrics]\nretention = \"Normal\"\n")
                .unwrap()
                .metrics
                .retention,
            RetentionPreset::Normal
        );
    }

    // -------------------------------------------------- 配置文件缺失与损坏

    #[test]
    fn 配置文件不存在时回落到默认值() {
        let missing = std::env::temp_dir().join(format!(
            "strixmaid-config-test-{}-不存在的配置.toml",
            std::process::id()
        ));
        assert!(!missing.exists(), "夹具前提：该路径确实不存在");

        // 全新安装的机器上没有 /etc/strixmaid/config.toml，此时必须照常起服务
        // （design.md §12），而不是因为读不到文件就退出。
        // 借 with_env 拿锁，避免并发测试改动的 STRIXMAID_* 干扰 env 层。
        let c = with_env(&[], || Config::load_from(&missing, None))
            .expect("配置文件不存在不应报错");
        assert_eq!(c.listen, DEFAULT_LISTEN);
        assert_eq!(c.metrics.retention, RetentionPreset::Normal);
    }

    #[test]
    fn 配置文件存在但解析失败必须报错且带上路径() {
        // 语法错误：`listen` 没有值。静默忽略这种文件比直接失败危险得多。
        let file = TempToml::new("语法错误", "listen = \n");
        let err = with_env(&[], || Config::load_from(file.path(), None))
            .expect_err("语法错误的配置文件必须报错");

        let msg = err.to_string();
        assert!(
            msg.contains(&file.path().display().to_string()),
            "报错信息必须指出是哪个文件：{msg}"
        );
    }

    #[test]
    fn 字段路径到环境变量名的映射() {
        let e = FieldError::new("metrics.interval_secs", 0, "x");
        assert_eq!(e.env_var(), "STRIXMAID_METRICS__INTERVAL_SECS");
        let e = FieldError::new("listen", "x", "y");
        assert_eq!(e.env_var(), "STRIXMAID_LISTEN");
    }
}
