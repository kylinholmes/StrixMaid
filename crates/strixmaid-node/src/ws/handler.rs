//! `GET /ws`：axum 升级处理器，把 `WebSocket` 适配成 [`Hub::serve`] 需要的流与汇。

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, State};
use axum::response::Response;
use axum::routing::get;
use futures::{SinkExt, StreamExt, future};
use strixmaid_core::session::Session;

use super::hub::{Frame, Hub, SubscribeContext};

/// `/ws` 路由，状态已注入；对外层 Router 的状态类型 `S` 泛型。
///
/// WebSocket 端点没有 OpenAPI 描述，因此返回普通 `Router` 而不是 `OpenApiRouter`，
/// 由宿主在顶层（不是 `/api/v1` 下）`merge` 进去。
pub fn router<S>(hub: Arc<Hub>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/ws", get(upgrade)).with_state(hub)
}

/// 升级为 WebSocket。
///
/// `Extension<Session>` 由 `auth::middleware::require_auth` 在**升级之前**放进
/// extensions（未认证的请求根本走不到这里），因此这里拿到的一定是一个有效会话。
/// 把它带进 [`SubscribeContext`] 是本端点存在身份概念的唯一途径——升级之后就没有
/// 请求了，`logs.follow` 要挑哪个 worker 全靠这一份会话。
///
/// 会话在连接建立时取一次快照，之后不再刷新：提权状态变化不影响已建立的订阅，
/// 而 `logs.follow` 用的始终是 user worker，快照过期不会造成越权。
async fn upgrade(
    State(hub): State<Arc<Hub>>,
    Extension(session): Extension<Session>,
    ws: WebSocketUpgrade,
) -> Response {
    // 鉴权已由 `auth::middleware::require_auth` 在升级前完成（token 走子协议）。
    // 这里必须把 `bearer` 子协议回给浏览器，否则握手被客户端中止。
    let ctx = SubscribeContext { session };
    ws.protocols([crate::auth::extract::WS_BEARER_PROTOCOL])
        .on_upgrade(move |socket| serve_socket(hub, socket, ctx))
}

/// 把 `WebSocket` 拆成进流 / 出汇交给 hub。
async fn serve_socket(hub: Arc<Hub>, socket: WebSocket, ctx: SubscribeContext) {
    let (sink, stream) = socket.split();
    let inbound = stream.map(|r| r.map(frame_from_message));
    let outbound = sink
        .with(|text: String| future::ready(Ok::<Message, axum::Error>(Message::Text(text.into()))));
    hub.serve(inbound, outbound, ctx).await;
}

/// WebSocket 消息 → 协议无关的 [`Frame`]。axum 已自动应答 ping。
fn frame_from_message(message: Message) -> Frame {
    match message {
        Message::Text(text) => Frame::Text(text.as_str().to_owned()),
        Message::Binary(_) => Frame::Binary,
        Message::Close(_) => Frame::Close,
        Message::Ping(_) | Message::Pong(_) => Frame::Control,
    }
}
