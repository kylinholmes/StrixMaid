//! `services.changed` 频道：systemd unit 状态变更推送。
//!
//! 数据来自 `ServiceProvider::subscribe()` 的 broadcast（zbus 信号驱动、已去抖 200ms）。
//! 每帧 `d` 是 `Vec<UnitSummary>`；被 GC 的 unit 以 `load_state = not_found` 出现，前端据此删行。
//! 降级到 `systemctl` 时 receiver 永远安静但不会关闭，订阅者只是收不到推送。

use std::sync::Arc;

use serde_json::Value;
use strixmaid_core::providers::service::{ServiceEvent, ServiceProvider};
use strixmaid_types::ApiError;
use strixmaid_types::ws::WsChannel;

use crate::ws::hub::{ChannelSource, ChannelStream, broadcast_stream};

pub struct ServicesChanged {
    provider: Arc<dyn ServiceProvider>,
}

impl ServicesChanged {
    pub fn new(provider: Arc<dyn ServiceProvider>) -> Self {
        ServicesChanged { provider }
    }
}

impl ChannelSource for ServicesChanged {
    fn name(&self) -> &'static str {
        WsChannel::ServicesChanged.as_str()
    }

    fn subscribe(&self, _params: Option<Value>) -> Result<ChannelStream, ApiError> {
        // `subscribe()` 是 async（要先在 bus 上注册 match rule），而 trait 方法是同步的：
        // 用 `block_in_place` 桥接——这只在订阅那一刻发生一次，不在热路径上。
        let provider = Arc::clone(&self.provider);
        let rx = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(provider.subscribe())
        });
        Ok(broadcast_stream(rx, |ev: ServiceEvent| {
            serde_json::to_value(ev).ok()
        }))
    }
}
