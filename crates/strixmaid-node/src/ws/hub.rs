//! 频道 hub：与传输层无关的连接循环。
//!
//! [`Hub::serve`] 只要求「一个进帧的 `Stream`」和「一个出文本的 `Sink`」，axum 的
//! `WebSocket` 与测试里的内存 channel 都能喂进来。每条连接一个任务：
//!
//! ```text
//! 客户端 ──sub/unsub/req/ping──▶ serve ──data/resp/err/ping──▶ 客户端
//!                                  ▲
//!             各频道的 ChannelStream（SelectAll，按频道名可 abort）
//! ```
//!
//! # 背压
//!
//! 频道源基于 `tokio::sync::broadcast`（见 [`broadcast_stream`]）：采集端 `send` 永不阻塞。
//! 客户端读得慢时连接任务卡在 `Sink::send` 上、不再拉 broadcast，receiver 落后超过队列
//! 深度后收到 `Lagged(n)`，hub 把它翻译成一条带频道名的 `err` 帧告知丢了 `n` 帧，然后继续。
//! 单次发送超过 [`SEND_TIMEOUT`] 则视为死连接、直接断开。

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::future::{AbortHandle, Abortable, BoxFuture};
use futures::stream::{self, BoxStream, SelectAll, StreamExt};
use futures::{Sink, SinkExt, Stream};
use serde_json::Value;
use strixmaid_core::session::Session;
use strixmaid_types::ws::{WS_PROTOCOL_VERSION, WsEnvelope, WsMsgType};
use strixmaid_types::{ApiError, ErrorCode};
use tokio::sync::broadcast;

/// 单帧发送超时：超过即视为死连接、断开。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

// ============================ 频道源 ============================

/// 频道源推给某个订阅者的一个事件。
#[derive(Debug, Clone, PartialEq)]
pub enum ChannelEvent {
    /// 一帧 `data` 的 payload。
    Data(Value),
    /// 订阅者消费过慢，源已丢弃 `n` 帧。hub 会发一条 `err` 告知，不断开。
    Lagged(u64),
}

/// 一个订阅的事件流。流结束表示频道源不再推送（hub 发 `err` 并移除订阅）。
pub type ChannelStream = BoxStream<'static, ChannelEvent>;

/// 订阅发生的上下文：**是谁在订阅**。
///
/// `roadmap/01-worker-execution.md` §4.4 引入它，是因为频道之间对身份的要求
/// 并不一致：
///
/// - `logs.follow` 的可见范围必须随用户——journald ACL 裁决的是执行
///   `journalctl -f` 的那个进程的 uid，所以它必须走该会话的 user worker；
/// - `metrics.live` 是全局指标，与谁在看无关，忽略本上下文；
/// - `services.changed` 推的是 unit 状态，本就对所有本地用户可见
///   （`systemctl list-units` 不需要任何权限），因此保留在主进程。
///
/// 换言之：**频道自己决定要不要看这个上下文**，hub 只负责把它送到。
#[derive(Debug, Clone)]
pub struct SubscribeContext {
    /// 发起订阅的会话。`upgrade` 从 `require_auth` 放进 extensions 的
    /// `Extension<Session>` 里取，一条 WS 连接的整个生命周期内不变。
    pub session: Session,
}

/// 频道源。每个频道一个实现，注册进 [`Hub`]。
pub trait ChannelSource: Send + Sync + 'static {
    /// 频道名，即 envelope 的 `ch`；已知取值见 `strixmaid_types::ws::WsChannel`。
    fn name(&self) -> &'static str;

    /// 建立一个订阅。`params` 是 `sub` 帧的 `d`；参数不合法时返回错误，
    /// hub 会带上原 `id` 回一条 `err`。`ctx` 是发起订阅的会话，见
    /// [`SubscribeContext`]。
    fn subscribe(
        &self,
        params: Option<Value>,
        ctx: &SubscribeContext,
    ) -> Result<ChannelStream, ApiError>;

    /// 处理该频道上的一次 `req`。默认不支持。
    fn request(&self, params: Option<Value>) -> BoxFuture<'static, Result<Value, ApiError>> {
        let _ = params;
        let name = self.name();
        Box::pin(async move { Err(ApiError::invalid_request(format!("频道 {name} 不支持 req"))) })
    }
}

