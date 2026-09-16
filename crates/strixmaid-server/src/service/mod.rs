//! Windows 服务（SCM）支持：注册、启停、查询，以及由 SCM 拉起时的运行宿主。
//!
//! # 为什么必须把这一层写进二进制
//!
//! Unix 上 StrixMaid 由 systemd 托管（`packaging/strixmaid.service`）：开机自启、
//! 崩溃重启、日志归集、关停信号全部由 init 负责，二进制本身只要「前台跑、
//! 收到 SIGTERM 就退」，对 systemd 一无所知也能工作。
//!
//! Windows 没有这种「对被托管者透明」的托管者。要让一个进程在**没有用户登录**
//! 时就随系统启动、并具备状态查询与依赖管理，唯一的机制是服务控制管理器（SCM），
//! 而 SCM 要求进程主动实现一套协议：
//!
//! 1. 进程启动后必须在 30 秒内调用 `StartServiceCtrlDispatcherW` 连回 SCM，
//!    否则被判定为「服务未能及时启动」并杀掉；
//! 2. 必须注册控制处理器（`RegisterServiceCtrlHandlerExW`）接收停止/关机请求；
//! 3. 启动与停止的全过程都要用 `SetServiceStatus` 持续上报**递增的**
//!    `dwCheckPoint`，SCM 靠它区分「还在启动」与「卡死了」。
//!
//! 这三件事都在进程内部，没有任何外部包装器能代劳（`srvany`、`nssm` 之类的
//! 第三方壳子是在替进程实现它，代价是多一个进程、多一层状态失真）。所以有了
//! 本模块。
//!
//! # 子模块分工
//!
//! | 模块 | 方向 | 内容 |
//! |---|---|---|
//! | [`host`] | SCM → 本进程 | `service run`：分派线程、控制处理器、状态上报 |
//! | [`scm`] | 本进程 → SCM | `install` / `uninstall` / `start` / `stop` / `status` |
//! | [`logging`] | —— | 服务模式下的日志落地（没有 stderr 可写） |
//!
//! # 权限
//!
//! 除 `status`（只读）之外的管理动作都要求**已提升**的令牌。这里在动手前先自查
//! 一次并给出中文提示，理由见 [`require_admin`]。

pub mod host;
pub mod logging;
pub mod scm;

use anyhow::bail;

use crate::cli::{GlobalArgs, ServiceAction};

/// SCM 里的服务名（键名）。
///
/// 这一个常量同时决定三处：`CreateServiceW` 注册用的名字、
/// `StartServiceCtrlDispatcherW` 的服务表项、以及注册表下
/// `HKLM\SYSTEM\CurrentControlSet\Services\<名字>`。三处必须一致，
/// 因此只在这里定义一次。
pub const SERVICE_NAME: &str = "StrixMaid";

/// 服务管理单元里显示的名字。
pub const DISPLAY_NAME: &str = "StrixMaid 服务器管理面板";

/// 服务描述，显示在服务管理单元的「描述」列。
pub const DESCRIPTION: &str = "StrixMaid：轻量、通用、现代化的服务器观测与管理平台。提供 HTTP/WebSocket 控制面，\
     按登录用户身份派生 worker 执行管理操作。";

/// 分派 `service` 子命令。
///
/// 由 `main` 在**进程主线程**上直接调用，不经过 tokio 运行时——`Run` 那一支要把
/// 主线程交给 `StartServiceCtrlDispatcherW`，详见 [`host`] 的模块文档。
pub fn dispatch(action: &ServiceAction, global: &GlobalArgs) -> anyhow::Result<()> {
    match action {
        ServiceAction::Run => host::run(global),
        ServiceAction::Install(args) => {
            require_admin("install")?;
            scm::install(args, global)
        }
        ServiceAction::Uninstall => {
            require_admin("uninstall")?;
            scm::uninstall()
        }
        ServiceAction::Start => {
            require_admin("start")?;
            scm::start()
        }
        ServiceAction::Stop => {
            require_admin("stop")?;
            scm::stop()
        }
        // 查询只要 `SC_MANAGER_CONNECT` + `SERVICE_QUERY_STATUS`，普通用户就有。
        ServiceAction::Status => scm::status(),
    }
}

/// 令牌未提升时提前拦下，给一句能照着做的话。
///
/// 这**不是**安全边界：真正的检查在内核里——没有管理员权限时
/// `OpenSCManagerW(SC_MANAGER_CREATE_SERVICE)` 会返回 `ERROR_ACCESS_DENIED`，
/// 服务照样装不上。提前自查的唯一目的是错误信息：`ERROR_ACCESS_DENIED` 经
/// `io::Error` 格式化出来是「拒绝访问。 (os error 5)」，看到它的人无从知道
/// 问题出在没用管理员命令提示符，还是出在组策略、还是出在服务名被占用。
fn require_admin(what: &str) -> anyhow::Result<()> {
    if strixmaid_core::platform::windows::is_elevated() {
        return Ok(());
    }
    bail!(
        "`strixmaid service {what}` 需要管理员权限。\
         请在「管理员命令提示符」或管理员 PowerShell 中重新运行；\
         若当前账户不在 Administrators 组内，先换用管理员账户。"
    );
}

