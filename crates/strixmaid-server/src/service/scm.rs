//! 从命令行这一侧操作 SCM：注册、注销、启停、查询。
//!
//! 与 [`super::host`] 的方向正好相反——那边是「SCM 调用本进程」，这边是
//! 「本进程调用 SCM」。两边共用 [`super::SERVICE_NAME`]，注册时写进
//! `ImagePath` 的命令行也正是 `<exe> service run`。
//!
//! # 为什么不调 `sc.exe`
//!
//! `sc.exe create` 能做同样的事，但它把结构化信息压成了一串本地化的控制台文本：
//! 失败时只能拿到「[SC] CreateService 失败 5:」再自己去猜 5 是什么，
//! 恢复策略要用 `sc failure name actions= restart/5000/...` 这种自定义小语言
//! 拼字符串，而且依赖 `%SystemRoot%\system32` 在 `PATH` 上。直接调 API 反而更短，
//! 错误码也能原样翻译（见 [`super::explain`]）。
//!
//! # 恢复策略
//!
//! 注册时一并设置 `SERVICE_FAILURE_ACTIONS`：失败后 5 秒 / 30 秒 / 60 秒各重启
//! 一次，一天无事则把失败计数清零。并打开
//! `SERVICE_CONFIG_FAILURE_ACTIONS_FLAG`——**默认情况下只有进程崩溃才算失败**，
//! 而本程序遇到致命错误是带非零退出码正常退出的（`main` 返回 `Err`），
//! 不打开这个开关，恢复策略对最常见的失败方式完全不生效。
//!
//! 这一组取值对应 systemd 单元里的 `Restart=on-failure` + `RestartSec=5`。

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use strixmaid_core::platform::windows::to_wide;
use windows_sys::Win32::Foundation::{ERROR_SERVICE_EXISTS, GetLastError};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW, QueryServiceStatusEx, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SC_STATUS_PROCESS_INFO, SERVICE_ALL_ACCESS, SERVICE_AUTO_START, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_FAILURE_ACTIONS_FLAG, SERVICE_CONTROL_STOP,
    SERVICE_DESCRIPTIONW, SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONS_FLAG,
    SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_QUERY_STATUS, SERVICE_RUNNING,
    SERVICE_START, SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_STOP, SERVICE_STOPPED,
    SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};

use super::{DESCRIPTION, DISPLAY_NAME, SERVICE_NAME, explain, state_text};
use crate::cli::{GlobalArgs, ServiceInstallArgs};

/// 等服务进入目标状态的总上限。
///
/// 启动要开库、起 helper、探测能力，慢的机器上十几秒并不罕见；停止要排空连接
/// 并收 worker。取 60 秒：比这还久就该去看日志，而不是继续等。
const WAIT_LIMIT: Duration = Duration::from_secs(60);

/// 轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 恢复策略里的失败计数清零周期（秒）：一天之内没再出事就既往不咎。
const FAILURE_RESET_SECS: u32 = 24 * 3600;

/// 三次重启的间隔（毫秒）。
const RESTART_DELAYS_MS: [u32; 3] = [5_000, 30_000, 60_000];

// ===========================================================================
// 句柄 RAII
// ===========================================================================

/// `SC_HANDLE` 的 RAII 包装。
///
/// SCM 句柄与内核句柄不是一回事（关它的是 `CloseServiceHandle` 而不是
/// `CloseHandle`），所以不能复用 `platform::windows::handle::Owned`。
/// 失败时 `SC_HANDLE` 返回**空指针**，不是 `INVALID_HANDLE_VALUE`。
struct ScHandle(SC_HANDLE);

impl ScHandle {
    /// 接管一个刚由 SCM 返回的句柄；空指针视为失败，返回当时的 Win32 错误码。
    ///
    /// # Safety
    ///
    /// `raw` 必须是刚由 `OpenSCManagerW` / `OpenServiceW` / `CreateServiceW`
    /// 返回、尚无其它持有者的句柄。
    unsafe fn new(raw: SC_HANDLE) -> Result<ScHandle, u32> {
        if raw.is_null() {
            // SAFETY: GetLastError 无参数、无副作用，紧跟在失败的调用之后。
            return Err(unsafe { GetLastError() });
        }
        Ok(ScHandle(raw))
    }

