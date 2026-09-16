//! SCM 的原始数值 → DTO 的映射。**全是纯函数**，因此全部能用固定输入做单元测试。
//!
//! 把这一层单独拆出来的理由与 `providers/service/mod.rs` 把 `apply_list_query`
//! 抽出来一样：四条实现路径必须产出同一套 DTO，映射规则是最容易悄悄跑偏的地方，
//! 而它恰好完全不需要真机就能验证。

use strixmaid_types::service::{UnitActiveState, UnitEnableState, UnitLoadState};
use strixmaid_types::{ApiError, ApiResult};
use windows_sys::Win32::Foundation::{ERROR_SERVICE_NEVER_STARTED, ERROR_SERVICE_SPECIFIC_ERROR};
use windows_sys::Win32::System::Services::{
    SERVICE_AUTO_START, SERVICE_BOOT_START, SERVICE_CONTINUE_PENDING, SERVICE_DEMAND_START,
    SERVICE_DISABLED, SERVICE_PAUSE_PENDING, SERVICE_PAUSED, SERVICE_RUNNING, SERVICE_START_PENDING,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_SYSTEM_START,
};

use crate::platform::windows::wide::from_wide;

/// 对外暴露的 unit 名后缀，见 [`super`] 的模块文档。
pub const UNIT_SUFFIX: &str = ".service";

/// 加载顺序组前缀（SDK 里的 `SC_GROUP_IDENTIFIERW`）。
///
/// windows-sys 没有导出这个常量（它在 winsvc.h 里是一个宏），这里照原值声明。
pub const GROUP_PREFIX: char = '+';

/// 服务在注册表里的键路径（相对 `HKEY_LOCAL_MACHINE`）。
pub const SERVICES_KEY: &str = r"SYSTEM\CurrentControlSet\Services";

/// 服务名 → 对外的 unit 名（补 `.service` 后缀）。
pub fn to_unit_name(service: &str) -> String {
    format!("{service}{UNIT_SUFFIX}")
}

/// 对外的 unit 名 → 服务名（剥掉 `.service` 后缀）。
///
/// 已经没有后缀时原样返回：调用方（API 路径参数）偶尔会直接给服务名，
/// 对它报 400 只会制造无谓的摩擦。
pub fn to_service_name(unit: &str) -> String {
    unit.strip_suffix(UNIT_SUFFIX).unwrap_or(unit).to_owned()
}

