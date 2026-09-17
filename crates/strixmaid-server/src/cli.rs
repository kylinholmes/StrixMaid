//! 命令行界面定义。
//!
//! `strixmaid` 是「UI + AgentCore + Server + worker 模式」的单一二进制（§2.1），
//! worker 不是独立可执行文件，而是本二进制的子命令。
//!
//! # 按平台分叉的子命令与参数
//!
//! 有两处刻意用 `#[cfg]` 把选项从**错误的平台上整个删掉**，而不是保留一个
//! 永远用不了的开关：
//!
//! * `worker` 的 IPC 端点。Unix 上是 helper 在 `exec` 前 `dup2` 到位的
//!   文件描述符（`--ipc-fd`，缺省 3）；Windows 上没有 fd 继承这一套，
//!   helper 建的是命名管道，交出去的要么是**句柄值**（`--ipc-handle`），
//!   要么是**管道名字**（`--ipc-pipe`），见 [`WorkerArgs`]。三者的取值域与
//!   缺省语义都不同，合成一个参数只会让 `--help` 骗人。
//! * `service` 子命令族只在 Windows 上存在。Unix 侧的等价物是 systemd
//!   （`packaging/strixmaid.service`），由 init 托管，二进制本身不需要
//!   任何注册/启停命令。
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
    /// 配置文件路径
    /// [默认: Unix /etc/strixmaid/config.toml，Windows C:\ProgramData\StrixMaid\config.toml]
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

    /// 数据目录，存放 SQLite [默认: Linux /var/lib/strixmaid，
    /// macOS /var/db/strixmaid，Windows C:\ProgramData\StrixMaid\data；
    /// 环境变量 STRIXMAID_DATA_DIR]
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

    /// 会话 worker：以登录用户身份运行，由 helper 认证并切换身份后拉起
    ///
    /// 不读配置文件；通过 --ipc-fd（Unix）/ --ipc-handle 或 --ipc-pipe（Windows）
    /// 指定的 IPC 端点与主进程通信。
    Worker(WorkerArgs),

    /// 配置工具
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Windows 服务（SCM）：注册、注销、启停、查询，以及由 SCM 拉起时的运行入口
    #[cfg(windows)]
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

/// `service` 的动作（仅 Windows）。
///
/// `run` 与其余五个不是一类东西：`run` 是**被 SCM 调用**的入口，其余五个是
/// 管理员在命令行上**调用 SCM**。两者共用同一个服务名常量
/// （`crate::service::SERVICE_NAME`），install 写进注册表的命令行里带的正是
/// `service run`。
#[cfg(windows)]
#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// 由服务控制管理器（SCM）调用的运行入口，不供手工执行
    ///
    /// 直接在命令行里跑会在连接 SCM 时失败并给出提示；前台运行请用 `serve`。
    Run,

    /// 注册为自动启动的 Windows 服务（需要管理员权限）
    Install(ServiceInstallArgs),

    /// 注销服务（需要管理员权限）
    Uninstall,

    /// 启动已注册的服务（需要管理员权限）
    Start,

    /// 停止正在运行的服务（需要管理员权限）
    Stop,

    /// 查询服务当前状态（只读，无需管理员权限）
    Status,
}

/// `service install` 的参数（仅 Windows）。
#[cfg(windows)]
#[derive(Debug, Args)]
pub struct ServiceInstallArgs {
    /// 写进服务 ImagePath 的可执行文件路径 [默认: 当前可执行文件]
    ///
    /// 注册的是**绝对路径**：服务由 `services.exe` 拉起，工作目录是
    /// `%SystemRoot%\system32`，相对路径解析不到。
    #[arg(long, value_name = "PATH")]
    pub exe: Option<PathBuf>,

    /// 服务账户 [默认: LocalSystem]
    ///
    /// 只接受三个内置的、无口令的服务账户：`LocalSystem`、
    /// `NT AUTHORITY\LocalService`、`NT AUTHORITY\NetworkService`。
    /// 域账户需要口令，而明文口令不进命令行、不进注册表——那种部署请在
    /// 安装后用「服务」管理单元设置登录账户。
    #[arg(long, value_name = "ACCOUNT")]
    pub account: Option<String>,

    /// 注册完成后立即启动一次
    #[arg(long)]
    pub start: bool,
}

