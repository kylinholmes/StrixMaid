//! 服务控制管理器（SCM）的 FFI 薄封装：句柄 RAII + 每个 Win32 调用一个安全函数。
//!
//! 这一层与 [`crate::platform::windows`] 同一定位——**不含任何业务判断**。
//! 它只做四件事：
//!
//! 1. 把「先问长度、再取数据」的两段式调用收进函数内部（`EnumServicesStatusExW`、
//!    `QueryServiceConfigW`、`QueryServiceConfig2W`、`EnumDependentServicesW`
//!    全是这个形状，且第一次调用必然「失败」并置 `ERROR_MORE_DATA` /
//!    `ERROR_INSUFFICIENT_BUFFER`）；
//! 2. 把 Win32 结构体里的 `PWSTR` 在缓冲还活着的时候转成 `String`——
//!    那些指针指向缓冲**尾部**，缓冲一释放就全是悬垂指针；
//! 3. 把失败统一转成 [`io::Error`]，错误码由调用方按 SCM 的语义解读
//!    （见 [`super::map`] 与 `super::mod` 的错误映射）；
//! 4. 用 [`ScHandle`] 保证 `CloseServiceHandle` 一定被调到。
//!
//! # 为什么不用 `platform::windows::handle::Owned`
//!
//! `Owned` 的析构是 `CloseHandle`，而 SCM 句柄必须用 `CloseServiceHandle` 关闭
//! （两者是不同的对象表，混用是未定义行为）。另一处差别是「失败」的表示：
//! `OpenSCManagerW` / `OpenServiceW` 失败返回**空**句柄，不返回
//! `INVALID_HANDLE_VALUE`，所以 `Owned::new` 对 `-1` 的那一道防线在这里用不上。
//!
//! # 对齐
//!
//! 几个枚举 API 要求调用方给一段裸字节缓冲，内核在里面摆放含指针的结构体。
//! 用 `Vec<u8>` 装它只有 1 字节对齐，在 x64 上把它当
//! `ENUM_SERVICE_STATUS_PROCESSW*` 解引用是未对齐访问。因此这里一律用
//! `Vec<u64>` 申请（8 字节对齐），再按字节数换算长度。

use std::io;

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, ENUM_SERVICE_STATUS_PROCESSW,
    ENUM_SERVICE_STATUSW, EnumDependentServicesW, EnumServicesStatusExW, OpenSCManagerW,
    OpenServiceW, QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW,
    QueryServiceStatusEx, SC_ENUM_PROCESS_INFO, SC_HANDLE, SC_STATUS_PROCESS_INFO,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DESCRIPTIONW, SERVICE_NO_CHANGE, SERVICE_STATE_ALL,
    SERVICE_STATUS, SERVICE_STATUS_PROCESS, SERVICE_WIN32, StartServiceW,
};

use crate::platform::windows::wide::{from_wide_ptr, to_wide};
use crate::platform::windows::{error_from_code, last_error};

/// 枚举缓冲的初始大小（字节）。本机上 `SERVICE_WIN32` 一般在 300 条上下，
/// 一条约 100 字节（结构体 + 两个名字），64 KiB 通常一轮就够；
/// 不够时按内核回填的 `pcbBytesNeeded` 扩容重试。
const ENUM_BUF_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// 句柄
// ---------------------------------------------------------------------------

/// SCM / 服务句柄，析构时 `CloseServiceHandle`。
#[derive(Debug)]
pub struct ScHandle(SC_HANDLE);

// SAFETY: SC_HANDLE 是进程内的对象编号，跨线程使用是 Win32 的常规做法；
// 本类型所有权唯一、无内部可变性。
unsafe impl Send for ScHandle {}
// SAFETY: 同上；`&ScHandle` 只能读出句柄值，真正的并发安全由 SCM 自身保证。
unsafe impl Sync for ScHandle {}

impl ScHandle {
    /// 接管一个刚由 SCM API 返回的句柄。**空**句柄视为失败
    /// （SCM 系列不用 `INVALID_HANDLE_VALUE` 表示失败），此时取当前错误码。
    ///
    /// # Safety
    ///
    /// `h` 必须是刚由 `OpenSCManagerW` / `OpenServiceW` 返回、尚无其它持有者的句柄。
    unsafe fn new(h: SC_HANDLE) -> io::Result<ScHandle> {
        if h.is_null() {
            return Err(last_error());
        }
        Ok(ScHandle(h))
    }

