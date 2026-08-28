//! `/ws/agent`：Agent 汇聚端点与在线注册表（roadmap/05 §3.2 §3.3）。
//!
//! # 鉴权不是会话
//!
//! token 走 `Sec-WebSocket-Protocol: bearer, <token>`（与浏览器 WS 同一携带方式，
//! 不进 URL 与日志），但比对对象是 `nodes.token_hash`（`kind = agent`），
//! **不经 PAM、不产生会话**。因此本路由挂在受保护 ws 之外、自带鉴权——
//! 套 `require_auth` 反而会把 Agent 的 token 当成会话 token 去查，必然 401。
//!
//! # 协议
//!
//! 帧格式与频道常量见 `strixmaid_types::agent`。要点：
//!
//! - 首帧必须是 `agent.hello`，其 `node_id` 必须等于 token 所属节点，不一致即断开
//!   ——token 是发给**某个节点**的，拿它冒充别的节点是配置错误，早断比静默错好；
//! - Server 回 `agent.resume { since_ts }`（该节点 `m_1m` 的最大 ts）；
//! - 此后接受 `agent.rows`（写入 `m_1m`，UPSERT 幂等；粗层由本进程的每分钟
//!   `maintain` 聚合——它不分节点，Agent 的行自然一起被卷进去）与
//!   `agent.snapshot`（进 [`AgentRegistry`]，供 `metrics.live?node=` 转发）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use strixmaid_core::session::hash_token;
use strixmaid_core::store::{MetricRow, NodeRecord, Store, now_unix};
use strixmaid_types::agent::{
    AGENT_WS_PATH, AgentHello, AgentResume, AgentRows, CH_AGENT_HELLO, CH_AGENT_RESUME,
    CH_AGENT_ROWS, CH_AGENT_SNAPSHOT,
};
use strixmaid_types::metrics::{MetricLayer, MetricSnapshot};
use strixmaid_types::ws::{WS_PROTOCOL_VERSION, WsEnvelope, WsMsgType};
use strixmaid_types::{ApiError, ErrorCode};
use tokio::sync::broadcast;

use crate::error::ApiErr;

/// 每节点快照广播的队列深度。快照是全量替换，丢几帧无所谓。
const SNAPSHOT_CAPACITY: usize = 8;

/// 心跳落库的最小间隔（内存里每帧都更新，库里限频）。
const DB_TOUCH_SECS: i64 = 60;

// ============================ 在线注册表 ============================

/// 在线 Agent 的注册表：连接状态、最近心跳、最近一帧快照与其广播。
///
/// 只记**本进程生命周期内**连接过的节点；`nodes` 表才是持久事实。
#[derive(Default)]
pub struct AgentRegistry {
    inner: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    connected: bool,
    last_seen: i64,
    latest: Option<Arc<MetricSnapshot>>,
    tx: broadcast::Sender<Arc<MetricSnapshot>>,
}

impl Entry {
    fn new() -> Entry {
        let (tx, _) = broadcast::channel(SNAPSHOT_CAPACITY);
        Entry {
            connected: false,
            last_seen: 0,
            latest: None,
            tx,
        }
    }
}

