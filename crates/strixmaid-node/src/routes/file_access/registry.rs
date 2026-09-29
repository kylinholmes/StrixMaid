//! 短期文件凭证只保存摘要；会话撤销同时作废记录和运行中的流。
use hmac::{Hmac, KeyInit, Mac};
use rand::Rng;
use sha2::Sha256;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use strixmaid_core::session::hash_token;
use strixmaid_types::{ApiError, ApiResult, ErrorCode, file::FileAccessPurpose};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

pub const TTL: Duration = Duration::from_secs(600);
pub const COOKIE: &str = "strixmaid_file";

#[derive(Clone)]
pub struct Entry {
    pub id: String,
    pub owner: String,
    pub path: String,
    pub purpose: FileAccessPurpose,
    expires: Instant,
}
struct Owner {
    secret_hash: String,
    expires: Instant,
    revoked: watch::Receiver<bool>,
    slots: Arc<Semaphore>,
}
#[derive(Default)]
struct Records {
    entries: HashMap<String, Entry>,
    owners: HashMap<String, Owner>,
}
pub struct Registry {
    key: [u8; 32],
    records: Mutex<Records>,
    slots: Arc<Semaphore>,
}
pub struct Lease {
    pub entry: Entry,
    pub revoked: watch::Receiver<bool>,
    _session_slot: OwnedSemaphorePermit,
    _node_slot: OwnedSemaphorePermit,
}
impl Default for Registry {
    fn default() -> Self {
        let mut key = [0; 32];
        rand::rng().fill_bytes(&mut key);
        Self {
            key,
            records: Mutex::new(Records::default()),
            slots: Arc::new(Semaphore::new(32)),
        }
    }
}
fn random_secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}
fn invalid() -> ApiError {
    ApiError::unauthenticated("文件访问记录不存在或已失效")
}
fn busy() -> ApiError {
    ApiError::new(
        ErrorCode::Conflict,
        "文件访问数量已达上限，请关闭其他预览或下载后重试",
    )
}
fn alive(revoked: &watch::Receiver<bool>) -> bool {
    !*revoked.borrow() && revoked.has_changed().is_ok()
}
fn matches(secret: &str, digest: &str) -> bool {
    let candidate = hash_token(secret);
    candidate
        .bytes()
        .zip(digest.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
        && candidate.len() == digest.len()
}
impl Records {
    fn prune(&mut self) {
        let now = Instant::now();
        self.owners.retain(|_, o| {
            // 活动下载允许跨过 Cookie 到期；它们仍共用原会话的并发名额。
            alive(&o.revoked) && (o.expires > now || o.slots.available_permits() < 4)
        });
        self.entries.retain(|_, e| {
            e.expires > now && self.owners.get(&e.owner).is_some_and(|o| o.expires > now)
        });
    }
}
impl Registry {
    /// 用进程内随机密钥派生同一会话的文件 Cookie；并发初始化得到同一值。
    /// 注册表只保存 Cookie 摘要，密钥不持久化，重启会话也已失效。
    fn secret(&self, owner: &str) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts 32-byte keys");
        mac.update(b"strixmaid:file-access:v1:");
        mac.update(owner.as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }
    pub fn prune(&self) {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prune();
    }
    pub fn create(
        &self,
        owner: &str,
        path: String,
        purpose: FileAccessPurpose,
        _cookie: Option<&str>,
        revoked: watch::Receiver<bool>,
    ) -> ApiResult<(Entry, String)> {
        if !alive(&revoked) {
            return Err(invalid());
        }
        let mut r = self.records.lock().unwrap_or_else(|e| e.into_inner());
        r.prune();
        if r.entries.len() >= 4096 || r.entries.values().filter(|e| e.owner == owner).count() >= 32
        {
            return Err(busy());
        }
        let secret = self.secret(owner);
        let expires = Instant::now() + TTL;
        let o = r.owners.entry(owner.to_owned()).or_insert_with(|| Owner {
            secret_hash: hash_token(&secret),
            expires,
            revoked,
            slots: Arc::new(Semaphore::new(4)),
        });
        o.expires = expires;
        let entry = Entry {
            id: random_secret(),
            owner: owner.to_owned(),
            path,
            purpose,
            expires,
        };
        r.entries.insert(entry.id.clone(), entry.clone());
        Ok((entry, secret))
    }
    pub fn authorize(&self, id: &str, cookie: &str) -> ApiResult<Lease> {
        let mut r = self.records.lock().unwrap_or_else(|e| e.into_inner());
        r.prune();
        let entry = r.entries.get(id).ok_or_else(invalid)?.clone();
        let owner = r.owners.get(&entry.owner).ok_or_else(invalid)?;
        if owner.expires <= Instant::now() || !matches(cookie, &owner.secret_hash) {
            return Err(invalid());
        }
        let session_slot = owner
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let node_slot = self.slots.clone().try_acquire_owned().map_err(|_| busy())?;
        Ok(Lease {
            entry,
            revoked: owner.revoked.clone(),
            _session_slot: session_slot,
            _node_slot: node_slot,
        })
    }
    pub fn renew(
        &self,
        id: &str,
        owner: &str,
        _cookie: Option<&str>,
    ) -> ApiResult<(Entry, String)> {
        let mut r = self.records.lock().unwrap_or_else(|e| e.into_inner());
        r.prune();
        r.entries
            .get(id)
            .filter(|e| e.owner == owner)
            .ok_or_else(invalid)?;
        let o = r.owners.get_mut(owner).ok_or_else(invalid)?;
        let secret = self.secret(owner);
        o.expires = Instant::now() + TTL;
        let e = r.entries.get_mut(id).ok_or_else(invalid)?;
        e.expires = Instant::now() + TTL;
        Ok((e.clone(), secret))
    }
    pub fn remove(&self, id: &str, owner: &str) -> ApiResult<()> {
        let mut r = self.records.lock().unwrap_or_else(|e| e.into_inner());
        r.prune();
        if let Some(e) = r.entries.get(id) {
            if e.owner != owner {
                return Err(invalid());
            }
            r.entries.remove(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expiry_and_parallel_first_cookies() {
        let registry = Registry::default();
        let (_sender, revoked) = watch::channel(false);
        let (a, secret_a) = registry
            .create(
                "a",
                "/a".into(),
                FileAccessPurpose::Preview,
                None,
                revoked.clone(),
            )
            .unwrap();
        let (b, secret_b) = registry
            .create("a", "/b".into(), FileAccessPurpose::Preview, None, revoked)
            .unwrap();
        assert!(registry.authorize(&a.id, &secret_b).is_ok());
        assert!(registry.authorize(&b.id, &secret_a).is_ok());
        registry
            .records
            .lock()
            .unwrap()
            .entries
            .get_mut(&a.id)
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(registry.authorize(&a.id, &secret_a).is_err());
        assert!(registry.authorize(&b.id, &secret_b).is_ok());
        registry
            .records
            .lock()
            .unwrap()
            .owners
            .get_mut("a")
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert!(registry.authorize(&b.id, &secret_b).is_err());
        assert!(registry.records.lock().unwrap().entries.is_empty());
    }
}

#[cfg(test)]
mod stream_limits_tests {
    use super::*;
    #[test]
    fn expired_credentials_do_not_reset_active_stream_slots() {
        let registry = Registry::default();
        let (_sender, revoked) = watch::channel(false);
        let (a, secret) = registry
            .create(
                "a",
                "/f".into(),
                FileAccessPurpose::Download,
                None,
                revoked.clone(),
            )
            .unwrap();
        let mut leases = (0..4)
            .map(|_| registry.authorize(&a.id, &secret).unwrap())
            .collect::<Vec<_>>();
        registry
            .records
            .lock()
            .unwrap()
            .owners
            .get_mut("a")
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        registry.prune();
        let (new, new_secret) = registry
            .create("a", "/f".into(), FileAccessPurpose::Download, None, revoked)
            .unwrap();
        assert!(registry.authorize(&new.id, &new_secret).is_err());
        leases.pop();
        assert!(registry.authorize(&new.id, &new_secret).is_ok());
    }
    #[test]
    fn thirty_two_concurrent_cookie_initializations_share_one_secret() {
        let registry = Registry::default();
        let (_sender, revoked) = watch::channel(false);
        let mut results = Vec::new();
        for _ in 0..32 {
            results.push(
                registry
                    .create(
                        "a",
                        "/f".into(),
                        FileAccessPurpose::Preview,
                        None,
                        revoked.clone(),
                    )
                    .unwrap(),
            );
        }
        for (entry, secret) in &results {
            assert_eq!(secret, &results[0].1);
            assert!(registry.authorize(&entry.id, secret).is_ok());
        }
    }
}
