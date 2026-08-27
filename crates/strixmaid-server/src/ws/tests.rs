//! WS 控制面测试。
//!
//! 大部分用例直接驱动 [`Hub::serve`]：进出两端用内存 channel，快且不依赖网络。
//! 最后一个用例起一个真实的 axum 服务，用手写的极简 WebSocket 客户端
//! （握手 + 带掩码的文本帧）走 `GET /ws` 一遍，证明 axum 适配层没接错。

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::channel::mpsc;
use futures::stream::{self, StreamExt};
use serde_json::{Value, json};
use strixmaid_core::metrics::{CollectError, Collector, MetricsEngine, Sample, SchedulerConfig};
use strixmaid_types::ws::{WS_PROTOCOL_VERSION, WsEnvelope, WsMsgType};
use strixmaid_types::{ApiError, ErrorCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::channels::MetricsLive;
use super::hub::{ChannelEvent, ChannelSource, ChannelStream, Frame, Hub, broadcast_stream};

// ============================ 夹具 ============================

/// 广播驱动的假频道。`sub` 带 `{"reject": true}` 时拒绝订阅。
struct FakeSource {
    tx: broadcast::Sender<Value>,
}

impl ChannelSource for FakeSource {
    fn name(&self) -> &'static str {
        "fake.events"
    }

    fn subscribe(&self, params: Option<Value>) -> Result<ChannelStream, ApiError> {
        if params
            .as_ref()
            .and_then(|p| p.get("reject"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Err(ApiError::invalid_request("订阅参数被拒"));
        }
        Ok(broadcast_stream(self.tx.subscribe(), Some))
    }
}

/// 推一帧就结束的频道。
struct EndingSource;

impl ChannelSource for EndingSource {
    fn name(&self) -> &'static str {
        "ending"
    }

    fn subscribe(&self, _params: Option<Value>) -> Result<ChannelStream, ApiError> {
        Ok(stream::iter([ChannelEvent::Data(json!(1))]).boxed())
    }
}

type InTx = mpsc::UnboundedSender<Result<Frame, Infallible>>;

/// 内存传输的一条连接。
struct Conn {
    /// 进流发送端；`drop_inbound` 后为 `None`。
    tx: Option<InTx>,
    rx: mpsc::UnboundedReceiver<String>,
    task: JoinHandle<()>,
}

fn connect(hub: &Arc<Hub>) -> Conn {
    let (tx, rx_in) = mpsc::unbounded();
    let (tx_out, rx) = mpsc::unbounded();
    let task = tokio::spawn(Arc::clone(hub).serve(rx_in, tx_out));
    Conn {
        tx: Some(tx),
        rx,
        task,
    }
}

impl Conn {
    fn send(&self, env: &WsEnvelope) {
        self.send_raw(serde_json::to_string(env).unwrap());
    }

    fn send_raw(&self, text: String) {
        self.send_frame(Frame::Text(text));
    }

    fn send_frame(&self, frame: Frame) {
        self.tx
            .as_ref()
            .expect("进流未关闭")
            .unbounded_send(Ok(frame))
            .unwrap();
    }

    /// 关闭进流（模拟传输层断开）。
    fn drop_inbound(&mut self) {
        self.tx.take();
    }

    async fn recv(&mut self) -> WsEnvelope {
        let text = tokio::time::timeout(Duration::from_secs(3), self.rx.next())
            .await
            .expect("3s 内应收到一帧")
            .expect("连接不应已关闭");
        serde_json::from_str(&text).unwrap()
    }

    /// 200ms 内没有任何帧。
    async fn expect_silence(&mut self) {
        let got = tokio::time::timeout(Duration::from_millis(200), self.rx.next()).await;
        assert!(got.is_err(), "不应再收到帧: {got:?}");
    }

    /// 连接任务已结束且出流已关闭。
    async fn expect_closed(mut self) {
        tokio::time::timeout(Duration::from_secs(3), self.task)
            .await
            .expect("连接任务应结束")
            .unwrap();
        assert!(self.rx.next().await.is_none(), "出流应关闭");
    }
}