impl AgentRegistry {
    pub fn new() -> Arc<AgentRegistry> {
        Arc::new(AgentRegistry::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn connect(&self, id: &str) {
        let mut map = self.lock();
        let e = map.entry(id.to_string()).or_insert_with(Entry::new);
        e.connected = true;
        e.last_seen = now_unix();
    }

    fn disconnect(&self, id: &str) {
        if let Some(e) = self.lock().get_mut(id) {
            e.connected = false;
        }
    }

    fn seen(&self, id: &str, ts: i64) {
        if let Some(e) = self.lock().get_mut(id) {
            e.last_seen = ts;
        }
    }

    fn publish(&self, id: &str, snap: Arc<MetricSnapshot>) {
        let mut map = self.lock();
        let e = map.entry(id.to_string()).or_insert_with(Entry::new);
        e.latest = Some(Arc::clone(&snap));
        let _ = e.tx.send(snap);
    }

    /// 此刻是否有存活连接。
    pub fn online(&self, id: &str) -> bool {
        self.lock().get(id).is_some_and(|e| e.connected)
    }

    /// 最近一次收到任何帧的时刻（比库里的 `last_seen` 新鲜）。
    pub fn last_seen(&self, id: &str) -> Option<i64> {
        self.lock().get(id).map(|e| e.last_seen)
    }

    /// 最近一帧快照。
    pub fn latest(&self, id: &str) -> Option<Arc<MetricSnapshot>> {
        self.lock().get(id).and_then(|e| e.latest.clone())
    }

    /// 订阅某节点的快照流；返回（当前最新帧，后续广播）。
    /// 节点自本进程启动以来没连过时返回 `None`。
    #[allow(clippy::type_complexity)]
    pub fn subscribe(
        &self,
        id: &str,
    ) -> Option<(
        Option<Arc<MetricSnapshot>>,
        broadcast::Receiver<Arc<MetricSnapshot>>,
    )> {
        self.lock()
            .get(id)
            .map(|e| (e.latest.clone(), e.tx.subscribe()))
    }
}

// ============================ 端点 ============================

/// `/ws/agent` 的状态。
#[derive(Clone)]
pub struct AgentSocketState {
    pub store: Store,
    pub registry: Arc<AgentRegistry>,
}

/// `/ws/agent` 路由。自带 token 鉴权，**不要**套会话中间件（见模块文档）。
pub fn router<S>(state: AgentSocketState) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
        .route(AGENT_WS_PATH, get(upgrade))
        .with_state(state)
}

async fn upgrade(
    State(st): State<AgentSocketState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    // 鉴权在升级之前：失败要能回一个带状态码的 HTTP 响应。
    let Some(token) = crate::auth::extract::bearer_from_ws_protocol(&headers) else {
        return ApiErr(ApiError::new(
            ErrorCode::Unauthenticated,
            "缺少 Agent token（Sec-WebSocket-Protocol: bearer, <token>）",
        ))
        .into_response();
    };
    let node = match st.store.node_by_token_hash(&hash_token(&token)).await {
        Ok(Some(node)) => node,
        // token 查无此节点与格式错误同响应：错误码的差异会泄露「某个 token 存在」。
        Ok(None) => {
            return ApiErr(ApiError::new(
                ErrorCode::Unauthenticated,
                "token 无效或节点未登记",
            ))
            .into_response();
        }
        Err(e) => {
            return ApiErr(ApiError::internal("查询节点失败").with_detail(e.to_string()))
                .into_response();
        }
    };
    ws.protocols([crate::auth::extract::WS_BEARER_PROTOCOL])
        .on_upgrade(move |socket| serve(st, socket, node))
}

async fn serve(st: AgentSocketState, socket: WebSocket, node: NodeRecord) {
    let (mut sink, mut stream) = socket.split();

    // 首帧必须是 hello，且 node_id 与 token 所属一致。
    let hello = match read_hello(&mut stream).await {
        Ok(h) => h,
        Err(e) => {
            tracing::warn!(node = %node.id, error = %e.message, "Agent 握手失败");
            let _ = send_env(&mut sink, &WsEnvelope::err(None, &e)).await;
            let _ = sink.send(Message::Close(None)).await;
            return;
        }
    };
    if hello.node_id != node.id {
        let e = ApiError::invalid_request(format!(
            "hello 的 node_id（{}）与 token 所属节点（{}）不一致",
            hello.node_id, node.id
        ));
        tracing::warn!(node = %node.id, claimed = %hello.node_id, "Agent 身份不一致，断开");
        let _ = send_env(&mut sink, &WsEnvelope::err(None, &e)).await;
        // 优雅关闭：对端应看到 Close 帧而不是 RST。
        let _ = sink.send(Message::Close(None)).await;
        return;
    }

    st.registry.connect(&node.id);
    let mut last_db_touch = now_unix();
    let _ = st.store.touch_node(&node.id, last_db_touch).await;
    tracing::info!(
        node = %node.id,
        name = %hello.node_name,
        version = %hello.version,
        "Agent 已连接"
    );

    // resume：该节点 m_1m 的最大 ts；语义见 `strixmaid_types::agent` 模块文档。
    let since_ts = match st.store.tier_max_ts(MetricLayer::M1m, &node.id).await {
        Ok(ts) => ts.unwrap_or(0),
        Err(e) => {
            tracing::warn!(node = %node.id, error = %e, "查询补发起点失败，断开");
            st.registry.disconnect(&node.id);
            return;
        }
    };
    let resume = WsEnvelope::data(CH_AGENT_RESUME, serde_json::json!(AgentResume { since_ts }));
    if send_env(&mut sink, &resume).await.is_err() {
        st.registry.disconnect(&node.id);
        return;
    }

    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Ping(_) | Message::Pong(_)) => continue,
            Ok(Message::Binary(_)) => {
                let e = ApiError::invalid_request("Agent 通道不接受二进制帧");
                let _ = send_env(&mut sink, &WsEnvelope::err(None, &e)).await;
                continue;
            }
        };
        let now = now_unix();
        st.registry.seen(&node.id, now);
        if now - last_db_touch >= DB_TOUCH_SECS {
            last_db_touch = now;
            let _ = st.store.touch_node(&node.id, now).await;
        }
        if let Err(e) = handle_text(&st, &node.id, &mut sink, &text).await {
            // 单帧错误不断开：让 Agent 修正后继续。协议级致命错误在各分支内部断。
            let _ = send_env(&mut sink, &WsEnvelope::err(None, &e)).await;
        }
    }

    st.registry.disconnect(&node.id);
    let _ = st.store.touch_node(&node.id, now_unix()).await;
    tracing::info!(node = %node.id, "Agent 已断开");
}

