//! Windows 侧的认证与会话：`LogonUserW` + `LoadUserProfileW`。
//!
//! 接口形状与 Unix 侧一致，每一步对应哪个 PAM 调用见[父模块文档](super)的对照表。
//! 这里只讲 Windows 独有的那几件事。
//!
//! # 登录类型与需要的特权
//!
//! 用 `LOGON32_LOGON_INTERACTIVE` + `LOGON32_PROVIDER_DEFAULT`。
//!
//! - **`LogonUserW` 不需要 `SeTcbPrivilege`。** 那是 Windows 2000 时代的要求，
//!   XP / Server 2003 起就取消了（需要它的是 `LogonUserExExW` 的某些用法，
//!   以及自行调用 `LsaLogonUser` 注册成登录进程）。helper 以 LocalSystem 运行时
//!   本来就持有 `SeTcbPrivilege`，但本模块不依赖这一点。
//! - **以 LocalSystem 运行时 `LOGON32_LOGON_INTERACTIVE` 可用。** LocalSystem 是
//!   服务进程，没有交互式桌面，但登录类型描述的是**被登录的那个账户**要拿一张
//!   什么样的令牌，与调用方是谁无关。
//! - 代价是**目标账户**必须有本机的「允许本地登录」（`SeInteractiveLogonRight`），
//!   否则 `LogonUserW` 以 `ERROR_LOGON_TYPE_NOT_GRANTED` 失败。这是部署时最常踩的
//!   一脚，所以那个错误码单独翻译了一句话。
//! - 不用 `LOGON32_LOGON_NETWORK`（拿不到能加载配置文件的令牌）也不用
//!   `LOGON32_LOGON_BATCH`（要求另一项用户权限，且组成员关系的展开方式不同）：
//!   worker 要做的是「以这个人的身份在这台机器上干活」，交互式登录才是对的语义。
//! - [`Session::open_session`] 里的 `LoadUserProfileW` 另需 `SeBackupPrivilege` 与
//!   `SeRestorePrivilege`——它要往注册表挂载用户单元。LocalSystem 与管理员都有；
//!   没有时按 Unix 侧 `pam_open_session` 的老规矩**降级继续**，只是用户级的
//!   环境变量不到位。
//!
//! # 账户检查为什么没有单独一步
//!
//! Unix 侧 `pam_authenticate` 之后还要 `pam_acct_mgmt`，因为 PAM 把「密码对不对」
//! 与「这个账户现在能不能用」拆成了两个栈。Windows 上 LSA 把两件事做在一次
//! `LogonUserW` 里：账户禁用、锁定、过期、密码过期、登录时段 / 工作站限制、
//! 未授予登录类型，全部表现为 `LogonUserW` 失败加一个特定的 `GetLastError`。
//! 所以这里没有第二次调用，只有一张错误码翻译表。
//!
//! # 错误信息为什么要「合并」
//!
//! [`logon_failure_message`] 把 `ERROR_LOGON_FAILURE`（密码错）、
//! `ERROR_NO_SUCH_USER`（没这个账户）与 `ERROR_NONE_MAPPED`（名字解析不到 SID）
//! 翻译成**同一句话**，且不把错误号带出去。
//!
//! 理由是账户枚举：这三个码的区别正好等于「这个用户名是否存在」。攻击者拿一份
//! 用户名字典配一个随便的密码来跑，只看错误文案就能筛出真实账户，再对这批账户
//! 做定向爆破或撞库。把它们合并成一句，攻击者的每次尝试都只得到「不正确」，
//! 字典就筛不出东西。
//!
//! 剩下的码（禁用 / 锁定 / 过期 / 登录时段）保留各自的文案，这是一次**有意的
//! 取舍**：它们同样泄漏「该账户存在」，但都是**用户或管理员必须知道才能修好**的
//! 状态，合并掉会让「我密码明明是对的」变成一条查不出来的故障；而且 Windows
//! 自己的登录界面本来就会如实报这些状态。真正要靠登录界面保密的信息——
//! 账户是否存在——已经在上一段里堵住了。
//!
//! # SID → uid 的映射
//!
//! 取 SID 的最后一段子权威（RID）作 uid，`S-1-5-18`（LocalSystem）特判为 0；
//! 组名除了本地化的显示名之外，再补一个英文规范名。
//!
//! **这份映射与 core 侧的 `strixmaid_core::platform::windows::token` 是同一套规则，
//! 改一处必须改另一处。** 两边必须一致不是洁癖：主进程会拿 helper 报的
//! `AuthOk.uid` 与 worker 自报的 `Hello.uid` 做相等比对（见
//! `strixmaid_core::session::spawn_worker` 的 `expected_uid`），而后者是 core 算的。
//! 两套规则一旦分叉，每一次登录都会以「身份切换没有发生」被拒绝。
//! helper 不依赖 core（见 `Cargo.toml`），所以只能重写一份，理由见
//! [`crate::win32`] 的模块文档。
//!
//! # 凭据处理（§5.3）
//!
//! 主进程回传的密码落在 `Zeroizing<String>` 里；交给 `LogonUserW` 需要 UTF-16，
//! 那个缓冲装在 `Zeroizing<Vec<u16>>` 里，调用一结束立即擦除。本模块
//! **不打印任何消息内容**，错误文本只来自上面那张固定的翻译表。

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::PathBuf;