    fn raw(&self) -> SC_HANDLE {
        self.0
    }
}

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: 句柄由本类型独占，构造时已排除空指针，且只关一次。
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

/// 打开 SCM。
fn open_manager(access: u32) -> anyhow::Result<ScHandle> {
    // SAFETY: 机器名与数据库名取空 = 本机 + 默认数据库（`ServicesActive`）。
    let raw = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), access) };
    // SAFETY: raw 刚由 OpenSCManagerW 返回，尚无其它持有者。
    unsafe { ScHandle::new(raw) }
        .map_err(|code| anyhow::anyhow!("连接 SCM 失败：{}", explain(code)))
}

/// 打开本服务。
fn open_service(mgr: &ScHandle, access: u32) -> anyhow::Result<ScHandle> {
    let name = to_wide(SERVICE_NAME);
    // SAFETY: name 以 NUL 结尾并在调用期间存活；mgr 是有效的 SCM 句柄。
    let raw = unsafe { OpenServiceW(mgr.raw(), name.as_ptr(), access) };
    // SAFETY: raw 刚由 OpenServiceW 返回，尚无其它持有者。
    unsafe { ScHandle::new(raw) }
        .map_err(|code| anyhow::anyhow!("打开服务 {SERVICE_NAME} 失败：{}", explain(code)))
}

/// 查一次服务状态。
fn query(svc: &ScHandle) -> anyhow::Result<SERVICE_STATUS_PROCESS> {
    let mut st = SERVICE_STATUS_PROCESS::default();
    let mut needed: u32 = 0;
    let size = size_of::<SERVICE_STATUS_PROCESS>() as u32;
    // SAFETY: 缓冲是一个完整的 SERVICE_STATUS_PROCESS，size 如实描述其大小；
    // needed 是一个合法的 u32 可写位置。
    let ok = unsafe {
        QueryServiceStatusEx(
            svc.raw(),
            SC_STATUS_PROCESS_INFO,
            (&raw mut st).cast::<u8>(),
            size,
            &raw mut needed,
        )
    };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        bail!("查询服务状态失败：{}", explain(code));
    }
    Ok(st)
}

// ===========================================================================
// 子命令
// ===========================================================================

