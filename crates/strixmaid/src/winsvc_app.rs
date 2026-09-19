//! 作为 Windows 服务跑起来时，交给 [`strixmaid_node::winsvc`] 的那份实现。
//!
//! 托管的机制（分派线程、控制处理器、状态上报、SCM 调用）全在 node 里。这里只回答
//! 四个「你是谁、你要跑什么」的问题：服务身份、注册进 SCM 的命令行、怎么读配置、
//! 怎么跑主循环。
//!
//! # 一个二进制、两种身份
//!
//! `serve` 与 `agent` 是同一个 exe 的两种模式，服务名却必须分开——否则同一台机器上
//! 既当 Server 又当上级的 Agent 时会撞名。两份 [`ServiceIdentity`] 因此并列在这里，
//! 由 `service --mode` 选，`install` 把选中的模式写死进注册表那条命令行，
//! `service run` 被 SCM 拉起时靠它知道自己该跑哪一边。
//!
//! 服务账户也随模式不同：Server 要以任意登录用户的身份派生 worker，只有
//! `LocalSystem` 拿得到那个特权；Agent 现在只读采集，按最小权限起步用
//! `NT AUTHORITY\LocalService`，实测缺项再提权。
//!
//! # 为什么要 `&'static`
//!
//! `StartServiceCtrlDispatcherW` 的上下文会被 SCM 在任意时刻、任意线程上解引用，
//! 直到服务停止。栈上的对象与 `Box` 都要靠人去保证「别比回调活得长」，而一个进程
//! 只可能有一个服务实例，进程级 `OnceLock` 天然满足这个前提。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use strixmaid_node::winsvc::{ServiceApp, ServiceIdentity};
use strixmaid_node::{ShutdownKind, StartupReporter};

use crate::cli::{AgentArgs, GlobalArgs, ServiceMode};

/// Server 模式的身份。
///
/// `name` 同时决定三处：`CreateServiceW` 注册用的名字、`StartServiceCtrlDispatcherW`
/// 的服务表项、注册表下 `HKLM\SYSTEM\CurrentControlSet\Services\<名字>`。
static SERVER: ServiceIdentity = ServiceIdentity {
    name: "StrixMaid",
    display_name: "StrixMaid 服务器管理面板",
    description: "StrixMaid：轻量、通用、现代化的服务器观测与管理平台。提供 HTTP/WebSocket 控制面，\
         按登录用户身份派生 worker 执行管理操作。",
    // 取**本二进制**的版本。node 里不能写 `env!`，那会取到 node 的版本。
    version: env!("CARGO_PKG_VERSION"),
    // 要以任意登录用户的身份派生 worker，只有 LocalSystem 拿得到那个特权。
    default_account: "LocalSystem",
};

/// Agent 模式的身份。与 Server 并存不冲突——服务名不同。
static AGENT: ServiceIdentity = ServiceIdentity {
    name: "StrixMaidAgent",
    display_name: "StrixMaid 采集 Agent",
    description: "StrixMaid Agent：在本机采集指标并主动连接上级 StrixMaid Server 推送，\
         本地保留自己的历史数据。",
    version: env!("CARGO_PKG_VERSION"),
    // 只读采集器，最小权限起步。缺项再提权，结论记在 docs/windows-platform.md。
    default_account: r"NT AUTHORITY\LocalService",
};

static APP: OnceLock<Service> = OnceLock::new();

struct Service {
    mode: ServiceMode,
    global: GlobalArgs,
}

/// 取（必要时初始化）进程级的服务实现。
pub fn app(mode: ServiceMode, global: &GlobalArgs) -> &'static dyn ServiceApp {
    APP.get_or_init(|| Service {
        mode,
        global: global.clone(),
    })
}

