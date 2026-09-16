//! Windows 侧拉起 worker：命名管道 + `CreateProcessAsUserW`。
//!
//! 接口形状与 Unix 侧一致，每一步对应什么见[父模块文档](super)的对照表。
//! 这里讲 Windows 独有的四件事：通道怎么交、管道的 SDDL 为什么必须写、
//! 创建进程的三级阶梯各要什么特权、以及 `as_root` 在 Windows 上究竟是什么。
//!
//! # 通道怎么交到 worker 手里
//!
//! Unix 上是 `fork` 之后 `dup2` 到约定的 fd 3。Windows 没有 fork，交接靠两步：
//!
//! 1. helper 建一条命名管道，**服务端**留着交给主进程（那是
//!    `WorkerSpawned.worker_handle`），**客户端**用 `CreateFileW` 自己开出来；
//! 2. 客户端句柄标成可继承，句柄的**数值**写进命令行
//!    `strixmaid.exe worker --ipc-handle <十进制>`。
//!
//! 客户端是 helper 开的，所以管道在 `CreateProcess` 之前就已经连上了；
//! 主进程那边把服务端挂到 IOCP 上就能直接收发，不必再 `ConnectNamedPipe`。
//!
//! 继承用 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 精确指定**只继承这一个**，
//! 而不是 `bInheritHandles = TRUE` 就完事。两者的默认方向正好相反：
//! Unix 的 fd 默认不带 `CLOEXEC` 就会继承，要靠 `CLOEXEC` 去关；
//! Windows 的 `bInheritHandles = TRUE` 会把本进程**全部**可继承句柄一起递过去。
//! helper 手里还攥着主进程通道与登录令牌，把它们漏给以用户身份运行的 worker
//! 是实打实的提权面。句柄列表把这件事收紧成白名单。
//!
//! # 管道的 SDDL 为什么必须写
//!
//! 默认安全描述符只给创建者与 SYSTEM。helper 以 LocalSystem 运行，worker 以
//! 登录用户运行，两者不是同一个身份，所以这条管道显式带一份 SDDL：
//! SYSTEM、内建 Administrators、以及**目标用户的 SID** 各一条 `GA`（见
//! [`pipe_sddl`]），并且用 `D:P` 挡掉继承来的 ACE。
//!
//! 需要说清的是它在哪条路上才是必需的：
//!
//! - 走 [`Rung::AsUser`]（句柄继承）时，访问检查发生在 **helper 开客户端那一刻**，
//!   之后 worker 用的是一个已经打开的句柄，不会再查一次。这条路上即使没有那条
//!   用户 ACE 也能跑通。
//! - 走 [`Rung::WithToken`] 时不行：`CreateProcessWithTokenW` 的进程实际上是
//!   seclogon 服务代为创建的，签名里连 `bInheritHandles` 都没有，helper 句柄表里的
//!   东西到不了子进程。那条路上 worker 只能**按名字**重新连一次管道，
//!   而那一次是要过访问检查的——没有这条 ACE 就是 `ERROR_ACCESS_DENIED`，
//!   表现为「worker 起来了但连不上通道」，是最难查的一类故障。
//!
//! 换句话说：这条 ACE 是给退路留的。名字是 128 位随机的
//! （[`crate::win32::random_hex`]），`nMaxInstances = 1` 且实例在
//! `CreateProcess` 之前就已被 helper 自己占满，所以多给这一条 ACE 并不会
//! 真的多出一个可以被连上的口子。
//!
//! # 创建进程的三级阶梯
//!
//! | 级 | 调用 | 需要什么 | 什么时候走到 |
//! |---|---|---|---|
//! | 1 | `CreateProcessAsUserW` | `SeAssignPrimaryTokenPrivilege` + `SeIncreaseQuotaPrivilege` | 正常路径。LocalSystem 两个都有 |
//! | 2 | `CreateProcessWithTokenW` | `SeImpersonatePrivilege` | 第 1 级失败。管理员账户有 `SeImpersonatePrivilege` 但默认没有 `SeAssignPrimaryTokenPrivilege` |
//! | 3 | `CreateProcessW` | 不需要特权 | 前两级都失败，**且目标身份就是 helper 自己**（开发机上主进程以本人身份运行的情形） |
//!
//! 第 3 级对应 Unix 侧「非 root 时只能以自己的身份拉起 worker」那条准入检查：
//! 身份没有变化时，不带令牌的 `CreateProcessW` 得到的结果与带令牌完全一样。
//!
//! # `as_root` 在 Windows 上是什么
//!
//! 不是「换成另一个账户」，而是**换一张令牌**：UAC 给每个管理员账户配一对
//! 令牌，`LogonUserW` 交回来的是过滤过的受限令牌，完整的那张挂在
//! `GetTokenInformation(TokenLinkedToken)` 上。这就是 Windows 上 `sudo` 的等价物。
//!
//! 因此 admin worker 的**用户 SID 与普通 worker 完全相同**，
//! `WorkerSpawned.uid` 报的也还是那个 uid（Unix 上报 0）。这不是偷懒：
//! Windows 上没有「变成另一个人」这回事，报 0 是编数据。主进程对 admin worker
//! 不做 uid 比对（`strixmaid_core::session::spawn_worker` 的 `expected_uid` 传
//! `None`），所以如实报不会破坏任何检查。
//!
//! 拿不到 linked token 时**直接失败，不降级**：降级的后果是用户点了「以管理员
//! 身份执行」、系统悄悄给了个普通权限的 worker，之后每一条命令都以难懂的方式
//! 失败。唯一的例外是那张令牌**本来就已提升**（UAC 关掉时 `LogonUserW` 直接给
//! 完整令牌，此时没有 linked token 可取），那种情况下原令牌就是要的东西。
//!
//! # 三处只能在真机上验证的地方
//!
//! 单元测试覆盖得到的是纯逻辑（SDDL 拼装、命令行引号、环境块编码、退出码翻译）
//! 与不需要特权的系统调用（建管道、建句柄列表）。下面三条要一台以 LocalSystem
//! 跑服务的机器才能确认，这里把判断与备选方案写下来，免得后来者从零查起：
//!
//! 1. **窗口站与桌面。** `lpDesktop` 传 NULL 表示沿用父进程的桌面，而 LocalSystem
//!    服务的桌面（会话 0 的 `Service-0x0-3e7$\Default`）登录用户没有访问权。
//!    worker 是纯控制台程序、没有 UI，按经验这条路走得通；万一以
//!    `ERROR_ACCESS_DENIED` 失败，标准做法有两个：给 `winsta0\default` 加一条
//!    目标用户的 ACE，或者改用 `WTSQueryUserToken` 拿用户自己那个会话的令牌。
//! 2. **worker 的 stderr 没有出口。** Unix 侧 worker 继承 helper 的 stdout/stderr，
//!    日志顺着进 journald。这里**故意**没有设 `STARTF_USESTDHANDLES`：那要求把
//!    三个标准句柄一并放进 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`，而控制台伪句柄
//!    放进句柄列表会让 `CreateProcess` 直接失败——为了一条日志通路去换整个
//!    worker 起不来的风险不值得。Windows 上 worker 的日志应当走事件日志，
//!    那是另一件事。
//! 3. **[`Rung::WithToken`] 那一级交不出通道句柄**，理由见上面「管道的 SDDL」。
//!    真要让那条退路可用，worker 需要多认一个「按名字连」的入口
//!    （例如 `--ipc-pipe <名字>`），那要改 worker 侧的命令行契约，
//!    不在本模块的范围内。走到那一级时日志里会明说这一点。

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, HANDLE, LocalFree, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_ELEVATION, TOKEN_LINKED_TOKEN, TokenElevation,
    TokenLinkedToken, TokenPrimary,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_WRITE,
    FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, CreateProcessW,
    CreateProcessWithTokenW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, InitializeProcThreadAttributeList, OpenProcessToken,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTUPINFOEXW,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::{WorkerProc, WorkerSpec};