/// `service install`。
///
/// **幂等**：服务已存在时改配置而不是报错。升级是最常见的重复安装场景
/// （`packaging/windows/install.ps1` 就直接依赖这一点），若在这里报
/// `ERROR_SERVICE_EXISTS`，安装脚本要么先卸载——那会丢掉管理员在服务管理单元里
/// 做过的调整（登录账户、依赖、延迟启动）——要么把错误吞掉，两条都不好。
pub fn install(args: &ServiceInstallArgs, global: &GlobalArgs) -> anyhow::Result<()> {
    let exe = match &args.exe {
        Some(p) => p.clone(),
        None => std::env::current_exe().context("取当前可执行文件路径失败")?,
    };
    // 服务由 `services.exe` 拉起，工作目录是 `%SystemRoot%\system32`，
    // 相对路径解析不到，必须在注册时就定成绝对路径。
    let exe = std::path::absolute(&exe)
        .with_context(|| format!("无法把 {} 展开成绝对路径", exe.display()))?;
    if !exe.is_file() {
        bail!("可执行文件不存在: {}", exe.display());
    }
    let account = match args.account.as_deref() {
        None => LOCAL_SYSTEM,
        Some(a) => normalize_account(a).ok_or_else(|| {
            anyhow::anyhow!(
                "不支持的服务账户 `{a}`。只接受三个无口令的内置账户：\
                 LocalSystem、NT AUTHORITY\\LocalService、NT AUTHORITY\\NetworkService。\
                 域账户需要口令，而明文口令不进命令行也不进注册表——\
                 请先安装，再用「服务」管理单元设置登录账户。"
            )
        })?,
    };
    let cmdline = image_path(&exe, global);

    let mgr = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE)?;

    let name = to_wide(SERVICE_NAME);
    let display = to_wide(DISPLAY_NAME);
    let path = to_wide(&cmdline);
    let acct = to_wide(account);
    // SAFETY: 四个宽字符串都以 NUL 结尾且在调用期间存活；
    // 载入顺序组、TagId、依赖表、口令按文档取空指针（= 无 / 默认）。
    let raw = unsafe {
        CreateServiceW(
            mgr.raw(),
            name.as_ptr(),
            display.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            path.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            acct.as_ptr(),
            std::ptr::null(),
        )
    };
    // SAFETY: raw 刚由 CreateServiceW 返回，尚无其它持有者。
    let (svc, created) = match unsafe { ScHandle::new(raw) } {
        Ok(svc) => (svc, true),
        Err(ERROR_SERVICE_EXISTS) => (reconfigure(&mgr, &path, &display, &acct)?, false),
        Err(code) => bail!("注册服务失败：{}", explain(code)),
    };

    set_description(&svc)?;
    set_failure_actions(&svc)?;

    if created {
        println!("已注册服务 {SERVICE_NAME}（{DISPLAY_NAME}）");
    } else {
        println!("服务 {SERVICE_NAME} 已存在，已更新其配置");
    }
    println!("  账户    : {account}");
    println!("  启动类型: 自动");
    println!("  命令行  : {cmdline}");
    println!("  日志    : {}", super::logging::describe_target(global));

    if args.start {
        drop(svc);
        drop(mgr);
        return start();
    }
    println!("用 `strixmaid service start` 启动它。");
    Ok(())
}

/// 服务已存在时，把 `install` 会写的那几项改成新值。
///
/// 只动本程序自己决定的三项：ImagePath（升级时二进制路径可能变了）、启动类型
/// （统一回到自动启动）、登录账户。其余一律传 `SERVICE_NO_CHANGE` / 空指针
/// ——依赖关系、载入顺序组这些可能是管理员手工调过的，重复安装不该把它们抹平。
fn reconfigure(
    mgr: &ScHandle,
    image_path: &[u16],
    display: &[u16],
    account: &[u16],
) -> anyhow::Result<ScHandle> {
    let svc = open_service(mgr, SERVICE_ALL_ACCESS)?;
    // SAFETY: 三个宽字符串都以 NUL 结尾且在调用期间存活；
    // TagId 与依赖表传空指针（= 不改），口令传空（内置账户无口令）。
    let ok = unsafe {
        ChangeServiceConfigW(
            svc.raw(),
            SERVICE_NO_CHANGE,
            SERVICE_AUTO_START,
            SERVICE_NO_CHANGE,
            image_path.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            account.as_ptr(),
            std::ptr::null(),
            display.as_ptr(),
        )
    };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        bail!("更新既有服务的配置失败：{}", explain(code));
    }
    Ok(svc)
}

/// `service uninstall`。
///
/// 先尽力停掉再删：`DeleteService` 对运行中的服务只会把它标成「待删除」，
/// 句柄全部关闭且服务停止后才真正消失。直接删会留下一个查得到、启不动的残影，
/// 紧接着再 `install` 还会撞上 `ERROR_SERVICE_MARKED_FOR_DELETE`。
pub fn uninstall() -> anyhow::Result<()> {
    let mgr = open_manager(SC_MANAGER_CONNECT)?;
    let svc = open_service(&mgr, SERVICE_ALL_ACCESS)?;

    let st = query(&svc)?;
    if st.dwCurrentState != SERVICE_STOPPED {
        println!("服务当前{}，先停止……", state_text(st.dwCurrentState));
        send_stop(&svc)?;
        wait_for(&svc, SERVICE_STOPPED)?;
    }

    // SAFETY: svc 是带 DELETE 权限（含在 SERVICE_ALL_ACCESS 内）的有效服务句柄。
    let ok = unsafe { DeleteService(svc.raw()) };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        bail!("注销服务失败：{}", explain(code));
    }
    println!("已注销服务 {SERVICE_NAME}");
    Ok(())
}