/// unit 名校验。
///
/// 与 launchd 侧同理，不用共享的 [`crate::providers::service::validate_unit_name`]：
/// 那套字符集按 systemd 的 unit 名设计，而 Windows 服务名允许空格
/// （`Net Driver HPZ12`）、括号（`Intel(R) ...`）这些它不认的字符——按那套规则，
/// 列表里能看到的服务点开详情却报 400。
///
/// 这里只拦真正会出问题的：空 / 超长（服务名上限 256 字符）、控制字符、
/// 路径分隔符（`\` 与 `/` 是服务名的非法字符，出现即意味着调用方在拼路径）。
pub fn validate_service_unit(unit: &str) -> ApiResult<()> {
    if unit.is_empty() || unit.chars().count() > 256 + UNIT_SUFFIX.len() {
        return Err(ApiError::invalid_request(format!(
            "unit 名为空或过长: {unit}"
        )));
    }
    if unit
        .chars()
        .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(ApiError::invalid_request(format!(
            "unit 名含非法字符（服务名不允许路径分隔符与控制字符）: {unit}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 运行状态
// ---------------------------------------------------------------------------

/// `dwCurrentState` + `dwWin32ExitCode` → (`ActiveState`, `sub_state`)。
///
/// | SCM | DTO | sub_state | 理由 |
/// |---|---|---|---|
/// | `RUNNING` | `Active` | `running` | 直接对应 |
/// | `STOPPED` + 退出码 0 | `Inactive` | `dead` | 正常停止 |
/// | `STOPPED` + 退出码 1077 | `Inactive` | `dead` | 1077 = 本次开机以来**从未尝试启动**，不是失败 |
/// | `STOPPED` + 其它退出码 | `Failed` | `exited(N)` | 与 launchd 侧同一写法，前端不必区分平台 |
/// | `START_PENDING` | `Activating` | `start` | |
/// | `STOP_PENDING` | `Deactivating` | `stop` | |
/// | `PAUSED` | `Active` | `paused` | 进程还在、还占着资源，归 `Inactive` 会让「已停止」的统计虚高 |
/// | `PAUSE_PENDING` / `CONTINUE_PENDING` | `Reloading` | `pause-pending` / `continue-pending` | 见下 |
///
/// 暂停/恢复的两个 pending 态选 `Reloading` 而不是 `Activating` / `Deactivating`：
/// systemd 的 `Reloading` 语义是「仍然 active，正在做一次内部状态切换」，
/// 这正是 SCM 这两个态的处境——服务进程没有起也没有停。若报成
/// `Deactivating`，前端的「正在停止」提示会出现在一次纯粹的暂停上。
pub fn state_of(current_state: u32, win32_exit_code: u32) -> (UnitActiveState, String) {
    match current_state {
        SERVICE_RUNNING => (UnitActiveState::Active, "running".to_owned()),
        SERVICE_STOPPED => {
            if win32_exit_code == 0 || win32_exit_code == ERROR_SERVICE_NEVER_STARTED {
                (UnitActiveState::Inactive, "dead".to_owned())
            } else {
                (
                    UnitActiveState::Failed,
                    format!("exited({win32_exit_code})"),
                )
            }
        }
        SERVICE_START_PENDING => (UnitActiveState::Activating, "start".to_owned()),
        SERVICE_STOP_PENDING => (UnitActiveState::Deactivating, "stop".to_owned()),
        SERVICE_PAUSED => (UnitActiveState::Active, "paused".to_owned()),
        SERVICE_PAUSE_PENDING => (UnitActiveState::Reloading, "pause-pending".to_owned()),
        SERVICE_CONTINUE_PENDING => (UnitActiveState::Reloading, "continue-pending".to_owned()),
        other => (UnitActiveState::Unknown, format!("unknown({other})")),
    }
}

/// 上次运行结果，对齐 systemd 的 `Result` 取值（`success` / `exit-code` / …）。
///
/// 只在**已停止**时给得出来：服务运行中 `dwWin32ExitCode` 恒为 0，那 0 是
/// 「当前没有错误」而不是「上次以 0 退出」，报成 `success` 就是编数据。
/// 从未启动过（1077）同样给 `None`。
pub fn result_of(current_state: u32, win32_exit_code: u32) -> Option<String> {
    if current_state != SERVICE_STOPPED || win32_exit_code == ERROR_SERVICE_NEVER_STARTED {
        return None;
    }
    Some(if win32_exit_code == 0 {
        "success".to_owned()
    } else {
        "exit-code".to_owned()
    })
}

/// 退出码。
///
/// `dwWin32ExitCode == ERROR_SERVICE_SPECIFIC_ERROR`（1066）是 SCM 约定的
/// 「真正的错误码在 `dwServiceSpecificExitCode` 里」，此时报后者；否则报前者。
/// 与 [`result_of`] 同样只在已停止且曾经启动过时给值。
pub fn exit_code_of(
    current_state: u32,
    win32_exit_code: u32,
    specific_exit_code: u32,
) -> Option<i32> {
    if current_state != SERVICE_STOPPED || win32_exit_code == ERROR_SERVICE_NEVER_STARTED {
        return None;
    }
    let code = if win32_exit_code == ERROR_SERVICE_SPECIFIC_ERROR {
        specific_exit_code
    } else {
        win32_exit_code
    };
    Some(code as i32)
}

// ---------------------------------------------------------------------------
// 启动类型
// ---------------------------------------------------------------------------

/// `dwStartType` → [`UnitEnableState`]。
///
/// | SCM | DTO | 理由 |
/// |---|---|---|
/// | `BOOT_START`(0) / `SYSTEM_START`(1) | `Static` | 内核驱动与引导期组件，SCM 不允许改它们的启动类型，正对应 systemd 里没有 `[Install]` 段的 unit |
/// | `AUTO_START`(2) | `Enabled` | 开机自启 |
/// | `DEMAND_START`(3) | `Disabled` | 不自启，但可以手动 / 被依赖拉起——systemd 的 `disabled` 正是这个意思 |
/// | `DISABLED`(4) | **`Masked`** | 连手动都起不来（`StartService` 直接返回 `ERROR_SERVICE_DISABLED`），这正是 systemd `mask` 的语义 |
///
/// 把 `SERVICE_DISABLED` 映射成 `Disabled` 是本模块最容易犯的错：那样一来
/// 前端的「启用」按钮会以为一次 enable 就能救活它，而实际上 Windows 的
/// Disabled 是硬拦——必须先改回 `DEMAND_START`（= unmask）。
pub fn enable_state_of(start_type: u32) -> Option<UnitEnableState> {
    Some(match start_type {
        SERVICE_BOOT_START | SERVICE_SYSTEM_START => UnitEnableState::Static,
        SERVICE_AUTO_START => UnitEnableState::Enabled,
        SERVICE_DEMAND_START => UnitEnableState::Disabled,
        SERVICE_DISABLED => UnitEnableState::Masked,
        _ => return None,
    })
}

/// 加载状态。SCM 里的服务只要能枚举出来就是「已加载」，
/// 唯一的例外是被 `SERVICE_DISABLED` 的——与 systemd 对 masked unit 报
/// `LoadState=masked` 一致（见 `summary_for_unloaded_file` 的同款处理）。
pub fn load_state_of(start_type: u32) -> UnitLoadState {
    if start_type == SERVICE_DISABLED {
        UnitLoadState::Masked
    } else {
        UnitLoadState::Loaded
    }
}

/// 该启动类型能否被 enable / disable。
///
/// `BOOT_START` / `SYSTEM_START` 是驱动的档位，`ChangeServiceConfigW` 对它们
/// 要么直接失败、要么会把驱动降级成普通服务导致下次开机起不来。
/// 因此在发起调用前就拦下，报法对齐 systemd 对 `static` unit 的拒绝。
pub fn is_configurable(start_type: u32) -> bool {
    !matches!(start_type, SERVICE_BOOT_START | SERVICE_SYSTEM_START)
}

// ---------------------------------------------------------------------------
// 依赖
// ---------------------------------------------------------------------------

/// 解析 `QUERY_SERVICE_CONFIGW::lpDependencies`。
///
/// 格式是「双 NUL 结尾的 NUL 分隔串表」，其中**以 `+` 开头的条目是加载顺序组，
/// 不是服务**。返回 `(服务名表, 组名表)`，组名已剥掉 `+`。
///
/// 为什么要分开：组不是可枚举、可查询、可操作的对象，把 `+TDI` 混进服务名表里，
/// 前端会给它生成一个点进去必然 404 的详情链接。
pub fn parse_dependencies(buf: &[u16]) -> (Vec<String>, Vec<String>) {
    let mut services = Vec::new();
    let mut groups = Vec::new();
    let mut start = 0usize;
    for (i, c) in buf.iter().enumerate() {
        if *c != 0 {
            continue;
        }
        if i == start {
            // 空串 = 表结束（双 NUL）。
            break;
        }
        let item = from_wide(&buf[start..i]);
        start = i + 1;
        match item.strip_prefix(GROUP_PREFIX) {
            Some(g) if !g.is_empty() => groups.push(g.to_owned()),
            // 只有一个 `+`：SCM 不会产出这种条目，出现了也当垃圾丢掉。
            Some(_) => {}
            None => services.push(item),
        }
    }
    (services, groups)
}

// ---------------------------------------------------------------------------
// 注册表路径
// ---------------------------------------------------------------------------

/// 服务的注册表键路径（相对 HKLM）：`SYSTEM\CurrentControlSet\Services\<名字>`。
pub fn service_subkey(service: &str) -> String {
    format!(r"{SERVICES_KEY}\{service}")
}

/// 同上，但带 `HKEY_LOCAL_MACHINE` 前缀——这是对外展示的「unit 文件路径」。
pub fn service_key_display(service: &str) -> String {
    format!(r"HKEY_LOCAL_MACHINE\{}", service_subkey(service))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一组字符串编码成「双 NUL 结尾的多串」，模拟 SCM 回填的缓冲。
    fn multi_sz(items: &[&str]) -> Vec<u16> {
        let mut buf: Vec<u16> = Vec::new();
        for s in items {
            buf.extend(s.encode_utf16());
            buf.push(0);
        }
        buf.push(0);
        buf
    }

    #[test]
    fn 名字加后缀与剥后缀是可逆的() {
        assert_eq!(to_unit_name("Spooler"), "Spooler.service");
        assert_eq!(to_service_name("Spooler.service"), "Spooler");
        // 已经没有后缀时不该再剥一层
        assert_eq!(to_service_name("Spooler"), "Spooler");
        // 加了后缀之后 unit_type 才是有意义的值
        assert_eq!(
            crate::providers::service::unit_type_of(&to_unit_name("Spooler")),
            "service"
        );
        // 带点的服务名（NVIDIA / Intel 的一大堆都长这样）不能被误剥
        assert_eq!(
            to_service_name("NVDisplay.ContainerLocalSystem.service"),
            "NVDisplay.ContainerLocalSystem"
        );
    }

    #[test]
    fn unit_名校验比_systemd_宽松但仍拦住路径() {
        assert!(validate_service_unit("Spooler.service").is_ok());
        // 真实存在的带空格 / 括号的服务名，systemd 的字符集会误杀它们
        assert!(validate_service_unit("Net Driver HPZ12.service").is_ok());
        assert!(validate_service_unit("Intel(R) TPM Provisioning.service").is_ok());
        assert!(validate_service_unit("").is_err());
        assert!(validate_service_unit(r"..\..\evil.service").is_err());
        assert!(validate_service_unit("a/b.service").is_err());
        assert!(validate_service_unit("bad\u{7}name.service").is_err());
        assert!(validate_service_unit(&"x".repeat(400)).is_err());
    }

    #[test]
    fn 运行状态映射() {
        assert_eq!(
            state_of(SERVICE_RUNNING, 0),
            (UnitActiveState::Active, "running".to_owned())
        );
        // 正常停止
        assert_eq!(
            state_of(SERVICE_STOPPED, 0),
            (UnitActiveState::Inactive, "dead".to_owned())
        );
        // 1077 = 本次开机以来从未启动过，是常态不是失败
        assert_eq!(
            state_of(SERVICE_STOPPED, ERROR_SERVICE_NEVER_STARTED),
            (UnitActiveState::Inactive, "dead".to_owned())
        );
        // 其它非零退出码才是失败
        assert_eq!(
            state_of(SERVICE_STOPPED, 1053),
            (UnitActiveState::Failed, "exited(1053)".to_owned())
        );
        assert_eq!(state_of(SERVICE_START_PENDING, 0).0, UnitActiveState::Activating);
        assert_eq!(state_of(SERVICE_STOP_PENDING, 0).0, UnitActiveState::Deactivating);
        // 暂停中的服务进程还在，算 active
        assert_eq!(
            state_of(SERVICE_PAUSED, 0),
            (UnitActiveState::Active, "paused".to_owned())
        );
        assert_eq!(state_of(SERVICE_PAUSE_PENDING, 0).0, UnitActiveState::Reloading);
        assert_eq!(
            state_of(SERVICE_CONTINUE_PENDING, 0).0,
            UnitActiveState::Reloading
        );
        // 未知取值不能冒充成任何已知态
        let (st, sub) = state_of(99, 0);
        assert_eq!(st, UnitActiveState::Unknown);
        assert_eq!(sub, "unknown(99)");
    }

    #[test]
    fn 退出码与运行结果只在停止后才给() {
        // 运行中：dwWin32ExitCode 的 0 不是「上次成功退出」
        assert_eq!(result_of(SERVICE_RUNNING, 0), None);
        assert_eq!(exit_code_of(SERVICE_RUNNING, 0, 0), None);
        // 从未启动过
        assert_eq!(result_of(SERVICE_STOPPED, ERROR_SERVICE_NEVER_STARTED), None);
        assert_eq!(
            exit_code_of(SERVICE_STOPPED, ERROR_SERVICE_NEVER_STARTED, 0),
            None
        );
        // 正常停止
        assert_eq!(result_of(SERVICE_STOPPED, 0).as_deref(), Some("success"));
        assert_eq!(exit_code_of(SERVICE_STOPPED, 0, 0), Some(0));
        // 普通 Win32 错误
        assert_eq!(result_of(SERVICE_STOPPED, 1053).as_deref(), Some("exit-code"));
        assert_eq!(exit_code_of(SERVICE_STOPPED, 1053, 0), Some(1053));
        // 服务自定义错误：真正的码在 dwServiceSpecificExitCode 里
        assert_eq!(
            exit_code_of(SERVICE_STOPPED, ERROR_SERVICE_SPECIFIC_ERROR, 42),
            Some(42)
        );
    }

    #[test]
    fn 启动类型映射() {
        assert_eq!(
            enable_state_of(SERVICE_AUTO_START),
            Some(UnitEnableState::Enabled)
        );
        assert_eq!(
            enable_state_of(SERVICE_DEMAND_START),
            Some(UnitEnableState::Disabled)
        );
        // Windows 的 Disabled = systemd 的 masked，不是 disabled
        assert_eq!(
            enable_state_of(SERVICE_DISABLED),
            Some(UnitEnableState::Masked)
        );
        assert_eq!(
            enable_state_of(SERVICE_BOOT_START),
            Some(UnitEnableState::Static)
        );
        assert_eq!(
            enable_state_of(SERVICE_SYSTEM_START),
            Some(UnitEnableState::Static)
        );
        // 未知取值 → 不知道，而不是随便挑一个
        assert_eq!(enable_state_of(9), None);

        assert_eq!(load_state_of(SERVICE_DISABLED), UnitLoadState::Masked);
        assert_eq!(load_state_of(SERVICE_AUTO_START), UnitLoadState::Loaded);

        assert!(!is_configurable(SERVICE_BOOT_START));
        assert!(!is_configurable(SERVICE_SYSTEM_START));
        assert!(is_configurable(SERVICE_AUTO_START));
        assert!(is_configurable(SERVICE_DISABLED));
    }

    #[test]
    fn 依赖串解析_双nul多串与组前缀() {
        // 纯服务
        let (svc, grp) = parse_dependencies(&multi_sz(&["RpcSs", "http"]));
        assert_eq!(svc, vec!["RpcSs", "http"]);
        assert!(grp.is_empty());

        // 服务 + 组混排（Tcpip 这类驱动的真实形状）
        let (svc, grp) = parse_dependencies(&multi_sz(&["+TDI", "RpcSs", "+PNP_TDI", "nsi"]));
        assert_eq!(svc, vec!["RpcSs", "nsi"]);
        assert_eq!(grp, vec!["TDI", "PNP_TDI"], "组名要剥掉 + 前缀");

        // 没有依赖：SCM 给的是空指针，我们拷出来就是空表
        assert_eq!(parse_dependencies(&[]), (Vec::new(), Vec::new()));
        assert_eq!(parse_dependencies(&[0, 0]), (Vec::new(), Vec::new()));

        // 结尾少一个 NUL 也不能读越界或丢条目
        let mut 半截 = multi_sz(&["RpcSs"]);
        半截.pop();
        assert_eq!(parse_dependencies(&半截).0, vec!["RpcSs"]);

        // 光一个 + 不是组名，丢掉
        let (svc, grp) = parse_dependencies(&multi_sz(&["+"]));
        assert!(svc.is_empty() && grp.is_empty());
    }

    #[test]
    fn 注册表键路径() {
        assert_eq!(
            service_subkey("Spooler"),
            r"SYSTEM\CurrentControlSet\Services\Spooler"
        );
        assert_eq!(
            service_key_display("Spooler"),
            r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\Spooler"
        );
    }
}