    /// 裸句柄，只在调用期间借用。
    pub fn raw(&self) -> SC_HANDLE {
        self.0
    }
}

impl Drop for ScHandle {
    fn drop(&mut self) {
        // SAFETY: 构造成功才有实例，所有权唯一，只关这一次。
        unsafe {
            CloseServiceHandle(self.0);
        }
    }
}

/// 取最近一次 Win32 错误码。`last_error()` 返回的 `io::Error` 一定带 raw code，
/// 取不到时按 `ERROR_SUCCESS` 处理（调用方会因此报一个「未知原因」的失败，
/// 而不是把 0 当成成功）。
pub fn last_code() -> WIN32_ERROR {
    last_error().raw_os_error().unwrap_or(0) as WIN32_ERROR
}

// ---------------------------------------------------------------------------
// 打开
// ---------------------------------------------------------------------------

/// 打开本机 SCM。`access` 按需申请，不要一上来就 `SC_MANAGER_ALL_ACCESS`——
/// 那会让非管理员连服务列表都拉不出来。
pub fn open_manager(access: u32) -> io::Result<ScHandle> {
    // SAFETY: 前两个参数为空表示「本机、默认数据库（ServicesActive）」，
    // 这是文档规定的用法；返回值由 ScHandle::new 判空。
    unsafe { ScHandle::new(OpenSCManagerW(std::ptr::null(), std::ptr::null(), access)) }
}

/// 打开一个服务。`access` 同样按需申请（只读用
/// `SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS`，操作才加 `SERVICE_START` 等）。
pub fn open_service(mgr: &ScHandle, name: &str, access: u32) -> io::Result<ScHandle> {
    let w = to_wide(name);
    // SAFETY: mgr 在本次调用期间有效；w 是以 NUL 结尾的 UTF-16 缓冲。
    unsafe { ScHandle::new(OpenServiceW(mgr.raw(), w.as_ptr(), access)) }
}

// ---------------------------------------------------------------------------
// 取数：状态 / 配置 / 依赖
// ---------------------------------------------------------------------------

/// `SERVICE_STATUS_PROCESS` / `SERVICE_STATUS` 里我们用得到的那几个字段。
///
/// 刻意不把 windows-sys 的结构体往上传：[`super::map`] 的映射函数因此可以用
/// 固定输入做单元测试，不需要构造 Win32 类型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RawStatus {
    /// `dwServiceType`：`SERVICE_WIN32_OWN_PROCESS` 等。
    pub service_type: u32,
    /// `dwCurrentState`：`SERVICE_RUNNING` 等。
    pub current_state: u32,
    /// `dwControlsAccepted`：`SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_PARAMCHANGE …`。
    pub controls_accepted: u32,
    /// `dwWin32ExitCode`。服务正在运行时恒为 0；从未启动过时是
    /// `ERROR_SERVICE_NEVER_STARTED`（1077）。
    pub win32_exit_code: u32,
    /// `dwServiceSpecificExitCode`。仅当 `win32_exit_code` 是
    /// `ERROR_SERVICE_SPECIFIC_ERROR`（1066）时有意义。
    pub service_specific_exit_code: u32,
    /// `dwProcessId`。未运行时为 0。`SERVICE_STATUS`（`ControlService` 回填的那个）
    /// 没有这一项，此时恒为 0。
    pub process_id: u32,
}

impl RawStatus {
    fn from_process(s: &SERVICE_STATUS_PROCESS) -> RawStatus {
        RawStatus {
            service_type: s.dwServiceType,
            current_state: s.dwCurrentState,
            controls_accepted: s.dwControlsAccepted,
            win32_exit_code: s.dwWin32ExitCode,
            service_specific_exit_code: s.dwServiceSpecificExitCode,
            process_id: s.dwProcessId,
        }
    }