/// `service start`。
///
/// 服务已在运行不算失败：`install --start` 与安装脚本都会重复调用它，
/// 「已经是想要的状态」应当安静地通过（与 systemd `start` 的语义一致）。
pub fn start() -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SERVICE_ALREADY_RUNNING;

    let mgr = open_manager(SC_MANAGER_CONNECT)?;
    let svc = open_service(&mgr, SERVICE_START | SERVICE_QUERY_STATUS)?;

    // SAFETY: svc 带 SERVICE_START；不传启动参数，故个数为 0、指针为空。
    let ok = unsafe { StartServiceW(svc.raw(), 0, std::ptr::null()) };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        if code != ERROR_SERVICE_ALREADY_RUNNING {
            bail!("启动服务失败：{}", explain(code));
        }
    }
    let st = wait_for(&svc, SERVICE_RUNNING)?;
    println!("服务 {SERVICE_NAME} 已启动（pid {}）", st.dwProcessId);
    Ok(())
}

/// `service stop`。
pub fn stop() -> anyhow::Result<()> {
    let mgr = open_manager(SC_MANAGER_CONNECT)?;
    let svc = open_service(&mgr, SERVICE_STOP | SERVICE_QUERY_STATUS)?;
    send_stop(&svc)?;
    wait_for(&svc, SERVICE_STOPPED)?;
    println!("服务 {SERVICE_NAME} 已停止");
    Ok(())
}

/// `service status`。
pub fn status() -> anyhow::Result<()> {
    let mgr = open_manager(SC_MANAGER_CONNECT)?;
    let svc = open_service(&mgr, SERVICE_QUERY_STATUS)?;
    let st = query(&svc)?;

    println!("服务  : {SERVICE_NAME}（{DISPLAY_NAME}）");
    println!("状态  : {}", state_text(st.dwCurrentState));
    if st.dwProcessId != 0 {
        println!("进程  : {}", st.dwProcessId);
    }
    if st.dwCurrentState == SERVICE_STOPPED && st.dwWin32ExitCode != 0 {
        println!(
            "退出码: {}（{}）",
            st.dwWin32ExitCode,
            explain(st.dwWin32ExitCode)
        );
    }
    if st.dwCheckPoint != 0 || st.dwWaitHint != 0 {
        println!(
            "进度  : checkpoint {}，wait hint {} 毫秒",
            st.dwCheckPoint, st.dwWaitHint
        );
    }
    Ok(())
}

// ===========================================================================
// 内部
// ===========================================================================

/// 发一次 `SERVICE_CONTROL_STOP`。已经停了不算错。
fn send_stop(svc: &ScHandle) -> anyhow::Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SERVICE_NOT_ACTIVE;

    let mut st = SERVICE_STATUS::default();
    // SAFETY: svc 带 SERVICE_STOP；st 是一个完整的、可写的 SERVICE_STATUS。
    let ok = unsafe { ControlService(svc.raw(), SERVICE_CONTROL_STOP, &raw mut st) };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        if code == ERROR_SERVICE_NOT_ACTIVE {
            return Ok(());
        }
        bail!("发送停止请求失败：{}", explain(code));
    }
    Ok(())
}

