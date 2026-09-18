//! 与宿主无关的命令行片段。
//!
//! # 什么该放这里，什么不该
//!
//! 2026-09-18 之前这里的设想是「两个二进制各有一套全局参数」；合并之后只剩一个
//! `strixmaid`，全局参数也只有一套（在 `strixmaid::cli`），两种模式共用。
//!
//! 那这里还剩什么：**值解析器**。`LogLevel` 定义在 `strixmaid-core`，而 core 不
//! 依赖 clap（那条边界由依赖表守着），所以「把级别名解析成 `LogLevel`」这件事
//! 既不能放 core、也不该在每个用到的地方各抄一遍——级别名是配置契约的一部分，
//! 认的字面量必须一致。服务子命令的形状同理，放在 [`crate::winsvc`]，与实现一起。

use strixmaid_core::config::LogLevel;

/// 把日志级别名解析成 core 的 [`LogLevel`]。
///
/// core 的 `LogLevel` 不派生 `clap::ValueEnum`（types / core 不依赖 clap），
/// 所以在这里手工列出候选，让 clap 给出可读的报错而不是等到 figment 反序列化才失败。
///
/// 只此一份：级别名是配置契约的一部分，配置文件、环境变量、命令行认的字面量
/// 必须一致，各处各抄一遍迟早会漂。
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
