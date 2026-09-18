//! 别的节点的快照来源——node 唯一一处「多节点」的缝。
//!
//! node 只认识**本机这一个节点**。但 `GET /metrics/current?node=X` 与
//! `metrics.live` 频道的 `?node=X` 都允许问别的节点，而「别的节点是什么」是宿主
//! 的知识：Server 有一张 agent 注册表，Agent 什么都没有。
//!
//! 所以这里定义缝、宿主填实现（依赖倒置）。node 的依赖表因此不含任何多节点的东西，
//! `ws::agent`、`AgentRegistry`、`/nodes` 全都留在 Server。
//!
//! 传 `None` 时行为与「该节点没连过」一致：查询返回 404，订阅直接结束。

use std::sync::Arc;

use strixmaid_types::metrics::MetricSnapshot;
use tokio::sync::broadcast;

/// 宿主提供的远端节点快照来源。
pub trait RemoteSnapshots: Send + Sync + 'static {
    /// 某节点最近一帧快照。没连过或还没有快照时 `None`。
    fn latest(&self, node: &str) -> Option<Arc<MetricSnapshot>>;

    /// 订阅某节点的快照流，返回（当前最新帧，后续广播）。
    ///
    /// 「当前最新帧」本身也可能是 `None`——节点连上了但还没推过任何一帧。
    /// 外层的 `Option` 才表示「这个节点自本进程启动以来没连过」。
    #[allow(clippy::type_complexity)]
    fn subscribe(
        &self,
        node: &str,
    ) -> Option<(
        Option<Arc<MetricSnapshot>>,
        broadcast::Receiver<Arc<MetricSnapshot>>,
    )>;
}