/// 轮询直到服务进入 `target`。
///
/// 判定「卡死」的口径跟着 `dwCheckPoint` 走：只要这个数还在涨，就说明服务在
/// 推进，把计时重置。这正是 SCM 自己的口径，照抄它可以避免「服务其实在慢慢
/// 启动，客户端却先报了超时」。总时长另有 [`WAIT_LIMIT`] 兜底。
fn wait_for(svc: &ScHandle, target: u32) -> anyhow::Result<SERVICE_STATUS_PROCESS> {
    let begin = Instant::now();
    let mut last_checkpoint: Option<u32> = None;
    let mut last_progress = Instant::now();

    loop {
        let st = query(svc)?;
        if st.dwCurrentState == target {
            return Ok(st);
        }
        if target != SERVICE_STOPPED && st.dwCurrentState == SERVICE_STOPPED {
            bail!(
                "服务在到达目标状态前就停止了（退出码 {}：{}），请查看 {}",
                st.dwWin32ExitCode,
                explain(st.dwWin32ExitCode),
                super::logging::DEFAULT_LOG_DIR
            );
        }

        if last_checkpoint != Some(st.dwCheckPoint) {
            last_checkpoint = Some(st.dwCheckPoint);
            last_progress = Instant::now();
        } else {
            // `dwWaitHint` 是服务自己声明的「下一次推进前最多要这么久」。
            // 取它与 WAIT_LIMIT 的较小值，避免一个离谱的 hint 把等待拖到天亮。
            let hint = Duration::from_millis(u64::from(st.dwWaitHint)).min(WAIT_LIMIT);
            if last_progress.elapsed() > hint.max(POLL_INTERVAL * 8) {
                bail!(
                    "服务停在{}且 checkpoint 不再推进（停留 {} 毫秒）",
                    state_text(st.dwCurrentState),
                    last_progress.elapsed().as_millis()
                );
            }
        }

        if begin.elapsed() > WAIT_LIMIT {
            bail!(
                "等待服务进入「{}」超时（{} 秒），当前{}",
                state_text(target),
                WAIT_LIMIT.as_secs(),
                state_text(st.dwCurrentState)
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// 写服务描述。
fn set_description(svc: &ScHandle) -> anyhow::Result<()> {
    let mut text = to_wide(DESCRIPTION);
    let info = SERVICE_DESCRIPTIONW {
        lpDescription: text.as_mut_ptr(),
    };
    // SAFETY: info 指向的宽字符串以 NUL 结尾且在调用期间存活；
    // dwInfoLevel 与结构体类型一一对应。
    let ok = unsafe {
        ChangeServiceConfig2W(
            svc.raw(),
            SERVICE_CONFIG_DESCRIPTION,
            (&raw const info).cast::<core::ffi::c_void>(),
        )
    };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        bail!("设置服务描述失败：{}", explain(code));
    }
    Ok(())
}

/// 写恢复策略（失败后重启），见模块文档。
fn set_failure_actions(svc: &ScHandle) -> anyhow::Result<()> {
    let mut actions: [SC_ACTION; 3] = RESTART_DELAYS_MS.map(|delay| SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: delay,
    });
    let info = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: FAILURE_RESET_SECS,
        // 空指针 = 这一项不改。新服务本来就没有重启消息与失败命令，正合适。
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: actions.len() as u32,
        lpsaActions: actions.as_mut_ptr(),
    };
    // SAFETY: actions 在调用期间存活，cActions 如实描述其长度；
    // dwInfoLevel 与结构体类型一一对应。
    let ok = unsafe {
        ChangeServiceConfig2W(
            svc.raw(),
            SERVICE_CONFIG_FAILURE_ACTIONS,
            (&raw const info).cast::<core::ffi::c_void>(),
        )
    };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        bail!("设置恢复策略失败：{}", explain(code));
    }

    // 默认只有「进程崩溃」才触发恢复策略。本程序遇到致命错误是带非零退出码
    // 正常退出的，不打开这个开关，上面那套重启规则对最常见的失败方式无效。
    let flag = SERVICE_FAILURE_ACTIONS_FLAG {
        fFailureActionsOnNonCrashFailures: 1,
    };
    // SAFETY: 同上。
    let ok = unsafe {
        ChangeServiceConfig2W(
            svc.raw(),
            SERVICE_CONFIG_FAILURE_ACTIONS_FLAG,
            (&raw const flag).cast::<core::ffi::c_void>(),
        )
    };
    if ok == 0 {
        // SAFETY: 紧跟失败的调用之后。
        let code = unsafe { GetLastError() };
        bail!("打开「非崩溃失败也触发恢复」开关失败：{}", explain(code));
    }
    Ok(())
}

