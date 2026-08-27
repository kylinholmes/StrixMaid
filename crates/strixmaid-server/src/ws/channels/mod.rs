//! 频道源。`metrics.live` 由指标引擎驱动；`services.changed` / `logs.follow`
//! 由各自 provider 驱动。`system.health` / `processes.live` 尚未实现。

pub mod logs_follow;
pub mod metrics_live;
pub mod services_changed;

pub use logs_follow::LogsFollow;
pub use metrics_live::MetricsLive;
pub use services_changed::ServicesChanged;
