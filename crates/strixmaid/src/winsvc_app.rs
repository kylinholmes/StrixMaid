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

use strixmaid_core::config::Config;
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
        // `winsvc::logging` 决定；Agent 模式暂时与 Server 共用同一份，见 §未决。
        let config = crate::load_config(&self.global).ok();
        strixmaid_node::winsvc::logging::describe_target(config.as_ref())
    }

    fn prepare(&self) -> anyhow::Result<(Config, PathBuf)> {
        // Agent 模式读的是 agent.toml，形状与 `Config` 不同，没法从这里返回。
        // 但 `winsvc::logging::init` 只用到 `data_dir` 与 `log`，所以这里返回一个
        // **仅用于日志落点**的 `Config`：Agent 的真实配置在 `serve` 里自己再读一次。
        //
        // 这是本次合并留下的一处别扭，记在 `docs/roadmap/11-multi-host.md` 的未决里：
        // 干净的做法是让 `ServiceApp::prepare` 返回一个宿主自定义的类型。
        let config = match self.mode {
            ServiceMode::Server => crate::load_config(&self.global)?,
            ServiceMode::Agent => {
                let agent = crate::agent::load(&self.global, &AgentArgs { server_url: None })?;
                Config {
                    data_dir: agent.data_dir.clone(),
                    ..Config::default()
                }
            }
        };
        let log_path =
            strixmaid_node::winsvc::logging::init(&config, self.global.log_level.is_some())?;
        Ok((config, log_path))
    }

    fn serve(
        &self,
        config: Config,
        reporter: Arc<dyn StartupReporter>,
        shutdown: Pin<Box<dyn Future<Output = ShutdownKind> + Send>>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        match self.mode {
            ServiceMode::Server => Box::pin(crate::serve_with(config, reporter, shutdown)),
            ServiceMode::Agent => {
                // `prepare` 那份 Config 只为定日志落点，Agent 的真实配置在这里读。
                let global = self.global.clone();
                Box::pin(async move {
                    let cfg = crate::agent::load(&global, &AgentArgs { server_url: None })?;
                    crate::agent::run_with(cfg, reporter, shutdown).await
                })
            }
        }
    }
}