use strixmaid_types::auth::{AuthUser, Prompt, PromptStyle};
use strixmaid_types::ipc::{FromHelper, IpcError, ToHelper};
use windows_sys::Win32::Foundation::{
    ERROR_ACCOUNT_DISABLED, ERROR_ACCOUNT_EXPIRED, ERROR_ACCOUNT_LOCKED_OUT,
    ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_LOGON_HOURS, ERROR_INVALID_WORKSTATION,
    ERROR_LOGON_FAILURE, ERROR_LOGON_TYPE_NOT_GRANTED, ERROR_NONE_MAPPED, ERROR_NO_SUCH_USER,
    ERROR_PASSWORD_EXPIRED, ERROR_PASSWORD_MUST_CHANGE, ERROR_PRIVILEGE_NOT_HELD, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LOGON32_LOGON_INTERACTIVE,
    LOGON32_PROVIDER_DEFAULT, LogonUserW, LookupAccountSidW, PSID, SID_NAME_USE, TOKEN_GROUPS,
    TOKEN_PRIMARY_GROUP, TOKEN_QUERY, TOKEN_USER, TokenGroups, TokenPrimaryGroup, TokenUser,
};
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows_sys::Win32::UI::Shell::{LoadUserProfileW, PROFILEINFOW, UnloadUserProfile};
use zeroize::Zeroizing;

use super::Identity;
use crate::ipc::Ipc;
use crate::win32::{from_wide, from_wide_ptr, last_error_code, last_error_text, own, to_wide};

// ===========================================================================
// 常量
// ===========================================================================

/// 合成提示的编号。只有一条，恒为 0——与 PAM 那边「每轮从 0 重新编号」一致。
const PASSWORD_PROMPT_ID: u32 = 0;

/// 合成提示的文案。前端原样展示（`strixmaid_types::auth::Prompt::text` 的约定）。
const PASSWORD_PROMPT_TEXT: &str = "密码：";

/// `PI_NOUI`（userenv.h）：加载配置文件失败时**不要弹对话框**。
///
/// helper 跑在服务里，没有可交互的桌面；不设这个标志，一次失败就会挂在
/// 一个没人看得见的消息框上，表现为「登录卡住」。
const PI_NOUI: u32 = 0x0000_0001;

/// LocalSystem 的 SID。本项目把它映射成 uid 0，见模块文档。
const SID_LOCAL_SYSTEM: &str = "S-1-5-18";

/// 已知内建 SID → 英文规范组名。
///
/// 内建组的显示名是**本地化**的（德文 Windows 上 `S-1-5-32-544` 叫
/// `Administratoren`），而 `session.elevate_groups` 里写的是英文。只按显示名匹配，
/// 德文机器上提权会静默地对所有人关闭。所以这些组的英文名**额外**进一份。
///
/// 只列与授权判断有关的那几个，与 core 侧的 `CANONICAL_GROUPS` 同源。
const CANONICAL_GROUPS: &[(&str, &str)] = &[
    ("S-1-5-32-544", "Administrators"),
    ("S-1-5-32-545", "Users"),
    ("S-1-5-32-551", "Backup Operators"),
    ("S-1-5-32-555", "Remote Desktop Users"),
];

// ===========================================================================
// 错误
// ===========================================================================

/// 认证失败：带 Win32 错误码与一句人话，**不含任何凭据**。
///
/// 字段与 Unix 侧的 `PamError` 同名同义，`main.rs` 因此不需要分平台取值。
#[derive(Debug)]
pub struct AuthError {
    /// 失败的 Win32 函数名（Unix 侧是 PAM 函数名）。
    pub func: &'static str,
    /// `GetLastError` 的值；不是系统调用失败时为 0。
    pub code: u32,
    /// 给人看的原因。会原样进 `AuthFail.reason` 送到浏览器，
    /// 因此**必须**过 [`logon_failure_message`] 那张表，不能直接塞系统文案。
    pub message: String,
}

impl AuthError {
    fn new(func: &'static str, code: u32, message: impl Into<String>) -> AuthError {
        AuthError {
            func,
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} (code {})", self.func, self.message, self.code)
    }
}

impl std::error::Error for AuthError {}

/// `LogonUserW` 失败的错误码 → 固定文案。`None` 表示这张表里没有。
///
/// 合并与不合并的取舍见模块文档「错误信息为什么要合并」。
pub fn describe_logon_error(code: u32) -> Option<&'static str> {
    let text = match code {
        // ---- 合并：这三个码的区别等于「用户名是否存在」 ----
        ERROR_LOGON_FAILURE | ERROR_NO_SUCH_USER | ERROR_NONE_MAPPED => "用户名或密码不正确",
        // ---- 账户状态：保留区分，用户必须知道才能修 ----
        ERROR_ACCOUNT_DISABLED => "账户已被禁用",
        ERROR_ACCOUNT_LOCKED_OUT => "账户已被锁定，请等锁定期结束或联系管理员解锁",
        ERROR_ACCOUNT_EXPIRED => "账户已过期",
        ERROR_PASSWORD_EXPIRED => "密码已过期，请先在 Windows 上修改密码",
        ERROR_PASSWORD_MUST_CHANGE => "该账户必须先修改密码才能登录",
        ERROR_INVALID_LOGON_HOURS => "当前时间不在该账户允许的登录时段内",
        ERROR_INVALID_WORKSTATION => "该账户不允许从这台计算机登录",
        ERROR_LOGON_TYPE_NOT_GRANTED => {
            "该账户没有本机的「允许本地登录」权限（SeInteractiveLogonRight）"
        }
        // ---- 调用方自己的问题，不是用户的 ----
        ERROR_PRIVILEGE_NOT_HELD => "helper 缺少执行登录所需的特权",
        _ => return None,
    };
    Some(text)
}

/// 送进 `AuthFail.reason` 的那句话。
///
/// 表里有的只给文案、**不带错误号**——错误号本身就能把上面合并掉的那三个码
/// 区分开来，带出去等于白合并。表里没有的才附上号码，那是给排错用的，
/// 且不构成账户枚举的信道（未知码与用户名是否存在无关）。
pub fn logon_failure_message(code: u32) -> String {
    match describe_logon_error(code) {
        Some(text) => text.to_owned(),
        None => format!("登录失败（Win32 错误 {code}）"),
    }
}

