//! 访问令牌与 SID：身份、组、提权状态。
//!
//! # SID 怎么塞进 `uid: u32`
//!
//! `AuthUser` / `ProcessSummary` / `UserCapabilities` 的身份字段在 API 契约里是
//! `u32`（design.md §5.2、§9.1），那是 Unix 的 uid。Windows 的身份是 **SID**，
//! 一个变长结构，塞不进 32 位。
//!
//! 取 SID 的**最后一段子权威（RID）** 作为 uid，并做一处映射：
//!
//! | SID | RID | 本项目的 uid | 理由 |
//! |---|---|---|---|
//! | `S-1-5-18`（LocalSystem） | 18 | **0** | 它就是 Windows 的 root：`capability::derive_user_caps` 的 `uid == 0` 判断、审计里的「以最高权限执行」都靠这一条 |
//! | `S-1-5-21-…-500`（内建 Administrator） | 500 | 500 | 是管理员但不是 SYSTEM，不该冒充 uid 0 |
//! | 普通本地用户 | 1001+ | 1001+ | 与 Linux 的 1000+ 同一量级，纯属巧合但读起来顺 |
//!
//! **RID 在域环境下不是全局唯一的**（两个域里都可能有 RID 1105）。这不影响本项目：
//! uid 只用于同一台机器内的展示与比对，跨节点的身份映射按 design.md §11 本来就
//! 不做。真正权威的标识是 [`TokenIdentity::sid`] 里的完整 SID 串，需要精确比对
//! 的地方（如 [`account_by_sid`] 的缓存键）一律用它。
//!
//! # 组名为什么要规范化
//!
//! `may_elevate` 按**组名**判断提权资格，而 `session.elevate_groups` 是一份配置里
//! 写死的英文名。可内建组的显示名是**本地化**的：德文 Windows 上
//! `S-1-5-32-544` 叫 `Administratoren`，中文上叫 `Administrators`（恰好没翻译）。
//! 若直接用 `LookupAccountSidW` 的结果，德文机器上提权会静默地对所有人关闭。
//! 因此 [`group_names`] 对已知的内建 SID **额外补一个英文规范名**，
//! 两个名字都进 `groups`——本地化名给人看，规范名给判断用。

use std::collections::HashMap;
use std::io;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, HANDLE};
use windows_sys::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, LookupAccountSidW, PSID,
    SID_NAME_USE, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_PRIMARY_GROUP, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenGroups, TokenPrimaryGroup, TokenUser,
};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::handle::Owned;
use super::wide::{from_wide, from_wide_ptr};
use super::{last_error, wide};

/// 内建管理员组的 SID。提权资格的规范判据。
pub const SID_ADMINISTRATORS: &str = "S-1-5-32-544";
/// LocalSystem 的 SID。本项目把它映射成 uid 0，见模块文档。
pub const SID_LOCAL_SYSTEM: &str = "S-1-5-18";

/// 已知内建 SID → 英文规范组名。见模块文档「组名为什么要规范化」。
///
/// 只列与授权判断有关的那几个：本地化会让按名字匹配失效的，正是这些内建组。
const CANONICAL_GROUPS: &[(&str, &str)] = &[
    (SID_ADMINISTRATORS, "Administrators"),
    ("S-1-5-32-545", "Users"),
    ("S-1-5-32-544", "Administrators"),
    ("S-1-5-32-551", "Backup Operators"),
    ("S-1-5-32-555", "Remote Desktop Users"),
];

/// 一个账户的名字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountName {
    /// 账户名，如 `alice`。
    pub name: String,
    /// 域或机器名，如 `WORKSTATION` / `CONTOSO`；本地内建账户可能为空。
    pub domain: String,
}

impl AccountName {
    /// `DOMAIN\name` 形式；域为空时只有 `name`。
    pub fn qualified(&self) -> String {
        if self.domain.is_empty() {
            self.name.clone()
        } else {
            format!("{}\\{}", self.domain, self.name)
        }
    }
}

/// 一个令牌所代表的身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenIdentity {
    /// 用户 SID 的字符串形式（`S-1-5-21-…`）。**权威标识**。
    pub sid: String,
    /// 账户名。查不到时为 `None`（孤儿 SID：账户已删除，或域不可达）。
    pub account: Option<AccountName>,
    /// 映射出的 uid，见模块文档。
    pub uid: u32,
    /// 主组的 RID，映射规则同 uid。
    pub gid: u32,
    /// 所属组名。含本地化名与英文规范名，见模块文档。
    pub groups: Vec<String>,
    /// 所属组的 RID，见 [`group_rids`]。仅供展示（`WhoAmI::groups`）。
    pub group_rids: Vec<u32>,
}