/// 把一个 `broadcast::Receiver` 变成 [`ChannelStream`]：`map` 把消息投影成 payload
/// （返回 `None` 表示这条不推给该订阅者），lag 翻译成 [`ChannelEvent::Lagged`]，
/// 发送端全部关闭时流结束。供所有基于广播的频道复用。
pub fn broadcast_stream<T, F>(rx: broadcast::Receiver<T>, mut map: F) -> ChannelStream
where
    T: Clone + Send + 'static,
    F: FnMut(T) -> Option<Value> + Send + 'static,
{
    stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(v) => Some((Ok(v), rx)),
            Err(broadcast::error::RecvError::Lagged(n)) => Some((Err(n), rx)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
    .filter_map(move |item| {
        let ev = match item {
            Ok(v) => map(v).map(ChannelEvent::Data),
            Err(n) => Some(ChannelEvent::Lagged(n)),
        };
        futures::future::ready(ev)
    })
    .boxed()
}

// ============================ 传输层适配 ============================

/// 从传输层收到的一帧，已剥掉与协议无关的细节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 文本帧，应为一个 [`WsEnvelope`] 的 JSON。
    Text(String),
    /// 二进制帧。控制面不接受，回 `err`。
    Binary,
    /// 对端关闭。
    Close,
    /// WebSocket 层的 ping / pong 等，忽略。
    Control,
}

// ============================ Hub ============================

/// 频道 hub。整个进程一个，`Arc` 共享给所有连接。
pub struct Hub {
    sources: RwLock<HashMap<&'static str, Arc<dyn ChannelSource>>>,
    next_conn_id: AtomicU64,
    active: AtomicUsize,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    /// 空 hub。
    pub fn new() -> Hub {
        Hub {
            sources: RwLock::new(HashMap::new()),
            next_conn_id: AtomicU64::new(0),
            active: AtomicUsize::new(0),
        }
    }

    /// 注册一个频道源；同名的会被替换（记 warn）。
    pub fn register(&self, source: Arc<dyn ChannelSource>) -> &Self {
        let name = source.name();
        let mut sources = self.sources.write().unwrap_or_else(|e| e.into_inner());
        if sources.insert(name, source).is_some() {
            tracing::warn!(channel = name, "频道源被重复注册，已替换");
        } else {
            tracing::debug!(channel = name, "频道源已注册");
        }
        self
    }

    /// 已注册的频道名，按字母序。
    pub fn channels(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self
            .sources
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .copied()
            .collect();
        v.sort_unstable();
        v
    }

    /// 按名字取频道源。
    pub fn source(&self, name: &str) -> Option<Arc<dyn ChannelSource>> {
        self.sources
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
    }

    /// 当前活跃连接数。
    pub fn connections(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// 跑一条连接直到对端关闭、协议错误或发送失败。
    ///
    /// `inbound` 出错（`Err`）视为连接断开；`outbound` 是序列化好的 JSON 文本。
    /// `ctx` 携带这条连接背后的会话，转交给每次 `sub`（见 [`SubscribeContext`]）。
    pub async fn serve<I, O, E>(
        self: Arc<Self>,
        mut inbound: I,
        mut outbound: O,
        ctx: SubscribeContext,
    ) where
        I: Stream<Item = Result<Frame, E>> + Unpin + Send,
        O: Sink<String> + Unpin + Send,
        O::Error: Display,
        E: Display,
    {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed) + 1;
        self.active.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(conn_id, active = self.connections(), "WS 控制面连接建立");

        let mut subs = Subscriptions::default();
        let reason = 'conn: loop {
            tokio::select! {
                frame = inbound.next() => {
                    let outcome = match frame {
                        None => break 'conn "对端断开",
                        Some(Err(e)) => {
                            tracing::debug!(conn_id, error = %e, "WS 读取失败");
                            break 'conn "读取失败";
                        }
                        Some(Ok(Frame::Close)) => break 'conn "对端关闭",
                        Some(Ok(Frame::Control)) => continue,
                        Some(Ok(Frame::Binary)) => Outcome::reply(WsEnvelope::err(
                            None,
                            &ApiError::invalid_request("控制面只接受文本帧"),
                        )),
                        Some(Ok(Frame::Text(text))) => {
                            self.handle_text(conn_id, &text, &mut subs, &ctx).await
                        }
                    };
                    for env in outcome.replies {
                        if !self.send(&mut outbound, env).await {
                            break 'conn "发送失败";
                        }
                    }
                    if outcome.close {
                        break 'conn "协议错误";
                    }
                }
                Some(ev) = subs.streams.next(), if !subs.streams.is_empty() => {
                    let env = match ev {
                        SubEvent::Event(ch, ChannelEvent::Data(d)) => WsEnvelope::data(ch, d),
                        SubEvent::Event(ch, ChannelEvent::Lagged(n)) => {
                            tracing::warn!(conn_id, channel = %ch, dropped = n, "客户端消费过慢，已丢帧");
                            err_on(None, Some(ch), &ApiError::new(
                                ErrorCode::Unavailable,
                                format!("客户端消费过慢，已丢弃 {n} 帧"),
                            ))
                        }
                        SubEvent::Ended(ch) => {
                            subs.handles.remove(&ch);
                            err_on(None, Some(ch.clone()), &ApiError::new(
                                ErrorCode::Unavailable,
                                format!("频道 {ch} 已结束推送"),
                            ))
                        }
                    };
                    if !self.send(&mut outbound, env).await {
                        break 'conn "发送失败";
                    }
                }
            }
        };

