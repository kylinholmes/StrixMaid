//! `logs.follow` 频道：journald 实时追加。
//!
//! 订阅参数就是 `LogQuery`（priority / unit / q / boot …），每帧 `d` 是 `Vec<LogEntry>`。
//! 底层 `LogFollow` 持有一个 `journalctl -f` 子进程的共享句柄，drop 即退订；
//! 同一过滤条件的多个订阅共享一个子进程，全部退订后子进程被 kill。

use std::sync::Arc;

use futures::stream::{self, StreamExt};
use serde_json::Value;
use strixmaid_core::providers::log::LogProvider;
use strixmaid_types::ApiError;
use strixmaid_types::log::LogQuery;
use strixmaid_types::ws::WsChannel;

use crate::ws::hub::{ChannelEvent, ChannelSource, ChannelStream};

pub struct LogsFollow {
    provider: Arc<dyn LogProvider>,
}

impl LogsFollow {
    pub fn new(provider: Arc<dyn LogProvider>) -> Self {
        LogsFollow { provider }
    }
}

impl ChannelSource for LogsFollow {
    fn name(&self) -> &'static str {
        WsChannel::LogsFollow.as_str()
    }

    fn subscribe(&self, params: Option<Value>) -> Result<ChannelStream, ApiError> {
        let query: LogQuery = match params {
            None | Some(Value::Null) => LogQuery::default(),
            Some(v) => serde_json::from_value(v).map_err(|e| {
                ApiError::invalid_request("logs.follow 的订阅参数不合法").with_detail(e.to_string())
            })?,
        };
        let provider = Arc::clone(&self.provider);
        let follow = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(provider.follow(&query))
        })?;
        Ok(stream::unfold(follow, |mut f| async move {
            f.next().await.map(|batch| {
                let ev = serde_json::to_value(&*batch)
                    .map(ChannelEvent::Data)
                    .unwrap_or(ChannelEvent::Lagged(0));
                (ev, f)
            })
        })
        .boxed())
    }
}