/// 读首帧 hello（15 秒超时）。
async fn read_hello(
    stream: &mut (impl StreamExt<Item = Result<Message, axum::Error>> + Unpin),
) -> Result<AgentHello, ApiError> {
    let msg = tokio::time::timeout(std::time::Duration::from_secs(15), stream.next())
        .await
        .map_err(|_| ApiError::invalid_request("15 秒内未收到 agent.hello"))?
        .ok_or_else(|| ApiError::invalid_request("连接在 hello 之前关闭"))?
        .map_err(|e| ApiError::invalid_request(format!("读取 hello 失败: {e}")))?;
    let Message::Text(text) = msg else {
        return Err(ApiError::invalid_request("首帧必须是文本 envelope"));
    };
    let env: WsEnvelope = serde_json::from_str(&text)
        .map_err(|e| ApiError::invalid_request(format!("hello 不是合法 envelope: {e}")))?;
    if env.ch.as_deref() != Some(CH_AGENT_HELLO) {
        return Err(ApiError::invalid_request("首帧必须是 agent.hello"));
    }
    serde_json::from_value(env.d.unwrap_or(serde_json::Value::Null))
        .map_err(|e| ApiError::invalid_request(format!("hello 的 payload 不合法: {e}")))
}

async fn handle_text(
    st: &AgentSocketState,
    node_id: &str,
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    text: &str,
) -> Result<(), ApiError> {
    let env: WsEnvelope = serde_json::from_str(text)
        .map_err(|e| ApiError::invalid_request(format!("无法解析 envelope: {e}")))?;
    if env.v != WS_PROTOCOL_VERSION {
        return Err(ApiError::invalid_request(format!(
            "不支持的协议版本 {}",
            env.v
        )));
    }
    if env.t == WsMsgType::Ping {
        let _ = send_env(sink, &env).await;
        return Ok(());
    }
    match env.ch.as_deref() {
        Some(CH_AGENT_ROWS) => {
            let frame: AgentRows =
                serde_json::from_value(env.d.unwrap_or(serde_json::Value::Null))
                    .map_err(|e| ApiError::invalid_request(format!("agent.rows 不合法: {e}")))?;
            let n = apply_rows(&st.store, node_id, &frame).await?;
            tracing::debug!(node = node_id, rows = n, "已写入 Agent 行");
            Ok(())
        }
        Some(CH_AGENT_SNAPSHOT) => {
            let snap: MetricSnapshot =
                serde_json::from_value(env.d.unwrap_or(serde_json::Value::Null)).map_err(|e| {
                    ApiError::invalid_request(format!("agent.snapshot 不合法: {e}"))
                })?;
            st.registry.publish(node_id, Arc::new(snap));
            Ok(())
        }
        other => Err(ApiError::invalid_request(format!(
            "Agent 通道不认识 {other:?} 帧"
        ))),
    }
}