impl TokenIdentity {
    /// 用于 `AuthUser::username` 的名字：查得到账户名就用它，否则退回 SID 串。
    ///
    /// 退回 SID 而不是空串：这个值会进审计的 `username` 列，空值会让
    /// 「谁做的」变成一条查不出来的记录。
    pub fn username(&self) -> String {
        self.account
            .as_ref()
            .map_or_else(|| self.sid.clone(), |a| a.name.clone())
    }
}

/// SID → uid 的映射，见模块文档。
///
/// # Safety
///
/// `sid` 必须指向一个有效的 SID 结构，并在调用期间保持有效。
/// `PSID` 是裸指针而非内核句柄：本函数会真的解引用它内部的子权威计数与
/// 子权威数组，传进来一个悬垂或伪造的指针就是未定义行为。
pub unsafe fn rid_of_sid(sid: PSID) -> u32 {
    // SAFETY: 调用方保证 SID 有效。GetSidSubAuthorityCount 返回
    // 指向 SID 内部计数字节的指针，SID 有效期间它一直有效。
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
/// `sid` 必须指向一个有效的 SID 结构，并在调用期间保持有效，理由同
/// [`rid_of_sid`]。
pub unsafe fn sid_to_string(sid: PSID) -> Option<String> {
    let mut out: *mut u16 = std::ptr::null_mut();
    // SAFETY: sid 由调用方保证有效；stringsid 是输出参数，成功时指向
    // 一块由 LocalAlloc 分配、需我们 LocalFree 的缓冲。
    let ok = unsafe { ConvertSidToStringSidW(sid, &raw mut out) };
    if ok == 0 || out.is_null() {
        return None;
    }
    // SAFETY: 调用成功即 out 指向以 NUL 结尾的串。
    let s = unsafe { from_wide_ptr(out) };
    // SAFETY: out 由 ConvertSidToStringSidW 用 LocalAlloc 分配，按文档用 LocalFree 归还。
    unsafe {
        windows_sys::Win32::Foundation::LocalFree(out.cast());
    }
    Some(s)
}

/// SID → 账户名。带进程内缓存：一次进程列表会对几百个进程查同一批 SID，
/// 而 `LookupAccountSidW` 在域环境下可能走网络。
///
/// # Safety
///
/// `sid` 必须指向一个有效的 SID 结构，并在调用期间保持有效，理由同
/// [`rid_of_sid`]。
pub unsafe fn account_by_sid(sid: PSID) -> Option<AccountName> {
    // SAFETY: 调用方保证 SID 有效。
    let key = unsafe { sid_to_string(sid) }?;
    static CACHE: OnceLock<Mutex<HashMap<String, Option<AccountName>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Some(hit) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned()
    {
        return hit;
    }
    // SAFETY: 同上。
    let looked = unsafe { lookup_account(sid) };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, looked.clone());
    looked
}

/// # Safety
///
/// `sid` 必须指向一个有效的 SID 结构，并在调用期间保持有效：
/// `LookupAccountSidW` 会解引用它。
unsafe fn lookup_account(sid: PSID) -> Option<AccountName> {
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
    if last_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || name_len == 0 {
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
    Some(AccountName {
        name: from_wide(&name[..name_len as usize]),
        domain: from_wide(&domain[..domain_len as usize]),
    })
}

/// 打开一个进程的令牌（只读）。
///
/// # Safety
///
/// `process` 必须是一个有效的进程句柄，且带 `PROCESS_QUERY_INFORMATION` 或
/// `PROCESS_QUERY_LIMITED_INFORMATION` 访问权。
pub unsafe fn open_process_token(process: HANDLE) -> io::Result<Owned> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: 调用方保证进程句柄有效；token 是输出参数。
    let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &raw mut token) };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: 调用成功即 token 是一个新句柄，尚无其它持有者。
    unsafe { Owned::new(token) }
}