    fn from_status(s: &SERVICE_STATUS) -> RawStatus {
        RawStatus {
            service_type: s.dwServiceType,
            current_state: s.dwCurrentState,
            controls_accepted: s.dwControlsAccepted,
            win32_exit_code: s.dwWin32ExitCode,
            service_specific_exit_code: s.dwServiceSpecificExitCode,
            // ControlService 回填的是 SERVICE_STATUS，没有 pid 这一项。
            process_id: 0,
        }
    }
}

/// `EnumServicesStatusExW` 的一条结果。
#[derive(Debug, Clone)]
pub struct ServiceEntry {
    /// 服务名（注册表键名），大小写不敏感但原样保留。
    pub name: String,
    /// 显示名（`DisplayName`）。SCM 已经把 `@dll,-123` 这类间接字符串资源解析过了。
    pub display_name: String,
    /// 运行状态。
    pub status: RawStatus,
}

/// `QueryServiceConfigW` 的结果。
#[derive(Debug, Clone, Default)]
pub struct RawConfig {
    /// `dwServiceType`。
    pub service_type: u32,
    /// `dwStartType`：`SERVICE_BOOT_START`(0) … `SERVICE_DISABLED`(4)。
    pub start_type: u32,
    /// `lpBinaryPathName`：ImagePath，含命令行参数。
    pub binary_path: Option<String>,
    /// `lpLoadOrderGroup`：本服务**所属**的加载顺序组（不是它依赖的组）。
    pub load_order_group: Option<String>,
    /// `lpDependencies` 的原始双 NUL 多串，交给 [`super::map::parse_dependencies`] 解析。
    /// 在这里就转成 `Vec<String>` 会丢掉「哪些是组」的信息载体（`+` 前缀），
    /// 且让那段解析没法用固定输入测。
    pub dependencies: Vec<u16>,
    /// `lpServiceStartName`：启动身份（`LocalSystem` / `NT AUTHORITY\LocalService` /
    /// `.\Administrator`）。驱动的这一项是驱动对象名，不是账户。
    pub start_name: Option<String>,
    /// `lpDisplayName`。
    pub display_name: Option<String>,
}

/// 枚举全部 `SERVICE_WIN32` 服务（含已停止的）。
///
/// 两处易错点都在这里处理掉：
///
/// - **分批**：服务多于一次缓冲能装下时，函数返回 FALSE 且
///   `GetLastError() == ERROR_MORE_DATA`，同时回填 `lpResumeHandle`；
///   必须带着它继续枚举，否则只能拿到第一批。
/// - **一条都装不下**：此时 `returned == 0`，按内核给出的 `pcbBytesNeeded` 扩容重试，
///   不然会死循环。
pub fn enum_services(mgr: &ScHandle) -> io::Result<Vec<ServiceEntry>> {
    let mut out: Vec<ServiceEntry> = Vec::new();
    let mut resume: u32 = 0;
    let mut buf: Vec<u64> = vec![0; ENUM_BUF_BYTES / 8];

    loop {
        let mut needed: u32 = 0;
        let mut returned: u32 = 0;
        let cap = (buf.len() * 8) as u32;
        // SAFETY: buf 有 cap 字节可写且 8 字节对齐；三个输出参数都指向本栈帧上的
        // 可写变量；pszGroupName 为空表示「不按组过滤」。
        let ok = unsafe {
            EnumServicesStatusExW(
                mgr.raw(),
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                buf.as_mut_ptr().cast::<u8>(),
                cap,
                &raw mut needed,
                &raw mut returned,
                &raw mut resume,
                std::ptr::null(),
            )
        };
        if ok == 0 {
            let code = last_code();
            if code != ERROR_MORE_DATA {
                return Err(error_from_code(code));
            }
            if returned == 0 {
                // 缓冲连一条都放不下：按内核要的大小扩容，本轮不算数。
                buf.resize(needed as usize / 8 + 2, 0);
                continue;
            }
        }

        // SAFETY: 内核在 buf 头部连续摆放了 returned 个 ENUM_SERVICE_STATUS_PROCESSW，
        // 缓冲 8 字节对齐，且在本次借用期间不会被移动或释放。
        let items = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr().cast::<ENUM_SERVICE_STATUS_PROCESSW>(),
                returned as usize,
            )
        };
        for it in items {
            // SAFETY: 两个名字指针指向同一块缓冲的尾部，以 NUL 结尾，
            // 在 buf 存活期间有效；这里立刻拷成 String。
            let name = unsafe { from_wide_ptr(it.lpServiceName) };
            // SAFETY: 同上。
            let display_name = unsafe { from_wide_ptr(it.lpDisplayName) };
            if name.is_empty() {
                continue;
            }
            out.push(ServiceEntry {
                name,
                display_name,
                status: RawStatus::from_process(&it.ServiceStatusProcess),
            });
        }
        if ok != 0 {
            // 成功返回即「已经枚举完」。
            break;
        }
    }
    Ok(out)
}