use crate::auth::Session;
use crate::auth::windows::{current_user_sid, environment_block};
use crate::win32::{last_error_code, last_error_text, own, random_hex, to_wide};

/// 本项目全部命名管道的名字前缀，与 `strixmaid_core::session::channel` 一致：
/// 排查时 `Get-ChildItem \\.\pipe\` 列出来的东西太多，要能一眼认出是谁开的。
const PIPE_PREFIX: &str = r"\\.\pipe\strixmaid-";

/// 管道两个方向的缓冲大小。与主进程↔helper 那条一致（64 KiB）。
const PIPE_BUFFER: u32 = 64 * 1024;

/// 创建进程的三级阶梯，见模块文档。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rung {
    /// `CreateProcessAsUserW`。
    AsUser,
    /// `CreateProcessWithTokenW`。
    WithToken,
    /// 不带令牌的 `CreateProcessW`。
    Plain,
}

impl Rung {
    /// 失败时写进日志的名字。
    fn name(self) -> &'static str {
        match self {
            Rung::AsUser => "CreateProcessAsUserW",
            Rung::WithToken => "CreateProcessWithTokenW",
            Rung::Plain => "CreateProcessW",
        }
    }
}

/// `OwnedHandle` → Win32 的 `HANDLE`（两者都是 `*mut c_void`，只是别名不同）。
fn raw(h: &OwnedHandle) -> HANDLE {
    h.as_raw_handle().cast()
}

/// 一条随机名的 worker 通道管道名。
fn random_pipe_name() -> String {
    format!("{PIPE_PREFIX}worker-{}", random_hex(16))
}

/// worker 通道那条管道的 SDDL。
///
/// `D:P` = 只用这里列的 ACE，不接受任何继承来的；三条 `GA`（Generic All）分别给
/// `SY`（LocalSystem）、`BA`（内建 Administrators）与目标用户。为什么非写不可、
/// 以及它在哪条路上才是必需的，见模块文档。
///
/// `sid` 必须是 `S-1-…` 形式。SDDL 是一门有语法的小语言，把一个来路不明的串
/// 拼进去等于拼字符串造 SQL；这里在源头上只接受合法形状，形状不对宁可整次
/// spawn 失败，也不要拼出一条语义不明的 ACL。
pub fn pipe_sddl(sid: &str) -> Result<String, String> {
    let shaped = sid.starts_with("S-1-")
        && sid.len() <= 184
        && sid
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == 'S');
    if !shaped {
        return Err(format!("用户 SID 的形状不像 SID，拒绝拼进 SDDL：{sid}"));
    }
    Ok(format!("D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;{sid})"))
}

