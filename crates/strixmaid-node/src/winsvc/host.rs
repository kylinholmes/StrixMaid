//! `strixmaid service run`：被 SCM 拉起时的宿主。
//!
//! # 三个线程，谁也不能占错位置
//!
//! | 线程 | 谁创建 | 干什么 |
//! |---|---|---|
//! | 进程主线程 | 操作系统 | 调 `StartServiceCtrlDispatcherW`，**一直阻塞**到 [`service_main`] 返回 |
//! | 分派线程 | SCM | 调 [`service_main`]：注册控制处理器 → 起 tokio 运行时 → `block_on` 整个服务 |
//! | 控制线程 | SCM | 每来一个控制请求调一次 [`control_handler`]，**必须立刻返回** |
//!
//! ## 为什么 tokio 运行时建在分派线程上，而不是主线程上
//!
//! `StartServiceCtrlDispatcherW` 的语义是「把本线程借给 SCM」：调用之后它不返回，
//! 只会在 SCM 那边准备好时回调 `service_main`，等 `service_main` 返回、服务真正
//! 停止，它才连本带利地还回来。
//!
//! 于是只有两种摆法：
//!
//! 1. **先在主线程建运行时，再从运行时里去调分派器**。分派器会阻塞，只能丢进
//!    `spawn_blocking`；那样 `service_main` 就跑在运行时的阻塞线程池上，
//!    **带着运行时上下文**。此时若想在 `service_main` 里 `Handle::block_on`
//!    整个服务，tokio 会直接 panic（不允许在运行时线程上阻塞等待）；绕开它就得
//!    改成「spawn 出去 + 用 `std` 通道回等」，凭空多出一层转发，而且运行时的
//!    关停顺序与服务的关停顺序互相纠缠。
//! 2. **主线程只做阻塞分派，运行时整个建在 `service_main` 里**。两者的生命周期
//!    天然嵌套：运行时随 `block_on` 返回而落下 → `service_main` 报 `STOPPED`
//!    并返回 → 主线程的 `StartServiceCtrlDispatcherW` 返回 → 进程退出。
//!
//! 本模块取第二种，代价是 `main` 不能用 `#[tokio::main]`（见 `main.rs` 的模块
//! 文档）。这也是为什么 `service` 子命令在 `main` 里被最先判掉、走的是同步路径。
//!
//! ## 控制处理器怎么叫醒异步代码
//!
//! [`control_handler`] 跑在 SCM 自己的线程上：那里没有 tokio 的运行时上下文，
//! 而且**绝不能阻塞**（SCM 对控制处理器的容忍是 30 秒，超时即认为服务失去响应）。
//! 它只做两件事——先向 SCM 报 `SERVICE_STOP_PENDING`，再把关停档位写进一个
//! `tokio::sync::watch`（`send_replace` 是同步的、不等待、可在任意线程调用）。
//! 真正的关停由分派线程上的 `serve_with` 自己完成，见 `main.rs` 的
//! `ShutdownKind`。
//!
//! ## 为什么用 `static` 承载控制状态
//!
//! `RegisterServiceCtrlHandlerExW` 的上下文指针会被 SCM 在任意时刻、任意线程上
//! 解引用，直到服务停止。栈上的对象与 `Box` 都要靠人去保证「别比回调活得短」，
//! 而一个进程只可能有一个服务实例（服务表里只注册了一项），进程级 `OnceLock`
//! 天然满足这个前提且不需要任何额外证明。指针仍然如实经 `lpContext` 传递，
//! 回调不去读全局变量——这样即便将来真要跑多个服务，改动也局限在构造处。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use anyhow::{Context as _, bail};
use strixmaid_core::platform::windows::to_wide;
use tokio::sync::watch;
use windows_sys::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_SERVICE_SPECIFIC_ERROR, GetLastError, NO_ERROR,
};
use windows_sys::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_SHUTDOWN,
    SERVICE_ACCEPT_STOP, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_PRESHUTDOWN,
    SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus, StartServiceCtrlDispatcherW,
};

use super::{ServiceApp, logging};

use crate::{ShutdownKind, StartupReporter};