/// 把 SCM 的 `dwCurrentState` 翻成中文。
///
/// 独立成函数是为了能单测：这七个取值是 SCM 状态机的全部，漏一个就会在
/// `status` 的输出里变成一个没有意义的数字。
pub fn state_text(state: u32) -> &'static str {
    use windows_sys::Win32::System::Services::{
        SERVICE_CONTINUE_PENDING, SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_RUNNING,
        SERVICE_START_PENDING, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    };

    match state {
        SERVICE_STOPPED => "已停止",
        SERVICE_START_PENDING => "正在启动",
        SERVICE_STOP_PENDING => "正在停止",
        SERVICE_RUNNING => "正在运行",
        SERVICE_CONTINUE_PENDING => "正在恢复",
        SERVICE_PAUSE_PENDING => "正在暂停",
        SERVICE_PAUSED => "已暂停",
        _ => "未知状态",
    }
}

/// 把与 SCM 有关的 Win32 错误码翻成「该怎么办」。
///
/// 只覆盖这几个真正会遇到、且裸错误码看不出所以然的码。其余一律回落到
/// `io::Error` 的系统文本——瞎猜一个解释比没有解释更糟。
pub fn explain(code: u32) -> String {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FAILED_SERVICE_CONTROLLER_CONNECT,
        ERROR_SERVICE_ALREADY_RUNNING, ERROR_SERVICE_DOES_NOT_EXIST, ERROR_SERVICE_EXISTS,
        ERROR_SERVICE_MARKED_FOR_DELETE, ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_REQUEST_TIMEOUT,
        ERROR_SERVICE_SPECIFIC_ERROR,
    };

    let hint = match code {
        ERROR_ACCESS_DENIED => "权限不足，请在管理员命令提示符下运行",
        ERROR_SERVICE_DOES_NOT_EXIST => "服务尚未注册，先运行 `strixmaid service install`",
        ERROR_SERVICE_EXISTS => "服务已注册，若要改配置请先 `strixmaid service uninstall`",
        ERROR_SERVICE_ALREADY_RUNNING => "服务已在运行",
        ERROR_SERVICE_NOT_ACTIVE => "服务当前未运行",
        ERROR_SERVICE_MARKED_FOR_DELETE => "服务已标记为待删除，等它完全停止（或重启机器）后再注册",
        ERROR_SERVICE_REQUEST_TIMEOUT => "服务未在规定时间内响应控制请求",
        ERROR_FAILED_SERVICE_CONTROLLER_CONNECT => {
            "本进程不是由 SCM 拉起的；前台运行请用 `strixmaid serve`"
        }
        // 这一个码的含义就是「具体原因不在 Win32 错误空间里」，SCM 只转述了
        // 服务自报的退出码。真正的原因只在日志里。
        ERROR_SERVICE_SPECIFIC_ERROR => "服务自身报告了启动/运行失败，具体原因见日志",
        _ => return format!("{}", std::io::Error::from_raw_os_error(code as i32)),
    };
    format!("{hint}（Win32 错误 {code}）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SERVICE_DOES_NOT_EXIST};
    use windows_sys::Win32::System::Services::{
        SERVICE_CONTINUE_PENDING, SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_RUNNING,
        SERVICE_START_PENDING, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    };

    #[test]
    fn 七个状态码各有各的译名() {
        let all = [
            SERVICE_STOPPED,
            SERVICE_START_PENDING,
            SERVICE_STOP_PENDING,
            SERVICE_RUNNING,
            SERVICE_CONTINUE_PENDING,
            SERVICE_PAUSE_PENDING,
            SERVICE_PAUSED,
        ];
        let mut seen: Vec<&str> = all.iter().map(|s| state_text(*s)).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "译名不该重复：{seen:?}");
        assert!(
            !seen.contains(&"未知状态"),
            "已知状态码不该落到兜底分支：{seen:?}"
        );
        assert_eq!(state_text(u32::MAX), "未知状态", "未知码走兜底");
    }

    #[test]
    fn 常见错误码给出可执行的建议() {
        let denied = explain(ERROR_ACCESS_DENIED);
        assert!(denied.contains("管理员"), "{denied}");
        assert!(denied.contains("5"), "应当带上原始错误码：{denied}");

        let missing = explain(ERROR_SERVICE_DOES_NOT_EXIST);
        assert!(missing.contains("install"), "{missing}");
    }

    #[test]
    fn 未覆盖的错误码回落到系统文本而不是瞎猜() {
        // 2 = ERROR_FILE_NOT_FOUND，与 SCM 无关，不该被安上一个 SCM 解释。
        let text = explain(2);
        assert!(!text.contains("服务"), "不该编造 SCM 相关的解释：{text}");
        assert!(!text.is_empty());
    }

    #[test]
    fn 非管理员时提示写明该怎么做() {
        if strixmaid_core::platform::windows::is_elevated() {
            eprintln!("当前进程已提升，跳过：这条断言只在未提升的令牌下有意义");
            return;
        }
        let err = require_admin("install").expect_err("未提升时应当报错");
        let text = err.to_string();
        assert!(text.contains("管理员"), "{text}");
        assert!(text.contains("service install"), "{text}");
    }

    #[test]
    fn 服务名与显示名不为空且不含分隔符() {
        assert!(!SERVICE_NAME.is_empty());
        // SCM 的服务名不允许含正斜杠与反斜杠。
        assert!(!SERVICE_NAME.contains(['/', '\\']), "{SERVICE_NAME}");
        assert!(SERVICE_NAME.len() < 256, "SCM 的服务名上限是 256 个字符");
        assert!(!DISPLAY_NAME.is_empty());
        assert!(!DESCRIPTION.is_empty());
    }
}