/// 按 `CommandLineToArgvW` 的规则给一个参数加引号。
///
/// worker 的路径由主进程下发（`AuthStart.worker_exe`），可能带空格
/// （`C:\Program Files\…`）。反斜杠只有在紧挨着结尾引号时才需要成对转义，
/// 这正是 `CommandLineToArgvW` 那条容易写错的规则。
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => {
                backslashes += 1;
                out.push('\\');
            }
            '"' => {
                // 结尾引号前的反斜杠要加倍，再转义这个引号本身。
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push('\\');
                out.push('"');
            }
            _ => {
                backslashes = 0;
                out.push(c);
            }
        }
    }
    for _ in 0..backslashes {
        out.push('\\');
    }
    out.push('"');
    out
}

/// worker 的命令行：`"<exe>" worker --ipc-handle <十进制句柄值>`。
///
/// 句柄值写十进制而不是十六进制：worker 那边用 `u64` 的默认解析读它
/// （`strixmaid_core::worker::run_from_ipc`），两边都不必约定前缀。
pub fn command_line(exe: &Path, ipc_handle: u64) -> String {
    format!(
        "{} worker --ipc-handle {ipc_handle}",
        quote_arg(&exe.to_string_lossy())
    )
}

/// worker 的命令行，**按名字**连通道：`"<exe>" worker --ipc-pipe <名字>`。
///
/// 只给 [`Rung::WithToken`] 用。那一级的进程由 seclogon 服务代建，
/// 函数签名里没有 `bInheritHandles`，helper 句柄表里的东西到不了 worker，
/// 所以只能把名字给它、让它自己连一次（对应
/// `strixmaid_core::worker::run_from_pipe`）。
///
/// 这一次连接是要过访问检查的——这正是管道 SDDL 里必须写上目标用户 SID 的
/// 理由，见模块文档「管道的 SDDL 为什么必须写」。
pub fn command_line_by_name(exe: &Path, pipe: &str) -> String {
    format!(
        "{} worker --ipc-pipe {}",
        quote_arg(&exe.to_string_lossy()),
        quote_arg(pipe)
    )
}

/// 建 worker 通道的服务端。这一半最终交给主进程。
///
/// `FILE_FLAG_OVERLAPPED` 是主进程把它挂到 tokio 的 IOCP 上的前提
/// （`IpcChannel::from_server_handle` 的安全约定）。
fn create_pipe_server(name: &str, sddl: &str) -> Result<OwnedHandle, String> {
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let w_sddl = to_wide(sddl);
    // SAFETY: w_sddl 以 NUL 结尾；SDDL_REVISION_1 = 1；descriptor 是输出参数，
    // 成功时指向一块 LocalAlloc 的内存，由下面的 LocalFree 归还。
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            w_sddl.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(format!("SDDL 解析失败：{}", last_error_text()));
    }
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        // 服务端这一半不给 worker，别让它跟着继承过去。
        bInheritHandle: 0,
    };
    let w_name = to_wide(name);
    // SAFETY: w_name 以 NUL 结尾；attrs 在本调用期间有效，其安全描述符刚解析出来；
    // 其余参数为常量。失败返回 INVALID_HANDLE_VALUE，由 own 挡下。
    let handle = unsafe {
        CreateNamedPipeW(
            w_name.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            PIPE_BUFFER,
            PIPE_BUFFER,
            0,
            &raw const attrs,
        )
    };
    let code = last_error_code();
    // SAFETY: descriptor 由 ConvertStringSecurityDescriptor… 用 LocalAlloc 分配。
    unsafe {
        LocalFree(descriptor.cast::<c_void>());
    }
    // SAFETY: handle 刚由 CreateNamedPipeW 返回，本进程独占。
    unsafe { own(handle) }.ok_or_else(|| format!("CreateNamedPipeW 失败（Win32 错误 {code}）"))
}

/// 连上刚建好的管道，拿到要交给 worker 的那一半。
///
/// 句柄标成可继承（`bInheritHandle = 1`），并且带 `FILE_FLAG_OVERLAPPED`
/// ——worker 那边同样要把它挂到自己的 IOCP 上。
fn open_pipe_client(name: &str) -> Result<OwnedHandle, String> {
    let attrs = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let w_name = to_wide(name);
    // SAFETY: w_name 以 NUL 结尾；attrs 在本调用期间有效；其余参数为常量。
    let handle = unsafe {
        CreateFileW(
            w_name.as_ptr(),
            GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_NONE,
            &raw const attrs,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            std::ptr::null_mut(),
        )
    };
    let code = last_error_code();
    // SAFETY: handle 刚由 CreateFileW 返回，本进程独占。
    unsafe { own(handle) }.ok_or_else(|| format!("连接 worker 通道失败（Win32 错误 {code}）"))
}