/// `LocalSystem`：服务的默认账户，也是本服务需要的身份——它要以最高权限拉起
/// helper，再由 helper 切换到登录用户（见 `docs/design.md` §2）。
const LOCAL_SYSTEM: &str = "LocalSystem";

/// 把用户写的账户名规范成 `CreateServiceW` 认识的形式。
///
/// 只认三个**无口令**的内置服务账户。域账户与本地账户需要口令，而口令写进
/// 命令行就会落到进程表与命令历史里，写进注册表就会以可逆加密长期留在磁盘上
/// ——两者都与「明文口令只存在于 `Zeroizing<String>`」的约定相冲突。
/// 那种部署请在安装后用「服务」管理单元设置登录账户，由 LSA 保管口令。
fn normalize_account(input: &str) -> Option<&'static str> {
    let trimmed = input.trim();
    const CANDIDATES: [(&str, &str); 7] = [
        ("localsystem", LOCAL_SYSTEM),
        ("system", LOCAL_SYSTEM),
        (r"nt authority\system", LOCAL_SYSTEM),
        ("localservice", r"NT AUTHORITY\LocalService"),
        (r"nt authority\localservice", r"NT AUTHORITY\LocalService"),
        ("networkservice", r"NT AUTHORITY\NetworkService"),
        (
            r"nt authority\networkservice",
            r"NT AUTHORITY\NetworkService",
        ),
    ];
    CANDIDATES
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(trimmed))
        .map(|(_, canonical)| *canonical)
}

/// 拼出写进服务 `ImagePath` 的命令行。
///
/// 透传的只有那几个**决定行为**的全局参数。理由是服务进程没有 shell、没有交互，
/// 注册时是什么，之后每次开机启动就是什么——没有别的地方能补。
///
/// 环境变量那一层在服务模式下形同虚设：服务继承的是 `services.exe` 的系统环境块，
/// 用户会话里 `set STRIXMAID_...` 对它没有任何影响。要靠环境变量配置服务，
/// 得改**系统**环境变量并重启服务；更常规的做法是把配置写进配置文件、
/// 安装时用 `--config` 指过去。
fn image_path(exe: &Path, global: &GlobalArgs) -> String {
    let mut parts = vec![
        quote_arg(&exe.display().to_string()),
        "service".to_owned(),
        "run".to_owned(),
    ];
    if let Some(path) = &global.config {
        parts.push("--config".to_owned());
        parts.push(quote_arg(&path.display().to_string()));
    }
    if let Some(addr) = &global.listen {
        parts.push("--listen".to_owned());
        parts.push(addr.to_string());
    }
    if let Some(dir) = &global.data_dir {
        parts.push("--data-dir".to_owned());
        parts.push(quote_arg(&dir.display().to_string()));
    }
    if let Some(level) = &global.log_level {
        parts.push("--log-level".to_owned());
        parts.push(level.as_str().to_owned());
    }
    parts.join(" ")
}

