use super::registry::Registry;
use strixmaid_types::file::FileAccessPurpose;
use tokio::sync::watch;

#[test]
fn credentials_are_scoped_revocable_and_bounded() {
    let registry = Registry::default();
    let (sender, revoked) = watch::channel(false);
    let (entry, secret) = registry
        .create(
            "alice",
            "/file".into(),
            FileAccessPurpose::Download,
            None,
            revoked.clone(),
        )
        .unwrap();
    assert!(!entry.id.contains(&secret));
    let (_, fresh) = registry.renew(&entry.id, "alice", None).unwrap();
    assert!(registry.authorize(&entry.id, &fresh).is_ok());
    assert!(registry.authorize(&entry.id, "wrong").is_err());
    assert!(registry.renew(&entry.id, "bob", Some(&secret)).is_err());
    assert!(registry.remove(&entry.id, "bob").is_err());
    let (_other, reused) = registry
        .create(
            "alice",
            "/other".into(),
            FileAccessPurpose::Preview,
            Some(&secret),
            revoked.clone(),
        )
        .unwrap();
    assert_eq!(secret, reused);
    let (b, bob_secret) = registry
        .create(
            "bob",
            "/file".into(),
            FileAccessPurpose::Download,
            None,
            revoked,
        )
        .unwrap();
    assert!(registry.authorize(&entry.id, &bob_secret).is_err());
    assert!(registry.authorize(&b.id, &secret).is_err());
    let mut leases = Vec::new();
    for _ in 0..4 {
        leases.push(registry.authorize(&entry.id, &secret).unwrap());
    }
    assert!(registry.authorize(&entry.id, &secret).is_err());
    leases.pop();
    assert!(registry.authorize(&entry.id, &secret).is_ok());
    sender.send_replace(true);
    assert!(registry.authorize(&entry.id, &secret).is_err());
    assert!(leases.iter().all(|l| *l.revoked.borrow()));
}

#[test]
fn per_session_records_have_limit_and_deletion_reclaims_capacity() {
    let registry = Registry::default();
    let (_sender, revoked) = watch::channel(false);
    let (first, secret) = registry
        .create(
            "alice",
            "/f".into(),
            FileAccessPurpose::Preview,
            None,
            revoked.clone(),
        )
        .unwrap();
    for _ in 1..32 {
        registry
            .create(
                "alice",
                "/f".into(),
                FileAccessPurpose::Preview,
                Some(&secret),
                revoked.clone(),
            )
            .unwrap();
    }
    assert!(
        registry
            .create(
                "alice",
                "/f".into(),
                FileAccessPurpose::Preview,
                Some(&secret),
                revoked.clone()
            )
            .is_err()
    );
    registry.remove(&first.id, "alice").unwrap();
    assert!(
        registry
            .create(
                "alice",
                "/f".into(),
                FileAccessPurpose::Preview,
                Some(&secret),
                revoked
            )
            .is_ok()
    );
    assert!(registry.authorize(&first.id, &secret).is_err());
}

#[tokio::test]
async fn stream_propagates_early_eof_and_logout_without_polling_body() {
    use axum::body::to_bytes;
    use std::time::Duration;
    use strixmaid_core::session::channel::IpcChannel;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let registry = Registry::default();
    let (_worker_alive, closed) = watch::channel(false);
    let (sender, revoked) = watch::channel(false);
    let (entry, secret) = registry
        .create(
            "alice",
            "/f".into(),
            FileAccessPurpose::Download,
            None,
            revoked,
        )
        .unwrap();
    let lease = registry.authorize(&entry.id, &secret).unwrap();
    let (node, mut worker) = IpcChannel::pair().unwrap();
    let body = super::http::stream_body(node, 10, lease, closed.clone());
    worker.write_all(b"short").await.unwrap();
    drop(worker);
    assert!(to_bytes(body, 100).await.is_err(), "提前EOF不能伪装成成功");

    let lease = registry.authorize(&entry.id, &secret).unwrap();
    let (node, mut worker) = IpcChannel::pair().unwrap();
    let body = super::http::stream_body(node, 1024 * 1024, lease, closed);
    // 不消费 body；撤销仍必须让后台任务放弃通道，不能等 HTTP 再 poll。
    sender.send_replace(true);
    let mut buf = [0; 1];
    let result = tokio::time::timeout(Duration::from_secs(2), worker.read(&mut buf))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
    assert!(to_bytes(body, 1024 * 1024).await.is_err());
}

#[tokio::test]
async fn http_requires_the_right_credential_and_exposes_both_methods() {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use std::sync::Arc;
    use strixmaid_core::{
        config::Config,
        session::{ProcessHelperLauncher, SessionManager, SessionManagerConfig},
        store::Store,
    };
    use tower::ServiceExt;
    let config = Config::default();
    let store = Store::open_in_memory().await.unwrap();
    let sessions = SessionManager::new(
        store,
        SessionManagerConfig::from_config(&config),
        Arc::new(ProcessHelperLauncher::new("unused-test-helper")),
    )
    .await
    .unwrap();
    let st = super::FileAccessState::new(
        crate::auth::AuthState::new(sessions, vec![]),
        &config.files.allowed_roots,
        Arc::new(Registry::default()),
    );
    let (app, spec) = super::router(st).split_for_parts();
    let item = &spec.paths.paths["/file-access/{id}/content"];
    assert!(item.get.is_some());
    assert!(item.head.is_some());
    for (method, path, header, value) in [
        (
            "GET",
            "/file-access/id/content",
            "authorization",
            "Bearer arbitrary",
        ),
        (
            "HEAD",
            "/file-access/id/content",
            "authorization",
            "Bearer arbitrary",
        ),
        ("POST", "/file-access", "cookie", "strixmaid_file=arbitrary"),
        (
            "POST",
            "/file-access/id/renew",
            "cookie",
            "strixmaid_file=arbitrary",
        ),
        (
            "DELETE",
            "/file-access/id",
            "cookie",
            "strixmaid_file=arbitrary",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path}"
        );
    }
}

#[tokio::test]
async fn worker_exit_cancels_a_full_send_queue_without_body_poll() {
    use axum::body::to_bytes;
    use std::time::Duration;
    use strixmaid_core::session::channel::IpcChannel;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let registry = Registry::default();
    let (_session_alive, revoked) = watch::channel(false);
    let (worker_alive, closed) = watch::channel(false);
    let (entry, secret) = registry
        .create("a", "/f".into(), FileAccessPurpose::Download, None, revoked)
        .unwrap();
    let lease = registry.authorize(&entry.id, &secret).unwrap();
    let _others = (0..3)
        .map(|_| registry.authorize(&entry.id, &secret).unwrap())
        .collect::<Vec<_>>();
    let (node, mut worker) = IpcChannel::pair().unwrap();
    let body = super::http::stream_body(node, 1024 * 1024, lease, closed);
    worker.write_all(&[1; 8192]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    worker.write_all(&[2; 8192]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(registry.authorize(&entry.id, &secret).is_err());
    worker_alive.send_replace(true);
    let mut byte = [0; 1];
    let end = tokio::time::timeout(Duration::from_secs(2), worker.read(&mut byte))
        .await
        .unwrap();
    assert!(matches!(end, Ok(0) | Err(_)));
    assert!(
        registry.authorize(&entry.id, &secret).is_ok(),
        "退出立即释放名额"
    );
    assert!(to_bytes(body, 1024 * 1024).await.is_err());
}