impl ServiceApp for Service {
    fn identity(&self) -> &'static ServiceIdentity {
        match self.mode {
            ServiceMode::Server => &SERVER,
            ServiceMode::Agent => &AGENT,
        }
    }

    fn run_args(&self) -> Vec<String> {
        // 服务由 `services.exe` 拉起，环境与工作目录都不是安装时那一套，所以凡是
        // 从命令行来的配置都要如实带上。引号由 node 侧统一加。
        //
        // `--mode` 必须写进去：`service run` 被拉起时没有别的地方能知道自己该跑哪边。
        let g = &self.global;
        let mut out = vec![
            "--mode".to_owned(),
            match self.mode {
                ServiceMode::Server => "server".to_owned(),
                ServiceMode::Agent => "agent".to_owned(),
            },
        ];
        if let Some(path) = &g.config {
            out.push("--config".to_owned());
            out.push(path.display().to_string());
        }
        if let Some(dir) = &g.data_dir {
            out.push("--data-dir".to_owned());
            out.push(dir.display().to_string());
        }
        if let Some(level) = &g.log_level {
            out.push("--log-level".to_owned());
            out.push(level.as_str().to_owned());
        }
        // `--listen` 只对 server 有意义，agent 模式下给了会被 `agent::run` 拒绝，
        // 所以这里也按模式挑。
        if self.mode == ServiceMode::Server
            && let Some(addr) = &g.listen
        {
            out.push("--listen".to_owned());
            out.push(addr.to_string());
        }
        out
    }

    fn log_target(&self) -> String {
        // 读不出来就按默认位置显示——宁可报一个「默认位置」，也不猜一个不存在的路径。
        //
        // 两种模式的日志落在同一个目录（`%ProgramData%\StrixMaid\logs`），文件名由
        // `winsvc::logging` 决定；Agent 模式暂时与 Server 共用同一份。
        strixmaid_node::winsvc::logging::describe_target(self.data_dir().ok().as_deref())
    }

    fn prepare(&self) -> anyhow::Result<PathBuf> {
        // 两种模式的配置形状不同，但**日志只要 data_dir 与一个过滤器**，
        // 这两样两边都拿得出来。所以这里不必伪造一个谁都不用的 `Config`。
        strixmaid_node::winsvc::logging::init(&self.data_dir()?, self.log_filter()?)
    }

    fn serve(
        &self,
        reporter: Arc<dyn StartupReporter>,
        shutdown: Pin<Box<dyn Future<Output = ShutdownKind> + Send>>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        // 配置在这里才读第二遍。`prepare` 只为定日志落点，往后的报错要能进日志，
        // 所以真正的加载放在日志就位之后——顺序反了的话，配置读不出来那一次的
        // 报错就没有地方可去（服务进程没有 stderr）。
        let global = self.global.clone();
        match self.mode {
            ServiceMode::Server => Box::pin(async move {
                let config = crate::load_config(&global)?;
                crate::serve_with(config, reporter, shutdown).await
            }),
            ServiceMode::Agent => Box::pin(async move {
                let cfg = crate::agent::load(&global, &AgentArgs { server_url: None })?;
                crate::agent::run_with(cfg, reporter, shutdown).await
            }),
        }
    }
}

impl Service {
    /// 这个模式的数据目录——日志目录由它推出来（`logging::log_dir_for`）。
    ///
    /// 两种模式读不同的配置文件，但都答得出这一个问题。
    fn data_dir(&self) -> anyhow::Result<PathBuf> {
        Ok(match self.mode {
            ServiceMode::Server => crate::load_config(&self.global)?.data_dir,
            ServiceMode::Agent => {
                crate::agent::load(&self.global, &AgentArgs { server_url: None })?.data_dir
            }
        })
    }

