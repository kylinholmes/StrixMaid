//! uid ↔ 用户名映射：直接解析 `/etc/passwd`，按文件 mtime 缓存。
//!
//! 不走 `getpwuid`：静态 musl 下 NSS 不可用，LDAP / SSSD 用户本来就解析不到；
//! 直读 `/etc/passwd` 至少行为是确定的。NSS 代理是 helper 的 P1 职责（`docs/design.md` §10）。

use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

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

#[derive(Debug, Default)]
struct Cache {
    mtime: Option<SystemTime>,
    table: Arc<UserTable>,
}

/// 带 mtime 失效的 `/etc/passwd` 缓存。
#[derive(Debug, Default)]
pub struct UserDb {
    cache: Mutex<Cache>,
}

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

    #[test]
    fn 本机_root_可解析() {
        let db = UserDb::new();
        assert_eq!(db.name_of(0).as_deref(), Some("root"));
        assert_eq!(db.uid_of("root"), Some(0));
        // 第二次走缓存，结果一致
        assert!(Arc::ptr_eq(&db.snapshot(), &db.snapshot()));
    }
}
