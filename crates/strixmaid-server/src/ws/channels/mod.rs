//! 频道源。事件从哪里来，取决于**内容的可见范围是否随用户**
//! （`roadmap/01-worker-execution.md` §4.4、`roadmap/04` §B）：
//!
//! | 频道 | 事件源 | 为什么 |
//! |---|---|---|
//! | `metrics.live` | 主进程的指标引擎 | 全局指标，与谁在看无关 |
//! | `services.changed` | 主进程的 `ServiceProvider`（zbus 信号） | unit 状态对所有本地用户可见 |
//! | `system.health` | 主进程每 30s 重算 | 健康是全局事实；见 `system_health` 模块文档 |
//! | `logs.follow` | 该会话的 **user worker** | journald ACL 按 uid 裁决，可见范围必须随用户 |
//! | `processes.live` | 该会话的 **user worker** | CPU% 差分基线随 worker，与 REST 共享 |

pub mod logs_follow;
pub mod metrics_live;
pub mod processes_live;
pub mod services_changed;
pub mod system_health;

pub use logs_follow::LogsFollow;
pub use metrics_live::MetricsLive;
pub use processes_live::ProcessesLive;
pub use services_changed::ServicesChanged;
pub use system_health::SystemHealth;

use serde_json::Value;
use strixmaid_core::session::Session;
use strixmaid_types::{ApiError, ErrorCode};

use crate::auth::AuthState;

/// 取该会话的 user worker 并发起订阅——`logs.follow` 与 `processes.live` 共用。
///
/// worker 不在 = 会话的执行者已经没了。与 `auth::exec::call` 同一判断：
/// 这不是「没权限」，是「没人能替你执行」，因此报 401 让客户端重新登录。
pub(crate) async fn worker_subscribe(
    auth: &AuthState,
    session: &Session,
    channel: &str,
    params: Value,
) -> Result<tokio::sync::mpsc::Receiver<Value>, ApiError> {
    let worker = auth
        .sessions
        .user_worker(&session.token_hash)
        .await
        .ok_or_else(|| {
            ApiError::new(
                ErrorCode::Unauthenticated,
                "会话的 worker 已退出，请重新登录",
            )
        })?;
    worker.subscribe(channel, params).await
}
