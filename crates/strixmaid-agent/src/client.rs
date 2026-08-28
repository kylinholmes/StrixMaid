//! 与 Server 的连接循环（roadmap/05 §3.2）。
//!
//! ```text
//! connect → hello → resume(since_ts) ──┬─ 快照转发：engine.subscribe() → agent.snapshot
//!                                      ├─ 行同步：每 sync_interval 把本地 m_1m 新行推成 agent.rows
//!                                      └─ 入站：ping 回显、agent.request（host.info / caps.probe）
//! 断连 → 指数退避（5s 起、封顶 60s，稳定运行过 60s 即重置）→ 重连
//! ```
//!
//! # 补发与常规推送是同一条路
//!
//! 水位 `(ts, series_id)` 从 `resume.since_ts` 起步（含 `since_ts` 本身，语义见
//! `strixmaid_types::agent`），之后每个节拍把水位之后的新行分批推出去——
//! 「刚重连的大批补发」与「平时每分钟的几十行」只是批数不同。发送中途断开
//! 也不用记账：重连后 Server 会重新给出它已有的最大 ts，UPSERT 吸收重叠。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt};
use strixmaid_core::metrics::MetricsEngine;
use strixmaid_core::providers::system::HostProvider;
use strixmaid_core::store::{ExportRow, MetricLayer, Store};
use strixmaid_types::agent::{
    AGENT_WS_PATH, AgentHello, AgentResume, AgentRowItem, AgentRows, AgentSeriesDesc,
    CH_AGENT_HELLO, CH_AGENT_REQUEST, CH_AGENT_RESUME, CH_AGENT_ROWS, CH_AGENT_SNAPSHOT,
    ROWS_PER_FRAME,
};
use strixmaid_types::capability::SystemCapabilities;
use strixmaid_types::ws::{WsEnvelope, WsMsgType};
use tokio::sync::{broadcast, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::connect_async;

/// 等 `agent.resume` 的上限。
const RESUME_TIMEOUT: Duration = Duration::from_secs(15);
/// 重连退避的起点与封顶。
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// 一次连接会话所需的全部东西。
pub struct AgentRuntime {
    pub server_url: String,
    pub node_id: String,
    pub node_name: String,
    pub token: String,
    pub caps: SystemCapabilities,
    pub store: Store,
    pub engine: MetricsEngine,
    pub sync_interval: Duration,
}

/// 一次连接的结束方式。
enum ConnectionEnd {
    /// 对端关闭或网络断开，应重连。
    Closed,
    /// 收到关停信号，整个循环退出。
    Shutdown,
}

/// 连接循环主体。`shutdown` 翻真即退出。
pub async fn run(rt: AgentRuntime, mut shutdown: watch::Receiver<bool>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let started = Instant::now();
        match connect_once(&rt, &mut shutdown).await {
            Ok(ConnectionEnd::Shutdown) => return,
            Ok(ConnectionEnd::Closed) => {
                tracing::info!("与 Server 的连接结束，准备重连");
            }
            Err(e) => {
                tracing::warn!(error = %e, "连接失败");
            }
        }
        // 稳定运行过一段时间说明上次的失败原因已经过去，退避从头来。
        if started.elapsed() >= Duration::from_secs(60) {
            backoff = BACKOFF_MIN;
        }
        tracing::info!(secs = backoff.as_secs(), "退避后重连");
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = shutdown.changed() => return,
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn connect_once(
    rt: &AgentRuntime,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<ConnectionEnd> {
    let url = format!("{}{}", rt.server_url.trim_end_matches('/'), AGENT_WS_PATH);
    let mut request = url
        .as_str()
        .into_client_request()
        .context("server_url 不合法")?;
    // 与浏览器同一携带方式：token 走子协议，不进 URL 与日志。
    request.headers_mut().insert(
        "sec-websocket-protocol",
        format!("bearer, {}", rt.token)
            .parse()
            .context("token 含非法字符")?,
    );
    let (ws, _resp) = connect_async(request).await.context("WS 握手失败")?;
    let (mut sink, mut stream) = ws.split();

    send_env(
        &mut sink,
        &WsEnvelope::data(
            CH_AGENT_HELLO,
            serde_json::json!(AgentHello {
                node_id: rt.node_id.clone(),
                node_name: rt.node_name.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
                caps: rt.caps,
            }),
        ),
    )
    .await?;

    let since_ts = wait_resume(&mut stream).await?;
    tracing::info!(since_ts, "已连接 Server，从该时刻起补发");
    // 含 since_ts 本身：`(since_ts - 1, MAX)` 之后的第一行就是 ts == since_ts。
    let mut watermark = (since_ts - 1, i64::MAX);

    let mut snapshots = rt.engine.subscribe();
    let mut sync = tokio::time::interval(rt.sync_interval);
    sync.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                let _ = sink.send(Message::Close(None)).await;
                return Ok(ConnectionEnd::Shutdown);
            }
            snap = snapshots.recv() => match snap {
                Ok(s) => {
                    let env = WsEnvelope::data(CH_AGENT_SNAPSHOT, serde_json::to_value(&*s)?);
                    send_env(&mut sink, &env).await?;
                }
                // 落后就丢：快照是全量替换，追最新的就行。
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => {
                    bail!("指标引擎已停止");
                }
            },
            // 第一个 tick 立即到期：连上就先把积压推掉。
            _ = sync.tick() => {
                watermark = push_rows(&rt.store, &mut sink, watermark).await?;
            }
            msg = stream.next() => match msg {
                None => return Ok(ConnectionEnd::Closed),
                Some(Err(e)) => return Err(e).context("读取 Server 帧失败"),
                Some(Ok(Message::Close(_))) => return Ok(ConnectionEnd::Closed),
                Some(Ok(Message::Ping(p))) => {
                    let _ = sink.send(Message::Pong(p)).await;
                }
                Some(Ok(Message::Text(text))) => {
                    handle_incoming(rt, &mut sink, &text).await?;
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

/// 把水位之后的本地 `m_1m` 新行推给 Server，返回推进后的水位。
async fn push_rows(
    store: &Store,
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    mut watermark: (i64, i64),
) -> anyhow::Result<(i64, i64)> {
    loop {
        let batch = store
            .export_after(
                MetricLayer::M1m,
                watermark.0,
                watermark.1,
                ROWS_PER_FRAME as i64,
            )
            .await
            .context("读取本地待发行失败")?;
        if batch.is_empty() {
            return Ok(watermark);
        }
        let last = batch.last().expect("非空").row;
        watermark = (last.ts, last.series_id);
        let full = batch.len() == ROWS_PER_FRAME;
        let frame = build_rows(&batch);
        tracing::debug!(rows = frame.rows.len(), series = frame.series.len(), "推送行帧");
        send_env(
            sink,
            &WsEnvelope::data(CH_AGENT_ROWS, serde_json::to_value(&frame)?),
        )
        .await?;
        if !full {
            return Ok(watermark);
        }
    }
}

/// 把一批导出行组装成 `agent.rows`：帧内 series 去重，行引用下标。
fn build_rows(batch: &[ExportRow]) -> AgentRows {
    let mut index: HashMap<(&str, &str), u32> = HashMap::new();
    let mut series = Vec::new();
    let mut rows = Vec::with_capacity(batch.len());
    for e in batch {
        let key = (e.metric.as_str(), e.labels.as_str());
        let s = *index.entry(key).or_insert_with(|| {
            series.push(AgentSeriesDesc {
                metric: e.metric.clone(),
                labels: e.labels.clone(),
                unit: e.unit.clone(),
            });
            (series.len() - 1) as u32
        });
        rows.push(AgentRowItem {
            s,
            ts: e.row.ts,
            cnt: e.row.cnt,
            min: e.row.min,
            max: e.row.max,
            sum: e.row.sum,
            med: e.row.med,
        });
    }
    AgentRows {
        layer: MetricLayer::M1m,
        series,
        rows,
    }
}

/// 等待并解析 `agent.resume`。
async fn wait_resume(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
              + Unpin),
) -> anyhow::Result<i64> {
    let deadline = tokio::time::Instant::now() + RESUME_TIMEOUT;
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .context("等待 agent.resume 超时")?
            .context("连接在 resume 之前关闭")?
            .context("读取 resume 失败")?;
        let Message::Text(text) = msg else { continue };
        let env: WsEnvelope = serde_json::from_str(&text).context("resume 不是合法 envelope")?;
        if env.t == WsMsgType::Err {
            bail!("Server 拒绝：{}", serde_json::to_string(&env.d).unwrap_or_default());
        }
        if let Some(since) = parse_resume(&env) {
            return Ok(since);
        }
        // 别的帧（不太可能）先跳过，继续等。
    }
}

/// 从 envelope 里取 `agent.resume.since_ts`。不是 resume 帧时 `None`。
fn parse_resume(env: &WsEnvelope) -> Option<i64> {
    if env.ch.as_deref() != Some(CH_AGENT_RESUME) {
        return None;
    }
    let resume: AgentResume = serde_json::from_value(env.d.clone()?).ok()?;
    Some(resume.since_ts)
}

/// 处理 Server 发来的文本帧：协议 ping 回显；`agent.request` 只允许
/// `host.info` 与 `caps.probe`（roadmap/05 §3.2 的预留通道）。
async fn handle_incoming(
    rt: &AgentRuntime,
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    text: &str,
) -> anyhow::Result<()> {
    let Ok(env) = serde_json::from_str::<WsEnvelope>(text) else {
        tracing::debug!(text, "无法解析 Server 帧，忽略");
        return Ok(());
    };
    if env.t == WsMsgType::Ping {
        send_env(sink, &env).await?;
        return Ok(());
    }
    if env.ch.as_deref() != Some(CH_AGENT_REQUEST) {
        tracing::debug!(?env.ch, "未预期的 Server 帧，忽略");
        return Ok(());
    }
    let method = env
        .d
        .as_ref()
        .and_then(|d| d.get("method"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let result = match method {
        "host.info" => match HostProvider::new().system_info().await {
            Ok(info) => Ok(serde_json::to_value(info)?),
            Err(e) => Err(e),
        },
        "caps.probe" => Ok(serde_json::to_value(rt.caps)?),
        other => Err(strixmaid_types::ApiError::invalid_request(format!(
            "Agent 不接受 {other}（MVP 只允许 host.info / caps.probe）"
        ))),
    };
    let reply = match result {
        Ok(value) => WsEnvelope {
            v: strixmaid_types::ws::WS_PROTOCOL_VERSION,
            t: WsMsgType::Resp,
            ch: Some(CH_AGENT_REQUEST.into()),
            id: env.id,
            d: Some(value),
        },
        Err(e) => WsEnvelope::err(env.id, &e),
    };
    send_env(sink, &reply).await
}

async fn send_env(
    sink: &mut (impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin),
    env: &WsEnvelope,
) -> anyhow::Result<()> {
    let text = serde_json::to_string(env)?;
    sink.send(Message::Text(text.into()))
        .await
        .context("发送帧失败")
}


#[cfg(test)]
mod tests {
    use strixmaid_core::store::MetricRow;

    use super::*;

    fn export(metric: &str, labels: &str, sid: i64, ts: i64) -> ExportRow {
        ExportRow {
            metric: metric.into(),
            labels: labels.into(),
            unit: Some("count".into()),
            row: MetricRow {
                series_id: sid,
                ts,
                cnt: 1,
                min: 1.0,
                max: 1.0,
                sum: 1.0,
                med: 1.0,
            },
        }
    }

    #[test]
    fn 行帧的_series_去重与下标引用() {
        let batch = vec![
            export("cpu.usage", "", 1, 60),
            export("mem.used", "", 2, 60),
            export("cpu.usage", "", 1, 120),
            export("disk.util", "dev=sda", 3, 120),
        ];
        let frame = build_rows(&batch);
        assert_eq!(frame.layer, MetricLayer::M1m);
        assert_eq!(frame.series.len(), 3, "同一 series 只登记一次");
        assert_eq!(frame.rows.len(), 4);
        // 每行的下标都指回自己的 series。
        for (e, r) in batch.iter().zip(&frame.rows) {
            let d = &frame.series[r.s as usize];
            assert_eq!(d.metric, e.metric);
            assert_eq!(d.labels, e.labels);
            assert_eq!(r.ts, e.row.ts);
        }
    }

    #[test]
    fn resume_解析() {
        let env = WsEnvelope::data(CH_AGENT_RESUME, serde_json::json!({ "since_ts": 1200 }));
        assert_eq!(parse_resume(&env), Some(1200));
        let other = WsEnvelope::data(CH_AGENT_ROWS, serde_json::json!({}));
        assert_eq!(parse_resume(&other), None);
        let no_payload = WsEnvelope {
            d: None,
            ..WsEnvelope::data(CH_AGENT_RESUME, serde_json::Value::Null)
        };
        assert_eq!(parse_resume(&no_payload), None);
    }
}
