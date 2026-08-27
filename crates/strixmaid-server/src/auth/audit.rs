//! 审计写入（`roadmap/02-audit.md`）。
//!
//! 记录「谁、何时、对什么、以何种权限、结果如何、来自哪里」。
//!
//! # 写入点只有两处
//!
//! 1. [`crate::auth::exec`] 的调用出口——**全部写操作的必经之路**，因此审计
//!    不会漏，也不用散落到每个路由里；
//! 2. 认证路由与会话回收——登录、提权、登出、超时。
//!
//! **读操作不审计**（`roadmap/02` §4.1）。读的量比写大两三个数量级，全记下来
//! 只会把真正要看的那几条淹掉。
//!
//! # 绝不写入凭据
//!
//! `design.md` §5.3 的硬约束：密码不进日志、不入库。审计记录的 `params` 只来自
//! **RPC 参数**（unit 名、pid、主机名这类），而认证事件的参数根本不进 `params`
//! ——`PromptResponse` 里的 `Zeroizing<String>` 连 `Serialize` 都没实现，
//! 编译期就到不了这里。[`record`] 的签名也只接受已序列化好的 JSON 值，
//! 不接受任意结构体，减少「顺手把整个请求塞进去」的可能。
//!
//! # 写入失败不阻断请求
//!
//! 审计写不进去是运维问题（磁盘满、库损坏），不该让用户的操作跟着失败——
//! 那会把一个可观测性故障放大成可用性故障。失败只记 `tracing::error`。

use std::net::SocketAddr;

use axum::http::HeaderMap;
use serde_json::Value;
use strixmaid_core::session::Session;
use strixmaid_core::store::{AuditOutcome, NewAuditEntry, Store};
use strixmaid_types::{ApiError, ErrorCode};

/// 一次审计写入的全部内容。
///
/// 用结构体而不是一长串位置参数：这些字段里有四个都是 `Option<String>`，
/// 位置参数极易串位，而串位的审计记录比没有记录更糟——它是错的却看着像真的。
#[derive(Debug, Clone)]
pub struct Record<'a> {
    /// 动作，点分标识（`service.restart` / `auth.login`）。
    pub action: &'a str,
    /// 操作目标（unit 名、pid、主机名）。
    pub target: Option<String>,
    /// 去掉 target 之后剩下的参数。**不得含凭据。**
    pub params: Option<Value>,
    /// 结果。
    pub outcome: AuditOutcome,
    /// 补充说明，通常是错误消息。
    pub detail: Option<String>,
}

impl<'a> Record<'a> {
    /// 最小构造。
    pub fn new(action: &'a str, outcome: AuditOutcome) -> Self {
        Record {
            action,
            target: None,
            params: None,
            outcome,
            detail: None,
        }
    }

    #[must_use]
    pub fn target(mut self, t: impl Into<String>) -> Self {
        self.target = Some(t.into());
        self
    }

    #[must_use]
    pub fn params(mut self, p: Value) -> Self {
        self.params = Some(p);
        self
    }

    #[must_use]
    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.detail = Some(d.into());
        self
    }
}

/// 按会话写一条审计。
pub async fn record(store: &Store, session: &Session, remote: Option<&str>, rec: Record<'_>) {
    let mut entry = NewAuditEntry::new(
        session.node.clone(),
        session.user.username.clone(),
        rec.action,
        rec.outcome,
    )
    .actor(i64::from(session.user.uid), session.elevated);
    entry = fill(entry, remote, rec);
    write(store, entry).await;
}

/// 写一条**还没有会话**的审计（登录失败、登录成功之前的那一刻）。
///
/// 登录失败时用请求里给的用户名，那是唯一已知的信息；它可能是伪造的，
/// 但「有人用这个名字尝试登录并失败了」本身就是要记的事实。
pub async fn record_anonymous(
    store: &Store,
    node_id: &str,
    username: &str,
    remote: Option<&str>,
    rec: Record<'_>,
) {
    let entry = NewAuditEntry::new(node_id, username, rec.action, rec.outcome);
    let entry = fill(entry, remote, rec);
    write(store, entry).await;
}

fn fill(mut entry: NewAuditEntry, remote: Option<&str>, rec: Record<'_>) -> NewAuditEntry {
    if let Some(t) = rec.target {
        entry = entry.target(t);
    }
    if let Some(p) = rec.params
        && !p.is_null()
    {
        // 序列化失败时宁可丢掉 params 也要把这条记录写进去——
        // 「谁在什么时候做了什么」比「参数细节」重要得多。
        match serde_json::to_string(&p) {
            Ok(s) => entry = entry.params(s),
            Err(e) => tracing::warn!(error = %e, "审计参数序列化失败，本条记录不带 params"),
        }
    }
    if let Some(d) = rec.detail {
        entry = entry.detail(d);
    }
    if let Some(r) = remote {
        entry = entry.remote_addr(r);
    }
    entry
}

async fn write(store: &Store, entry: NewAuditEntry) {
    if let Err(e) = store.audit_write(&entry).await {
        // 见模块文档：审计写不进去不该让用户的操作跟着失败。
        tracing::error!(error = %e, action = %entry.action, "写审计记录失败");
    }
}