/// 这张令牌当前是否已提升（UAC 的完整管理员令牌）。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
unsafe fn token_is_elevated(token: HANDLE) -> bool {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len: u32 = 0;
    // SAFETY: 缓冲是一个完整的 TOKEN_ELEVATION，长度如实给出。
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast::<c_void>(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// 取 UAC 的完整管理员令牌，并复制成**主令牌**。
///
/// `TokenLinkedToken` 给的是一张**模拟**令牌，而 `CreateProcessAsUserW` 只收主令牌，
/// 所以必须 `DuplicateTokenEx(…, TokenPrimary)` 转一道。这一步漏掉的症状是
/// `ERROR_BAD_TOKEN_TYPE`，从错误码上看不出跟 UAC 有关系。
fn linked_primary_token(token: &OwnedHandle) -> Result<OwnedHandle, String> {
    let mut linked = TOKEN_LINKED_TOKEN {
        LinkedToken: std::ptr::null_mut(),
    };
    let mut len: u32 = 0;
    // SAFETY: token 有效且带 TOKEN_QUERY；缓冲是一个完整的 TOKEN_LINKED_TOKEN，
    // 长度如实给出。成功时 LinkedToken 是一个新句柄，归本进程所有。
    let ok = unsafe {
        GetTokenInformation(
            raw(token),
            TokenLinkedToken,
            (&raw mut linked).cast::<c_void>(),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(format!(
            "取不到 UAC 的完整令牌（TokenLinkedToken，Win32 错误 {}）：\
             这个账户很可能不属于 Administrators 组，因此没有可提升的第二张令牌",
            last_error_code()
        ));
    }
    // SAFETY: 调用成功即 LinkedToken 是一个新句柄，尚无其它持有者。
    let impersonation = unsafe { own(linked.LinkedToken) }
        .ok_or_else(|| "TokenLinkedToken 报告成功但没给出句柄".to_owned())?;

    let mut primary: HANDLE = std::ptr::null_mut();
    // SAFETY: impersonation 是刚拿到的有效令牌；lptokenattributes 为空表示默认安全性；
    // primary 是输出参数。
    let ok = unsafe {
        DuplicateTokenEx(
            raw(&impersonation),
            TOKEN_ALL_ACCESS,
            std::ptr::null(),
            SecurityImpersonation,
            TokenPrimary,
            &raw mut primary,
        )
    };
    if ok == 0 {
        return Err(format!(
            "把完整令牌复制成主令牌失败（DuplicateTokenEx，Win32 错误 {}）",
            last_error_code()
        ));
    }
    // SAFETY: 调用成功即 primary 是一个新句柄，尚无其它持有者。
    let primary = unsafe { own(primary) }
        .ok_or_else(|| "DuplicateTokenEx 报告成功但没给出句柄".to_owned())?;

    // SAFETY: primary 带 TOKEN_ALL_ACCESS，包含 TOKEN_QUERY。
    if !unsafe { token_is_elevated(raw(&primary)) } {
        return Err(
            "取到的 linked token 并未提升，拒绝用它创建 admin worker\
             （降级会让用户以为自己拿到了管理员权限）"
                .to_owned(),
        );
    }
    Ok(primary)
}

/// 合并环境：用户环境块打底，`extra_env` 覆盖同名项，再透传 `RUST_LOG`。
///
/// 覆盖按**不分大小写**比较——Windows 的环境变量名不区分大小写，
/// 用 `Path` 去覆盖 `PATH` 必须真的覆盖掉，而不是变成并存的两条
/// （`CreateProcess` 不会替你去重，子进程看到哪一条取决于查找顺序）。
fn merged_env(base: Vec<(String, String)>, overrides: &[(String, String)]) -> Vec<(String, String)> {
    fn put(env: &mut Vec<(String, String)>, key: &str, value: String) {
        match env.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(key)) {
            Some(slot) => slot.1 = value,
            None => env.push((key.to_owned(), value)),
        }
    }
    let mut env = base;
    for (k, v) in overrides {
        put(&mut env, k, v.clone());
    }
    // 让 worker 的 tracing 级别可控：主进程的 RUST_LOG 透传（与 Unix 侧一致）。
    if let Ok(v) = std::env::var("RUST_LOG") {
        put(&mut env, "RUST_LOG", v);
    }
    env
}

/// 把环境表编成 `CREATE_UNICODE_ENVIRONMENT` 要的块：
/// 一串 `KEY=VALUE\0`，整体再以一个空串收尾。
///
/// **必须按变量名排序**（不分大小写），这是 `CreateProcess` 对环境块的硬要求；
/// `CreateEnvironmentBlock` 给出来时本来是有序的，合并之后要重排。
pub fn environment_to_block(env: &[(String, String)]) -> Vec<u16> {
    let mut items: Vec<&(String, String)> = env.iter().collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then_with(|| a.0.cmp(&b.0))
    });
    let mut out: Vec<u16> = Vec::new();
    for (k, v) in items {
        out.extend(format!("{k}={v}").encode_utf16());
        out.push(0);
    }
    // 空表也要占一个 NUL，再加整块的结束 NUL：空环境块是 "\0\0"。
    if out.is_empty() {
        out.push(0);
    }
    out.push(0);
    out
}

/// 一个填好的 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 属性列表。
///
/// 这块内存必须活到 `CreateProcess*` 返回，所以单独做成一个持有者，
/// `Drop` 里负责 `DeleteProcThreadAttributeList`。
struct AttributeList {
    /// 用 `Vec<usize>` 而不是 `Vec<u8>` 存：属性列表内部是一串指针，
    /// 要求指针对齐，而 `Vec<u8>` 的分配只保证 1 字节对齐。
    buf: Vec<usize>,
    /// 句柄数组要活到 `CreateProcess*`——`UpdateProcThreadAttribute` 只记指针，
    /// 不拷贝内容。这是这个 API 最容易踩的一脚。
    _handles: Vec<HANDLE>,
}

