//! `GET /ws/terminal/{id}` —— 终端的字节流（`design.md` §9.2、`roadmap/03-terminal.md` §4.4）。
//!
//! # 为什么不进 `/ws` 控制面
//!
//! Q13 的结论：终端是纯二进制流、延迟敏感、生命周期独立于页面。塞进 `/ws` 的
//! envelope 多路复用意味着每个按键都要包一层 JSON 再 base64，而且一个刷屏的
//! `cat 大文件` 会把同一条连接上的指标、日志订阅一起顶住。独立连接让它们互不牵连。
//!
//! # 协议
//!
//! | 方向 | 帧 | 含义 |
//! |---|---|---|
//! | 双向 | **二进制** | PTY 的原始字节，原样透传 |
//! | 客户端 → 服务端 | 文本 `{"t":"resize","cols":N,"rows":M}` | 改窗口大小 |
//! | 服务端 → 客户端 | 文本 `{"t":"exit",...}` | 终端没了，随后关闭连接 |
//!
//! # 鉴权
//!
//! 与 `/ws` 完全相同：token 走子协议，由 `auth::middleware::require_auth` 在**升级之前**
//! 完成。此外还有一层：[`TerminalRegistry::attach`] 只认**本会话**的终端 id，
//! 别的会话的 id 一律 404。这不是「多一道保险」，而是必需的——终端 id 出现在
//! URL 里，而 URL 会进浏览器历史、反代日志、截图。

use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use strixmaid_core::session::Session;
use strixmaid_core::terminal::{AttachEvent, Attachment, CloseReason, TerminalRegistry};

use crate::error::ApiErr;

/// `/ws/terminal/{id}` 路由。
///
/// 与 [`crate::ws::handler::router`] 一样返回普通 `Router`（WebSocket 没有 OpenAPI 描述），
/// 由宿主在顶层 merge 并套上认证中间件。
pub fn router<S>(registry: Arc<TerminalRegistry>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route("/ws/terminal/{id}", get(upgrade))
        .with_state(registry)
}

/// 客户端发来的控制帧。目前只有 resize 一种。
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum ClientControl {
    Resize { cols: u16, rows: u16 },
}

async fn upgrade(
    State(registry): State<Arc<TerminalRegistry>>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    // **附着必须在升级之前**：升级之后就没有 HTTP 响应可用了，「终端不存在」
    // 只能变成一个刚连上就关闭的 WS，客户端分不清是没权限、打错 id，还是网络问题。
    let attachment = match registry.attach(&session.token_hash, &id) {
        Ok(a) => a,
        Err(e) => return ApiErr(e).into_response(),
    };

    ws.protocols([crate::auth::extract::WS_BEARER_PROTOCOL])
        .on_upgrade(move |socket| serve(socket, attachment, registry, session.token_hash, id))
}

/// 在 WS 与终端之间双向泵字节，直到任一侧结束。
async fn serve(
    socket: WebSocket,
    mut attachment: Attachment,
    registry: Arc<TerminalRegistry>,
    session_hash: String,
    id: String,
) {
    let (mut sink, mut stream) = socket.split();
    // 终端侧结束的原因。`None` 表示是浏览器先走的（关标签页、断网），
    // 那种情况下终端还活着，不该给它发 exit。
    let mut closed: Option<CloseReason> = None;

    loop {
        tokio::select! {
            // 终端 → 浏览器
            event = attachment.next() => match event {
                Some(AttachEvent::Data(bytes)) => {
                    if sink.send(Message::Binary(bytes.into())).await.is_err() {
                        break; // 浏览器已经不在了
                    }
                }
                Some(AttachEvent::Closed(reason)) => {
                    closed = Some(reason);
                    break;
                }
                // 通道关了而没收到 Closed：`AttachEvent::Closed` 是尽力而为的
                // （队列满时投不进去），所以判断「结束」必须以 None 为准。
                None => break,
            },

            // 浏览器 → 终端
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    if attachment.write(&data).await.is_err() {
                        // 写不进 PTY = shell 那头没了。等下一轮从 attachment
                        // 收 Closed 拿到准确原因，这里不猜。
                        continue;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    handle_control(&registry, &session_hash, &id, &text).await;
                }
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Err(e)) => {
                    tracing::debug!(id, error = %e, "终端 WS 读出错");
                    break;
                }
            },
        }
    }

    // 终端真的没了才发 exit；只是换了个 WS（Replaced）或本端被判死（Stalled）不发。
    if let Some(reason) = closed.filter(|r| r.is_terminal_gone()) {
        let frame = serde_json::json!({ "t": "exit", "reason": reason.as_str() });
        let _ = sink.send(Message::Text(frame.to_string().into())).await;
    }
    let _ = sink.close().await;
    // attachment 在这里 drop → 解除附着。终端本身继续跑（除非它自己没了），
    // 刷新页面重新连上即可（`roadmap/03-terminal.md` §4.3）。
}

/// 处理一条文本控制帧。
///
/// 解析不了就记一条 debug 日志丢掉，**不关连接**：一个拼错的控制帧不该让用户
/// 正在用的 shell 断掉。
async fn handle_control(registry: &TerminalRegistry, session_hash: &str, id: &str, text: &str) {
    match serde_json::from_str::<ClientControl>(text) {
        Ok(ClientControl::Resize { cols, rows }) => {
            if let Err(e) = registry.resize(session_hash, id, cols, rows).await {
                // resize 失败不致命：尺寸不对最多显示错位，断开连接的代价更大。
                tracing::debug!(id, cols, rows, error = %e.message, "终端 resize 失败");
            }
        }
        Err(e) => tracing::debug!(id, error = %e, "无法解析的终端控制帧"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 只认识_resize_控制帧() {
        let ok: ClientControl = serde_json::from_str(r#"{"t":"resize","cols":120,"rows":32}"#).unwrap();
        assert!(matches!(
            ok,
            ClientControl::Resize {
                cols: 120,
                rows: 32
            }
        ));
    }

    #[test]
    fn 未知控制帧被拒而不是被当成_resize() {
        // 关键在于「拒绝」而不是「用默认值当成 resize」：后者会把终端
        // 悄悄改成 0×0，比直接忽略这条帧糟糕得多。
        assert!(serde_json::from_str::<ClientControl>(r#"{"t":"exit","code":0}"#).is_err());
        assert!(serde_json::from_str::<ClientControl>(r#"{"t":"resize"}"#).is_err());
        assert!(serde_json::from_str::<ClientControl>("not json").is_err());
    }

    #[test]
    fn 只有终端真的没了才发_exit() {
        // Replaced / Stalled 表示终端还活着，只是这条 WS 不再是它的附着方。
        // 给这两种情况发 exit 会让前端以为 shell 死了，从而把一个还能用的
        // 终端从列表里划掉。
        assert!(!CloseReason::Replaced.is_terminal_gone());
        assert!(!CloseReason::Stalled.is_terminal_gone());
        for r in [
            CloseReason::Deleted,
            CloseReason::Exited,
            CloseReason::Idle,
            CloseReason::Logout,
            CloseReason::Failed,
        ] {
            assert!(r.is_terminal_gone(), "{} 应当意味着终端已经没了", r.as_str());
        }
    }
}
