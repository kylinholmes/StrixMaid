//! 共享应用状态。
//!
//! Phase 0 只需要进程启动时间。后续 Phase 会把 `AgentCore` 句柄、会话管理器、
//! 节点注册表挂在这里（§11：Server 内含一个 AgentCore 实例，即 `local` 节点）。

use std::time::Instant;

/// axum 的共享状态；`Clone` 成本 = 一个 `Instant` 的拷贝。
#[derive(Debug, Clone)]
pub struct AppState {
    started_at: Instant,
}

impl AppState {
    /// 以「此刻」为启动时间创建状态。
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }

    /// 进程已运行秒数。
    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