/// 启动期每一步的 `dwWaitHint`：告诉 SCM「下一次 checkpoint 推进之前最多要这么久」。
///
/// 取 30 秒。开库要跑迁移、探测能力要起 helper，慢盘上单步几秒到十几秒都正常；
/// 给小了 SCM 会在服务还在正常启动时就判它超时。
const START_WAIT_HINT_MS: u32 = 30_000;

/// `SERVICE_CONTROL_STOP` / `PRESHUTDOWN` 之后向 SCM 声明的时限。
///
/// 覆盖 `main.rs` 里 `GRACEFUL_DRAIN`（20 秒排空连接）加上收 worker、关库的时间。
const GRACEFUL_STOP_HINT_MS: u32 = 30_000;

/// `SERVICE_CONTROL_SHUTDOWN` 之后向 SCM 声明的时限。
///
/// 关机场景下声明多久都没用——系统的 `WaitToKillServiceTimeout` 才是上限
/// （默认 5000 毫秒）。如实报一个小于它的值，好过报 30 秒然后被硬杀。
const URGENT_STOP_HINT_MS: u32 = 3_000;

/// 编译期守住上一条：关机档声明的时限必须短于正常停止档。写反了根本编不过。
const _: () = assert!(URGENT_STOP_HINT_MS < GRACEFUL_STOP_HINT_MS);

/// 服务在 `SERVICE_RUNNING` 期间接受的控制请求。
///
/// 同时接受 `SHUTDOWN` 与 `PRESHUTDOWN`：后者让系统在真正开始关机**之前**先通知
/// 我们，时限宽得多（默认 3 分钟），足够走完整的关停路径；前者是兜底——
/// 并非所有关机路径都会发 `PRESHUTDOWN`，漏接的话服务就只能被硬杀。
/// 两者折算成不同的 [`ShutdownKind`]，见 [`control_handler`]。
const ACCEPTED_CONTROLS: u32 =
    SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN | SERVICE_ACCEPT_PRESHUTDOWN;

/// `dwServiceSpecificExitCode`：服务自报的失败码。
///
/// 目前只区分「成功」与「失败」一种：真正的原因在日志里，往这里塞一套自定义
/// 错误码表只会制造一份没人维护的对照表。
const EXIT_FAILURE: u32 = 1;

/// 运行时关停的等待上限。超过就放弃等后台线程，直接落进程。
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(5);

/// 进程内唯一的服务控制状态，见模块文档。
static CONTROL: OnceLock<ServiceControl> = OnceLock::new();

/// 宿主给的服务实现，供分派线程上的 [`service_main`] 取用。
///
/// 分派线程拿不到 `main` 的栈，而 `service_main` 的签名里没有自定义上下文
/// （SCM 只给「启动参数」，那是另一回事，见 [`service_main`]）。
static APP: OnceLock<&'static dyn ServiceApp> = OnceLock::new();

/// `SERVICE_STATUS_HANDLE` 的跨线程包装。
///
/// 它是裸指针，因此默认既不是 `Send` 也不是 `Sync`，而分派线程与 SCM 的控制
/// 线程都要拿它上报状态。它并不是可关闭的内核句柄（没有对应的 Close 函数），
/// 本质上只是 SCM 内部表里的一个编号；Win32 明确允许跨线程使用。
#[derive(Clone, Copy)]
struct StatusHandle(SERVICE_STATUS_HANDLE);

// SAFETY: 该值只被读取并原样传回 `SetServiceStatus`，本类型没有内部可变性，
// 真正的并发安全由 SCM 侧保证。
unsafe impl Send for StatusHandle {}
// SAFETY: 同上。
unsafe impl Sync for StatusHandle {}

/// 与 SCM 之间的全部共享状态。
struct ServiceControl {
    /// `RegisterServiceCtrlHandlerExW` 的返回值。用 `OnceLock` 而不是直接持有：
    /// 注册调用**返回之前**控制处理器理论上就可能被调用，那时还没有句柄可用，
    /// 此时只能跳过上报（关停请求本身仍会被记下）。
    status: OnceLock<StatusHandle>,
    /// `dwCheckPoint`。SCM 靠「这个数在涨」区分「还在启动/停止」与「卡死了」。
    checkpoint: AtomicU32,
    /// 最近一次上报的状态与时限，供 `SERVICE_CONTROL_INTERROGATE` 原样重报。
    current: AtomicU32,
    wait_hint: AtomicU32,
    /// 关停档位。`watch` 的发送端不需要运行时上下文，可在 SCM 的线程上直接用。
    shutdown: watch::Sender<Option<ShutdownKind>>,
}