/// 把一帧 `agent.rows` 写进本地库：series 按 `(节点, metric, labels)` 映射到
/// 本地 id，行走 `m_1m` 的 UPSERT（幂等）。
async fn apply_rows(store: &Store, node_id: &str, frame: &AgentRows) -> Result<u64, ApiError> {
    if frame.layer != MetricLayer::M1m {
        // 粗层由本进程的每分钟 maintain 聚合，直接收粗层会与之打架。
        return Err(ApiError::invalid_request(format!(
            "agent.rows 只接受 m_1m，收到 {}",
            frame.layer
        )));
    }
    let internal =
        |what: &str, e: strixmaid_core::store::StoreError| ApiError::internal(what.to_owned()).with_detail(e.to_string());
    let mut ids = Vec::with_capacity(frame.series.len());
    for d in &frame.series {
        ids.push(
            store
                .get_or_create_series_raw(node_id, &d.metric, &d.labels, d.unit.as_deref())
                .await
                .map_err(|e| internal("登记 series 失败", e))?,
        );
    }
    let mut rows = Vec::with_capacity(frame.rows.len());
    for r in &frame.rows {
        let Some(&series_id) = ids.get(r.s as usize) else {
            return Err(ApiError::invalid_request(format!(
                "行引用了越界的 series 下标 {}",
                r.s
            )));
        };
        rows.push(MetricRow {
            series_id,
            ts: r.ts,
            cnt: r.cnt,
            min: r.min,
            max: r.max,
            sum: r.sum,
            med: r.med,
        });
    }
    store
        .insert_1m(&rows)
        .await
        .map_err(|e| internal("写入 Agent 行失败", e))
}

