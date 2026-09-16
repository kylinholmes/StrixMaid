//! uid ↔ 用户名映射。
//!
//! # Unix
//!
//! 直接解析 `/etc/passwd`，按文件 mtime 缓存。不走 `getpwuid`：静态 musl 下 NSS
//! 不可用，LDAP / SSSD 用户本来就解析不到；直读 `/etc/passwd` 至少行为是确定的。
//! NSS 代理是 helper 的 P1 职责（`docs/design.md` §10）。
//!
//! # Windows
//!
//! 没有 passwd 文件，两个方向也不对称，所以只实现真正用得到的那一个：
//!
//! | 方向 | Windows 上怎么做 |
//! |---|---|
//! | 名字 → uid（[`UserDb::uid_of`]） | `LookupAccountNameW` 拿到 SID，再取 RID |
//! | uid → 名字（[`UserDb::name_of`]） | **不实现**，恒为 `None` |
//!
//! 反方向做不了是 RID 的性质决定的：SID 能确定地映射成 RID，反过来不行——
//! 一个 RID 在不同域里对应不同账户，没有「按 RID 查账户」的系统接口。
//!
//! 这不构成缺失：进程列表里的用户名由 `providers::process::windows` 在枚举时
//! **从每个进程自己的令牌 SID** 直接解析（`platform::windows::token::account_by_sid`），
//! 那条路拿到的是完整 SID，不经过 RID 这一层有损的中转。本模块在 Windows 上
//! 只为「按用户名过滤进程」而存在。
//!
//! 空实现会让过滤**静默匹配不到任何进程**，比报错更难查，所以这里真的实现了
//! `uid_of`，而不是留一个恒为 `None` 的桩。

use std::collections::HashMap;
use std::sync::Arc;

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::SystemTime;

#[cfg(unix)]
const PASSWD: &str = "/etc/passwd";

/// 一份不可变的用户表快照。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct UserTable {
    by_uid: HashMap<u32, String>,
    by_name: HashMap<String, u32>,
}

impl UserTable {
    /// 解析 passwd 文本。同一 uid 多个名字时取第一个（`getpwuid` 的行为）。
    pub fn parse(raw: &str) -> Self {
        let mut t = UserTable::default();
        for line in raw.lines() {
            let mut fields = line.split(':');
            let (Some(name), Some(_pw), Some(uid)) = (fields.next(), fields.next(), fields.next()) else {
                continue;
            };
            let Ok(uid) = uid.parse::<u32>() else {
                continue;
            };
            t.by_uid.entry(uid).or_insert_with(|| name.to_owned());
            t.by_name.entry(name.to_owned()).or_insert(uid);
        }
        t
    }

    pub fn name_of(&self, uid: u32) -> Option<&str> {
        self.by_uid.get(&uid).map(String::as_str)
    }

    pub fn uid_of(&self, name: &str) -> Option<u32> {
        self.by_name.get(name).copied()
    }

    pub fn len(&self) -> usize {
        self.by_uid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_uid.is_empty()
    }
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct Cache {
    mtime: Option<SystemTime>,
    table: Arc<UserTable>,
}

/// 带 mtime 失效的 `/etc/passwd` 缓存。
#[cfg(unix)]
#[derive(Debug, Default)]
pub struct UserDb {
    cache: Mutex<Cache>,
}

#[cfg(unix)]
impl UserDb {
    pub fn new() -> Self {
        Self::default()
    }

    /// 取当前快照；`/etc/passwd` 的 mtime 变了就重新读。一次列表只调一次，之后全在快照上查。
    pub fn snapshot(&self) -> Arc<UserTable> {
        let mtime = fs::metadata(PASSWD).and_then(|m| m.modified()).ok();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.mtime != mtime || (cache.table.is_empty() && mtime.is_some()) {
            let raw = fs::read_to_string(PASSWD).unwrap_or_default();
            cache.table = Arc::new(UserTable::parse(&raw));
            cache.mtime = mtime;
        }
        Arc::clone(&cache.table)
    }

    pub fn name_of(&self, uid: u32) -> Option<String> {
        self.snapshot().name_of(uid).map(str::to_owned)
    }

    pub fn uid_of(&self, name: &str) -> Option<u32> {
        self.snapshot().uid_of(name)
    }
}

/// 见 Unix 版。Windows 上没有可缓存的用户表，只在需要时查一次账户。
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct UserDb;

#[cfg(windows)]
impl UserDb {
    pub fn new() -> Self {
        Self
    }

    /// Windows 上恒为空表，见模块文档。
    ///
    /// 保留这个方法只为让 `ProcProvider` 的上下文构造不必写 `cfg`；
    /// Windows 的进程枚举不读它。
    pub fn snapshot(&self) -> Arc<UserTable> {
        Arc::new(UserTable::default())
    }

    /// uid → 名字在 Windows 上做不到，见模块文档。
    pub fn name_of(&self, _uid: u32) -> Option<String> {
        None
    }

