//! 命令行界面定义。
//!
//! `strixmaid` 是「UI + AgentCore + Server + worker 模式」的单一二进制（§2.1），
//! worker 不是独立可执行文件，而是本二进制的子命令。
//!
//! # 与配置文件的关系
//!
//! 全局参数全部是 `Option<T>`，**没有 clap 的 `default_value`**：带默认值会让命令行层
//! 永远「有值」，从而无条件压过配置文件，破坏 §12 的优先级链
//! （内置默认 < `/etc/strixmaid/config.toml` < 环境变量 `STRIXMAID_*` < 命令行）。
//! 默认值由 `strixmaid_core::config::Config` 提供，[`GlobalArgs::overrides`] 只把
//! 用户**显式给出**的项交给 figment 的最高优先级层
//! （`strixmaid_core::config::cli_layer` 会把 `None` 递归剔除）。
//!
//! # 环境变量一律不走 clap
//!
//! 除 `--config` 外，这里**没有任何 `env = ...` 属性**。环境变量统一由
//! `strixmaid_core::config::Config::env_provider()`（`Env::prefixed("STRIXMAID_")`，
//! 嵌套用双下划线）在 figment 里单独成层处理。
//!
//! 理由：clap 的 `env` 会把环境变量的值**当成命令行参数**交上来，于是它实际落在了
//! 「命令行」那一层——优先级比 §12 规定的高了一级。对 `listen` 这种顶层键看不出区别，
//! 但对 `log.level` 这种嵌套键，`STRIXMAID_LOG_LEVEL`（clap，单下划线）会和
//! `STRIXMAID_LOG__LEVEL`（figment，双下划线）变成两个名字、两种优先级的同一个设置，
//! 出问题时无从排查。
//!
//! `--config` 是唯一例外，保留 `env = "STRIXMAID_CONFIG"`：它决定「读哪个文件」，
//! 必须在构建 Figment **之前**就确定，没法参与合并（§12）。

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use strixmaid_core::config::LogLevel;

/// `--version` 输出：`0.1.0 (<git sha>, <target>)`。两个环境变量由
/// `build.rs` 注入（roadmap/06 §3.5），无 git 时 sha 为 `unknown`。
pub const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("STRIXMAID_GIT_SHA"),
    ", ",
    env!("STRIXMAID_BUILD_TARGET"),
    ")"
);

/// StrixMaid —— 轻量、通用、现代化的服务器观测与管理平台。
#[derive(Debug, Parser)]
#[command(name = "strixmaid", version = LONG_VERSION, about, long_about = None)]
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalArgs,

    /// 加载并校验配置后立即退出（roadmap/06 §3.4）。
    /// 供 systemd 的 ExecStartPre 与安装脚本使用；校验失败时非零退出。
    #[arg(long, global = true)]
    pub check_config: bool,

    /// 不给子命令时等价于 `serve`。
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// 全局参数，对所有子命令可见。
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// 配置文件路径 [默认: /etc/strixmaid/config.toml]
    #[arg(
        short = 'c',
        long,
        global = true,
        env = "STRIXMAID_CONFIG",
        value_name = "PATH"
    )]
    pub config: Option<PathBuf>,

    /// HTTP 监听地址 [默认: 127.0.0.1:9700，环境变量 STRIXMAID_LISTEN]
    #[arg(short = 'l', long, global = true, value_name = "ADDR")]
    pub listen: Option<SocketAddr>,

    /// 数据目录，存放 SQLite [默认: /var/lib/strixmaid，环境变量 STRIXMAID_DATA_DIR]
    #[arg(short = 'd', long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// 日志级别：off / error / warn / info / debug / trace
    /// [默认: info，环境变量 STRIXMAID_LOG__LEVEL]
    ///
    /// 需要更细的过滤（如 `info,tower_http=debug`）时用 `RUST_LOG`；
    /// 本参数显式给出时优先级高于 `RUST_LOG`。
    #[arg(
        long,
        global = true,
        value_name = "LEVEL",
        value_parser = parse_log_level
    )]
    pub log_level: Option<LogLevel>,
}

impl GlobalArgs {
    /// 折算成 figment 的命令行层。字段名与 `strixmaid_core::config::Config` 一一对应。
    ///
    /// `config` 不在其中：它决定「读哪个文件」，本身不是配置项。
    pub fn overrides(&self) -> CliOverrides {
        CliOverrides {
            // Config::listen 是 String；SocketAddr 序列化出的正是 `IP:端口`。
            listen: self.listen.map(|addr| addr.to_string()),
            data_dir: self.data_dir.clone(),
            log: CliLogOverrides {
                level: self.log_level,
            },
        }
    }
}

/// 命令行覆盖层。全 `None` 的子表会被 `cli_layer` 整个剔除，不会覆盖下层。
#[derive(Debug, Serialize)]
pub struct CliOverrides {
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    log: CliLogOverrides,
}

/// `[log]` 子表的覆盖层。
#[derive(Debug, Serialize)]
struct CliLogOverrides {
    level: Option<LogLevel>,
}

/// 把日志级别名解析成 core 的 [`LogLevel`]。
///
/// core 的 `LogLevel` 不派生 `clap::ValueEnum`（types/core 不依赖 clap），
/// 所以在这里手工列出候选，让 clap 给出可读的报错而不是等到 figment 反序列化才失败。
fn parse_log_level(raw: &str) -> Result<LogLevel, String> {
    LogLevel::ALL
        .iter()
        .copied()
        .find(|level| level.as_str().eq_ignore_ascii_case(raw))
        .ok_or_else(|| {
            let candidates: Vec<&str> = LogLevel::ALL.iter().map(|l| l.as_str()).collect();
            format!("必须是以下之一: {}", candidates.join(" / "))
        })
}

/// 子命令。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 启动 HTTP 服务（缺省行为）
    Serve,

    /// 会话 worker：以登录用户身份运行，由 helper setuid 后 exec 拉起
    ///
    /// 不读配置文件；通过 --ipc-fd 指定的 socketpair 与主进程通信。
    Worker(WorkerArgs),

    /// 配置工具
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

/// `config` 的动作。
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// 输出带注释的示例配置（安装脚本用它生成 /etc/strixmaid/config.toml）
    Example,
}

/// `worker` 子命令参数。
#[derive(Debug, Args)]
pub struct WorkerArgs {
    /// 与主进程通信的 socketpair fd，由 helper 在 exec 前 dup2 到位（§10）[默认: 3]
    #[arg(long, value_name = "FD")]
    pub ipc_fd: Option<i32>,
}
