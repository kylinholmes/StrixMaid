//! 配置来源与字段校验错误。

use super::{ENV_NESTED_SEPARATOR, ENV_PREFIX};
use std::fmt;

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