impl ServiceControl {
    /// 向 SCM 上报一次状态。
    ///
    /// 稳定态（`RUNNING` / `STOPPED`）的 `dwCheckPoint` 与 `dwWaitHint` 必须为 0，
    /// 这是 SCM 的要求；非零会被当成「还在过渡中」。
    fn set_status(
        &self,
        state: u32,
        checkpoint: u32,
        wait_hint_ms: u32,
        win32_exit: u32,
        specific_exit: u32,
    ) {
        self.current.store(state, Ordering::SeqCst);
        self.wait_hint.store(wait_hint_ms, Ordering::SeqCst);
        let Some(handle) = self.status.get() else {
            return;
        };
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: state,
            // 只有运行中才接受控制请求：过渡状态下再收一个 STOP 只会让状态机打架。
            dwControlsAccepted: if state == SERVICE_RUNNING {
                ACCEPTED_CONTROLS
            } else {
                0
            },
            dwWin32ExitCode: win32_exit,
            dwServiceSpecificExitCode: specific_exit,
            dwCheckPoint: checkpoint,
            dwWaitHint: wait_hint_ms,
        };
        // SAFETY: handle 是 RegisterServiceCtrlHandlerExW 返回的有效状态句柄；
        // status 是一个完整、已初始化的 SERVICE_STATUS，调用期间一直存活。
        unsafe {
            SetServiceStatus(handle.0, &raw const status);
        }
    }

    /// 推进一个过渡步：checkpoint 自增后上报。
    fn advance(&self, state: u32, wait_hint_ms: u32) {
        let checkpoint = self.checkpoint.fetch_add(1, Ordering::SeqCst) + 1;
        self.set_status(state, checkpoint, wait_hint_ms, NO_ERROR, 0);
    }

    /// 进入 `SERVICE_RUNNING`。
    fn running(&self) {
        self.checkpoint.store(0, Ordering::SeqCst);
        self.set_status(SERVICE_RUNNING, 0, 0, NO_ERROR, 0);
    }

    /// 进入 `SERVICE_STOPPED`。`error` 为真时用 `ERROR_SERVICE_SPECIFIC_ERROR`
    /// 告诉 SCM「失败原因不在 Win32 错误空间里，去看服务自己的日志」。
    fn stopped(&self, error: bool) {
        self.checkpoint.store(0, Ordering::SeqCst);
        let (win32, specific) = if error {
            (ERROR_SERVICE_SPECIFIC_ERROR, EXIT_FAILURE)
        } else {
            (NO_ERROR, 0)
        };
        self.set_status(SERVICE_STOPPED, 0, 0, win32, specific);
    }

    /// 收到停止/关机请求：先让 SCM 知道我们收下了，再把档位交给异步侧。
    fn request(&self, kind: ShutdownKind, wait_hint_ms: u32) {
        self.advance(SERVICE_STOP_PENDING, wait_hint_ms);
        // `send_replace` 而不是 `send`：后者在当前没有接收者时会连值都不写，
        // 而接收者要到 `run_service` 里才 `subscribe`。
        let _ = self.shutdown.send_replace(Some(kind));
    }

    /// `SERVICE_CONTROL_INTERROGATE`：原样重报当前状态。
    fn repeat(&self) {
        let state = self.current.load(Ordering::SeqCst);
        let checkpoint = self.checkpoint.load(Ordering::SeqCst);
        let hint = self.wait_hint.load(Ordering::SeqCst);
        self.set_status(state, checkpoint, hint, NO_ERROR, 0);
    }
}

/// 把启动阶段折算成 SCM 的 `SERVICE_START_PENDING` 上报。
struct ScmReporter {
    control: &'static ServiceControl,
}

