//! Provider：对某项系统能力的封装，并能自报可用性。
//!
//! 设计原则（`docs/design.md` §1）：除 systemd 外默认假设系统里什么都没有。
//! 每个 provider 在启动时 [`Provider::probe`] 一次，结果进入 `/api/v1/capabilities`
//! 的 `system` 层；探测不到的能力由前端隐藏对应页面——**优雅降级，而非报错**。
//!
//! 各子模块只暴露实现，具体 trait（`ServiceProvider` / `LogProvider` / …）
//! 定义在各自的 `mod.rs` 里，本文件只放公共部分。

use async_trait::async_trait;

pub mod log;
pub mod process;
pub mod service;
pub mod system;

/// 探测结果。`Degraded` 表示能力存在但走的是降级路径
/// （例如 systemd 连不上 bus、退化成 `systemctl` 子进程），
/// 前端应展示但可提示功能受限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    Available,
    Degraded { reason: String },
    Unavailable { reason: String },
}

impl Probe {
    pub fn is_available(&self) -> bool {
        !matches!(self, Probe::Unavailable { .. })
    }
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Probe::Unavailable { reason: reason.into() }
    }
    pub fn degraded(reason: impl Into<String>) -> Self {
        Probe::Degraded { reason: reason.into() }
    }
}

/// 所有 provider 的公共接口。
///
/// `id` 是稳定的机器可读标识（`"systemd"` / `"journald"` / `"proc"` / `"host"`），
/// 会直接出现在 capabilities 响应与日志里，定了就不要改。
#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &'static str;
    /// 探测能力是否可用。可能需要 I/O（连 bus、试读文件），故为 async。
    /// 必须**快速且无副作用**，启动期会对所有 provider 调用一次。
    async fn probe(&self) -> Probe;
}

/// 让 `Arc<dyn ServiceProvider>` 这类共享句柄也能直接注册进 `CapabilityRegistry`
/// （它要 `Box<dyn Provider>`），不必为每种 provider 写适配器。
#[async_trait]
impl<T: Provider + ?Sized> Provider for std::sync::Arc<T> {
    fn id(&self) -> &'static str {
        (**self).id()
    }
    async fn probe(&self) -> Probe {
        (**self).probe().await
    }
}
