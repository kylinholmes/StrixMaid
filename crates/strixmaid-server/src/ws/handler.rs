//! `GET /ws`：axum 升级处理器，把 `WebSocket` 适配成 [`Hub::serve`] 需要的流与汇。

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures::{SinkExt, StreamExt, future};

use super::hub::{Frame, Hub};

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
async fn upgrade(State(hub): State<Arc<Hub>>, ws: WebSocketUpgrade) -> Response {
    // 鉴权已由 `auth::middleware::require_auth` 在升级前完成（token 走子协议）。
    // 这里必须把 `bearer` 子协议回给浏览器，否则握手被客户端中止。
    ws.protocols([crate::auth::extract::WS_BEARER_PROTOCOL])
        .on_upgrade(move |socket| serve_socket(hub, socket))
}

/// 把 `WebSocket` 拆成进流 / 出汇交给 hub。
async fn serve_socket(hub: Arc<Hub>, socket: WebSocket) {
    let (sink, stream) = socket.split();
    let inbound = stream.map(|r| r.map(frame_from_message));
    let outbound = sink
        .with(|text: String| future::ready(Ok::<Message, axum::Error>(Message::Text(text.into()))));
    hub.serve(inbound, outbound).await;
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