// ===========================================================================
// SID 与令牌
// ===========================================================================

/// 令牌里读出来的身份。字段与 core 侧的 `TokenIdentity` 一一对应。
struct TokenIdentity {
    /// 用户 SID 的字符串形式。**权威标识**，uid 只是它的一段。
    sid: String,
    /// 账户名。查不到时为 `None`（账户已删除、或域不可达）。
    account: Option<String>,
    /// 映射出的 uid，见模块文档。
    uid: u32,
    /// 主组的 RID，映射规则同 uid。
    gid: u32,
    /// 所属组名，含本地化名与英文规范名。
    groups: Vec<String>,
}

impl TokenIdentity {
    /// 用于 `AuthUser::username` 的名字：查得到账户名就用它，否则退回 SID 串。
    ///
    /// 退回 SID 而不是空串：这个值会进审计的 `username` 列，空值会让
    /// 「谁做的」变成一条查不出来的记录。
    fn username(&self) -> String {
        self.account.clone().unwrap_or_else(|| self.sid.clone())
    }
}

/// SID → uid。见模块文档「SID → uid 的映射」。
fn uid_of(sid_string: &str, rid: u32) -> u32 {
    if sid_string == SID_LOCAL_SYSTEM { 0 } else { rid }
}

/// 取 SID 的最后一段子权威（RID）。
///
/// # Safety
///
/// `sid` 必须是一个有效的 SID 结构。
unsafe fn rid_of_sid(sid: PSID) -> u32 {
    // SAFETY: 调用方保证 SID 有效。GetSidSubAuthorityCount 返回指向 SID 内部
    // 计数字节的指针，SID 有效期间它一直有效。
    let count = unsafe {
        let p = GetSidSubAuthorityCount(sid);
        if p.is_null() { 0 } else { *p }
    };
    if count == 0 {
        return 0;
    }
    // SAFETY: 下标 count-1 在界内（刚由 GetSidSubAuthorityCount 给出）。
    unsafe {
        let p = GetSidSubAuthority(sid, u32::from(count) - 1);
        if p.is_null() { 0 } else { *p }
    }
}

/// SID → `S-1-5-…` 字符串。
///
/// # Safety
///
/// `sid` 必须是一个有效的 SID 结构。
unsafe fn sid_to_string(sid: PSID) -> Option<String> {
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: 调用方保证 SID 有效；out 是输出参数，成功时指向一块
    // LocalAlloc 的缓冲，由下面的 LocalFree 归还。
    let ok = unsafe { ConvertSidToStringSidW(sid, &raw mut out) };
    if ok == 0 || out.is_null() {
        return None;
    }
    // SAFETY: 调用成功即 out 指向以 NUL 结尾的串。
    let s = unsafe { from_wide_ptr(out) };
    // SAFETY: out 由 ConvertSidToStringSidW 用 LocalAlloc 分配，按文档用 LocalFree 归还。
    unsafe {
        LocalFree(out.cast::<c_void>());
    }
    Some(s)
}

/// SID → 账户名（只要名字，不要域）。
///
/// 不做缓存：helper 一辈子只解析一次身份，几十个组的查询一次就够了。
/// core 那边要给几百个进程查 SID，所以它带了进程内缓存。
///
/// # Safety
///
/// `sid` 必须是一个有效的 SID 结构。
unsafe fn account_name_of_sid(sid: PSID) -> Option<String> {
    let mut name_len: u32 = 0;
    let mut domain_len: u32 = 0;
    let mut use_kind: SID_NAME_USE = 0;
    // 第一次只问长度：必然失败于 ERROR_INSUFFICIENT_BUFFER。
    // SAFETY: 两个缓冲指针为空时函数只回填两个长度。
    unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            std::ptr::null_mut(),
            &raw mut name_len,
            std::ptr::null_mut(),
            &raw mut domain_len,
            &raw mut use_kind,
        );
    }
    if last_error_code() != ERROR_INSUFFICIENT_BUFFER || name_len == 0 {
        return None;
    }

    let mut name = vec![0u16; name_len as usize];
    let mut domain = vec![0u16; domain_len.max(1) as usize];
    // SAFETY: 两个缓冲各有上一步问出的容量，长度变量如实描述它们。
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &raw mut name_len,
            domain.as_mut_ptr(),
            &raw mut domain_len,
            &raw mut use_kind,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(from_wide(&name[..name_len as usize]))
}

/// 读一项变长的令牌信息，返回原始字节。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
unsafe fn token_info(token: HANDLE, class: i32) -> Result<Vec<u8>, String> {
    let mut len: u32 = 0;
    // SAFETY: 缓冲为空时函数只回填所需长度。
    unsafe {
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &raw mut len);
    }
    if len == 0 {
        return Err(format!("GetTokenInformation 问不到长度：{}", last_error_text()));
    }
    // 多要 8 字节：TOKEN_GROUPS 尾部的变长数组要求指针对齐，而我们把 Vec<u8>
    // 当结构体读，宁可多给一点也不要贴着边界。
    let mut buf = vec![0u8; len as usize + 8];
    // SAFETY: buf 至少有 len 字节可写，len 如实描述容量。
    let ok = unsafe {
        GetTokenInformation(
            token,
            class,
            buf.as_mut_ptr().cast::<c_void>(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(format!("GetTokenInformation 失败：{}", last_error_text()));
    }
    buf.truncate(len as usize);
    Ok(buf)
}

/// 令牌里的组名列表（本地化名 + 英文规范名）。
///
/// **被标成 `SE_GROUP_USE_FOR_DENY_ONLY` 的组也算数**：UAC 过滤后的管理员令牌里，
/// `Administrators` 正是以「仅用于拒绝」的形式存在的。提权资格问的是
/// 「这个账户属不属于管理员组」，而不是「这张令牌现在有没有管理员权限」。
/// 漏掉它会让每个管理员都提不了权，所以这里**不**看 `Attributes`。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
unsafe fn group_names(token: HANDLE) -> Vec<String> {
    // SAFETY: 调用方保证令牌有效。
    let Ok(buf) = (unsafe { token_info(token, TokenGroups) }) else {
        return Vec::new();
    };
    // SAFETY: TokenGroups 的返回布局就是 TOKEN_GROUPS：一个计数后跟变长数组。
    let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
    let count = groups.GroupCount as usize;
    // SAFETY: 内核保证 Groups 后面确实跟着 GroupCount 个 SID_AND_ATTRIBUTES。
    let items = unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), count) };

    let mut out: Vec<String> = Vec::with_capacity(count + 2);
    for item in items {
        // SAFETY: 内核填进来的 SID 指针在 buf 存活期间有效。
        let sid_string = unsafe { sid_to_string(item.Sid) };
        // SAFETY: 同上。
        if let Some(name) = unsafe { account_name_of_sid(item.Sid) }
            && !out.contains(&name)
        {
            out.push(name);
        }
        if let Some(s) = &sid_string
            && let Some(canon) = canonical_group_name(s)
            && !out.iter().any(|g| g == canon)
        {
            out.push(canon.to_owned());
        }
    }
    out
}