/// 读一项变长的令牌信息，返回原始字节。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
pub unsafe fn token_info(token: HANDLE, class: i32) -> io::Result<Vec<u8>> {
    let mut len: u32 = 0;
    // SAFETY: 缓冲为空时函数只回填所需长度。
    unsafe {
        GetTokenInformation(token, class, std::ptr::null_mut(), 0, &raw mut len);
    }
    if len == 0 {
        return Err(last_error());
    }
    // 多要 8 字节：某些信息类（TOKEN_GROUPS）的尾部数组要求指针对齐，
    // 而我们把 Vec<u8> 当成结构体来读，宁可多给一点也不要贴着边界。
    let mut buf = vec![0u8; len as usize + 8];
    // SAFETY: buf 至少有 len 字节可写，len 如实描述容量。
    let ok = unsafe {
        GetTokenInformation(
            token,
            class,
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            len,
            &raw mut len,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    buf.truncate(len as usize);
    Ok(buf)
}

/// 令牌所代表的完整身份。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
pub unsafe fn identity_of_token(token: HANDLE) -> io::Result<TokenIdentity> {
    // SAFETY: 调用方保证令牌有效。
    let user_buf = unsafe { token_info(token, TokenUser) }?;
    // SAFETY: TokenUser 的返回布局就是 TOKEN_USER，缓冲至少有它那么大。
    let user = unsafe { &*(user_buf.as_ptr().cast::<TOKEN_USER>()) };
    let sid = user.User.Sid;
    // SAFETY: sid 指向 user_buf 内部，该缓冲在本函数返回前一直存活。
    let sid_string = unsafe { sid_to_string(sid) }.unwrap_or_default();
    let uid = if sid_string == SID_LOCAL_SYSTEM {
        0
    } else {
        // SAFETY: 同上。
        unsafe { rid_of_sid(sid) }
    };

    // SAFETY: 同上。
    let gid = unsafe { token_info(token, TokenPrimaryGroup) }
        .ok()
        .map(|buf| {
            // SAFETY: TokenPrimaryGroup 的返回布局就是 TOKEN_PRIMARY_GROUP。
            let pg = unsafe { &*(buf.as_ptr().cast::<TOKEN_PRIMARY_GROUP>()) };
            // SAFETY: PrimaryGroup 指向 buf 内部，buf 在闭包内一直存活。
            unsafe { rid_of_sid(pg.PrimaryGroup) }
        })
        .unwrap_or(0);

    Ok(TokenIdentity {
        // SAFETY: sid 指向 user_buf 内部，该缓冲在本函数返回前一直存活。
        account: unsafe { account_by_sid(sid) },
        sid: sid_string,
        uid,
        gid,
        // SAFETY: 同上。
        groups: unsafe { group_names(token) },
        // SAFETY: 同上。
        group_rids: unsafe { group_rids(token) },
    })
}

/// 令牌里的组名列表（含本地化名与英文规范名，见模块文档）。
///
/// **被标成 `SE_GROUP_USE_FOR_DENY_ONLY` 的组也算数**：UAC 过滤后的管理员令牌里，
/// `Administrators` 正是以「仅用于拒绝」的形式存在的。提权资格问的是
/// 「这个账户属不属于管理员组」，而不是「当前这张令牌现在有没有管理员权限」
/// ——后者是 [`is_elevated`] 的事。漏掉它会让每个管理员都提不了权。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
pub unsafe fn group_names(token: HANDLE) -> Vec<String> {
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
        // SAFETY: item.Sid 指向 buf 内部，该缓冲在本函数返回前一直存活。
        let sid_string = unsafe { sid_to_string(item.Sid) };
        // SAFETY: 同上。
        if let Some(name) = unsafe { account_by_sid(item.Sid) }.map(|a| a.name)
            && !out.contains(&name)
        {
            out.push(name);
        }
        // 英文规范名：本地化的 Windows 上，上面查出来的名字可能是德文/法文的，
        // 而 `session.elevate_groups` 里写的是英文。
        if let Some(s) = &sid_string
            && let Some((_, canon)) = CANONICAL_GROUPS.iter().find(|(k, _)| k == s)
            && !out.contains(&(*canon).to_owned())
        {
            out.push((*canon).to_owned());
        }
    }
    out
}

/// 令牌里所有组 SID 的 RID。
///
/// 供 `WhoAmI::groups`（`Vec<u32>`）用。RID 在 Windows 上**不是**全局唯一的
/// ——两个不同域里可以有相同 RID 的组——所以它只适合展示与「跟 Unix 那边形状
/// 对齐」，任何判断都该用 SID 串或 [`group_names`]。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
pub unsafe fn group_rids(token: HANDLE) -> Vec<u32> {
    // SAFETY: 调用方保证令牌有效。
    let Ok(buf) = (unsafe { token_info(token, TokenGroups) }) else {
        return Vec::new();
    };
    // SAFETY: TokenGroups 的返回布局就是 TOKEN_GROUPS：一个计数后跟变长数组。
    let groups = unsafe { &*(buf.as_ptr().cast::<TOKEN_GROUPS>()) };
    let count = groups.GroupCount as usize;
    // SAFETY: 内核保证 Groups 后面确实跟着 GroupCount 个 SID_AND_ATTRIBUTES。
    let items = unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), count) };

    let mut out = Vec::with_capacity(count);
    for item in items {
        // SAFETY: item.Sid 指向 buf 内部，该缓冲在本函数返回前一直存活。
        let rid = unsafe { rid_of_sid(item.Sid) };
        if !out.contains(&rid) {
            out.push(rid);
        }
    }
    out
}

