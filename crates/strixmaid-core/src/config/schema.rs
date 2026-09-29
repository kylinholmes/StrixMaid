//! 配置字段、Serde 契约和默认值。

use super::defaults::*;
use super::{MetricLayer, Result, RetentionPreset};
use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, path::PathBuf, time::Duration};
use strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS;

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
}

impl Default for MetricsConfig {
    fn default() -> Self {
        MetricsConfig {
            interval_secs: 2,
            ring_secs: HOUR,
            retention: RetentionPreset::default(),
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
    /// 允许启用管理访问（提权）的系统组。
    ///
    /// 用户属于其中任一组才能提权；root 无条件可以。默认
    /// [`DEFAULT_ELEVATE_GROUPS`]，覆盖 Debian 的 `sudo`、RHEL/Arch 的 `wheel`、
    /// macOS 与老 Ubuntu 的 `admin`。
    ///
    /// **配成空列表表示禁止任何人提权**，这是合法配置，不是「不限制」。
    pub elevate_groups: Vec<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            idle_timeout_secs: 900,
            elevated_idle_timeout_secs: 300,
            elevate_groups: DEFAULT_ELEVATE_GROUPS
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
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

/// 审计配置（`roadmap/02-audit.md` §4.4）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuditConfig {
    /// 审计记录保留天数，超过即被后台任务清理。默认 90，允许 7–3650。
    ///
    /// 下限 7 天不是随便定的：审计的用途之一是事后追查，而「事后」往往是几天后
    /// 才有人发现异常。保留期短于一周基本等于没有。
    pub retention_days: u32,
}

impl Default for AuditConfig {
    fn default() -> Self {
        AuditConfig {
            retention_days: DEFAULT_AUDIT_RETENTION_DAYS,
        }
    }
}

impl AuditConfig {
    /// 保留期对应的秒数。
    pub const fn retention_secs(&self) -> i64 {
        self.retention_days as i64 * 86_400
    }
}

/// 终端配置（`roadmap/03-terminal.md` §4.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// 无 WS 附着、且没有输出，持续这么久即关闭该终端。默认 1800 秒。
    ///
    /// 终端不像 HTTP 请求那样自己会结束：一个开着 root shell 的终端只要没人管，
    /// 就会一直活着。空闲回收是唯一会收拾它的机制，所以它必须存在。
    pub idle_timeout_secs: u64,
    /// 单会话终端数上限，超出时 `POST /terminals` 返回 `409 conflict`。默认 8。
    ///
    /// 每个终端都是一个真实的 shell 进程加一个 PTY。没有上限时，
    /// 一个跑飞的前端能把机器的 pty 耗光——那会连累 SSH 登录。
    pub max_per_session: usize,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        TerminalConfig {
            idle_timeout_secs: DEFAULT_TERMINAL_IDLE_TIMEOUT_SECS,
            max_per_session: DEFAULT_TERMINAL_MAX_PER_SESSION,
        }
    }
}

impl TerminalConfig {
    /// 空闲上限的 `Duration` 形式。
    pub const fn idle_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.idle_timeout_secs)
    }
}

/// 文件浏览配置（roadmap/04 §A、design.md Q21）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// 文件面板允许浏览的根路径列表，默认 `["/"]`。每项必须是绝对路径，
    /// 列表不能为空。
    ///
    /// **它不是安全边界**——文件的可见性由 worker 的 uid 与文件权限裁决——
    /// 只是文件面板的展示范围。
    pub allowed_roots: Vec<PathBuf>,
}

impl Default for FilesConfig {
    fn default() -> Self {
        FilesConfig {
            allowed_roots: vec![PathBuf::from("/")],
        }
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
    /// 数据目录，SQLite 数据库存放于此。
    /// 默认见 [`DEFAULT_DATA_DIR`]（Linux `/var/lib/strixmaid`、
    /// macOS `/var/db/strixmaid`、Windows `C:\ProgramData\StrixMaid\data`）。
    pub data_dir: PathBuf,
    /// 运行目录，helper 的 Unix socket 存放于此。
    /// 默认见 [`DEFAULT_RUN_DIR`]（Linux `/run/strixmaid`、
    /// macOS `/var/run/strixmaid`、Windows `C:\ProgramData\StrixMaid\run`）。
    pub run_dir: PathBuf,
    /// `strixmaid-helper` 二进制路径。默认 `strixmaid-helper`——
    /// 不含 `/` 的名字会被 `Command::new` 按 `PATH` 查找。
    pub helper_path: PathBuf,
    /// PAM 服务名，对应 `/etc/pam.d/<名字>`。默认 `strixmaid`。
    pub pam_service: String,
    /// 受信任的反向代理地址。
    ///
    /// **只有直连地址在这个列表里时，才采信 `X-Forwarded-For`**（`roadmap/02-audit.md` §4.2）。
    /// 默认空 = 谁都不信、一律用直连地址。这个默认值是有意的：审计记录里的来源地址
    /// 若能被任意客户端用一个请求头伪造，那条记录就失去了意义。
    ///
    /// 取值是**精确的 IP 字面量**（`127.0.0.1`、`::1`）。暂不支持 CIDR——
    /// 反代通常就固定那么一两个地址，为此引入一个网段解析依赖不划算。
    /// 写成 CIDR 会在启动时报错而不是被静默忽略，见 `validate`。
    pub trusted_proxies: Vec<String>,
    /// 日志配置。
    pub log: LogConfig,
    /// 指标配置。
    pub metrics: MetricsConfig,
    /// 会话配置。
    pub session: SessionConfig,
    /// 审计配置。
    pub audit: AuditConfig,
    /// 终端。
    pub terminal: TerminalConfig,
    /// 文件浏览。
    pub files: FilesConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            listen: DEFAULT_LISTEN.to_string(),
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            run_dir: PathBuf::from(DEFAULT_RUN_DIR),
            helper_path: PathBuf::from(DEFAULT_HELPER_PATH),
            trusted_proxies: Vec::new(),
            pam_service: DEFAULT_PAM_SERVICE.to_string(),
            log: LogConfig::default(),
            metrics: MetricsConfig::default(),
            session: SessionConfig::default(),
            audit: AuditConfig::default(),
            terminal: TerminalConfig::default(),
            files: FilesConfig::default(),
        }
    }
}