    /// 名字 → uid：`LookupAccountNameW` 拿 SID，再取 RID。
    ///
    /// 接受 `alice`、`DOMAIN\alice`、`alice@example.com` 三种写法——
    /// `LookupAccountNameW` 本身就认这三种，不必在这里先拆。
    ///
    /// 不加缓存：这个方法每次「按用户名过滤进程列表」只会被调用一次，
    /// 而 `LookupAccountNameW` 对本地账户是一次注册表查询。域账户可能走网络，
    /// 但那种环境下过滤本来就不是热路径。
    pub fn uid_of(&self, name: &str) -> Option<u32> {
        use crate::platform::windows::token::{SID_LOCAL_SYSTEM, rid_of_sid, sid_to_string};
        use crate::platform::windows::wide::to_wide;
        use windows_sys::Win32::Security::{LookupAccountNameW, SID_NAME_USE};

        if name.is_empty() {
            return None;
        }
        let wide = to_wide(name);
        let mut sid_len: u32 = 0;
        let mut domain_len: u32 = 0;
        let mut kind: SID_NAME_USE = 0;
        // 第一次只问两个缓冲要多大；必然失败，这里不看返回值。
        // SAFETY: 两个缓冲指针为空时函数只回填两个长度；name 以 NUL 结尾。
        unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                wide.as_ptr(),
                std::ptr::null_mut(),
                &raw mut sid_len,
                std::ptr::null_mut(),
                &raw mut domain_len,
                &raw mut kind,
            );
        }
        if sid_len == 0 {
            return None;
        }

        let mut sid = vec![0u8; sid_len as usize];
        let mut domain = vec![0u16; domain_len.max(1) as usize];
        // SAFETY: 两个缓冲各有上一步问出的容量，长度变量如实描述它们。
        let ok = unsafe {
            LookupAccountNameW(
                std::ptr::null(),
                wide.as_ptr(),
                sid.as_mut_ptr().cast(),
                &raw mut sid_len,
                domain.as_mut_ptr(),
                &raw mut domain_len,
                &raw mut kind,
            )
        };
        if ok == 0 {
            return None;
        }

        let psid = sid.as_ptr().cast_mut().cast();
        // LocalSystem 映射成 0，与 `platform::windows::token` 的规则一致——
        // 两处分叉会让「按 SYSTEM 过滤」在这里查到 18、在进程表里显示 0。
        // SAFETY: psid 指向紧邻上方由 LookupAccountNameW 填好的 sid 缓冲，
        // 该缓冲在本作用域内一直存活，调用期间不会被移动或释放。
        let uid = if unsafe { sid_to_string(psid) }.as_deref() == Some(SID_LOCAL_SYSTEM) {
            0
        } else {
            // SAFETY: 同上。
            unsafe { rid_of_sid(psid) }
        };
        Some(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析_passwd() {
        let raw = "root:x:0:0:root:/root:/bin/bash\nwww-data:x:33:33:www-data:/var/www:/usr/sbin/nologin\nbroken line\nalias:x:0:0::/:/bin/sh\n";
        let t = UserTable::parse(raw);
        assert_eq!(t.name_of(0), Some("root"));
        assert_eq!(t.name_of(33), Some("www-data"));
        assert_eq!(t.name_of(1), None);
        assert_eq!(t.uid_of("www-data"), Some(33));
        assert_eq!(t.uid_of("alias"), Some(0));
        assert_eq!(t.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn 本机_root_可解析() {
        let db = UserDb::new();
        assert_eq!(db.name_of(0).as_deref(), Some("root"));
        assert_eq!(db.uid_of("root"), Some(0));
        // 第二次走缓存，结果一致
        assert!(Arc::ptr_eq(&db.snapshot(), &db.snapshot()));
    }

    /// 上一条在 Windows 上的对应物：那里没有 `/etc/passwd`，快照就该是**空的**。
    ///
    /// 断言「空」而不是跳过：Windows 上进程的用户名另有来源（令牌里的 SID →
    /// 账户名，见 `providers/process/windows.rs`），本模块绝不能拿别的东西冒充。
    /// uid → 名字同理恒为 `None`，理由见模块文档。
    #[cfg(windows)]
    #[test]
    fn windows_上没有用户表也没有反向映射() {
        let db = UserDb::new();
        assert!(db.snapshot().is_empty(), "Windows 上不该解析出任何用户表");
        assert_eq!(db.name_of(0), None);
        assert_eq!(db.name_of(1001), None);
    }

    /// 名字 → uid 必须**真的能解析**，否则「按用户名过滤进程」会静默地
    /// 一个都匹配不到——那比报错难查得多。
    ///
    /// 用 `SYSTEM` 做判据：它是内建账户，任何 Windows 上都存在，且按本项目的
    /// 映射规则必须是 0（`platform::windows::token` 的模块文档）。
    #[cfg(windows)]
    #[test]
    fn windows_上按名字能查到_uid() {
        let db = UserDb::new();
        // 本地化系统上 SYSTEM 的显示名会被翻译，但 `LookupAccountNameW` 认英文名。
        let Some(uid) = db.uid_of("SYSTEM") else {
            eprintln!("跳过：本机查不到内建账户 SYSTEM（可能是受限的域环境）");
            return;
        };
        assert_eq!(uid, 0, "LocalSystem 必须映射成 uid 0，与令牌那一侧一致");

        // 当前登录用户也该查得到，且与令牌报出来的 uid 相同——
        // 这一条把「过滤用的 uid」与「进程表里显示的 uid」钉在一起。
        let me = crate::worker::whoami();
        if let Some(name) = me.user.as_deref() {
            let bare = name.rsplit('\\').next().unwrap_or(name);
            match db.uid_of(bare) {
                Some(uid) => assert_eq!(
                    uid, me.uid,
                    "按名字查到的 uid 与令牌报告的不一致：{bare}"
                ),
                None => eprintln!("跳过：查不到当前账户 {bare}"),
            }
        }
    }

    /// 查不到的名字给 `None`，空串也不例外——绝不能编一个 uid 出来。
    #[cfg(windows)]
    #[test]
    fn windows_上查不到的名字给_none() {
        let db = UserDb::new();
        assert_eq!(db.uid_of(""), None);
        assert_eq!(db.uid_of("这个账户一定不存在-9f3a2b"), None);
    }
}