async fn send_env(
    sink: &mut (impl SinkExt<Message, Error = axum::Error> + Unpin),
    env: &WsEnvelope,
) -> Result<(), axum::Error> {
    let text = serde_json::to_string(env).unwrap_or_default();
    sink.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use strixmaid_core::session::hash_token;
    use strixmaid_core::store::NodeKind;
    use strixmaid_types::agent::{AgentRowItem, AgentSeriesDesc};
    use tokio_tungstenite::tungstenite::Message as TgMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

    use super::*;

    async fn start_server() -> (Store, Arc<AgentRegistry>, std::net::SocketAddr) {
        let store = Store::open_in_memory().await.unwrap();
        let registry = AgentRegistry::new();
        let app: Router = router(AgentSocketState {
            store: store.clone(),
            registry: Arc::clone(&registry),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (store, registry, addr)
    }

    async fn connect(
        addr: std::net::SocketAddr,
        token: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        tokio_tungstenite::tungstenite::Error,
    > {
        let mut req = format!("ws://{addr}/ws/agent").into_client_request().unwrap();
        req.headers_mut().insert(
            "sec-websocket-protocol",
            format!("bearer, {token}").parse().unwrap(),
        );
        tokio_tungstenite::connect_async(req).await.map(|(ws, _)| ws)
    }

    fn env_data(ch: &str, d: serde_json::Value) -> TgMessage {
        TgMessage::Text(
            serde_json::to_string(&WsEnvelope::data(ch, d))
                .unwrap()
                .into(),
        )
    }

    async fn recv_env(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> WsEnvelope {
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
                .await
                .expect("5 秒内应有帧")
                .expect("流不该结束")
                .expect("读帧失败");
            if let TgMessage::Text(t) = msg {
                return serde_json::from_str(&t).unwrap();
            }
        }
    }

    fn hello(node_id: &str) -> serde_json::Value {
        serde_json::json!(AgentHello {
            node_id: node_id.into(),
            node_name: "测试节点".into(),
            version: "0.0.0".into(),
            caps: Default::default(),
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn 端到端_鉴权_补发起点_行写入_快照() {
        let (store, registry, addr) = start_server().await;
        store
            .upsert_node("web-01", "Web 1", NodeKind::Agent, Some(&hash_token("t0k3n")))
            .await
            .unwrap();

        // 错误 token 在握手就被拒（roadmap/05 §5.3）。
        let err = connect(addr, "wrong").await.expect_err("错误 token 必须被拒");
        match err {
            tokio_tungstenite::tungstenite::Error::Http(resp) => {
                assert_eq!(resp.status(), 401, "应是 401");
            }
            other => panic!("预期 HTTP 401，实际 {other:?}"),
        }

        // 正确 token：hello → resume(0)。
        let mut ws = connect(addr, "t0k3n").await.unwrap();
        ws.send(env_data(CH_AGENT_HELLO, hello("web-01"))).await.unwrap();
        let resume = recv_env(&mut ws).await;
        assert_eq!(resume.ch.as_deref(), Some(CH_AGENT_RESUME));
        assert_eq!(resume.d.unwrap()["since_ts"], 0);
        assert!(registry.online("web-01"));

        // 两条 series × 两个 ts 的行帧。
        let frame = AgentRows {
            layer: MetricLayer::M1m,
            series: vec![
                AgentSeriesDesc {
                    metric: "cpu.usage".into(),
                    labels: String::new(),
                    unit: Some("percent".into()),
                },
                AgentSeriesDesc {
                    metric: "mem.used".into(),
                    labels: String::new(),
                    unit: Some("bytes".into()),
                },
            ],
            rows: vec![
                AgentRowItem { s: 0, ts: 60, cnt: 30, min: 1.0, max: 9.0, sum: 90.0, med: 3.0 },
                AgentRowItem { s: 1, ts: 60, cnt: 30, min: 1.0, max: 2.0, sum: 45.0, med: 1.5 },
                AgentRowItem { s: 0, ts: 120, cnt: 30, min: 2.0, max: 8.0, sum: 80.0, med: 2.5 },
                AgentRowItem { s: 1, ts: 120, cnt: 30, min: 1.0, max: 2.0, sum: 46.0, med: 1.6 },
            ],
        };
        ws.send(env_data(CH_AGENT_ROWS, serde_json::json!(frame))).await.unwrap();

        // 快照帧。
        let snap = serde_json::json!({ "ts": 130, "values": [
            { "metric": "cpu.usage", "labels": "", "value": 3.3, "unit": "percent" }
        ]});
        ws.send(env_data(CH_AGENT_SNAPSHOT, snap)).await.unwrap();

        // 轮询等待落库与注册表更新。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let max = store.tier_max_ts(MetricLayer::M1m, "web-01").await.unwrap();
            if max == Some(120) && registry.latest("web-01").is_some() {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "5 秒内应完成写入，当前 {max:?}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let cpu_id = store.find_series("web-01", "cpu.usage", "").await.unwrap().unwrap();
        assert_eq!(store.count_tier(MetricLayer::M1m, cpu_id).await.unwrap(), 2);
        assert_eq!(registry.latest("web-01").unwrap().ts, 130);

        // 断开重连：resume 从服务端最大 ts 继续。
        drop(ws);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while registry.online("web-01") {
            assert!(std::time::Instant::now() < deadline, "断开应被感知");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let mut ws = connect(addr, "t0k3n").await.unwrap();
        ws.send(env_data(CH_AGENT_HELLO, hello("web-01"))).await.unwrap();
        let resume = recv_env(&mut ws).await;
        assert_eq!(resume.d.unwrap()["since_ts"], 120, "补发起点是服务端已有的最大 ts");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hello_的_node_id_必须与_token_所属一致() {
        let (store, _registry, addr) = start_server().await;
        store
            .upsert_node("web-01", "Web 1", NodeKind::Agent, Some(&hash_token("t0k3n")))
            .await
            .unwrap();
        let mut ws = connect(addr, "t0k3n").await.unwrap();
        ws.send(env_data(CH_AGENT_HELLO, hello("evil-node"))).await.unwrap();
        let env = recv_env(&mut ws).await;
        assert_eq!(env.t, WsMsgType::Err);
        // 之后连接应被服务端关闭。
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
            .await
            .expect("应收到关闭");
        // Close 帧、流结束、或对端已撤走连接（Err）都算断开；
        // 绝不能是又一条正常数据帧。
        assert!(
            !matches!(next, Some(Ok(TgMessage::Text(_) | TgMessage::Binary(_)))),
            "身份不一致必须断开：{next:?}"
        );
    }

    #[tokio::test]
    async fn apply_rows_只认_m_1m_且下标越界被拒() {
        let store = Store::open_in_memory().await.unwrap();
        let bad_layer = AgentRows {
            layer: MetricLayer::M5m,
            series: vec![],
            rows: vec![],
        };
        let err = apply_rows(&store, "n1", &bad_layer).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        let bad_idx = AgentRows {
            layer: MetricLayer::M1m,
            series: vec![AgentSeriesDesc {
                metric: "cpu.usage".into(),
                labels: String::new(),
                unit: None,
            }],
            rows: vec![AgentRowItem { s: 7, ts: 60, cnt: 1, min: 0.0, max: 0.0, sum: 0.0, med: 0.0 }],
        };
        let err = apply_rows(&store, "n1", &bad_idx).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("越界"));
    }
}
