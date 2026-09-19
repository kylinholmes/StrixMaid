//! 把这台主机的 API 摆到**一条流**上，而不是一个 TCP 监听器上。
//!
//! # 为什么需要这个
//!
//! `11-multi-host.md` 的约束 4：A 是管道不是翻译器——A 不为任何端点写转发代码，
//! B 上跑的是完整的 `Router`，A 只搬字节。要让这句话成立，B 必须能在「一条
//! 拿到手的双向流」上提供 HTTP，因为经多路复用送过来的就是这么个东西：
//! 没有监听器、没有 accept、没有对端地址。
//!
//! [`serve_connection`] 就是那道口子。它对流的来历一无所知——yamux 的一条逻辑流、
//! 一个 `tokio::io::duplex`、还是一个真的 `TcpStream`，对它没有区别。
//!
//! # 对端地址要由调用方给
//!
//! 走 TCP 时 `ConnectInfo` 由 axum 的 `into_make_service_with_connect_info` 填，
//! 审计与会话记录靠它记下客户端地址。一条多路复用流上没有这个东西——它的「对端」
//! 在协议上是上级 Server，在语义上是坐在浏览器前的那个人。
//!
//! 这里**不替调用方猜**：`peer` 传什么就记什么，传 `None` 就照
//! [`crate::auth::exec::RequestOrigin`] 既有的口径退化（审计少一列，请求照常处理）。
//! 猜一个地址比留空更糟——审计里一个错的来源地址会把人引到别处去。

use std::net::SocketAddr;

use axum::extract::ConnectInfo;
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tower::Service as _;

/// 在一条流上提供 `router`，直到对端关闭或出错。
///
/// `peer` 会作为 `ConnectInfo` 放进每个请求的 extensions，见模块文档。
///
/// 开了 `with_upgrades`：`Upgrade: websocket` 因此端到端有效，终端与 live 频道
/// 不需要另一套转发代码。**注意**这只是说这一层不挡着；升级能不能真的穿过
/// 整条管道，要等 yamux 那一层接上之后才谈得上验证。
pub async fn serve_connection<I>(
    router: axum::Router,
    io: I,
    peer: Option<SocketAddr>,
) -> anyhow::Result<()>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |mut req: Request<Incoming>| {
        if let Some(addr) = peer {
            req.extensions_mut().insert(ConnectInfo(addr));
        }
        // `Router` 每次调用要一个独立的 `&mut`，克隆是廉价的（内部是 Arc）。
        router.clone().call(req)
    });

    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(io), service)
        .with_upgrades()
        .await
        .map_err(|e| anyhow::anyhow!("流上的 HTTP 连接异常结束: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::StatusCode;

    /// 在一条内存流上跑真正的 node router。
    ///
    /// 这条用例证明的是 `11-multi-host.md` §3.1 的中心主张：B 侧没有任何转发
    /// 逻辑，它收到的就是一个 `AsyncRead + AsyncWrite`，上面跑着真 HTTP。
    /// 这一条不成立，整个「A 是管道不是翻译器」的设计就要推翻重来。
    #[tokio::test]
    async fn 完整的_router_能跑在一条内存流上() {
        let dir = std::env::temp_dir().join(format!("strixmaid-onstream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config = strixmaid_core::config::Config {
            data_dir: dir.clone(),
            ..Default::default()
        };
        let node = crate::Node::start(config, &crate::NoReporter, None)
            .await
            .expect("Node 应当起得来");

        // 一对内存流，两端互为对方的读写端——正是一条多路复用逻辑流的形状。
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let served = tokio::spawn(serve_connection(node.router(None), server_io, None));

        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(client_io))
                .await
                .expect("客户端握手");
        let pumping = tokio::spawn(conn);

        let health = sender
            .send_request(
                Request::get("/api/v1/health")
                    .header("host", "node")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("请求应当走通");
        assert_eq!(health.status(), StatusCode::OK);

        // 鉴权照旧生效：这条流上跑的是同一个 Router，不是一个放宽了的副本。
        let protected = sender
            .send_request(
                Request::get("/api/v1/services")
                    .header("host", "node")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("请求应当走通");
        assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);

        drop(sender);
        let _ = pumping.await;
        let _ = served.await;
        node.shutdown(crate::ShutdownKind::Graceful).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