/// 当前进程的身份。
pub fn current_identity() -> io::Result<TokenIdentity> {
    // SAFETY: GetCurrentProcess 返回伪句柄，永远有效且不需要关闭。
    let token = unsafe { open_process_token(GetCurrentProcess()) }?;
    // SAFETY: token 刚打开，带 TOKEN_QUERY。
    unsafe { identity_of_token(token.raw()) }
}

/// 这张令牌当前是否处于「已提升」状态（UAC 的完整管理员令牌）。
///
/// 与「属于 Administrators 组」不是一回事，见 [`group_names`] 的文档。
///
/// # Safety
///
/// `token` 必须是带 `TOKEN_QUERY` 的有效令牌句柄。
pub unsafe fn token_is_elevated(token: HANDLE) -> bool {
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len: u32 = 0;
    // SAFETY: 缓冲是一个完整的 TOKEN_ELEVATION，长度如实给出。
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&raw mut elevation).cast::<core::ffi::c_void>(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &raw mut len,
        )
    };
    ok != 0 && elevation.TokenIsElevated != 0
}

/// 当前进程是否已提升。
pub fn is_elevated() -> bool {
    // SAFETY: 伪句柄永远有效。
    let Ok(token) = (unsafe { open_process_token(GetCurrentProcess()) }) else {
        return false;
    };
    // SAFETY: token 刚打开，带 TOKEN_QUERY。
    unsafe { token_is_elevated(token.raw()) }
}

/// 把用户名拆成 `(域, 账户)`，供 `LogonUserW` 使用。
///
/// 认三种写法：`DOMAIN\user`、`user@domain`（UPN，域部分原样留在名字里）、
/// 裸 `user`（域取 `.`，即本机）。
pub fn split_account(input: &str) -> (String, String) {
    if let Some((domain, user)) = input.split_once('\\') {
        return (domain.to_owned(), user.to_owned());
    }
    if input.contains('@') {
        // UPN：整串作为名字交给 LSA，域必须为空。
        return (String::new(), input.to_owned());
    }
    (".".to_owned(), input.to_owned())
}

/// `to_wide` 的转发，省得调用方多 `use` 一次。
pub fn wide_of(s: &str) -> Vec<u16> {
    wide::to_wide(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 当前进程身份可解析() {
        let id = current_identity().expect("本进程的令牌总该读得到");
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

    /// 管理员身份运行时，`Administrators` 必须出现在组里——哪怕令牌被 UAC 过滤。
    /// 这是提权资格判定的地基。
    #[test]
    fn 管理员组以规范名出现() {
        let id = current_identity().unwrap();
        if is_elevated() {
            assert!(
                id.groups.iter().any(|g| g == "Administrators"),
                "已提升的进程必须报出 Administrators：{:?}",
                id.groups
            );
        }
    }

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
        assert_eq!(
            split_account("alice"),
            (".".to_owned(), "alice".to_owned())
        );
    }

    #[test]
    fn 账户名的限定形式() {
        let a = AccountName {
            name: "alice".into(),
            domain: "WS".into(),
        };
        assert_eq!(a.qualified(), "WS\\alice");
        let b = AccountName {
            name: "SYSTEM".into(),
            domain: String::new(),
        };
        assert_eq!(b.qualified(), "SYSTEM");
    }

    /// 查不到账户名时用 SID 兜底，绝不给空串——它会进审计的 username 列。
    #[test]
    fn 孤儿_sid_退回_sid_串() {
        let id = TokenIdentity {
            sid: "S-1-5-21-1-2-3-1234".into(),
            account: None,
            uid: 1234,
            gid: 513,
            groups: vec![],
            group_rids: vec![],
        };
        assert_eq!(id.username(), "S-1-5-21-1-2-3-1234");
    }
}
