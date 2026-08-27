//! `metrics.live`：每轮采集推一帧 [`MetricSnapshot`]（design.md §9.2）。
//!
//! - `sub` 的 `d`：可为空；或 `{"prefixes": ["cpu.", "mem."]}` 只要指标名前缀匹配的值；
//! - `data` 的 `d`：[`MetricSnapshot`]；
//! - 订阅成功后**立即**推一帧当前快照（若已有），客户端不用干等一个采集周期；
//! - `req`：返回当前快照（同 `GET /api/v1/metrics/current`）。

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use serde::Deserialize;
use serde_json::Value;
use strixmaid_core::metrics::MetricsEngine;
use strixmaid_types::ApiError;
use strixmaid_types::metrics::MetricSnapshot;
use strixmaid_types::ws::WsChannel;

use crate::ws::hub::{ChannelEvent, ChannelSource, ChannelStream, SubscribeContext, broadcast_stream};

/// 订阅参数。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    /// 指标名前缀过滤；空表示全部。
    #[serde(default)]
    prefixes: Vec<String>,
}

/// `metrics.live` 频道源。
pub struct MetricsLive {
    engine: MetricsEngine,
}

impl MetricsLive {
    /// 绑定到一个引擎。
    pub fn new(engine: MetricsEngine) -> Self {
        MetricsLive { engine }
    }
}

/// 按前缀过滤后序列化。
fn project(prefixes: &[String], snap: &MetricSnapshot) -> Option<Value> {
    if prefixes.is_empty() {
        return serde_json::to_value(snap).ok();
    }
    let filtered = MetricSnapshot {
        ts: snap.ts,
        values: snap
            .values
            .iter()
            .filter(|v| prefixes.iter().any(|p| v.metric.starts_with(p.as_str())))
            .cloned()
            .collect(),
    };
    serde_json::to_value(filtered).ok()
}

impl ChannelSource for MetricsLive {
    fn name(&self) -> &'static str {
        WsChannel::MetricsLive.as_str()
    }

    /// 忽略 `ctx`：全局指标（CPU、内存、磁盘、网络）与谁在看无关，
    /// 采集也留在主进程（`design.md` §2.2），没有按用户区分的余地。
    fn subscribe(
        &self,
        params: Option<Value>,
        _ctx: &SubscribeContext,
    ) -> Result<ChannelStream, ApiError> {
        let params: Params = match params {
            None | Some(Value::Null) => Params::default(),
            Some(v) => serde_json::from_value(v).map_err(|e| {
                ApiError::invalid_request("metrics.live 的订阅参数不合法")
                    .with_detail(e.to_string())
            })?,
        };
        let prefixes = Arc::new(params.prefixes);

        // 先把手头的快照推出去（启动后第一轮之前是空的，此时不推）。
        let current = self.engine.snapshot();
        let initial = if current.values.is_empty() {
            None
        } else {
            project(&prefixes, &current).map(ChannelEvent::Data)
        };

        let live = broadcast_stream(self.engine.subscribe(), move |snap: Arc<MetricSnapshot>| {
            project(&prefixes, &snap)
        });
        Ok(stream::iter(initial).chain(live).boxed())
    }

    fn request(&self, _params: Option<Value>) -> BoxFuture<'static, Result<Value, ApiError>> {
        let snap = self.engine.snapshot();
        Box::pin(async move {
            serde_json::to_value(&*snap)
                .map_err(|e| ApiError::internal("序列化快照失败").with_detail(e.to_string()))
        })
    }
}