    /// 服务模式的日志过滤器。
    ///
    /// Server 认配置里的 `log.level`（那是它的配置契约的一部分）；Agent 的
    /// `agent.toml` 里没有这一项，级别只能来自命令行或 `RUST_LOG`——与
    /// `agent::init_tracing` 前台那条路口径一致。
    fn log_filter(&self) -> anyhow::Result<tracing_subscriber::EnvFilter> {
        match self.mode {
            ServiceMode::Server => {
                let config = crate::load_config(&self.global)?;
                crate::log_filter(&config, self.global.log_level.is_some())
            }
            ServiceMode::Agent => Ok(tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new(
                        self.global.log_level.map_or("info", |l| l.as_str()),
                    )
                })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    /// 从一条真实命令行解析出 `GlobalArgs`，再看它会往注册表里写什么。
    ///
    /// 走 clap 解析而不是手工构造结构体：`run_args` 要复刻的正是「用户当初怎么
    /// 调用的」，从命令行进来才是真实路径。
    fn args_of(mode: ServiceMode, argv: &[&str]) -> Vec<String> {
        let cli = crate::cli::Cli::try_parse_from(argv).expect("命令行应当解析成功");
        Service {
            mode,
            global: cli.global,
        }
        .run_args()
    }

    /// `--mode` 必须写进注册表那条命令行。
    ///
    /// 这是**合并成一个二进制之后新增的失败模式**：`service run` 被 SCM 拉起时，
    /// 除了这条命令行没有任何地方能告诉它该跑 server 还是 agent。漏了它，
    /// 「装了 agent 服务、起来却是 server」——而且一路上不会报错。
    #[test]
    fn 模式一定写进注册表的命令行() {
        let a = args_of(ServiceMode::Agent, &["strixmaid"]);
        assert_eq!(a, vec!["--mode".to_owned(), "agent".to_owned()]);

        let s = args_of(ServiceMode::Server, &["strixmaid"]);
        assert_eq!(s, vec!["--mode".to_owned(), "server".to_owned()]);
    }

    #[test]
    fn 全局参数逐项透传() {
        let a = args_of(
            ServiceMode::Server,
            &[
                "strixmaid",
                "--config",
                r"C:\ProgramData\StrixMaid\config.toml",
                "--data-dir",
                r"D:\data",
                "--log-level",
                "debug",
                "--listen",
                "0.0.0.0:9700",
            ],
        );
        assert_eq!(
            a,
            vec![
                "--mode",
                "server",
                "--config",
                r"C:\ProgramData\StrixMaid\config.toml",
                "--data-dir",
                r"D:\data",
                "--log-level",
                "debug",
                "--listen",
                "0.0.0.0:9700",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>()
        );
    }

    /// `--listen` 只对 server 有意义。agent 模式下 `agent::run` 会拒绝它，
    /// 所以注册时就不该写进去——否则装出来的服务每次启动都直接失败。
    #[test]
    fn agent_模式不带_listen() {
        let a = args_of(
            ServiceMode::Agent,
            &["strixmaid", "--listen", "0.0.0.0:9700", "--config", "a.toml"],
        );
        assert!(!a.contains(&"--listen".to_owned()), "{a:?}");
        assert!(a.contains(&"--config".to_owned()), "其余参数仍要带上：{a:?}");
    }


    /// 临时配置文件，用完即删。
    struct TempToml(std::path::PathBuf);

    impl TempToml {
        fn new(tag: &str, body: &str) -> TempToml {
            let p = std::env::temp_dir()
                .join(format!("strixmaid-winsvc-{}-{tag}.toml", std::process::id()));
            std::fs::write(&p, body).expect("写临时配置");
            TempToml(p)
        }
        fn arg(&self) -> String {
            self.0.display().to_string()
        }
    }

    impl Drop for TempToml {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn service(mode: ServiceMode, argv: &[&str]) -> Service {
        let cli = crate::cli::Cli::try_parse_from(argv).expect("命令行应当解析成功");
        Service {
            mode,
            global: cli.global,
        }
    }

    /// 两种模式各读各的配置文件，日志落点跟着各自的 `data_dir` 走。
    ///
    /// 2026-09-18 之前 `log_target()` 不分模式，一律去读 **server** 的配置：
    /// `service install --mode agent --config agent.toml` 会把 agent.toml 当成
    /// server 配置来解析，失败后静默回落到默认目录——于是 `install` 打印的
    /// 「日志去哪了」指向一个 agent 根本不会写的路径。装服务的人照着去看，
    /// 看到的是一个空目录。
    #[test]
    fn 两种模式的日志落点各按各的配置() {
        let agent_toml = TempToml::new(
            "agent",
            "server_url = \"ws://up:9700\"\ntoken = \"t\"\ndata_dir = \"D:\\\\agentdata\\\\data\"\n",
        );
        let a = service(
            ServiceMode::Agent,
            &["strixmaid", "--config", &agent_toml.arg()],
        )
        .log_target();
        assert!(
            a.starts_with(r"D:\agentdata\logs"),
            "agent 的日志该跟着 agent.toml 的 data_dir 走，实际 {a}"
        );

        let server_toml = TempToml::new("server", "data_dir = \"D:\\\\srvdata\\\\data\"\n");
        let s = service(
            ServiceMode::Server,
            &["strixmaid", "--config", &server_toml.arg()],
        )
        .log_target();
        assert!(
            s.starts_with(r"D:\srvdata\logs"),
            "server 的日志该跟着 server 配置的 data_dir 走，实际 {s}"
        );
    }

    /// `--data-dir` 是全局参数，两种模式都该认，且优先级高于配置文件。
    #[test]
    fn 命令行的数据目录压过配置文件() {
        let agent_toml = TempToml::new(
            "override",
            "server_url = \"ws://up:9700\"\ntoken = \"t\"\ndata_dir = \"D:\\\\fromfile\\\\data\"\n",
        );
        let a = service(
            ServiceMode::Agent,
            &[
                "strixmaid",
                "--config",
                &agent_toml.arg(),
                "--data-dir",
                r"D:\fromcli\data",
            ],
        )
        .log_target();
        assert!(a.starts_with(r"D:\fromcli\logs"), "实际 {a}");
    }
    /// 两种模式的服务名必须不同，否则同一台机器上装不了两个。
    #[test]
    fn 两种身份的服务名不冲突且都合法() {
        assert_ne!(SERVER.name, AGENT.name);
        SERVER.validate().expect("Server 身份应当合法");
        AGENT.validate().expect("Agent 身份应当合法");
    }
}