/// 内建组 SID → 英文规范名。见 [`CANONICAL_GROUPS`]。
fn canonical_group_name(sid: &str) -> Option<&'static str> {
    CANONICAL_GROUPS
        .iter()
        .find(|(k, _)| *k == sid)
        .map(|(_, v)| *v)
}

/// 令牌所代表的完整身份。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
unsafe fn identity_of_token(token: HANDLE) -> Result<TokenIdentity, String> {
    // SAFETY: 调用方保证令牌有效。
    let user_buf = unsafe { token_info(token, TokenUser) }?;
    // SAFETY: TokenUser 的返回布局就是 TOKEN_USER，缓冲至少有它那么大。
    let user = unsafe { &*(user_buf.as_ptr().cast::<TOKEN_USER>()) };
    let sid = user.User.Sid;
    // SAFETY: 内核填进来的 SID 指针在 user_buf 存活期间有效。
    let sid_string = unsafe { sid_to_string(sid) }.unwrap_or_default();
    // SAFETY: 同上。
    let rid = unsafe { rid_of_sid(sid) };
    let uid = uid_of(&sid_string, rid);

    // SAFETY: 同上。
    let gid = unsafe { token_info(token, TokenPrimaryGroup) }
        .ok()
        .map(|buf| {
            // SAFETY: TokenPrimaryGroup 的返回布局就是 TOKEN_PRIMARY_GROUP。
            let pg = unsafe { &*(buf.as_ptr().cast::<TOKEN_PRIMARY_GROUP>()) };
            // SAFETY: 内核填进来的 SID 指针在 buf 存活期间有效。
            unsafe { rid_of_sid(pg.PrimaryGroup) }
        })
        .unwrap_or(0);

    Ok(TokenIdentity {
        // SAFETY: 同上。
        account: unsafe { account_name_of_sid(sid) },
        sid: sid_string,
        uid,
        gid,
        // SAFETY: 同上。
        groups: unsafe { group_names(token) },
    })
}

/// 把用户名拆成 `(域, 账户)`，供 `LogonUserW` 使用。
///
/// 认三种写法：`DOMAIN\user`、`user@domain`（UPN，域部分**原样留在名字里**，
/// 域必须传 NULL 让 LSA 自己拆）、裸 `user`（域取 `.`，即本机）。
///
/// 与 `strixmaid_core::platform::windows::token::split_account` 同一套规则。
pub fn split_account(input: &str) -> (String, String) {
    if let Some((domain, user)) = input.split_once('\\') {
        return (domain.to_owned(), user.to_owned());
    }
    if input.contains('@') {
        return (String::new(), input.to_owned());
    }
    (".".to_owned(), input.to_owned())
}

/// `OwnedHandle` → Win32 的 `HANDLE`（两者都是 `*mut c_void`，只是别名不同）。
fn raw(h: &OwnedHandle) -> HANDLE {
    h.as_raw_handle().cast()
}

/// helper 自己的用户 SID 串。
///
/// [`crate::spawn::windows`] 用它判断「要拉起的 worker 身份是不是就是 helper
/// 自己」——只有那种情况下，不带令牌的 `CreateProcessW` 才会得到正确的进程。
/// 读不到时返回 `None`，调用方据此放弃那条退路，而不是猜。
pub(crate) fn current_user_sid() -> Option<String> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess 返回伪句柄，永远有效且不需要关闭；token 是输出参数。
    let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return None;
    }
    // SAFETY: token 刚打开，本进程独占。
    let owned = unsafe { own(token) }?;
    // SAFETY: owned 带 TOKEN_QUERY。
    let buf = unsafe { token_info(raw(&owned), TokenUser) }.ok()?;
    // SAFETY: TokenUser 的返回布局就是 TOKEN_USER，缓冲至少有它那么大。
    let user = unsafe { &*(buf.as_ptr().cast::<TOKEN_USER>()) };
    // SAFETY: 内核填进来的 SID 指针在 buf 存活期间有效。
    unsafe { sid_to_string(user.User.Sid) }
}