/// 把 API 错误映射成审计结果。
///
/// 「被拒绝」与「出错了」必须分开：前者是系统按策略正常工作（polkit 拒了、
/// 没提权），后者是真的出了故障。混成一类，事后翻审计时就分不清
/// 「这个人被挡住了」和「这台机器坏了」。
pub fn outcome_of(result: &Result<(), &ApiError>) -> AuditOutcome {
    match result {
        Ok(()) => AuditOutcome::Ok,
        Err(e) => match e.code {
            ErrorCode::PermissionDenied | ErrorCode::ElevationRequired => AuditOutcome::Denied,
            _ => AuditOutcome::Error,
        },
    }
}

/// 判定这次请求的来源地址。
///
/// **只有直连地址在 `trusted_proxies` 里时才采信 `X-Forwarded-For`**
/// （`roadmap/02` §4.2）。否则任何客户端加一个请求头就能把审计里的来源地址
/// 写成别人的——那条记录会变成主动的误导，比没有记录更坏。
///
/// 采信时取 XFF 的**第一项**：那是最初的客户端，后面依次是各级代理。
pub fn remote_addr(
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    trusted_proxies: &[String],
) -> Option<String> {
    let peer_ip = peer.map(|a| a.ip().to_string());

    let trusted = peer_ip
        .as_deref()
        .is_some_and(|ip| trusted_proxies.iter().any(|t| t == ip));
    if trusted
        && let Some(first) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return Some(first.to_owned());
    }

    peer.map(|a| a.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(xff: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = xff {
            h.insert("x-forwarded-for", HeaderValue::from_str(v).unwrap());
        }
        h
    }

    fn peer(s: &str) -> Option<SocketAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn 没有代理头时用直连地址() {
        assert_eq!(
            remote_addr(&headers(None), peer("203.0.113.5:44444"), &[]),
            Some("203.0.113.5:44444".to_owned())
        );
    }

    #[test]
    fn 直连地址不在信任列表时不采信代理头() {
        // 这是最要紧的一条：默认空列表下，任何人都能伪造 X-Forwarded-For
        let got = remote_addr(
            &headers(Some("1.2.3.4")),
            peer("203.0.113.5:44444"),
            &[],
        );
        assert_eq!(
            got,
            Some("203.0.113.5:44444".to_owned()),
            "不信任的来源发来的 XFF 必须被忽略"
        );

        // 列表里有别的地址，也不该采信
        let got = remote_addr(
            &headers(Some("1.2.3.4")),
            peer("203.0.113.5:44444"),
            &["127.0.0.1".to_owned()],
        );
        assert_eq!(got, Some("203.0.113.5:44444".to_owned()));
    }

    #[test]
    fn 直连地址在信任列表时采信代理头的第一项() {
        let got = remote_addr(
            &headers(Some("1.2.3.4, 10.0.0.1, 10.0.0.2")),
            peer("127.0.0.1:8080"),
            &["127.0.0.1".to_owned()],
        );
        assert_eq!(got, Some("1.2.3.4".to_owned()), "第一项才是最初的客户端");
    }

    #[test]
    fn 信任但没有代理头时退回直连地址() {
        assert_eq!(
            remote_addr(&headers(None), peer("127.0.0.1:8080"), &["127.0.0.1".to_owned()]),
            Some("127.0.0.1:8080".to_owned())
        );
        // 空的 XFF 同理
        assert_eq!(
            remote_addr(&headers(Some("  ")), peer("127.0.0.1:8080"), &["127.0.0.1".to_owned()]),
            Some("127.0.0.1:8080".to_owned())
        );
    }

    #[test]
    fn ipv6_的信任判断按_ip_而非_ip_端口() {
        let got = remote_addr(
            &headers(Some("1.2.3.4")),
            peer("[::1]:8080"),
            &["::1".to_owned()],
        );
        assert_eq!(got, Some("1.2.3.4".to_owned()));
    }

    #[test]
    fn 拿不到直连地址时返回_none() {
        assert_eq!(remote_addr(&headers(Some("1.2.3.4")), None, &[]), None);
    }

    #[test]
    fn 被拒与出错必须分开() {
        let denied = ApiError::permission_denied("polkit 拒绝");
        assert_eq!(outcome_of(&Err(&denied)), AuditOutcome::Denied);

        let elevation = ApiError::new(ErrorCode::ElevationRequired, "需要管理访问");
        assert_eq!(outcome_of(&Err(&elevation)), AuditOutcome::Denied);

        let boom = ApiError::internal("磁盘炸了");
        assert_eq!(outcome_of(&Err(&boom)), AuditOutcome::Error);

        let missing = ApiError::not_found("没这个 unit");
        assert_eq!(
            outcome_of(&Err(&missing)),
            AuditOutcome::Error,
            "「找不到」不是「被拒绝」"
        );

        assert_eq!(outcome_of(&Ok(())), AuditOutcome::Ok);
    }
}