/// `config` 的动作。
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// 输出带注释的示例配置（安装脚本用它生成 /etc/strixmaid/config.toml）
    Example,
}

/// `worker` 子命令参数。
///
/// 字段按平台分叉，理由见模块文档。
///
/// # Windows 上为什么有两种端点写法
///
/// helper 拉起 worker 有三级阶梯，其中 `CreateProcessWithTokenW` 那一级
/// **交不出句柄**：进程实际上由 seclogon 服务代建，函数签名里连
/// `bInheritHandles` 都没有，helper 句柄表里的东西根本到不了 worker。
/// 那一级只能把管道的**名字**告诉 worker，由 worker 自己连一次
/// （`strixmaid_core::worker::run_from_pipe`）。另外两级能直接交出可继承的
/// 句柄，走 `--ipc-handle`（`run_from_ipc`），省掉一次连接与一次访问检查。
///
/// 两者**互斥且必须给一个**。判定放在 [`WorkerArgs::endpoint`] 里，
/// 而不是交给 clap 的 `conflicts_with`：这两种参数背后是两条不同的拉起路径，
/// 给错了该说清「哪一级该用哪个」，而不是甩一句 `cannot be used with`。
#[derive(Debug, Args)]
pub struct WorkerArgs {
    /// 与主进程通信的 socketpair fd，由 helper 在 exec 前 dup2 到位（§10）[默认: 3]
    #[cfg(unix)]
    #[arg(long, value_name = "FD")]
    pub ipc_fd: Option<i32>,

    /// 与主进程通信的命名管道客户端句柄，由 helper 以十进制写在命令行上（§10）
    ///
    /// 没有缺省值：Windows 上不存在「约定的 3 号 fd」那种东西，句柄值由内核
    /// 在建管道时分配，只能由 helper 如实告知。与 --ipc-pipe 互斥。
    #[cfg(windows)]
    #[arg(long, value_name = "HANDLE")]
    pub ipc_handle: Option<u64>,

    /// 与主进程通信的命名管道名字，由 worker 自己连一次（§10）
    ///
    /// 只有 helper 走 `CreateProcessWithTokenW` 那一级时才用它——那一级交不出
    /// 句柄。与 --ipc-handle 互斥。
    #[cfg(windows)]
    #[arg(long, value_name = "NAME")]
    pub ipc_pipe: Option<String>,
}

/// worker 与主进程之间那条通道的「名字」在本平台上长什么样。
#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerEndpoint {
    /// 已经在本进程句柄表里的命名管道客户端句柄。
    Handle(u64),
    /// 还没连上的命名管道名字，形如 `\\.\pipe\strixmaid-worker-<随机>`。
    Pipe(String),
}