impl StartupReporter for ScmReporter {
    fn stage(&self, name: &str) {
        self.control
            .advance(SERVICE_START_PENDING, START_WAIT_HINT_MS);
        tracing::debug!(stage = name, "启动阶段推进");
    }

    fn ready(&self) {
        self.control.running();
        tracing::info!(
            service = APP.get().map_or("", |a| a.identity().name),
            "已向 SCM 报告 SERVICE_RUNNING"
        );
    }
}

/// `service run` 的入口：把主线程交给 SCM。
///
/// 手工在命令行里跑会失败并给出提示，见 [`super::explain`] 对
/// `ERROR_FAILED_SERVICE_CONTROLLER_CONNECT` 的翻译。
pub fn run(app: &'static dyn ServiceApp) -> anyhow::Result<()> {
    if APP.set(app).is_err() {
        bail!("`service run` 在一个进程里只能调用一次");
    }

    let mut name = to_wide(app.identity().name);
    // 服务表以「全零项」结尾，这是 `StartServiceCtrlDispatcherW` 的约定。
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];

    // SAFETY: table 是合法的、以全零项结尾的服务表，name 以 NUL 结尾；
    // 两者都是本函数的局部变量，而本函数在调用返回之前不会结束，
    // 因此在 SCM 使用它们的整个期间都存活。
    let ok = unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后，中间没有别的系统调用。
        let code = unsafe { GetLastError() };
        bail!("连接服务控制管理器失败：{}", super::explain(code));
    }
    Ok(())
}

/// SCM 在分派线程上调用的服务主函数。
///
/// 参数是 SCM 传来的「启动参数」——管理员在服务管理单元里临时填的、**一次性**
/// 的东西，下次启动就没了。`docs/design.md` §12 的四层配置里没有它的位置，
/// 因此一律忽略；真正的配置来自 `ImagePath` 上的命令行，已经由 clap 在主线程
/// 解析好并存进 [`STARTUP`]。
unsafe extern "system" fn service_main(_argc: u32, _argv: *mut windows_sys::core::PWSTR) {
    let Some(control) = register() else {
        // 连状态句柄都拿不到，什么都报不出去，只能让 SCM 按超时处理。
        logging::last_resort("RegisterServiceCtrlHandlerExW 失败，无法向 SCM 上报状态");
        return;
    };
    control.advance(SERVICE_START_PENDING, START_WAIT_HINT_MS);

    match run_service(control) {
        Ok(()) => {
            tracing::info!("服务正常退出");
            control.stopped(false);
        }
        Err(e) => {
            let text = format!("{e:#}");
            tracing::error!(error = %text, "服务异常退出");
            // 失败可能发生在日志初始化之前，那时 tracing 没有订阅者。
            logging::last_resort(&text);
            control.stopped(true);
        }
    }
}

/// 注册控制处理器，返回进程级的控制状态。
fn register() -> Option<&'static ServiceControl> {
    let control = CONTROL.get_or_init(|| ServiceControl {
        status: OnceLock::new(),
        checkpoint: AtomicU32::new(0),
        current: AtomicU32::new(SERVICE_START_PENDING),
        wait_hint: AtomicU32::new(START_WAIT_HINT_MS),
        shutdown: watch::channel(None).0,
    });

    let name = to_wide(APP.get().expect("register 只在 run 之后被调用").identity().name);
    // SAFETY: name 以 NUL 结尾且在调用期间存活；上下文指针指向 `CONTROL`
    // 这个进程级 `OnceLock` 里的值，其地址在进程存续期间一直有效，
    // 因此 SCM 在任何时刻回调都不会悬垂。
    let handle = unsafe {
        RegisterServiceCtrlHandlerExW(
            name.as_ptr(),
            Some(control_handler),
            std::ptr::from_ref(control).cast::<core::ffi::c_void>(),
        )
    };
    if handle.is_null() {
        return None;
    }
    let _ = control.status.set(StatusHandle(handle));
    Some(control)
}

