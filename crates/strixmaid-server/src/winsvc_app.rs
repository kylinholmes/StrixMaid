//! Server 作为 Windows 服务跑起来时，交给 [`strixmaid_node::winsvc`] 的那份实现。
//!
//! 托管的机制（分派线程、控制处理器、状态上报、SCM 调用）全在 node 里，两个宿主
//! 共用。这里只回答四个「你是谁、你要跑什么」的问题：服务身份、注册进 SCM 的命令行、
//! 怎么读配置、怎么跑主循环。
//!
//! 为什么要 `&'static`：`StartServiceCtrlDispatcherW` 的上下文会被 SCM 在任意时刻、
//! 任意线程上解引用，直到服务停止。栈上的对象与 `Box` 都要靠人去保证「别比回调活得
//! 长」，而一个进程只可能有一个服务实例，进程级 `OnceLock` 天然满足这个前提。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use strixmaid_core::config::Config;
use strixmaid_node::winsvc::{ServiceApp, ServiceIdentity};
use strixmaid_node::{ShutdownKind, StartupReporter};

use crate::cli::GlobalArgs;

/// SCM 里的服务名（键名）。
///
/// 这一个字段同时决定三处：`CreateServiceW` 注册用的名字、
/// `StartServiceCtrlDispatcherW` 的服务表项、以及注册表下
/// `HKLM\SYSTEM\CurrentControlSet\Services\<名字>`。三处必须一致。
static IDENTITY: ServiceIdentity = ServiceIdentity {
    name: "StrixMaid",
    display_name: "StrixMaid 服务器管理面板",
    description: "StrixMaid：轻量、通用、现代化的服务器观测与管理平台。提供 HTTP/WebSocket 控制面，\
         按登录用户身份派生 worker 执行管理操作。",
    // 取**本二进制**的版本。node 里不能写 `env!`，那会取到 node 的版本。
    version: env!("CARGO_PKG_VERSION"),
    // Server 要以任意登录用户的身份派生 worker，只有 LocalSystem 拿得到那个特权。
    default_account: "LocalSystem",
};

static APP: OnceLock<ServerService> = OnceLock::new();

struct ServerService {
    global: GlobalArgs,
}

/// 取（必要时初始化）进程级的服务实现。
pub fn app(global: &GlobalArgs) -> &'static dyn ServiceApp {
    APP.get_or_init(|| ServerService {
        global: global.clone(),
    })
}

impl ServiceApp for ServerService {
    fn identity(&self) -> &'static ServiceIdentity {
        &IDENTITY
    }

    fn run_args(&self) -> Vec<String> {
        // 服务由 `services.exe` 拉起，环境与工作目录都不是安装时那一套，
        // 所以凡是从命令行来的配置都要如实带上。引号由 node 侧统一加。
        let g = &self.global;
        let mut out = Vec::new();
        if let Some(path) = &g.config {
            out.push("--config".to_owned());
            out.push(path.display().to_string());
        }
        if let Some(addr) = &g.listen {
            out.push("--listen".to_owned());
            out.push(addr.to_string());
        }
        if let Some(dir) = &g.data_dir {
            out.push("--data-dir".to_owned());
            out.push(dir.display().to_string());
        }
        if let Some(level) = &g.log_level {
            out.push("--log-level".to_owned());
            out.push(level.as_str().to_owned());
        }
        out
    }

    fn log_target(&self) -> String {
        // 读不出来就按默认位置显示——宁可报一个「默认位置」，也不猜一个不存在的路径。
        let config = crate::load_config(&self.global).ok();
        strixmaid_node::winsvc::logging::describe_target(config.as_ref())
    }

    fn prepare(&self) -> anyhow::Result<(Config, PathBuf)> {
        let config = crate::load_config(&self.global)?;
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
        Box::pin(crate::serve_with(config, reporter, shutdown))
    }
}