/// 读出一个令牌对应的用户环境块（`CreateEnvironmentBlock`）。
///
/// 这是 `pam_getenvlist` 的对应物，但内容多得多：Windows 的环境块里有
/// `USERPROFILE` / `APPDATA` / `TEMP` / `PATH` 等一整套，用户配置文件加载之后
/// 才是正确的值（所以 [`Session::open_session`] 要在取环境之前做）。
pub(crate) fn environment_block(token: HANDLE) -> Result<Vec<(String, String)>, String> {
    let mut block: *mut c_void = std::ptr::null_mut();
    // SAFETY: block 是输出参数；token 由调用方保证有效。binherit = FALSE：
    // 不要把 helper 自己的环境混进去，worker 只该看到那个用户的环境。
    let ok = unsafe { CreateEnvironmentBlock(&raw mut block, token, 0) };
    if ok == 0 || block.is_null() {
        return Err(format!("CreateEnvironmentBlock 失败：{}", last_error_text()));
    }

    let mut out = Vec::new();
    let mut p = block.cast::<u16>();
    // SAFETY: 环境块是一串以 NUL 结尾的 UTF-16 串，整体再以一个空串
    // （即连续两个 NUL）收尾；循环在空串处停下，不会越过块尾。
    unsafe {
        while *p != 0 {
            let mut len = 0usize;
            while *p.add(len) != 0 {
                len += 1;
            }
            let entry = from_wide(std::slice::from_raw_parts(p, len));
            // 跳过 `=C:=C:\path` 这类隐藏项：键为空，是 cmd 用来记每个盘符当前
            // 目录的，传给子进程没有意义，还会让下面的覆盖逻辑撞在一起。
            if let Some((k, v)) = entry.split_once('=')
                && !k.is_empty()
            {
                out.push((k.to_owned(), v.to_owned()));
            }
            p = p.add(len + 1);
        }
    }
    // SAFETY: block 由 CreateEnvironmentBlock 分配，按文档用 DestroyEnvironmentBlock 归还。
    unsafe {
        DestroyEnvironmentBlock(block);
    }
    Ok(out)
}

/// 在一份环境里按**不分大小写**找一个变量。Windows 的环境变量名不区分大小写，
/// 而 `CreateEnvironmentBlock` 给出的拼写是 `UserProfile` 还是 `USERPROFILE`
/// 没有保证。
fn env_lookup(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.clone())
}

// ===========================================================================
// Session
// ===========================================================================

/// 一次登录对话 + （可选的）一个用户配置文件会话。
///
/// 与 Unix 的 `Pam` 一样，它必须活到登出：`LoadUserProfileW` 挂上的注册表单元
/// 只能由**同一个进程**用同一个令牌 `UnloadUserProfile` 卸掉，而令牌本身还要
/// 留着给后续的 `SpawnWorker` 用。
pub struct Session {
    /// `LogonUserW` 的第二个参数。空串表示传 NULL（UPN 写法）。
    domain: String,
    /// `LogonUserW` 的第一个参数，也是 `LoadUserProfileW` 的 `lpUserName`。
    account: String,
    /// 登录令牌。认证成功前为 `None`。
    token: Option<OwnedHandle>,
    /// 从令牌里解析出的身份，`AuthOk` 与 `SpawnWorker` 都用它。
    identity: Option<TokenIdentity>,
    /// `LoadUserProfileW` 回填的配置文件句柄。
    ///
    /// 它是个注册表键句柄，**只能**交给 `UnloadUserProfile`，不能自己
    /// `CloseHandle`——所以这里存裸 `HANDLE` 而不是 `OwnedHandle`。
    profile: Option<HANDLE>,
    session_opened: bool,
    /// 索要密码那一轮里遇到的 IPC 错误，留给调用方决定如何退出。
    ipc_error: Option<IpcError>,
}

impl Session {
    /// 记下目标账户。**不调用任何系统 API**，也不发生 IPC。
    ///
    /// `service` 是 PAM 服务名，Windows 上没有对应物（LSA 的认证包由登录类型
    /// 决定，不按调用方分栈），只记一行日志。`rhost` 同理：`LogonUserW` 没有
    /// 「来源主机」参数，LSA 记的工作站名恒为本机。两个参数保留在签名里是为了
    /// 与 Unix 侧同形。
    pub fn start(
        service: &str,
        username: &str,
        rhost: Option<&str>,
    ) -> Result<Session, AuthError> {
        if username.is_empty() {
            return Err(AuthError::new("LogonUserW", 0, "用户名为空"));
        }
        if username.contains('\0') {
            return Err(AuthError::new("LogonUserW", 0, "用户名含 NUL"));
        }
        let (domain, account) = split_account(username);
        crate::log::event(&format!(
            "Windows 上没有 PAM 服务名的对应物，忽略 service={service}；\
             rhost {}",
            if rhost.is_some() {
                "已收到但 LogonUserW 无处可放"
            } else {
                "未给出"
            }
        ));
        Ok(Session {
            domain,
            account,
            token: None,
            identity: None,
            profile: None,
            session_opened: false,
            ipc_error: None,
        })
    }