impl AttributeList {
    /// 只允许继承 `handles` 里那几个句柄。
    fn with_handles(handles: Vec<HANDLE>) -> Result<AttributeList, String> {
        let mut size: usize = 0;
        // SAFETY: 第一次调用按约定传空列表，只为问出所需字节数；
        // 它必然以 ERROR_INSUFFICIENT_BUFFER 失败，返回值无意义。
        unsafe {
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut size);
        }
        if size == 0 {
            return Err(format!(
                "InitializeProcThreadAttributeList 问不到长度：{}",
                last_error_text()
            ));
        }
        let words = size.div_ceil(std::mem::size_of::<usize>()).max(1);
        let mut buf = vec![0usize; words];
        // SAFETY: 指针指向刚分配的 size 字节（按 usize 对齐），size 如实描述容量。
        let ok = unsafe {
            InitializeProcThreadAttributeList(
                buf.as_mut_ptr().cast::<c_void>(),
                1,
                0,
                &raw mut size,
            )
        };
        if ok == 0 {
            return Err(format!(
                "InitializeProcThreadAttributeList 失败：{}",
                last_error_text()
            ));
        }
        // 初始化成功之后才交给 `AttributeList` 托管：它的 `Drop` 会调用
        // `DeleteProcThreadAttributeList`，而那个函数只能作用在**已初始化**的
        // 列表上。上面那条 `return` 之前先建结构体的话，失败路径就会把一块
        // 全零的内存当成属性列表去删。
        let mut list = AttributeList {
            buf,
            _handles: handles,
        };
        let ptr = list.as_ptr();
        // SAFETY: ptr 是刚初始化好的属性列表；lpvalue 指向 _handles 的内容，
        // 它与本结构同寿命，因此在 CreateProcess* 之前不会失效；cbsize 如实描述其字节数。
        let ok = unsafe {
            UpdateProcThreadAttribute(
                ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                list._handles.as_ptr().cast::<c_void>(),
                std::mem::size_of_val(list._handles.as_slice()),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(format!(
                "UpdateProcThreadAttribute(HANDLE_LIST) 失败：{}",
                last_error_text()
            ));
        }
        Ok(list)
    }

    /// 传给 `STARTUPINFOEXW::lpAttributeList` 的指针。
    fn as_ptr(&mut self) -> *mut c_void {
        self.buf.as_mut_ptr().cast::<c_void>()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: buf 里是一个已初始化的属性列表；只删一次。
        unsafe {
            DeleteProcThreadAttributeList(self.buf.as_mut_ptr().cast::<c_void>());
        }
    }
}

/// 走某一级阶梯创建进程。
///
/// `cmdline` 每次都现编一份可写缓冲：`CreateProcess*` 允许**就地修改**
/// 这个缓冲，复用上一次的会读到被改过的内容。
fn create_process(
    rung: Rung,
    token: HANDLE,
    cmdline: &str,
    env_block: &[u16],
    cwd: &[u16],
    attributes: &mut AttributeList,
) -> Result<PROCESS_INFORMATION, String> {
    let mut cmdline_w = to_wide(cmdline);
    let cwd_ptr = if cwd.len() <= 1 {
        std::ptr::null()
    } else {
        cwd.as_ptr()
    };
    let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW;

    let si = STARTUPINFOEXW {
        StartupInfo: windows_sys::Win32::System::Threading::STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            ..Default::default()
        },
        lpAttributeList: attributes.as_ptr(),
    };
    let mut pi = PROCESS_INFORMATION::default();
    let startup = (&raw const si.StartupInfo).cast();

    let ok = match rung {
        // SAFETY: token 有效且带 TOKEN_ASSIGN_PRIMARY / TOKEN_DUPLICATE / TOKEN_QUERY；
        // cmdline_w 以 NUL 结尾且可写；env_block 是双 NUL 收尾的 UTF-16 块；
        // cwd_ptr 为空或指向以 NUL 结尾的路径；si 的属性列表在本调用期间有效。
        // bInheritHandles = TRUE 是句柄列表生效的前提，实际继承的只有列表里那几个。
        Rung::AsUser => unsafe {
            CreateProcessAsUserW(
                token,
                std::ptr::null(),
                cmdline_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                env_block.as_ptr().cast::<c_void>(),
                cwd_ptr,
                startup,
                &raw mut pi,
            )
        },
        // SAFETY: 同上。这个 API 没有 bInheritHandles 参数——进程由 seclogon
        // 代为创建，句柄继承不在它的契约里，见模块文档。
        Rung::WithToken => unsafe {
            CreateProcessWithTokenW(
                token,
                0,
                std::ptr::null(),
                cmdline_w.as_mut_ptr(),
                flags,
                env_block.as_ptr().cast::<c_void>(),
                cwd_ptr,
                startup,
                &raw mut pi,
            )
        },
        // SAFETY: 同上，只是不带令牌——新进程继承 helper 自己的身份。
        Rung::Plain => unsafe {
            CreateProcessW(
                std::ptr::null(),
                cmdline_w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                env_block.as_ptr().cast::<c_void>(),
                cwd_ptr,
                startup,
                &raw mut pi,
            )
        },
    };
    if ok == 0 {
        return Err(format!(
            "{} 失败（Win32 错误 {}）",
            rung.name(),
            last_error_code()
        ));
    }
    Ok(pi)
}