        // 尽力发 close 帧；对端已经没了也无所谓。
        let _ = outbound.close().await;
        self.active.fetch_sub(1, Ordering::Relaxed);
        tracing::debug!(
            conn_id,
            reason,
            active = self.connections(),
            subscriptions = subs.handles.len(),
            "WS 控制面连接结束"
        );
    }

    /// 处理一帧文本。
    async fn handle_text(
        &self,
        conn_id: u64,
        text: &str,
        subs: &mut Subscriptions,
        ctx: &SubscribeContext,
    ) -> Outcome {
        let env: WsEnvelope = match serde_json::from_str(text) {
            Ok(e) => e,
            Err(e) => {
                return Outcome::reply(WsEnvelope::err(
                    None,
                    &ApiError::invalid_request("无法解析的帧").with_detail(e.to_string()),
                ));
            }
        };
        if env.v != WS_PROTOCOL_VERSION {
            return Outcome::close(WsEnvelope::err(
                env.id,
                &ApiError::invalid_request(format!(
                    "不支持的协议版本 {}，本端为 {WS_PROTOCOL_VERSION}",
                    env.v
                )),
            ));
        }
        let id = env.id;
        match env.t {
            WsMsgType::Ping => Outcome::reply(WsEnvelope {
                v: WS_PROTOCOL_VERSION,
                t: WsMsgType::Ping,
                ch: env.ch,
                id,
                d: None,
            }),
            WsMsgType::Sub => {
                let Some(ch) = env.ch else {
                    return Outcome::reply(WsEnvelope::err(
                        id,
                        &ApiError::invalid_request("sub 必须带 ch"),
                    ));
                };
                let Some(source) = self.source(&ch) else {
                    return Outcome::reply(err_on(
                        id,
                        Some(ch.clone()),
                        &self.unknown_channel(&ch),
                    ));
                };
                match source.subscribe(env.d, ctx) {
                    Ok(stream) => {
                        subs.attach(ch.clone(), stream);
                        tracing::debug!(conn_id, channel = %ch, "订阅");
                        Outcome::ack(id, ch, true)
                    }
                    Err(e) => Outcome::reply(err_on(id, Some(ch), &e)),
                }
            }
            WsMsgType::Unsub => {
                let Some(ch) = env.ch else {
                    return Outcome::reply(WsEnvelope::err(
                        id,
                        &ApiError::invalid_request("unsub 必须带 ch"),
                    ));
                };
                subs.detach(&ch);
                tracing::debug!(conn_id, channel = %ch, "退订");
                Outcome::ack(id, ch, false)
            }
            WsMsgType::Req => {
                let Some(ch) = env.ch else {
                    return Outcome::reply(WsEnvelope::err(
                        id,
                        &ApiError::invalid_request("req 必须带 ch"),
                    ));
                };
                if id.is_none() {
                    return Outcome::reply(err_on(
                        None,
                        Some(ch),
                        &ApiError::invalid_request("req 必须带 id"),
                    ));
                }
                let Some(source) = self.source(&ch) else {
                    return Outcome::reply(err_on(
                        id,
                        Some(ch.clone()),
                        &self.unknown_channel(&ch),
                    ));
                };
                match source.request(env.d).await {
                    Ok(d) => Outcome::reply(WsEnvelope {
                        v: WS_PROTOCOL_VERSION,
                        t: WsMsgType::Resp,
                        ch: Some(ch),
                        id,
                        d: Some(d),
                    }),
                    Err(e) => Outcome::reply(err_on(id, Some(ch), &e)),
                }
            }
            WsMsgType::Data | WsMsgType::Resp | WsMsgType::Err => Outcome::reply(WsEnvelope::err(
                id,
                &ApiError::invalid_request(format!(
                    "客户端不应发送 {} 帧",
                    serde_json::to_string(&env.t).unwrap_or_default()
                )),
            )),
        }
    }

    /// 「未知频道」错误，顺带列出已注册的频道，客户端一眼能看出是拼错还是没注册。
    fn unknown_channel(&self, ch: &str) -> ApiError {
        ApiError::invalid_request(format!("未知频道 {ch}"))
            .with_detail(format!("可用频道: {}", self.channels().join(", ")))
    }

    /// 序列化并发送一帧，带超时。返回 `false` 表示连接应当结束。
    async fn send<O>(&self, outbound: &mut O, env: WsEnvelope) -> bool
    where
        O: Sink<String> + Unpin,
        O::Error: Display,
    {
        let text = match serde_json::to_string(&env) {
            Ok(t) => t,
            Err(e) => {
                // envelope 全是普通字段，实际不可能失败；真失败也不该拖垮连接。
                tracing::error!(error = %e, "序列化 WS 帧失败");
                return true;
            }
        };
        match tokio::time::timeout(SEND_TIMEOUT, outbound.send(text)).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "WS 发送失败");
                false
            }
            Err(_) => {
                tracing::warn!(
                    timeout_secs = SEND_TIMEOUT.as_secs(),
                    "WS 发送超时，断开慢客户端"
                );
                false
            }
        }
    }
}

