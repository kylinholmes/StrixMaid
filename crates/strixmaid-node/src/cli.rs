//! 两个宿主共用的命令行片段。
//!
//! # 什么该放这里，什么不该
//!
//! `strixmaid` 与 `strixmaid-agent` 的全局参数**不是同一套**：环境变量前缀不同
//! （`STRIXMAID_` / `STRIXMAID_AGENT_`）、缺省路径不同、server 有 `--listen` 而
//! agent 有自己的 `--server-url`。clap 的 `env = "..."` 是写死在 derive 里的字面量，
//! 没法参数化，所以硬凑一个共用的 `GlobalArgs` 只会让两边的帮助信息都说错话。
//!
//! 因此这里只放**真正与宿主无关**的东西：值解析器，以及服务子命令的形状
//! （那个在 [`crate::winsvc`]，与实现放在一起）。各自的 `GlobalArgs` 留在各自的
//! crate 里，那是它们该有的差异。

use strixmaid_core::config::LogLevel;

/// 把日志级别名解析成 core 的 [`LogLevel`]。
///
/// core 的 `LogLevel` 不派生 `clap::ValueEnum`（types / core 不依赖 clap），
/// 所以在这里手工列出候选，让 clap 给出可读的报错而不是等到 figment 反序列化才失败。
///
/// 两个宿主共用一份：级别名是配置契约的一部分，两边认的字面量必须一致。
pub fn parse_log_level(raw: &str) -> Result<LogLevel, String> {
    LogLevel::ALL
        .iter()
        .copied()
        .find(|level| level.as_str().eq_ignore_ascii_case(raw))
        .ok_or_else(|| {
            let candidates: Vec<&str> = LogLevel::ALL.iter().map(|l| l.as_str()).collect();
            format!("必须是以下之一: {}", candidates.join(" / "))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 级别名大小写不敏感且认得全部取值() {
        for level in LogLevel::ALL {
            assert_eq!(parse_log_level(level.as_str()), Ok(level));
            assert_eq!(parse_log_level(&level.as_str().to_uppercase()), Ok(level));
        }
    }

    #[test]
    fn 未知级别的报错里列出全部候选() {
        let err = parse_log_level("verbose").expect_err("verbose 不是合法级别");
        for level in LogLevel::ALL {
            assert!(err.contains(level.as_str()), "{err} 里缺 {}", level.as_str());
        }
    }
}