/// 在分派线程上跑完整个服务。
fn run_service(control: &'static ServiceControl) -> anyhow::Result<()> {
    let app = APP
        .get()
        .expect("run() 在把主线程交给 SCM 之前已经写入服务实现");
    let id = app.identity();

    // 把日志落到文件。必须在这一步就位：往后的任何输出都只能进文件，
    // 服务进程没有 stderr。落点由宿主算——日志目录来自它自己那份配置。
    let log_path = app.prepare()?;
    tracing::info!(
        service = id.name,
        log = %log_path.display(),
        // 取宿主二进制的版本，不是 `strixmaid-node` 的：本模块搬进 node 之后，
        // 这里写 env!("CARGO_PKG_VERSION") 会取错。
        version = id.version,
        "以 Windows 服务身份启动"
    );
    control.advance(SERVICE_START_PENDING, START_WAIT_HINT_MS);

    // 运行时建在**本线程**（SCM 的分派线程）上，理由见模块文档。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("创建 tokio 运行时失败")?;

    let reporter = Arc::new(ScmReporter { control });
    let shutdown = Box::pin(wait_shutdown(control.shutdown.subscribe()));
    let result = runtime.block_on(app.serve(reporter, shutdown));

    // 先落运行时再报 STOPPED：SCM 一看到 STOPPED 就可能立刻按恢复策略重启服务，
    // 那时本进程最好已经没有仍在跑的线程，否则两代进程会抢同一个监听端口与
    // 同一个 SQLite 文件。给它一个上限，避免某个卡住的阻塞任务把关停拖死。
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    result
}

/// 把 `watch` 上的关停档位折算成 `serve_with` 要的 future。
async fn wait_shutdown(mut rx: watch::Receiver<Option<ShutdownKind>>) -> ShutdownKind {
    loop {
        // 先看当前值：控制处理器完全可能在 `subscribe` 之前就写过了。
        // `borrow_and_update` 返回的守卫必须在 `.await` 之前落掉，故先取值再判断。
        let current = *rx.borrow_and_update();
        if let Some(kind) = current {
            return kind;
        }
        if rx.changed().await.is_err() {
            // 发送端住在进程级的 `OnceLock` 里，永不 drop；真走到这里说明
            // 内部状态出了问题，按完整关停处理是两者中更安全的一个。
            tracing::warn!("关停通道意外关闭，按完整关停处理");
            return ShutdownKind::Graceful;
        }
    }
}