    /// 合成一轮「密码」提示，拿到回答后 `LogonUserW`。
    ///
    /// Unix 侧这一步是 PAM 在 conversation 回调里问几轮就转发几轮；Windows 的
    /// `LogonUserW` 只吃一个密码，所以提示由这里**自己造**一条（echo-off）。
    /// 协议两边因此完全一样，前端不知道底下是 PAM 还是 LSA。
    pub fn authenticate(&mut self, ipc: &mut Ipc) -> Result<(), AuthError> {
        if self.token.is_some() {
            return Err(AuthError::new("LogonUserW", 0, "本会话已经认证过一次"));
        }
        let prompts = vec![Prompt {
            id: PASSWORD_PROMPT_ID,
            style: PromptStyle::Prompt,
            text: PASSWORD_PROMPT_TEXT.to_owned(),
        }];
        crate::log::event("需要 1 项输入（合成的密码提示），转发主进程");

        let responses = match ipc.send_and_wait(FromHelper::Prompts { prompts }) {
            Ok(Some(ToHelper::AuthRespond { responses })) => responses,
            Ok(Some(_)) => {
                crate::log::event("等待 AuthRespond 时收到其它消息，认证中止");
                self.ipc_error =
                    Some(IpcError::Protocol("等待 AuthRespond 时收到其它消息".into()));
                return Err(AuthError::new("authenticate", 0, "主进程乱序"));
            }
            Ok(None) => {
                crate::log::event("等待 AuthRespond 时主进程关闭了通道，认证中止");
                self.ipc_error = Some(IpcError::Protocol("主进程已断开".into()));
                return Err(AuthError::new("authenticate", 0, "主进程已断开"));
            }
            Err(e) => {
                crate::log::event(&format!("等待 AuthRespond 时 IPC 出错: {e}"));
                self.ipc_error = Some(e);
                return Err(AuthError::new("authenticate", 0, "IPC 出错"));
            }
        };

        // 取走密码；`responses` 里其余各项在作用域结束时 drop 并擦除。
        let password = responses
            .into_iter()
            .find(|r| r.id == PASSWORD_PROMPT_ID)
            .map(|r| r.value)
            .ok_or_else(|| {
                crate::log::event("主进程未对密码提示作答，认证中止");
                AuthError::new("authenticate", 0, "主进程未对密码提示作答")
            })?;

        let token = logon(&self.domain, &self.account, &password)?;
        drop(password);

        // SAFETY: token 刚由 LogonUserW 返回，带完整访问权（含 TOKEN_QUERY），本进程独占。
        let identity = unsafe { identity_of_token(raw(&token)) }
            .map_err(|e| AuthError::new("identity_of_token", last_error_code(), e))?;
        crate::log::event(&format!(
            "LogonUserW 成功，sid={} uid={}",
            identity.sid, identity.uid
        ));
        self.token = Some(token);
        self.identity = Some(identity);
        Ok(())
    }

    /// 认证后系统眼中的用户名。
    ///
    /// 对应 Unix 侧「PAM 模块可能改写 `PAM_USER`」那一条：这里是
    /// `LookupAccountSidW` 从令牌的用户 SID 反查回来的规范名，
    /// 大小写、别名（`administrator` → `Administrator`）都以它为准。
    pub fn user(&self) -> Option<String> {
        self.identity.as_ref().map(TokenIdentity::username)
    }

    /// 认证通过后解析出目标身份。
    ///
    /// `name` 只用来在与令牌里的名字对不上时记一行日志——身份的权威来源是
    /// 令牌本身，理由见[父模块文档](super)。
    ///
    /// 注意 `home`：本方法在 `AuthStart` 阶段就被调用，那时
    /// [`Session::open_session`] 还没跑过，用户的注册表单元没挂上，
    /// `CreateEnvironmentBlock` 给的 `USERPROFILE` 可能是默认配置文件的路径。
    /// 因此 [`crate::spawn::windows`] 在真正创建进程时会用**那一刻**的环境块
    /// 重新取一次工作目录，这里给出的值只作为取不到时的兜底。
    pub fn lookup_identity(&self, name: &str) -> Result<Identity, String> {
        let (Some(token), Some(id)) = (self.token.as_ref(), self.identity.as_ref()) else {
            return Err("尚未认证，拿不到登录令牌".to_owned());
        };
        let resolved = id.username();
        if !name.is_empty() && !resolved.eq_ignore_ascii_case(name) {
            crate::log::event(&format!(
                "令牌里的账户名与请求的不同（以令牌为准），请求={name}"
            ));
        }

        let env = environment_block(raw(token)).unwrap_or_else(|e| {
            crate::log::event(&format!("读取用户环境块失败，home / shell 留空：{e}"));
            Vec::new()
        });
        // 取不到就留空：`spawn` 把空路径当成「不指定工作目录」，
        // 而不是拿一个猜来的路径去试（项目约定：不编数据）。
        let home = env_lookup(&env, "USERPROFILE").map_or_else(PathBuf::new, PathBuf::from);
        // `%ComSpec%` 是 Windows 上「登录 shell」最接近的东西。用户环境块里没有
        // 时退回 helper 自己的——那是同一台机器上的同一个 cmd.exe，不是猜的。
        let shell = env_lookup(&env, "ComSpec")
            .or_else(|| std::env::var("ComSpec").ok())
            .map_or_else(PathBuf::new, PathBuf::from);

        Ok(Identity {
            user: AuthUser {
                uid: id.uid,
                gid: id.gid,
                username: resolved,
                groups: id.groups.clone(),
            },
            home,
            shell,
        })
    }

    /// `LoadUserProfileW`：建 / 挂载用户的配置文件单元。
    ///
    /// 这是 `pam_open_session` 的对应物。失败**不致命**——没有配置文件，
    /// `CreateEnvironmentBlock` 会退回默认用户的环境，worker 照样能跑，
    /// 只是 `%APPDATA%` 之类指向的不是这个人的目录。上层据此降级并把原因
    /// 报进 `WorkerSpawned.session_error`。
    ///
    /// `ipc` 用不上：Unix 侧 `pam_open_session` 可能再触发 conversation 回调
    /// （`pam_motd` 之类），Windows 上没有这种回合。参数保留是为了同形。
    pub fn open_session(&mut self, _ipc: &mut Ipc) -> Result<(), AuthError> {
        let Some(token) = self.token.as_ref() else {
            return Err(AuthError::new("LoadUserProfileW", 0, "尚未认证"));
        };
        if self.session_opened {
            return Ok(());
        }
        let mut name = to_wide(&self.account);
        let mut info = PROFILEINFOW {
            dwSize: std::mem::size_of::<PROFILEINFOW>() as u32,
            dwFlags: PI_NOUI,
            lpUserName: name.as_mut_ptr(),
            ..Default::default()
        };
        // SAFETY: token 有效；info 是一个填好 dwSize 的 PROFILEINFOW，
        // lpUserName 指向本函数持有的、以 NUL 结尾的缓冲，调用期间不会移动。
        let ok = unsafe { LoadUserProfileW(raw(token), &raw mut info) };
        if ok == 0 {
            let code = last_error_code();
            return Err(AuthError::new(
                "LoadUserProfileW",
                code,
                format!(
                    "加载用户配置文件失败（Win32 错误 {code}）；\
                     这一步需要 SeBackupPrivilege 与 SeRestorePrivilege"
                ),
            ));
        }
        self.profile = if info.hProfile.is_null() {
            // 文档没有承诺成功时 hProfile 一定非空；为空就当作「没有要卸的东西」。
            None
        } else {
            Some(info.hProfile)
        };
        self.session_opened = true;
        Ok(())
    }