/// `QueryServiceStatusEx(SC_STATUS_PROCESS_INFO)`：状态 + pid + 退出码。
pub fn query_status(svc: &ScHandle) -> io::Result<RawStatus> {
    let mut st = SERVICE_STATUS_PROCESS::default();
    let mut needed: u32 = 0;
    let size = std::mem::size_of::<SERVICE_STATUS_PROCESS>() as u32;
    // SAFETY: st 是一个完整的 SERVICE_STATUS_PROCESS，size 如实描述其大小；
    // needed 指向本栈帧上的可写变量。
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
        return Err(last_error());
    }
    Ok(RawStatus::from_process(&st))
}

/// `QueryServiceConfigW`：启动类型 / 二进制路径 / 启动身份 / 依赖。
pub fn query_config(svc: &ScHandle) -> io::Result<RawConfig> {
    let mut needed: u32 = 0;
    // 第一次只问长度：必然返回 FALSE + ERROR_INSUFFICIENT_BUFFER。
    // SAFETY: 缓冲为空、大小为 0 是文档规定的「问长度」用法。
    let ok = unsafe { QueryServiceConfigW(svc.raw(), std::ptr::null_mut(), 0, &raw mut needed) };
    if ok == 0 {
        let code = last_code();
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(error_from_code(code));
        }
    }
    let mut buf: Vec<u64> = vec![0; needed as usize / 8 + 2];
    let cap = (buf.len() * 8) as u32;
    // SAFETY: buf 有 cap 字节可写且 8 字节对齐（结构体里全是指针）。
    let ok = unsafe {
        QueryServiceConfigW(
            svc.raw(),
            buf.as_mut_ptr().cast::<QUERY_SERVICE_CONFIGW>(),
            cap,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: 调用成功即 buf 头部是一个完整的 QUERY_SERVICE_CONFIGW。
    let cfg = unsafe { &*buf.as_ptr().cast::<QUERY_SERVICE_CONFIGW>() };
    Ok(RawConfig {
        service_type: cfg.dwServiceType,
        start_type: cfg.dwStartType,
        // SAFETY: 四个字符串指针指向同一缓冲的尾部、以 NUL 结尾，buf 仍存活。
        binary_path: unsafe { opt_wide(cfg.lpBinaryPathName) },
        // SAFETY: 同上。
        load_order_group: unsafe { opt_wide(cfg.lpLoadOrderGroup) },
        // SAFETY: 同上；这里读的是双 NUL 结尾的多串。
        dependencies: unsafe { copy_multi_sz(cfg.lpDependencies) },
        // SAFETY: 同上。
        start_name: unsafe { opt_wide(cfg.lpServiceStartName) },
        // SAFETY: 同上。
        display_name: unsafe { opt_wide(cfg.lpDisplayName) },
    })
}

/// `QueryServiceConfig2W(SERVICE_CONFIG_DESCRIPTION)`：服务描述。
///
/// 绝大多数服务有这一项，但**不是全部**（第三方服务经常不写），
/// 且系统服务的描述常以 `@%SystemRoot%\system32\xxx.dll,-123` 的形式存放，
/// 由这个 API 负责解析成人话——直接读注册表的 `Description` 值拿到的是那串资源引用。
pub fn query_description(svc: &ScHandle) -> io::Result<Option<String>> {
    let buf = query_config2(svc, SERVICE_CONFIG_DESCRIPTION)?;
    if buf.is_empty() {
        return Ok(None);
    }
    // SAFETY: 调用成功即 buf 头部是一个 SERVICE_DESCRIPTIONW。
    let d = unsafe { &*buf.as_ptr().cast::<SERVICE_DESCRIPTIONW>() };
    // SAFETY: lpDescription 要么为空，要么指向 buf 内以 NUL 结尾的串。
    Ok(unsafe { opt_wide(d.lpDescription) })
}

/// `QueryServiceConfig2W(SERVICE_CONFIG_DELAYED_AUTO_START_INFO)`：是否延迟自启。
///
/// 只对 `SERVICE_AUTO_START` 有意义。取不到（旧系统 / 驱动）时返回 `None`。
pub fn query_delayed_autostart(svc: &ScHandle) -> io::Result<Option<bool>> {
    let buf = query_config2(svc, SERVICE_CONFIG_DELAYED_AUTO_START_INFO)?;
    if buf.is_empty() {
        return Ok(None);
    }
    // SAFETY: 调用成功即 buf 头部是一个 SERVICE_DELAYED_AUTO_START_INFO。
    let d = unsafe { &*buf.as_ptr().cast::<SERVICE_DELAYED_AUTO_START_INFO>() };
    Ok(Some(d.fDelayedAutostart != 0))
}

/// `QueryServiceConfig2W` 的两段式调用，返回原始缓冲（空表示这一项没有内容）。
fn query_config2(svc: &ScHandle, level: u32) -> io::Result<Vec<u64>> {
    let mut needed: u32 = 0;
    // SAFETY: 缓冲为空、大小为 0 是文档规定的「问长度」用法。
    let ok =
        unsafe { QueryServiceConfig2W(svc.raw(), level, std::ptr::null_mut(), 0, &raw mut needed) };
    if ok == 0 {
        let code = last_code();
        if code != ERROR_INSUFFICIENT_BUFFER {
            return Err(error_from_code(code));
        }
    }
    if needed == 0 {
        return Ok(Vec::new());
    }
    let mut buf: Vec<u64> = vec![0; needed as usize / 8 + 2];
    let cap = (buf.len() * 8) as u32;
    // SAFETY: buf 有 cap 字节可写且 8 字节对齐。
    let ok = unsafe {
        QueryServiceConfig2W(
            svc.raw(),
            level,
            buf.as_mut_ptr().cast::<u8>(),
            cap,
            &raw mut needed,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(buf)
}

/// `EnumDependentServicesW`：哪些服务依赖本服务（反向依赖）。
///
/// 句柄需要 `SERVICE_ENUMERATE_DEPENDENTS` 访问权。与正向依赖不同，
/// 这一项**只有服务**，没有加载顺序组——组不是可依赖的对象，只是排序用的名字。
pub fn enum_dependents(svc: &ScHandle) -> io::Result<Vec<String>> {
    let mut needed: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: 缓冲为空、大小为 0 是文档规定的「问长度」用法。
    let ok = unsafe {
        EnumDependentServicesW(
            svc.raw(),
            SERVICE_STATE_ALL,
            std::ptr::null_mut(),
            0,
            &raw mut needed,
            &raw mut returned,
        )
    };
    if ok != 0 {
        // 成功且缓冲为 0：没有任何依赖者。
        return Ok(Vec::new());
    }
    let code = last_code();
    if code != ERROR_MORE_DATA {
        return Err(error_from_code(code));
    }
    if needed == 0 {
        return Ok(Vec::new());
    }

    let mut buf: Vec<u64> = vec![0; needed as usize / 8 + 2];
    let cap = (buf.len() * 8) as u32;
    // SAFETY: buf 有 cap 字节可写且 8 字节对齐（ENUM_SERVICE_STATUSW 含指针）。
    let ok = unsafe {
        EnumDependentServicesW(
            svc.raw(),
            SERVICE_STATE_ALL,
            buf.as_mut_ptr().cast::<ENUM_SERVICE_STATUSW>(),
            cap,
            &raw mut needed,
            &raw mut returned,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: 调用成功即 buf 头部连续摆放了 returned 个 ENUM_SERVICE_STATUSW。
    let items = unsafe {
        std::slice::from_raw_parts(
            buf.as_ptr().cast::<ENUM_SERVICE_STATUSW>(),
            returned as usize,
        )
    };
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        // SAFETY: 名字指针指向 buf 尾部、以 NUL 结尾，buf 仍存活。
        let name = unsafe { from_wide_ptr(it.lpServiceName) };
        if !name.is_empty() {
            out.push(name);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 操作
// ---------------------------------------------------------------------------

/// `StartServiceW`。不传启动参数——systemd 的 `start` 也不接受参数，
/// 让两边的 API 契约保持一致。
pub fn start_service(svc: &ScHandle) -> io::Result<()> {
    // SAFETY: 参数个数为 0 时 argv 允许为空，这是文档规定的用法。
    let ok = unsafe { StartServiceW(svc.raw(), 0, std::ptr::null()) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// `ControlService`：发一个控制码（`SERVICE_CONTROL_STOP` /
/// `SERVICE_CONTROL_PARAMCHANGE` …），返回服务回报的状态。
pub fn control_service(svc: &ScHandle, code: u32) -> io::Result<RawStatus> {
    let mut st = SERVICE_STATUS::default();
    // SAFETY: st 是本栈帧上一个完整的 SERVICE_STATUS，供内核回填。
    let ok = unsafe { ControlService(svc.raw(), code, &raw mut st) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(RawStatus::from_status(&st))
}

/// `ChangeServiceConfigW`：**只改启动类型**，其余一律 `SERVICE_NO_CHANGE` / 空指针。
///
/// 空指针在这个 API 里的含义是「这一项不变」，与 `SERVICE_NO_CHANGE` 对数值项同义。
/// 特别注意 `lpPassword` 必须留空——传任何非空值都会连带重设服务账户口令。
pub fn change_start_type(svc: &ScHandle, start_type: u32) -> io::Result<()> {
    // SAFETY: 除 dwStartType 外全部传「不变」；七个字符串参数均为空指针，
    // 这是文档规定的「保持原值」写法。
    let ok = unsafe {
        ChangeServiceConfigW(
            svc.raw(),
            SERVICE_NO_CHANGE,
            start_type,
            SERVICE_NO_CHANGE,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 指针 → 字符串
// ---------------------------------------------------------------------------

/// 可空的 `PWSTR` → `Option<String>`。空指针与空串都算「没有这一项」。
///
/// # Safety
///
/// `ptr` 为空，或指向一段以 NUL 结尾、在本次调用期间有效的 UTF-16 数据。
unsafe fn opt_wide(ptr: *const u16) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 调用方保证 ptr 指向以 NUL 结尾的有效缓冲。
    let s = unsafe { from_wide_ptr(ptr) };
    (!s.is_empty()).then_some(s)
}

/// 把一段「双 NUL 结尾的多串」原样拷出来（含结尾的两个 NUL）。
///
/// 拷贝而不是借用，是因为调用方的缓冲马上就要释放；解析留给
/// [`super::map::parse_dependencies`]，那样它能用固定输入做单元测试。
///
/// # Safety
///
/// `ptr` 为空，或指向一段以**连续两个** NUL 结尾、在本次调用期间有效的 UTF-16 数据。
unsafe fn copy_multi_sz(ptr: *const u16) -> Vec<u16> {
    if ptr.is_null() {
        return Vec::new();
    }
    let mut len = 0usize;
    loop {
        // SAFETY: 调用方保证缓冲以连续两个 NUL 结尾，因此循环必然停下。
        let cur = unsafe { *ptr.add(len) };
        // SAFETY: 同上；读到第一个 NUL 时它后面至少还有一个 u16 可读。
        let next = unsafe { *ptr.add(len + 1) };
        if cur == 0 && next == 0 {
            len += 2;
            break;
        }
        len += 1;
    }
    // SAFETY: 上面刚数出 len 个有效的 u16。
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

/// `RegEnumValueW` / SCM 系列共用的「成功」判据，供本模块外的注册表渲染复用。
pub const fn reg_ok(rc: WIN32_ERROR) -> bool {
    rc == ERROR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Services::{
        SC_MANAGER_CONNECT, SC_MANAGER_ENUMERATE_SERVICE, SERVICE_QUERY_CONFIG,
        SERVICE_QUERY_STATUS, SERVICE_WIN32,
    };

    /// 打开 SCM 并枚举——任何 Windows 上都该成功，且服务数量远不止 50。
    ///
    /// 不断言某个具体服务存在：精简版 Windows、Server Core、容器镜像里
    /// Spooler 之类很可能被裁掉。
    #[test]
    fn 本机能枚举服务() {
        let mgr = open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE)
            .expect("普通用户也有 SC_MANAGER_CONNECT | ENUMERATE_SERVICE");
        let list = enum_services(&mgr).expect("枚举服务失败");
        assert!(list.len() > 50, "只枚举到 {} 个服务", list.len());
        assert!(list.iter().all(|s| !s.name.is_empty()));
        let running = list
            .iter()
            .filter(|s| s.status.current_state == 4 /* SERVICE_RUNNING */)
            .count();
        assert!(running > 5, "在跑的服务只有 {running} 个，不合常理");
        eprintln!("本机服务总数 = {}，其中在跑 {}", list.len(), running);
        for s in list.iter().take(3) {
            eprintln!(
                "  {} ({}) state={} pid={}",
                s.name, s.display_name, s.status.current_state, s.status.process_id
            );
        }
    }

    /// 对第一个服务做一次只读的详情取数，验证两段式调用与指针转换都对。
    #[test]
    fn 本机能读服务配置() {
        let mgr =
            open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE).expect("打开 SCM");
        let list = enum_services(&mgr).expect("枚举服务");
        let mut 读到 = 0usize;
        for entry in list.iter().take(20) {
            let Ok(svc) = open_service(
                &mgr,
                &entry.name,
                SERVICE_QUERY_CONFIG | SERVICE_QUERY_STATUS,
            ) else {
                continue;
            };
            let cfg = query_config(&svc).expect("QueryServiceConfigW");
            let st = query_status(&svc).expect("QueryServiceStatusEx");
            assert!(cfg.start_type <= 4, "启动类型越界：{}", cfg.start_type);
            // 两处报的类型**不保证逐位相等**：共享 svchost 里的服务在
            // `QueryServiceConfigW` 里是 `SERVICE_WIN32_SHARE_PROCESS`(0x20)，
            // 而 `QueryServiceStatusEx` 报的是合并过的 `SERVICE_WIN32`(0x30)。
            // 真正该成立的是「两处对『这是不是一个 Win32 服务（而非驱动）』的
            // 判断一致」——那才是指针转换与两段式调用有没有取错结构的信号。
            let is_win32 = |t: u32| t & SERVICE_WIN32 != 0;
            assert_ne!(st.service_type, 0, "状态里的服务类型不该是 0");
            assert_ne!(cfg.service_type, 0, "配置里的服务类型不该是 0");
            assert_eq!(
                is_win32(st.service_type),
                is_win32(cfg.service_type),
                "两处对「是不是 Win32 服务」该一致：状态 {:#x}，配置 {:#x}",
                st.service_type,
                cfg.service_type
            );
            if 读到 < 3 {
                eprintln!(
                    "  {} start_type={} bin={:?} user={:?} desc={:?}",
                    entry.name,
                    cfg.start_type,
                    cfg.binary_path,
                    cfg.start_name,
                    query_description(&svc).ok().flatten()
                );
            }
            读到 += 1;
        }
        assert!(读到 > 0, "一个服务的配置都读不到，SCM 只读权限有问题");
    }

    /// 反向依赖：随便取几个服务，只要求调用不报错（多数服务没有依赖者）。
    #[test]
    fn 本机能枚举反向依赖() {
        use windows_sys::Win32::System::Services::SERVICE_ENUMERATE_DEPENDENTS;
        let mgr =
            open_manager(SC_MANAGER_CONNECT | SC_MANAGER_ENUMERATE_SERVICE).expect("打开 SCM");
        let list = enum_services(&mgr).expect("枚举服务");
        let mut 有依赖者的 = 0usize;
        for entry in list.iter().take(40) {
            let Ok(svc) = open_service(&mgr, &entry.name, SERVICE_ENUMERATE_DEPENDENTS) else {
                continue;
            };
            if let Ok(deps) = enum_dependents(&svc)
                && !deps.is_empty()
            {
                有依赖者的 += 1;
                if 有依赖者的 <= 2 {
                    eprintln!("  {} 被 {:?} 依赖", entry.name, deps);
                }
            }
        }
        eprintln!("前 40 个服务里有 {有依赖者的} 个被别的服务依赖");
    }
}