fn env(t: WsMsgType, ch: Option<&str>, id: Option<u64>, d: Option<Value>) -> WsEnvelope {
    WsEnvelope {
        v: WS_PROTOCOL_VERSION,
        t,
        ch: ch.map(str::to_owned),
        id,
        d,
    }
}

fn hub_with_fake(capacity: usize) -> (Arc<Hub>, broadcast::Sender<Value>) {
    let (tx, _) = broadcast::channel(capacity);
    let hub = Arc::new(Hub::new());
    hub.register(Arc::new(FakeSource { tx: tx.clone() }));
    hub.register(Arc::new(EndingSource));
    (hub, tx)
}

fn error_of(env: &WsEnvelope) -> ApiError {
    assert_eq!(env.t, WsMsgType::Err, "{env:?}");
    serde_json::from_value(env.d.clone().expect("err 帧应带 d")).unwrap()
}

// ============================ hub 用例 ============================

#[tokio::test]
async fn 订阅_收数据_退订后不再收到() {
    let (hub, tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    assert_eq!(hub.channels(), ["ending", "fake.events"]);

    c.send(&env(WsMsgType::Sub, Some("fake.events"), Some(1), None));
    let ack = c.recv().await;
    assert_eq!(
        (ack.t, ack.id, ack.ch.as_deref()),
        (WsMsgType::Resp, Some(1), Some("fake.events"))
    );
    assert_eq!(ack.d, Some(json!({ "subscribed": true })));
    assert_eq!(hub.connections(), 1);

    tx.send(json!({ "n": 1 })).unwrap();
    let data = c.recv().await;
    assert_eq!(data.t, WsMsgType::Data);
    assert_eq!(data.ch.as_deref(), Some("fake.events"));
    assert_eq!(data.id, None);
    assert_eq!(data.d, Some(json!({ "n": 1 })));

    c.send(&env(WsMsgType::Unsub, Some("fake.events"), Some(2), None));
    let ack = c.recv().await;
    assert_eq!((ack.t, ack.id), (WsMsgType::Resp, Some(2)));
    assert_eq!(ack.d, Some(json!({ "subscribed": false })));

    // 退订后广播已无任何接收者，send 会报「无接收者」——这正说明订阅流被真正撤掉了。
    assert!(tx.send(json!({ "n": 2 })).is_err(), "退订后不应再有接收者");
    c.expect_silence().await;

    // 不带 id 的 sub / unsub 静默生效
    c.send(&env(WsMsgType::Sub, Some("fake.events"), None, None));
    c.expect_silence().await;
    tx.send(json!({ "n": 3 })).unwrap();
    assert_eq!(c.recv().await.d, Some(json!({ "n": 3 })));

    c.drop_inbound();
    c.expect_closed().await;
    assert_eq!(hub.connections(), 0);
}

#[tokio::test]
async fn 未知频道与参数错误回_err_并带原_id() {
    let (hub, _tx) = hub_with_fake(16);
    let mut c = connect(&hub);

    c.send(&env(WsMsgType::Sub, Some("no.such"), Some(7), None));
    let e = c.recv().await;
    assert_eq!(e.id, Some(7));
    assert_eq!(e.ch.as_deref(), Some("no.such"));
    let err = error_of(&e);
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(err.message.contains("no.such"));
    assert!(
        err.detail.as_deref().unwrap_or("").contains("fake.events"),
        "detail 应列出可用频道: {err:?}"
    );

    c.send(&env(
        WsMsgType::Sub,
        Some("fake.events"),
        Some(8),
        Some(json!({ "reject": true })),
    ));
    let e = c.recv().await;
    assert_eq!(e.id, Some(8));
    assert_eq!(error_of(&e).message, "订阅参数被拒");

    c.send(&env(WsMsgType::Sub, None, Some(9), None));
    assert_eq!(c.recv().await.id, Some(9));

    // 客户端不该发的类型
    c.send(&env(WsMsgType::Data, Some("fake.events"), Some(10), None));
    let e = c.recv().await;
    assert_eq!(e.id, Some(10));
    assert!(error_of(&e).message.contains("data"));

    // 连接仍然活着
    c.send(&env(WsMsgType::Ping, None, Some(11), None));
    assert_eq!(c.recv().await.t, WsMsgType::Ping);
}

#[tokio::test]
async fn ping_原样回() {
    let (hub, _tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    c.send(&env(WsMsgType::Ping, None, Some(3), None));
    let p = c.recv().await;
    assert_eq!(
        (p.t, p.id, p.v),
        (WsMsgType::Ping, Some(3), WS_PROTOCOL_VERSION)
    );
    c.send(&env(WsMsgType::Ping, None, None, None));
    assert_eq!(c.recv().await.id, None);
}

#[tokio::test]
async fn 非法帧回_err_不断开() {
    let (hub, _tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    c.send_raw("{".into());
    let e = c.recv().await;
    assert_eq!(e.id, None);
    assert_eq!(error_of(&e).message, "无法解析的帧");

    c.send_frame(Frame::Binary);
    assert_eq!(error_of(&c.recv().await).code, ErrorCode::InvalidRequest);

    c.send_frame(Frame::Control);
    c.send(&env(WsMsgType::Ping, None, Some(1), None));
    assert_eq!(c.recv().await.id, Some(1));
}

#[tokio::test]
async fn 协议版本不符则回_err_并断开() {
    let (hub, _tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    c.send_raw(json!({ "v": 2, "t": "ping", "id": 5 }).to_string());
    let e = c.recv().await;
    assert_eq!(e.id, Some(5));
    assert!(error_of(&e).message.contains("协议版本"));
    c.expect_closed().await;
}

#[tokio::test]
async fn close_帧结束连接() {
    let (hub, _tx) = hub_with_fake(16);
    let c = connect(&hub);
    c.send_frame(Frame::Close);
    c.expect_closed().await;
}

#[tokio::test]
async fn 慢客户端_lag_时发_err_并继续() {
    // 广播队列只留 4 帧；出汇用容量极小的有界 channel，模拟读得慢的客户端。
    let (tx, _) = broadcast::channel::<Value>(4);
    let hub = Arc::new(Hub::new());
    hub.register(Arc::new(FakeSource { tx: tx.clone() }));

    let (in_tx, in_rx) = mpsc::unbounded::<Result<Frame, Infallible>>();
    let (out_tx, mut out_rx) = mpsc::channel::<String>(0);
    let _task = tokio::spawn(Arc::clone(&hub).serve(in_rx, out_tx));

    let sub = env(WsMsgType::Sub, Some("fake.events"), Some(1), None);
    in_tx
        .unbounded_send(Ok(Frame::Text(serde_json::to_string(&sub).unwrap())))
        .unwrap();
    let ack: WsEnvelope = serde_json::from_str(&out_rx.next().await.unwrap()).unwrap();
    assert_eq!(ack.t, WsMsgType::Resp);

    // 一口气发 12 帧，客户端一帧都没读：必然溢出。
    for n in 1..=12 {
        tx.send(json!({ "n": n })).unwrap();
    }
    let mut lagged = None;
    let mut last_n = 0;
    for _ in 0..20 {
        let Ok(Some(text)) = tokio::time::timeout(Duration::from_millis(500), out_rx.next()).await
        else {
            break;
        };
        let f: WsEnvelope = serde_json::from_str(&text).unwrap();
        match f.t {
            WsMsgType::Err => {
                let e = error_of(&f);
                assert_eq!(e.code, ErrorCode::Unavailable);
                assert_eq!(f.ch.as_deref(), Some("fake.events"), "lag 错误要带频道名");
                lagged = Some(e.message);
            }
            WsMsgType::Data => last_n = f.d.unwrap()["n"].as_i64().unwrap(),
            other => panic!("意外的帧类型 {other:?}"),
        }
        if last_n == 12 {
            break;
        }
    }
    let msg = lagged.expect("应收到一条 lag 错误");
    assert!(msg.contains("丢弃"), "{msg}");
    assert_eq!(last_n, 12, "丢帧后仍继续推送最新的帧");
}

#[tokio::test]
async fn 源结束时通知并移除订阅() {
    let (hub, _tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    c.send(&env(WsMsgType::Sub, Some("ending"), None, None));
    let d = c.recv().await;
    assert_eq!((d.t, d.d), (WsMsgType::Data, Some(json!(1))));
    let e = c.recv().await;
    assert_eq!(e.ch.as_deref(), Some("ending"));
    assert!(error_of(&e).message.contains("已结束"));
    c.expect_silence().await;
}

#[tokio::test]
async fn req_默认不支持() {
    let (hub, _tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    c.send(&env(WsMsgType::Req, Some("fake.events"), Some(5), None));
    let e = c.recv().await;
    assert_eq!(e.id, Some(5));
    assert!(error_of(&e).message.contains("不支持"));
    c.send(&env(WsMsgType::Req, Some("fake.events"), None, None));
    assert!(error_of(&c.recv().await).message.contains("id"));
}

#[tokio::test]
async fn 重复订阅替换旧的() {
    let (hub, tx) = hub_with_fake(16);
    let mut c = connect(&hub);
    c.send(&env(WsMsgType::Sub, Some("fake.events"), None, None));
    c.send(&env(WsMsgType::Sub, Some("fake.events"), None, None));
    c.expect_silence().await;
    tx.send(json!(1)).unwrap();
    assert_eq!(c.recv().await.d, Some(json!(1)));
    c.expect_silence().await; // 只收到一份，不是两份
}

// ============================ metrics.live ============================

struct ConstCollector;

impl Collector for ConstCollector {
    fn name(&self) -> &'static str {
        "const"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        Ok(vec![
            Sample::new("test.cpu", 1.0),
            Sample::new("other.value", 2.0),
        ])
    }
}

fn fast_engine() -> MetricsEngine {
    MetricsEngine::start_with(
        SchedulerConfig {
            interval: Duration::from_millis(50),
            ring_secs: 3600,
            node: "local".into(),
            collect_timeout: Duration::from_secs(5),
        },
        None,
        vec![Box::new(ConstCollector)],
    )
}

#[tokio::test]
async fn metrics_live_推送快照并支持前缀过滤() {
    let engine = fast_engine();
    let hub = Arc::new(Hub::new());
    hub.register(Arc::new(MetricsLive::new(engine.clone())));
    let mut c = connect(&hub);

    c.send(&env(
        WsMsgType::Sub,
        Some("metrics.live"),
        Some(1),
        Some(json!({ "prefixes": ["test."] })),
    ));
    assert_eq!(c.recv().await.t, WsMsgType::Resp);
    let d = c.recv().await;
    assert_eq!(d.t, WsMsgType::Data);
    let values = d.d.unwrap()["values"].as_array().unwrap().clone();
    assert_eq!(values.len(), 1, "只剩 test.* : {values:?}");
    assert_eq!(values[0]["metric"], "test.cpu");

    // 非法参数
    c.send(&env(
        WsMsgType::Sub,
        Some("metrics.live"),
        Some(2),
        Some(json!({ "bogus": 1 })),
    ));
    let mut got_err = false;
    for _ in 0..5 {
        let f = c.recv().await;
        if f.t == WsMsgType::Err {
            assert_eq!(f.id, Some(2));
            got_err = true;
            break;
        }
    }
    assert!(got_err, "非法参数应回 err");

    // req 返回当前快照
    c.send(&env(WsMsgType::Req, Some("metrics.live"), Some(3), None));
    for _ in 0..5 {
        let f = c.recv().await;
        if f.t == WsMsgType::Resp {
            assert_eq!(f.id, Some(3));
            assert_eq!(f.d.unwrap()["values"].as_array().unwrap().len(), 2);
            break;
        }
    }
    engine.stop().await;
}

// ============================ 真实 WebSocket 端到端 ============================

/// 极简 WebSocket 客户端：只做握手、发带掩码的文本帧、收服务端帧。
struct MiniWs {
    stream: TcpStream,
}

impl MiniWs {
    async fn connect(addr: std::net::SocketAddr, path: &str) -> MiniWs {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(req.as_bytes()).await.unwrap();
        // 读到响应头结束
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            buf.push(byte[0]);
            assert!(buf.len() < 8192, "响应头过长");
        }
        let head = String::from_utf8_lossy(&buf);
        assert!(head.starts_with("HTTP/1.1 101"), "握手失败: {head}");
        MiniWs { stream }
    }

    async fn send_text(&mut self, text: &str) {
        self.send_frame(0x1, text.as_bytes()).await;
    }

    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) {
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            n if n < 126 => frame.push(0x80 | n as u8),
            n if n < 65_536 => {
                frame.push(0x80 | 126);
                frame.extend((n as u16).to_be_bytes());
            }
            n => {
                frame.push(0x80 | 127);
                frame.extend((n as u64).to_be_bytes());
            }
        }
        frame.extend(mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).await.unwrap();
    }

    /// 下一帧文本（自动应答 ping、跳过 pong）。收到 close 返回 `None`。
    async fn recv_text(&mut self) -> Option<String> {
        loop {
            let mut h = [0u8; 2];
            self.stream.read_exact(&mut h).await.unwrap();
            let opcode = h[0] & 0x0f;
            assert_eq!(h[1] & 0x80, 0, "服务端帧不应带掩码");
            let mut len = usize::from(h[1] & 0x7f);
            if len == 126 {
                let mut b = [0u8; 2];
                self.stream.read_exact(&mut b).await.unwrap();
                len = usize::from(u16::from_be_bytes(b));
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.stream.read_exact(&mut b).await.unwrap();
                len = usize::try_from(u64::from_be_bytes(b)).unwrap();
            }
            let mut payload = vec![0u8; len];
            self.stream.read_exact(&mut payload).await.unwrap();
            match opcode {
                0x1 => return Some(String::from_utf8(payload).unwrap()),
                0x8 => return None,
                0x9 => self.send_frame(0xA, &payload).await,
                _ => {}
            }
        }
    }
}

#[tokio::test]
async fn 真实_websocket_端到端() {
    let engine = fast_engine();
    let hub = Arc::new(Hub::new());
    hub.register(Arc::new(MetricsLive::new(engine.clone())));
    let app: axum::Router = super::router(Arc::clone(&hub));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let mut ws = tokio::time::timeout(Duration::from_secs(5), MiniWs::connect(addr, "/ws"))
        .await
        .expect("握手超时");

    ws.send_text(&serde_json::to_string(&env(WsMsgType::Ping, None, Some(1), None)).unwrap())
        .await;
    let pong: WsEnvelope = serde_json::from_str(&ws.recv_text().await.unwrap()).unwrap();
    assert_eq!((pong.t, pong.id), (WsMsgType::Ping, Some(1)));

    ws.send_text(
        &serde_json::to_string(&env(WsMsgType::Sub, Some("metrics.live"), Some(2), None)).unwrap(),
    )
    .await;
    let ack: WsEnvelope = serde_json::from_str(&ws.recv_text().await.unwrap()).unwrap();
    assert_eq!((ack.t, ack.id), (WsMsgType::Resp, Some(2)));

    let data = tokio::time::timeout(Duration::from_secs(5), ws.recv_text())
        .await
        .expect("5s 内应收到推送")
        .unwrap();
    let data: WsEnvelope = serde_json::from_str(&data).unwrap();
    assert_eq!(data.t, WsMsgType::Data);
    assert_eq!(data.ch.as_deref(), Some("metrics.live"));
    let snap: strixmaid_types::metrics::MetricSnapshot =
        serde_json::from_value(data.d.unwrap()).unwrap();
    assert_eq!(snap.values.len(), 2);

    // 未知频道
    ws.send_text(
        &serde_json::to_string(&env(WsMsgType::Sub, Some("nope"), Some(3), None)).unwrap(),
    )
    .await;
    loop {
        let f: WsEnvelope = serde_json::from_str(&ws.recv_text().await.unwrap()).unwrap();
        if f.t == WsMsgType::Err {
            assert_eq!(f.id, Some(3));
            break;
        }
    }
    assert_eq!(hub.connections(), 1);

    // 客户端主动 close：服务端回 close，连接计数归零
    ws.send_frame(0x8, &[0x03, 0xe8]).await;
    loop {
        match tokio::time::timeout(Duration::from_secs(3), ws.recv_text()).await {
            Ok(None) | Err(_) => break,
            Ok(Some(_)) => {}
        }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.connections(), 0);

    engine.stop().await;
    server.abort();
}