    /// 会话是否已成功打开。
    pub fn session_opened(&self) -> bool {
        self.session_opened
    }

    /// 用户会话的环境变量，对应 `pam_getenvlist`。
    ///
    /// 与 Unix 侧的语义差别：那边给的是 PAM 模块**额外设置**的几条，
    /// 这边给的是那个用户的**整套**环境（`CreateEnvironmentBlock`）。
    /// 两者都作为「覆盖在基础环境之上的一层」交给 `spawn`，语义是一致的。
    pub fn envlist(&self) -> Vec<(String, String)> {
        let Some(token) = self.token.as_ref() else {
            return Vec::new();
        };
        environment_block(raw(token)).unwrap_or_else(|e| {
            crate::log::event(&format!("读取用户环境块失败：{e}"));
            Vec::new()
        })
    }

    /// 取出索要密码那一轮里记录的 IPC 错误（若有）。
    pub fn take_ipc_error(&mut self) -> Option<IpcError> {
        self.ipc_error.take()
    }

    /// 攒下的、尚未送出的纯信息消息数。
    ///
    /// **Windows 上恒为 0**：提示是本模块自己合成的单轮，不存在 PAM 那种
    /// 「模块只发了一条信息、没要输入」的回合，也就无从攒起。
    /// 保留这个方法是为了让 `main.rs` 不必分平台。
    pub fn stashed_info_count(&self) -> usize {
        0
    }

    /// 登录令牌。`spawn` 用它 `CreateProcessAsUserW`。
    pub fn token(&self) -> Option<&OwnedHandle> {
        self.token.as_ref()
    }

    /// 登录账户的 SID 串。`spawn` 用它拼命名管道的 SDDL。
    pub fn sid(&self) -> Option<&str> {
        self.identity.as_ref().map(|i| i.sid.as_str())
    }

    /// 卸载配置文件并结束会话。消费 `self`，令牌随之在 `Drop` 里关闭。
    pub fn close(mut self) {
        self.unload_profile();
    }

    /// `UnloadUserProfile`。只在真的加载过时才做，且只做一次。
    fn unload_profile(&mut self) {
        let (Some(token), Some(profile)) = (self.token.as_ref(), self.profile.take()) else {
            self.session_opened = false;
            return;
        };
        // SAFETY: token 是加载配置文件时用的那一个，profile 来自同一次
        // LoadUserProfileW 且尚未卸载；`take()` 保证不会卸第二次。
        let ok = unsafe { UnloadUserProfile(raw(token), profile) };
        if ok == 0 {
            crate::log::event(&format!(
                "UnloadUserProfile 失败：{}；用户注册表单元会留到下次登录",
                last_error_text()
            ));
        }
        self.session_opened = false;
    }
}

impl Drop for Session {
    /// 对应 Unix 侧「`Drop` 里做 `pam_end`」。
    ///
    /// 配置文件必须在令牌关闭**之前**卸载（`UnloadUserProfile` 要用它），
    /// 所以这里显式卸一次；[`Session::close`] 已经卸过时是无操作。
    /// 令牌本身由 `OwnedHandle` 的 `Drop` 关闭，不需要在这里写 `CloseHandle`。
    fn drop(&mut self) {
        self.unload_profile();
    }
}

