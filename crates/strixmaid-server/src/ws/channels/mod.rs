//! 频道源。三个频道的事件从哪里来，取决于**内容的可见范围是否随用户**
//! （`roadmap/01-worker-execution.md` §4.4）：
//!
//! | 频道 | 事件源 | 为什么 |
//! |---|---|---|
//! | `metrics.live` | 主进程的指标引擎 | 全局指标，与谁在看无关 |
//! | `services.changed` | 主进程的 `ServiceProvider`（zbus 信号） | unit 状态对所有本地用户可见 |
//! | `logs.follow` | 该会话的 **user worker** | journald ACL 按 uid 裁决，可见范围必须随用户 |
//!
//! `system.health` / `processes.live` 尚未实现。

pub mod logs_follow;
pub mod metrics_live;
pub mod services_changed;

pub use logs_follow::LogsFollow;
pub use metrics_live::MetricsLive;
pub use services_changed::ServicesChanged;
