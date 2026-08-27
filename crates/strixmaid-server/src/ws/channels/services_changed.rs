//! `services.changed` 频道：systemd unit 状态变更推送。
//!
//! 数据来自 `ServiceProvider::subscribe()` 的 broadcast（zbus 信号驱动、已去抖 200ms）。
//! 每帧 `d` 是 `Vec<UnitSummary>`；被 GC 的 unit 以 `load_state = not_found` 出现，前端据此删行。
//! 降级到 `systemctl` 时 receiver 永远安静但不会关闭，订阅者只是收不到推送。
//!
//! # 为什么它留在主进程（`roadmap/01-worker-execution.md` §8 未决问题 2）
//!
//! 与 `logs.follow` 不同，本频道推的内容对所有本地用户本来就是可见的——
//! `systemctl list-units` 不需要任何权限，任何能登录的用户跑一遍都能看到同样的
//! unit 状态。因此把事件源留在主进程不会泄露任何用户本来看不到的东西，换来的是
//! 一份共享的 zbus 订阅（每个会话各起一份 match rule 是纯粹的浪费）。
//!
//! 取舍要记在这里：这一条是**内容可见性**的判断，不是「订阅便宜所以就这样」。
//! 若将来本频道开始携带按用户区分的信息（例如 `scope = user` 的 unit），
//! 这个前提就不成立了，届时应改为 `WorkerHandle::subscribe`。

use std::sync::Arc;

use serde_json::Value;
use strixmaid_core::providers::service::{ServiceEvent, ServiceProvider};
use strixmaid_types::ApiError;
use strixmaid_types::ws::WsChannel;

use crate::ws::hub::{ChannelSource, ChannelStream, SubscribeContext, broadcast_stream};

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

    /// 忽略 `ctx`：见模块文档，unit 状态对所有本地用户可见。
    fn subscribe(
        &self,
        _params: Option<Value>,
        _ctx: &SubscribeContext,
    ) -> Result<ChannelStream, ApiError> {
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