/// `LogonUserW` 本体。明文只在两个 `Zeroizing` 缓冲里出现过。
fn logon(
    domain: &str,
    account: &str,
    password: &Zeroizing<String>,
) -> Result<OwnedHandle, AuthError> {
    let user_w = to_wide(account);
    let domain_w = to_wide(domain);
    // UPN（`user@domain`）必须把域传 NULL，由 LSA 自己从名字里拆。
    let domain_ptr = if domain.is_empty() {
        std::ptr::null()
    } else {
        domain_w.as_ptr()
    };
    // UTF-16 的那一份同样是明文，交给 Zeroizing 管：调用一结束就擦。
    let password_w: Zeroizing<Vec<u16>> = Zeroizing::new(to_wide(password.as_str()));

    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: 三个字符串指针都以 NUL 结尾、在调用期间有效（domain 可为 NULL，
    // 那是文档允许的）；token 是输出参数。LogonUserW 不保留这些指针。
    let ok = unsafe {
        LogonUserW(
            user_w.as_ptr(),
            domain_ptr,
            password_w.as_ptr(),
            LOGON32_LOGON_INTERACTIVE,
            LOGON32_PROVIDER_DEFAULT,
            &raw mut token,
        )
    };
    let code = last_error_code();
    // 明文的 UTF-16 副本到此为止。
    drop(password_w);

    if ok == 0 {
        return Err(AuthError::new(
            "LogonUserW",
            code,
            logon_failure_message(code),
        ));
    }
    // SAFETY: 调用成功即 token 是一个新句柄，尚无其它持有者。
    unsafe { own(token) }.ok_or_else(|| {
        AuthError::new("LogonUserW", 0, "登录报告成功但没有拿到令牌")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 账户枚举是这张表存在的理由：用户名错与密码错必须一个字都不差。
    #[test]
    fn 用户名错与密码错给同一句话() {
        let a = logon_failure_message(ERROR_LOGON_FAILURE);
        let b = logon_failure_message(ERROR_NO_SUCH_USER);
        let c = logon_failure_message(ERROR_NONE_MAPPED);
        assert_eq!(a, b);
        assert_eq!(b, c);
        // 更关键的是错误号不能漏出去——它本身就能把这三种情况区分开。
        for msg in [&a, &b, &c] {
            assert!(!msg.contains(&ERROR_LOGON_FAILURE.to_string()), "{msg}");
            assert!(!msg.contains(&ERROR_NO_SUCH_USER.to_string()), "{msg}");
        }
    }

    #[test]
    fn 账户状态各有各的说法() {
        let states = [
            ERROR_ACCOUNT_DISABLED,
            ERROR_ACCOUNT_LOCKED_OUT,
            ERROR_ACCOUNT_EXPIRED,
            ERROR_PASSWORD_EXPIRED,
            ERROR_PASSWORD_MUST_CHANGE,
            ERROR_INVALID_LOGON_HOURS,
            ERROR_INVALID_WORKSTATION,
            ERROR_LOGON_TYPE_NOT_GRANTED,
        ];
        let mut seen: Vec<String> = Vec::new();
        for code in states {
            let msg = logon_failure_message(code);
            assert!(describe_logon_error(code).is_some(), "{code} 应当在表里");
            assert!(!seen.contains(&msg), "两个账户状态给了同一句话：{msg}");
            seen.push(msg);
        }
    }

    #[test]
    fn 表外的错误码带上号码便于排错() {
        // 0x4D3 = 1235，不在表里的一个随便的码。
        assert!(describe_logon_error(1235).is_none());
        assert!(logon_failure_message(1235).contains("1235"));
    }

    #[test]
    fn 本地系统映射成_uid_0() {
        assert_eq!(uid_of(SID_LOCAL_SYSTEM, 18), 0);
        // 内建 Administrator 是管理员但不是 SYSTEM，不该冒充 uid 0。
        assert_eq!(uid_of("S-1-5-21-1-2-3-500", 500), 500);
        assert_eq!(uid_of("S-1-5-21-1-2-3-1001", 1001), 1001);
    }

    /// 取 RID 这一步走的是真 SID，不是自己算出来的。
    #[test]
    fn 取_rid_走真实_sid() {
        use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;

        for (text, want) in [("S-1-5-18", 18u32), ("S-1-5-32-544", 544), ("S-1-1-0", 0)] {
            let w = to_wide(text);
            let mut sid: PSID = std::ptr::null_mut();
            // SAFETY: w 以 NUL 结尾；sid 是输出参数，成功时指向 LocalAlloc 的缓冲。
            let ok = unsafe { ConvertStringSidToSidW(w.as_ptr(), &raw mut sid) };
            assert!(ok != 0 && !sid.is_null(), "{text} 应当能解析成 SID");
            // SAFETY: sid 刚由 ConvertStringSidToSidW 成功返回。
            let rid = unsafe { rid_of_sid(sid) };
            // SAFETY: 同上，按文档用 LocalFree 归还。
            unsafe {
                LocalFree(sid.cast::<c_void>());
            }
            assert_eq!(rid, want, "{text} 的 RID");
        }
    }

    #[test]
    fn 内建组补英文规范名() {
        assert_eq!(canonical_group_name("S-1-5-32-544"), Some("Administrators"));
        assert_eq!(canonical_group_name("S-1-5-32-545"), Some("Users"));
        // 普通用户 SID 没有规范名可补。
        assert_eq!(canonical_group_name("S-1-5-21-1-2-3-1001"), None);
    }

    /// 与 `strixmaid_core::platform::windows::token::split_account` 必须一致。
    #[test]
    fn 用户名拆分() {
        assert_eq!(
            split_account("CONTOSO\\alice"),
            ("CONTOSO".to_owned(), "alice".to_owned())
        );
        assert_eq!(
            split_account("alice@contoso.com"),
            (String::new(), "alice@contoso.com".to_owned())
        );
        assert_eq!(split_account("alice"), (".".to_owned(), "alice".to_owned()));
    }

    #[test]
    fn 环境变量查找不分大小写() {
        let env = vec![
            ("UserProfile".to_owned(), "C:\\Users\\alice".to_owned()),
            ("ComSpec".to_owned(), "C:\\Windows\\system32\\cmd.exe".to_owned()),
        ];
        assert_eq!(
            env_lookup(&env, "USERPROFILE").as_deref(),
            Some("C:\\Users\\alice")
        );
        assert_eq!(
            env_lookup(&env, "comspec").as_deref(),
            Some("C:\\Windows\\system32\\cmd.exe")
        );
        assert!(env_lookup(&env, "PATH").is_none());
    }

    /// 本进程自己的令牌就能验证整条「令牌 → 身份」的链路，不需要登录任何人。
    #[test]
    fn 本进程令牌可解析出身份() {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: GetCurrentProcess 返回伪句柄，永远有效；token 是输出参数。
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) };
        if ok == 0 {
            eprintln!("跳过：打不开本进程令牌（{}）", last_error_text());
            return;
        }
        // SAFETY: token 刚打开，带 TOKEN_QUERY，本进程独占。
        let owned = unsafe { own(token) }.expect("刚打开的令牌句柄");
        // SAFETY: owned 带 TOKEN_QUERY。
        let id = unsafe { identity_of_token(raw(&owned)) }.expect("本进程身份总该读得到");
        assert!(id.sid.starts_with("S-1-"), "SID 形状不对：{}", id.sid);
        assert!(!id.username().is_empty());
        assert!(!id.groups.is_empty(), "至少该有 Users / Everyone 之类");
        eprintln!(
            "本机身份：sid={} user={} uid={} gid={} groups={:?}",
            id.sid,
            id.username(),
            id.uid,
            id.gid,
            id.groups
        );
    }

    #[test]
    fn 空用户名被拒() {
        assert!(Session::start("strixmaid", "", None).is_err());
        assert!(Session::start("strixmaid", "a\0b", None).is_err());
    }
}