/// 拉起一个 worker。返回 `(进程, 交给主进程的那半条通道)`。
pub fn spawn_worker(
    exe: &Path,
    spec: &WorkerSpec,
    session: &Session,
) -> Result<(WorkerProc, OwnedHandle), String> {
    crate::log::event(&format!(
        "准备为 {}（uid {} gid {}，as_root={}）创建 worker",
        spec.username, spec.uid, spec.gid, spec.as_root
    ));
    let logon_token = session
        .token()
        .ok_or_else(|| "尚未认证，没有登录令牌，无法创建 worker".to_owned())?;
    let user_sid = session
        .sid()
        .ok_or_else(|| "尚未认证，拿不到用户 SID，无法给 worker 通道定安全描述符".to_owned())?;

    // ---- 选令牌：普通 worker 用登录令牌，admin worker 用 UAC 的完整令牌 ----
    let elevated;
    let token = if spec.as_root {
        // SAFETY: 登录令牌带 TOKEN_QUERY。
        if unsafe { token_is_elevated(raw(logon_token)) } {
            // UAC 关掉时 LogonUserW 直接给完整令牌，没有第二张可取。
            crate::log::event("登录令牌本身已提升，admin worker 直接用它");
            logon_token
        } else {
            elevated = linked_primary_token(logon_token)?;
            &elevated
        }
    } else {
        logon_token
    };

    // ---- 建通道 ----
    let name = random_pipe_name();
    let sddl = pipe_sddl(user_sid)?;
    let server = create_pipe_server(&name, &sddl)?;
    let client = open_pipe_client(&name)?;

    // ---- 环境与工作目录 ----
    let base = environment_block(raw(token)).unwrap_or_else(|e| {
        crate::log::event(&format!(
            "读不到用户环境块，worker 只拿到 extra_env：{e}"
        ));
        Vec::new()
    });
    let mut env = merged_env(base, &spec.extra_env);
    // 环境块读不到时至少把 `ComSpec` 补回去：`spec.shell` 就是从它来的
    // （见 `auth::windows::Session::lookup_identity`），worker 的终端要用它
    // 决定默认起什么进程。只在缺的时候补，不覆盖用户自己的值。
    if !spec.shell.as_os_str().is_empty()
        && !env.iter().any(|(k, _)| k.eq_ignore_ascii_case("ComSpec"))
    {
        env.push((
            "ComSpec".to_owned(),
            spec.shell.to_string_lossy().into_owned(),
        ));
    }
    let env_block = environment_to_block(&env);

    // 工作目录优先取**这一刻**环境块里的 `USERPROFILE`，而不是 `spec.home`。
    // `spec.home` 是认证阶段算出来的，那时用户的配置单元还没挂上
    // （`open_session` 在 `SpawnWorker` 里才做），`CreateEnvironmentBlock` 当时
    // 给的可能是默认配置文件的路径。两者一致时这一步不改变任何东西。
    // 都取不到就传 NULL 沿用默认工作目录，不拿一个猜的路径去试。
    let cwd_text = env
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("USERPROFILE"))
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| spec.home.to_string_lossy().into_owned());
    let cwd = if cwd_text.is_empty() {
        Vec::new()
    } else {
        to_wide(&cwd_text)
    };

    // ---- 只继承 worker 通道这一个句柄 ----
    let client_raw = raw(&client);
    let mut attributes = AttributeList::with_handles(vec![client_raw])?;
    // 命令行按级不同：只有能把句柄递过去的那两级才用 `--ipc-handle`，
    // `WithToken` 那一级必须改成按名字连（见 `command_line_by_name`）。
    let cmdline_by_handle = command_line(exe, client_raw as usize as u64);
    let cmdline_by_name = command_line_by_name(exe, &name);

    // ---- 三级阶梯 ----
    let mut errors: Vec<String> = Vec::new();
    let mut created = None;
    for rung in [Rung::AsUser, Rung::WithToken, Rung::Plain] {
        if rung == Rung::Plain {
            // 不带令牌只会得到 helper 自己的身份；目标不是自己时那是**错的**
            // worker，宁可失败。对应 Unix 侧「非 root 只能拉起自己」那条准入检查。
            if !target_is_self(user_sid) {
                continue;
            }
            // 同理，`as_root` 时这一级只有在 helper 自己已提升时才等价——
            // 否则就成了「用户点了以管理员身份执行、系统悄悄给了普通权限」，
            // 正是模块文档里说绝不能发生的那种降级。
            if spec.as_root && !current_process_is_elevated() {
                crate::log::event(
                    "as_root 且 helper 自身未提升，不走不带令牌的 CreateProcessW",
                );
                continue;
            }
        }
        let token_arg = if rung == Rung::Plain {
            std::ptr::null_mut()
        } else {
            raw(token)
        };
        let cmdline = if rung == Rung::WithToken {
            &cmdline_by_name
        } else {
            &cmdline_by_handle
        };
        match create_process(rung, token_arg, cmdline, &env_block, &cwd, &mut attributes) {
            Ok(pi) => {
                if rung != Rung::AsUser {
                    crate::log::event(&format!(
                        "已回落到 {}；前面失败的是：{}",
                        rung.name(),
                        errors.join("；")
                    ));
                }
                if rung == Rung::WithToken {
                    crate::log::event(
                        "CreateProcessWithTokenW 不传递句柄，已改让 worker 按名字连通道",
                    );
                }
                created = Some(pi);
                break;
            }
            Err(e) => errors.push(e),
        }
    }
    let Some(pi) = created else {
        return Err(format!("创建 worker 进程失败：{}", errors.join("；")));
    };

    // worker 已经拿到自己的副本，helper 这份关掉；不关的话管道永远读不到 EOF。
    drop(client);
    // 线程句柄用不上，立刻归还。
    // SAFETY: hThread 由 CreateProcess* 交给本进程，尚未关闭。
    unsafe {
        CloseHandle(pi.hThread);
    }
    // SAFETY: hProcess 由 CreateProcess* 交给本进程，尚未关闭。
    let handle = unsafe { own(pi.hProcess) }
        .ok_or_else(|| "CreateProcess 报告成功但没给出进程句柄".to_owned())?;

    Ok((
        WorkerProc {
            pid: pi.dwProcessId as i32,
            handle,
        },
        server,
    ))
}

