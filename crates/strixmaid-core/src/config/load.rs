//! 默认值、文件、环境、命令行四层合并。

use super::{
    CONFIG_PATH_ENV, Config, ConfigError, DEFAULT_CONFIG_PATH, ENV_NESTED_SEPARATOR, ENV_PREFIX,
    Result,
};
use figment::{
    Figment,
    providers::{Env, Serialized},
    value::{Dict, Value},
};
use serde::Serialize;
use std::path::{Path, PathBuf};

impl Config {
    /// 顶层配置键。用于把 `STRIXMAID_*` 里与配置无关的变量挡在外面
    /// （否则 `deny_unknown_fields` 会把 `STRIXMAID_CONFIG` 这类变量判成错误）。
    pub const TOP_LEVEL_KEYS: &'static [&'static str] = &[
        "listen",
        "data_dir",
        "files",
        "run_dir",
        "helper_path",
        "trusted_proxies",
        "pam_service",
        "log",
        "metrics",
        "session",
        "audit",
        "terminal",
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

        // 仅缺失文件可回落默认值；目录、设备和读取错误由 provider 原样报告。
        figment = figment.merge(super::ConfigFile::optional(path));

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