/// SCM 在控制线程上调用的控制处理器。必须立刻返回。
///
/// 三种停止语义的时限差着两个数量级，因此折算成不同的 [`ShutdownKind`]：
///
/// | 控制码 | 何时发 | 时限 | 档位 |
/// |---|---|---|---|
/// | `SERVICE_CONTROL_STOP` | `sc stop` / 服务管理单元 | 由本服务声明 | [`ShutdownKind::Graceful`] |
/// | `SERVICE_CONTROL_PRESHUTDOWN` | 关机流程开始前 | 默认 3 分钟 | [`ShutdownKind::Graceful`] |
/// | `SERVICE_CONTROL_SHUTDOWN` | 关机流程中 | `WaitToKillServiceTimeout`，默认 5 秒 | [`ShutdownKind::Urgent`] |
///
/// 未识别的控制码返回 `ERROR_CALL_NOT_IMPLEMENTED`，这是 SCM 约定的「不支持」，
/// 比返回 `NO_ERROR` 装作处理过要诚实。
unsafe extern "system" fn control_handler(
    control: u32,
    _event_type: u32,
    _event_data: *mut core::ffi::c_void,
    context: *mut core::ffi::c_void,
) -> u32 {
    // SAFETY: context 正是 `register` 通过 lpContext 交给 SCM 的
    // `&'static ServiceControl`，来自进程级 `OnceLock`，在进程存续期间有效。
    let Some(state) = (unsafe { context.cast::<ServiceControl>().as_ref() }) else {
        return ERROR_CALL_NOT_IMPLEMENTED;
    };

    match control {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_PRESHUTDOWN => {
            state.request(ShutdownKind::Graceful, GRACEFUL_STOP_HINT_MS);
            NO_ERROR
        }
        SERVICE_CONTROL_SHUTDOWN => {
            state.request(ShutdownKind::Urgent, URGENT_STOP_HINT_MS);
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => {
            state.repeat();
            NO_ERROR
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一个不带状态句柄的控制状态：`set_status` 会在 `status.get()` 处返回，
    /// 不会真的去调 `SetServiceStatus`，因此可以在单元测试里安全地驱动状态机。
    fn detached() -> ServiceControl {
        ServiceControl {
            status: OnceLock::new(),
            checkpoint: AtomicU32::new(0),
            current: AtomicU32::new(SERVICE_START_PENDING),
            wait_hint: AtomicU32::new(START_WAIT_HINT_MS),
            shutdown: watch::channel(None).0,
        }
    }

    #[test]
    fn 启动期的_checkpoint_单调递增() {
        let c = detached();
        let mut seen = Vec::new();
        for _ in 0..5 {
            c.advance(SERVICE_START_PENDING, START_WAIT_HINT_MS);
            seen.push(c.checkpoint.load(Ordering::SeqCst));
        }
        assert_eq!(seen, vec![1, 2, 3, 4, 5], "SCM 靠它判断进程没卡死");
        assert_eq!(c.current.load(Ordering::SeqCst), SERVICE_START_PENDING);
    }

    #[test]
    fn 稳定态把_checkpoint_清零() {
        let c = detached();
        c.advance(SERVICE_START_PENDING, START_WAIT_HINT_MS);
        c.running();
        assert_eq!(
            c.checkpoint.load(Ordering::SeqCst),
            0,
            "RUNNING 的 checkpoint 必须是 0，否则 SCM 认为还在过渡"
        );
        assert_eq!(c.current.load(Ordering::SeqCst), SERVICE_RUNNING);
        assert_eq!(c.wait_hint.load(Ordering::SeqCst), 0);

        c.stopped(false);
        assert_eq!(c.current.load(Ordering::SeqCst), SERVICE_STOPPED);
        assert_eq!(c.checkpoint.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn 停止请求按控制码折算档位() {
        let c = detached();
        c.request(ShutdownKind::Graceful, GRACEFUL_STOP_HINT_MS);
        assert_eq!(*c.shutdown.borrow(), Some(ShutdownKind::Graceful));
        assert_eq!(c.current.load(Ordering::SeqCst), SERVICE_STOP_PENDING);
        assert_eq!(c.wait_hint.load(Ordering::SeqCst), GRACEFUL_STOP_HINT_MS);

        let c = detached();
        c.request(ShutdownKind::Urgent, URGENT_STOP_HINT_MS);
        assert_eq!(*c.shutdown.borrow(), Some(ShutdownKind::Urgent));
        assert_eq!(c.wait_hint.load(Ordering::SeqCst), URGENT_STOP_HINT_MS);
        // 两档时限的大小关系由模块里的 `const _: () = assert!(..)` 在编译期守住。
    }

    #[test]
    fn 档位在订阅之前写入也能被读到() {
        // 控制处理器可能在 run_service 调用 subscribe 之前就收到 STOP，
        // 这正是用 send_replace 而不是 send 的原因。
        let c = detached();
        c.request(ShutdownKind::Urgent, URGENT_STOP_HINT_MS);
        let rx = c.shutdown.subscribe();
        assert_eq!(*rx.borrow(), Some(ShutdownKind::Urgent));
    }

    #[tokio::test]
    async fn 关停_future_解析出写入的档位() {
        let c = detached();
        let fut = wait_shutdown(c.shutdown.subscribe());
        c.request(ShutdownKind::Graceful, GRACEFUL_STOP_HINT_MS);
        assert_eq!(fut.await, ShutdownKind::Graceful);
    }

    #[test]
    fn 接受的控制码覆盖三种停止路径() {
        assert_ne!(ACCEPTED_CONTROLS & SERVICE_ACCEPT_STOP, 0);
        assert_ne!(ACCEPTED_CONTROLS & SERVICE_ACCEPT_SHUTDOWN, 0);
        assert_ne!(
            ACCEPTED_CONTROLS & SERVICE_ACCEPT_PRESHUTDOWN,
            0,
            "少了 PRESHUTDOWN，关机时就只剩几秒可用"
        );
    }
}