/// helper 自己这个进程当前是否已提升。
///
/// 读不到就当「没提升」：这条判断只用来决定要不要走那条会降权的退路，
/// 拿不准时不走才是安全的方向。
fn current_process_is_elevated() -> bool {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 返回伪句柄，永远有效且不需要关闭；token 是输出参数。
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return false;
    }
    // SAFETY: token 刚打开，本进程独占。
    let Some(owned) = (unsafe { own(token) }) else {
        return false;
    };
    // SAFETY: owned 带 TOKEN_QUERY。
    unsafe { token_is_elevated(raw(&owned)) }
}

/// 目标身份是不是 helper 自己。
///
/// 只有这一种情况下不带令牌的 `CreateProcessW` 才会得到正确的 worker。
/// 读不到本进程的 SID 时按「不是」处理——宁可整次 spawn 失败，
/// 也不要拿一个身份不对的 worker 去干活。
fn target_is_self(user_sid: &str) -> bool {
    match current_user_sid() {
        Some(mine) => mine == user_sid,
        None => {
            crate::log::event("读不到 helper 自己的 SID，不走不带令牌的那一级");
            false
        }
    }
}

/// 把 worker 的退出码翻译成可读原因（供回收时记日志）。
///
/// 与 Unix 版同形，但翻的是另一套东西：Unix 侧那三个码是 helper **自己**在
/// fork 之后、exec 之前约定的；Windows 上没有 fork 那段窗口，
/// 进程要么建起来了要么 `CreateProcess` 就失败了，所以这里翻的是
/// 内核在进程异常终止时写进退出码的那些 `NTSTATUS`。
pub fn describe_exit(code: i32) -> &'static str {
    match code {
        0 => "正常退出",
        // 0xC0000005
        -1073741819 => "访问冲突（STATUS_ACCESS_VIOLATION）",
        // 0xC00000FD
        -1073741571 => "栈溢出（STATUS_STACK_OVERFLOW）",
        // 0xC000013A
        -1073741510 => "被控制事件终止（STATUS_CONTROL_C_EXIT）",
        // 0xC0000135
        -1073741515 => "缺少依赖的 DLL（STATUS_DLL_NOT_FOUND）",
        // 0xC0000142
        -1073741502 => "DLL 初始化失败（STATUS_DLL_INIT_FAILED）",
        // 0xC0000409
        -1073740791 => "栈缓冲区越界（STATUS_STACK_BUFFER_OVERRUN）",
        _ => "worker 自行退出",
    }
}