/// 带频道名的 `err` 帧。
fn err_on(id: Option<u64>, ch: Option<String>, error: &ApiError) -> WsEnvelope {
    WsEnvelope {
        ch,
        ..WsEnvelope::err(id, error)
    }
}

/// 一帧处理的结果。
struct Outcome {
    replies: Vec<WsEnvelope>,
    close: bool,
}

impl Outcome {
    fn reply(env: WsEnvelope) -> Outcome {
        Outcome {
            replies: vec![env],
            close: false,
        }
    }

    fn close(env: WsEnvelope) -> Outcome {
        Outcome {
            replies: vec![env],
            close: true,
        }
    }

    /// `sub` / `unsub` 的确认：只在客户端带了 `id` 时回一条 `resp`，
    /// `d = {"subscribed": bool}`。不带 `id` 的订阅静默生效。
    fn ack(id: Option<u64>, ch: String, subscribed: bool) -> Outcome {
        match id {
            None => Outcome {
                replies: Vec::new(),
                close: false,
            },
            Some(_) => Outcome::reply(WsEnvelope {
                v: WS_PROTOCOL_VERSION,
                t: WsMsgType::Resp,
                ch: Some(ch),
                id,
                d: Some(serde_json::json!({ "subscribed": subscribed })),
            }),
        }
    }
}

// ============================ 每连接的订阅集合 ============================

/// 合并流里的一项。
enum SubEvent {
    Event(String, ChannelEvent),
    /// 该频道的源流自然结束（不是被 `unsub` 打断）。
    Ended(String),
}

/// 一条连接的全部订阅：按频道名可 abort 的流，合并进一个 `SelectAll`。
#[derive(Default)]
struct Subscriptions {
    handles: HashMap<String, AbortHandle>,
    streams: SelectAll<BoxStream<'static, SubEvent>>,
}

impl Subscriptions {
    /// 加入订阅；同频道已有订阅时先打断旧的（重新 `sub` 即换参数）。
    fn attach(&mut self, ch: String, stream: ChannelStream) {
        self.detach(&ch);
        let (handle, registration) = AbortHandle::new_pair();
        let tag = ch.clone();
        let ended = ch.clone();
        let mapped = stream
            .map(move |ev| SubEvent::Event(tag.clone(), ev))
            .chain(stream::once(async move { SubEvent::Ended(ended) }));
        self.streams
            .push(Abortable::new(mapped, registration).boxed());
        self.handles.insert(ch, handle);
    }

    /// 打断并移除订阅；未订阅时无操作。
    fn detach(&mut self, ch: &str) {
        if let Some(handle) = self.handles.remove(ch) {
            handle.abort();
        }
    }
}
