//! 认证与会话：一次登录对话，外加一个活到登出的系统会话句柄。
//!
//! 两个平台各有一套完全不同的系统机制，但对 `main.rs` 暴露的是**同一套方法名**，
//! 因此状态机那边一个 `cfg` 都不需要写。
//!
//! # 一环一环的对应关系
//!
//! | 这一步做什么 | Unix（[`unix`]） | Windows（[`windows`]） |
//! |---|---|---|
//! | 开始对话，记下目标用户 | `pam_start` | 拆 `DOMAIN\user` / `user@upn`，不调用任何系统 API |
//! | 要凭据 | conversation 回调，PAM 说要什么就问什么 | 合成一条 echo-off 的「密码」提示 |
//! | 验证凭据 | `pam_authenticate` | `LogonUserW` |
//! | 账户状态检查 | `pam_acct_mgmt`（+ 过期时 `pam_chauthtok`） | 无独立调用：`LogonUserW` 本身就会因禁用 / 锁定 / 过期而失败 |
//! | 解析身份 | `getpwnam` + `getgrouplist`（NSS） | 读登录令牌的 `TokenUser` / `TokenGroups` |
//! | 建会话 | `pam_setcred` + `pam_open_session` | `LoadUserProfileW` |
//! | 会话环境变量 | `pam_getenvlist` | `CreateEnvironmentBlock` |
//! | 收会话 | `pam_close_session` + `pam_setcred(DELETE)` | `UnloadUserProfile` |
//! | 收句柄 | `pam_end` | `CloseHandle` |
//!
//! # 最大的一处结构差异：轮数
//!
//! PAM 是**对话式**的：模块想问几轮就问几轮（2FA 的验证码、密码过期时的改密），
//! 所以 Unix 侧的提示是 PAM 现给的，helper 只做转发。Windows 的
//! `LogonUserW` 是**一次性**的：一个用户名加一个密码，要么成要么不成，没有回合。
//! 为了让协议两边一样，Windows 侧**自己合成**那一轮提示（见
//! [`windows::Session::authenticate`]）。协议因此不必分平台，
//! 前端也不知道底下是 PAM 还是 LSA。
//!
//! 由此还带来两个「Windows 上恒定」的行为，写在各自方法的文档里：
//! `stashed_info_count` 恒为 0（没有纯信息轮可攒），
//! `user()` 返回的是从令牌里查回来的规范账户名（对应 PAM 模块改写 `PAM_USER`）。
//!
//! # 为什么 `lookup_identity` 是方法而不是自由函数
//!
//! Unix 上「认证通过的用户是谁」可以只凭名字重查一遍（`getpwnam`）；
//! Windows 上不行——身份的权威来源是 `LogonUserW` 给出的**那张令牌**，
//! 按名字再查一次既多一次域访问，也给了「认证的账户与解析出的账户不是同一个」
//! 留下缝隙。所以它取 `&self`，Unix 实现忽略 `self`。

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

use std::path::PathBuf;

use strixmaid_types::auth::AuthUser;

#[cfg(unix)]
pub use unix::{PamError as AuthError, Pam as Session};
#[cfg(windows)]
pub use windows::{AuthError, Session};

/// 认证通过后记住的身份，供 `SpawnWorker` 使用。
///
/// 三个字段在两个平台上的来源见[模块文档](self)的对照表。`home` / `shell`
/// 在 Windows 上分别来自用户环境块里的 `USERPROFILE` 与 `ComSpec`，
/// 取不到时留空——[`crate::spawn`] 把空路径当成「不指定」，而不是拿一个
/// 猜来的路径去试。
pub struct Identity {
    /// 协议里的用户身份（uid / gid / 用户名 / 组名列表）。
    pub user: AuthUser,
    /// 家目录：worker 的工作目录。
    pub home: PathBuf,
    /// 登录 shell（Windows 上是 `%ComSpec%`）。目前只作为环境变量传给 worker。
    pub shell: PathBuf,
}