/// 非阻塞回收已退出的 worker。
///
/// Unix 用 `waitpid(WNOHANG)`：子进程退出后必须被 wait 一次，否则留下僵尸。
/// Windows 没有僵尸进程，但**句柄不关就不会释放进程对象**，所以这里做的其实是
/// 另一件事：等 0 毫秒问一下状态，退出了就记日志并把句柄丢掉。
pub fn reap_workers(workers: &mut Vec<WorkerProc>) {
    workers.retain(|w| {
        let handle = raw(&w.handle);
        // SAFETY: handle 是本进程持有的有效进程句柄；超时 0 表示只问不等。
        let state = unsafe { WaitForSingleObject(handle, 0) };
        match state {
            WAIT_TIMEOUT => true,
            WAIT_OBJECT_0 => {
                let mut code: u32 = 0;
                // SAFETY: handle 有效；code 是输出参数。
                let ok = unsafe { GetExitCodeProcess(handle, &raw mut code) };
                if ok == 0 {
                    crate::log::event(&format!(
                        "worker {} 已退出，但读不到退出码：{}",
                        w.pid,
                        last_error_text()
                    ));
                } else {
                    crate::log::event(&format!(
                        "worker {} 退出，code={}（{}）",
                        w.pid,
                        code as i32,
                        describe_exit(code as i32)
                    ));
                }
                false
            }
            _ => {
                crate::log::event(&format!(
                    "等待 worker {} 失败：{}；不再跟踪它",
                    w.pid,
                    last_error_text()
                ));
                false
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 管道名带前缀且每次不同() {
        let a = random_pipe_name();
        let b = random_pipe_name();
        assert!(a.starts_with(PIPE_PREFIX), "{a}");
        assert!(a.contains("worker-"), "{a}");
        assert_ne!(a, b);
    }

    #[test]
    fn sddl_含系统_管理员与目标用户() {
        let sddl = pipe_sddl("S-1-5-21-1111-2222-3333-1001").unwrap();
        assert!(sddl.starts_with("D:P"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;SY)"), "{sddl}");
        assert!(sddl.contains("(A;;GA;;;BA)"), "{sddl}");
        assert!(
            sddl.contains("(A;;GA;;;S-1-5-21-1111-2222-3333-1001)"),
            "{sddl}"
        );
        // 没给 Everyone / Authenticated Users 开口子。
        assert!(!sddl.contains(";;;WD)"), "{sddl}");
        assert!(!sddl.contains(";;;AU)"), "{sddl}");
    }

    /// SDDL 是有语法的，来路不明的串不能直接拼进去。
    #[test]
    fn 形状不对的_sid_被拒() {
        for bad in [
            "",
            "alice",
            "S-1-5-21-1)(A;;GA;;;WD",
            "S-1-5-21-1 2 3",
            "s-1-5-18",
        ] {
            assert!(pipe_sddl(bad).is_err(), "{bad} 应当被拒");
        }
        assert!(pipe_sddl("S-1-5-18").is_ok());
    }

    /// 真的拿这串 SDDL 去让系统解析一次——语法错在这里必须当场暴露，
    /// 而不是等到某台机器上建管道失败。
    #[test]
    fn sddl_能被系统解析() {
        let sddl = pipe_sddl("S-1-5-21-1111-2222-3333-1001").unwrap();
        let w = to_wide(&sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: w 以 NUL 结尾；descriptor 是输出参数。
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w.as_ptr(),
                1,
                &raw mut descriptor,
                std::ptr::null_mut(),
            )
        };
        assert!(ok != 0, "SDDL 解析失败：{}", last_error_text());
        // SAFETY: descriptor 由上面的调用用 LocalAlloc 分配。
        unsafe {
            LocalFree(descriptor.cast::<c_void>());
        }
    }

    #[test]
    fn 命令行按_commandlinetoargvw_的规则加引号() {
        assert_eq!(quote_arg("simple"), "simple");
        assert_eq!(quote_arg(r"C:\bin\strixmaid.exe"), r"C:\bin\strixmaid.exe");
        assert_eq!(
            quote_arg(r"C:\Program Files\s.exe"),
            "\"C:\\Program Files\\s.exe\""
        );
        // 结尾的反斜杠要加倍，否则会把结尾引号转义掉。
        assert_eq!(quote_arg(r"C:\dir with space\"), "\"C:\\dir with space\\\\\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn 命令行形状固定() {
        let cmd = command_line(Path::new(r"C:\Program Files\strixmaid.exe"), 1234);
        assert_eq!(
            cmd,
            "\"C:\\Program Files\\strixmaid.exe\" worker --ipc-handle 1234"
        );
        // 句柄值是十进制，worker 那边按 u64 默认解析读它。
        let cmd = command_line(Path::new("s.exe"), 0xFFFF_FFFF);
        assert!(cmd.ends_with("--ipc-handle 4294967295"), "{cmd}");
    }

    #[test]
    fn 环境块排序且双_nul_收尾() {
        let env = vec![
            ("Zeta".to_owned(), "1".to_owned()),
            ("alpha".to_owned(), "2".to_owned()),
            ("MIDDLE".to_owned(), "3".to_owned()),
        ];
        let block = environment_to_block(&env);
        let text = String::from_utf16_lossy(&block);
        let items: Vec<&str> = text.split('\0').filter(|s| !s.is_empty()).collect();
        assert_eq!(items, vec!["alpha=2", "MIDDLE=3", "Zeta=1"]);
        assert_eq!(&block[block.len() - 2..], &[0, 0], "必须双 NUL 收尾");

        // 空表也要是合法的空块。
        assert_eq!(environment_to_block(&[]), vec![0, 0]);
    }

    #[test]
    fn 合并环境时同名不分大小写() {
        let base = vec![
            ("PATH".to_owned(), "C:\\Windows".to_owned()),
            ("TEMP".to_owned(), "C:\\Temp".to_owned()),
        ];
        let over = vec![
            ("Path".to_owned(), "C:\\Other".to_owned()),
            ("NEW".to_owned(), "1".to_owned()),
        ];
        let merged = merged_env(base, &over);
        let path: Vec<&(String, String)> = merged
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("path"))
            .collect();
        assert_eq!(path.len(), 1, "PATH 必须被覆盖而不是变成两条：{merged:?}");
        assert_eq!(path[0].1, "C:\\Other");
        assert!(merged.iter().any(|(k, _)| k == "NEW"));
    }

    #[test]
    fn 退出码翻译覆盖常见的_ntstatus() {
        assert_eq!(describe_exit(0), "正常退出");
        assert!(describe_exit(0xC000_0005_u32 as i32).contains("访问冲突"));
        assert!(describe_exit(0xC000_0135_u32 as i32).contains("DLL"));
        assert_eq!(describe_exit(3), "worker 自行退出");
    }

    /// 句柄列表能不能建起来与权限无关，非管理员下也该通过。
    #[test]
    fn 句柄列表可以建起来() {
        // 用一个真句柄：本进程的伪句柄不能进列表，拿一条管道来。
        let name = random_pipe_name();
        let sddl = pipe_sddl("S-1-5-18").unwrap();
        let Ok(server) = create_pipe_server(&name, &sddl) else {
            eprintln!("跳过：本机建不了命名管道（{}）", last_error_text());
            return;
        };
        let list = AttributeList::with_handles(vec![raw(&server)]);
        assert!(list.is_ok(), "{:?}", list.err());
    }

    /// 建管道 + 自己连上自己，这一段完全不需要特权，非管理员下也该通过。
    /// 它验证的是「worker 通道的两半确实能成对建出来」。
    #[test]
    fn 管道两端可以成对建出来() {
        let name = random_pipe_name();
        let Some(sid) = current_user_sid() else {
            eprintln!("跳过：读不到本进程 SID");
            return;
        };
        let sddl = pipe_sddl(&sid).expect("本进程 SID 应当是合法形状");
        let Ok(server) = create_pipe_server(&name, &sddl) else {
            eprintln!("跳过：本机建不了命名管道（{}）", last_error_text());
            return;
        };
        let client = open_pipe_client(&name).expect("刚建好的管道应当能连上");
        // 两端都是真句柄，且互不相同。
        assert!(!raw(&server).is_null());
        assert!(!raw(&client).is_null());
        assert_ne!(raw(&server), raw(&client));
    }
}