#[cfg(windows)]
impl WorkerArgs {
    /// 定下用哪条路取 IPC 通道。
    ///
    /// 两个参数都给或都不给都是 helper 侧的 bug，报错时把「哪一级该用哪个」
    /// 一并说清：worker 是由 helper 拉起的，看到这条消息的人正在调 helper。
    pub fn endpoint(&self) -> anyhow::Result<WorkerEndpoint> {
        match (self.ipc_handle, self.ipc_pipe.as_deref()) {
            (Some(handle), None) => Ok(WorkerEndpoint::Handle(handle)),
            (None, Some(name)) if !name.trim().is_empty() => {
                Ok(WorkerEndpoint::Pipe(name.to_owned()))
            }
            (None, Some(_)) => {
                anyhow::bail!("--ipc-pipe 不能是空串：管道名字必须是 helper 实际创建的那一个")
            }
            (Some(_), Some(_)) => anyhow::bail!(
                "--ipc-handle 与 --ipc-pipe 只能给一个：\
                 helper 用 CreateProcessAsUserW / CreateProcessW 拉起时给 --ipc-handle，\
                 用 CreateProcessWithTokenW 拉起时（那一级交不出句柄）给 --ipc-pipe"
            ),
            (None, None) => anyhow::bail!(
                "必须给出 --ipc-handle 或 --ipc-pipe 其一：\
                 worker 不读配置文件，通道只能由 helper 经命令行告知"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clap_定义自洽() {
        // clap 自带的一致性检查：重名的参数、同一个短选项用两次之类的问题
        // 在这里就会 panic，而不是等到用户敲了某个子命令才崩。
        <Cli as clap::CommandFactory>::command().debug_assert();
    }

    #[test]
    fn 缺省无子命令等价于_serve() {
        let cli = Cli::try_parse_from(["strixmaid"]).expect("空命令行应当可解析");
        assert!(cli.command.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn worker_在_unix_上收_ipc_fd() {
        let cli = Cli::try_parse_from(["strixmaid", "worker", "--ipc-fd", "7"]).unwrap();
        let Some(Command::Worker(args)) = cli.command else {
            panic!("应当解析成 worker 子命令");
        };
        assert_eq!(args.ipc_fd, Some(7));
        assert!(
            Cli::try_parse_from(["strixmaid", "worker", "--ipc-handle", "7"]).is_err(),
            "--ipc-handle 不该出现在 Unix 上"
        );
    }

    #[cfg(windows)]
    fn worker_args(argv: &[&str]) -> WorkerArgs {
        let mut full = vec!["strixmaid", "worker"];
        full.extend_from_slice(argv);
        let cli = Cli::try_parse_from(full).expect("应当可解析");
        let Some(Command::Worker(args)) = cli.command else {
            panic!("应当解析成 worker 子命令");
        };
        args
    }

    #[cfg(windows)]
    #[test]
    fn worker_在_windows_上不认_ipc_fd() {
        assert!(
            Cli::try_parse_from(["strixmaid", "worker", "--ipc-fd", "3"]).is_err(),
            "--ipc-fd 不该出现在 Windows 上"
        );
    }

    #[cfg(windows)]
    #[test]
    fn 句柄与管道名各自选中一条取通道的路() {
        assert_eq!(
            worker_args(&["--ipc-handle", "424"]).endpoint().unwrap(),
            WorkerEndpoint::Handle(424)
        );
        assert_eq!(
            worker_args(&["--ipc-handle", "18446744073709551615"])
                .endpoint()
                .unwrap(),
            WorkerEndpoint::Handle(u64::MAX),
            "句柄值是完整的 u64，不能在解析处被截断"
        );
        assert_eq!(
            worker_args(&["--ipc-pipe", r"\\.\pipe\strixmaid-worker-abc"])
                .endpoint()
                .unwrap(),
            WorkerEndpoint::Pipe(r"\\.\pipe\strixmaid-worker-abc".to_owned())
        );
    }

    #[cfg(windows)]
    #[test]
    fn 两个端点参数都给或都不给都报错并指出该用哪个() {
        let both = worker_args(&["--ipc-handle", "1", "--ipc-pipe", r"\\.\pipe\x"])
            .endpoint()
            .expect_err("互斥参数同时给出应当报错");
        let both = both.to_string();
        assert!(both.contains("--ipc-handle"), "{both}");
        assert!(both.contains("--ipc-pipe"), "{both}");
        assert!(
            both.contains("CreateProcessWithTokenW"),
            "要说清哪一级拉起方式该用哪个：{both}"
        );

        let neither = worker_args(&[]).endpoint().expect_err("一个都不给应当报错");
        let neither = neither.to_string();
        assert!(neither.contains("--ipc-handle"), "{neither}");
        assert!(neither.contains("--ipc-pipe"), "{neither}");

        let empty = worker_args(&["--ipc-pipe", "   "])
            .endpoint()
            .expect_err("空的管道名连不上任何东西，不该被当成有效参数");
        assert!(empty.to_string().contains("--ipc-pipe"), "{empty}");
    }

    #[cfg(windows)]
    #[test]
    fn service_子命令族齐全() {
        for sub in ["run", "install", "uninstall", "start", "stop", "status"] {
            Cli::try_parse_from(["strixmaid", "service", sub])
                .unwrap_or_else(|e| panic!("service {sub} 应当可解析: {e}"));
        }
        let cli = Cli::try_parse_from([
            "strixmaid",
            "service",
            "install",
            "--start",
            "--account",
            "LocalSystem",
        ])
        .unwrap();
        let Some(Command::Service {
            action: ServiceAction::Install(args),
        }) = cli.command
        else {
            panic!("应当解析成 service install");
        };
        assert!(args.start);
        assert_eq!(args.account.as_deref(), Some("LocalSystem"));
        assert!(args.exe.is_none());
    }

    #[cfg(not(windows))]
    #[test]
    fn service_子命令不出现在非_windows_平台() {
        assert!(Cli::try_parse_from(["strixmaid", "service", "status"]).is_err());
    }
}