/// 按 `CommandLineToArgvW` 的规则给一个参数加引号。
///
/// `ImagePath` 保存的是一整条命令行，进程启动时由 `CommandLineToArgvW` 拆回参数表。
/// 路径里带空格（`C:\Program Files\StrixMaid\strixmaid.exe`）太常见，不加引号
/// 会被拆成两个参数。而反斜杠与引号的交互有一条容易写错的规则：
/// **只有紧挨着引号的那串反斜杠才需要翻倍**。Windows 路径末尾恰好可能是反斜杠
/// （`C:\ProgramData\StrixMaid\`），此时若不翻倍，闭合引号会被它转义掉，
/// 整条命令行从那里开始全错。
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // 紧挨引号的那串反斜杠翻倍，然后转义引号本身。
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    // 末尾的反斜杠紧挨着闭合引号，同样要翻倍。
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use strixmaid_core::config::LogLevel;

    use super::*;

    fn empty_global() -> GlobalArgs {
        GlobalArgs {
            config: None,
            listen: None,
            data_dir: None,
            log_level: None,
        }
    }

    #[test]
    fn 命令行最简形态只有可执行文件与子命令() {
        let line = image_path(Path::new(r"C:\tools\strixmaid.exe"), &empty_global());
        assert_eq!(line, r"C:\tools\strixmaid.exe service run");
    }

    #[test]
    fn 带空格的路径必须加引号() {
        let line = image_path(
            Path::new(r"C:\Program Files\StrixMaid\strixmaid.exe"),
            &empty_global(),
        );
        assert_eq!(
            line,
            r#""C:\Program Files\StrixMaid\strixmaid.exe" service run"#
        );
    }

    #[test]
    fn 显式给出的全局参数逐项透传() {
        let global = GlobalArgs {
            config: Some(PathBuf::from(r"C:\ProgramData\StrixMaid\config.toml")),
            listen: Some("0.0.0.0:9700".parse::<SocketAddr>().unwrap()),
            data_dir: Some(PathBuf::from(r"D:\data dir")),
            log_level: Some(LogLevel::Debug),
        };
        let line = image_path(Path::new(r"C:\tools\strixmaid.exe"), &global);
        assert_eq!(
            line,
            concat!(
                r"C:\tools\strixmaid.exe service run ",
                r"--config C:\ProgramData\StrixMaid\config.toml ",
                r"--listen 0.0.0.0:9700 ",
                r#"--data-dir "D:\data dir" "#,
                "--log-level debug"
            )
        );
    }

    #[test]
    fn 结尾反斜杠在加引号时翻倍() {
        // 不翻倍的话，`"C:\dir\"` 里的闭合引号会被反斜杠转义掉。
        assert_eq!(quote_arg(r"C:\a b\"), r#""C:\a b\\""#);
        // 中间的反斜杠不受影响。
        assert_eq!(quote_arg(r"C:\a b\c"), r#""C:\a b\c""#);
        // 没有空格就不加引号。
        assert_eq!(quote_arg(r"C:\ab\c"), r"C:\ab\c");
        // 内嵌引号要转义，它前面的反斜杠要翻倍。
        assert_eq!(quote_arg(r#"a\"b"#), r#""a\\\"b""#);
        // 空串必须留下一对引号，否则整个参数会消失。
        assert_eq!(quote_arg(""), r#""""#);
    }

    #[test]
    fn 内置账户名大小写与短写都认() {
        assert_eq!(normalize_account("LocalSystem"), Some(LOCAL_SYSTEM));
        assert_eq!(normalize_account("localsystem"), Some(LOCAL_SYSTEM));
        assert_eq!(
            normalize_account(r"NT AUTHORITY\SYSTEM"),
            Some(LOCAL_SYSTEM)
        );
        assert_eq!(
            normalize_account("NetworkService"),
            Some(r"NT AUTHORITY\NetworkService")
        );
        assert_eq!(
            normalize_account(r"nt authority\localservice"),
            Some(r"NT AUTHORITY\LocalService")
        );
        assert_eq!(normalize_account("  LocalSystem  "), Some(LOCAL_SYSTEM));
    }

    #[test]
    fn 需要口令的账户一律拒绝() {
        assert_eq!(normalize_account(r"CONTOSO\svc_strixmaid"), None);
        assert_eq!(normalize_account("Administrator"), None);
        assert_eq!(normalize_account(""), None);
    }

    #[test]
    fn 恢复策略的三次重启间隔递增() {
        assert!(
            RESTART_DELAYS_MS.windows(2).all(|w| w[0] < w[1]),
            "间隔应当递增，避免服务反复瞬间重启：{RESTART_DELAYS_MS:?}"
        );
        assert!(
            FAILURE_RESET_SECS > RESTART_DELAYS_MS[2] / 1000,
            "清零周期必须长于最后一次重启间隔，否则计数永远归零、第三次重启用不上"
        );
    }
}
