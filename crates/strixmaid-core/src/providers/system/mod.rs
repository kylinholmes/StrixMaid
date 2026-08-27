//! system provider（id `"host"`）：主机信息 / 健康聚合 / 时间 / 主机名·时区·电源操作。
//!
//! # 结构
//!
//! 本文件只有两样东西：对外的 [`HostProvider`] 接口形状，以及「调哪个平台的实现」。
//! 具体取数按平台分目录：
//!
//! | 目录 | 数据源 |
//! |---|---|
//! | [`linux`] | `/proc`、`/sys`、`/etc`（目标平台，`docs/design.md` §1） |
//! | [`macos`] | `sysctl`、`sw_vers`、`getfsstat`、`scutil` |
//!
//! [`health`] 是两个平台共用的判定逻辑（阈值、严重级别、版本比较），不做 I/O。
//!
//! 采集函数（`collect_*`）全是同步、永不失败的纯 I/O；[`HostProvider`] 的 async 方法把它们
//! 丢进 `spawn_blocking`——`statvfs` 碰上挂死的网络挂载会阻塞，不能占用运行时线程。

pub mod health;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;

use async_trait::async_trait;
use strixmaid_types::system::{
    HealthReport, PowerAction, SetHostnameReq, SystemInfo, TimeInfo,
};
use strixmaid_types::{ApiError, ApiResult};

use super::{Probe, Provider};

pub use health::RebootReason;
pub use sys::{collect_health, collect_system_info};

/// 主机信息 provider。无状态，可随意 `Clone` / `Copy`。
#[derive(Debug, Clone, Copy, Default)]
pub struct HostProvider;

impl HostProvider {
    /// 创建 provider。
    pub fn new() -> Self {
        Self
    }

    /// `GET /system/info`。
    pub async fn system_info(&self) -> ApiResult<SystemInfo> {
        blocking(sys::collect_system_info).await
    }

    /// `GET /system/health`。
    pub async fn health(&self) -> ApiResult<HealthReport> {
        blocking(sys::collect_health).await
    }

    /// `GET /system/time`。
    pub async fn time(&self) -> ApiResult<TimeInfo> {
        blocking(sys::collect_time_info).await
    }

    /// `PUT /system/hostname`。非 root 返回 `PermissionDenied`。
    pub async fn set_hostname(&self, req: SetHostnameReq) -> ApiResult<()> {
        blocking(move || sys::set_hostname(&req)).await?
    }

    /// `PUT /system/timezone`。非 root 返回 `PermissionDenied`。
    pub async fn set_timezone(&self, timezone: String) -> ApiResult<()> {
        blocking(move || sys::set_timezone(&timezone)).await?
    }

    /// `POST /system/power`。
    pub async fn power(&self, action: PowerAction) -> ApiResult<()> {
        sys::power(action).await
    }
}

#[async_trait]
impl Provider for HostProvider {
    fn id(&self) -> &'static str {
        "host"
    }

    async fn probe(&self) -> Probe {
        sys::probe()
    }
}

/// 在阻塞线程池里跑一段同步采集。
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal("采集任务异常终止").with_detail(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 平台无关的接口冒烟：字段是否有值由各平台自己的用例断言，
    /// 这里只保证 async 壳把调用正确地转发下去了。
    #[tokio::test]
    async fn provider_探测与_async_接口() {
        let p = HostProvider::new();
        assert_eq!(p.id(), "host");
        assert_eq!(p.probe().await, Probe::Available);
        let info = p.system_info().await.unwrap();
        assert!(!info.hostname.is_empty());
        assert!(info.memory.total_bytes > 0);
        let _ = p.health().await.unwrap();
        let t = p.time().await.unwrap();
        assert!(!t.timezone.is_empty());
        eprintln!("本机 TimeInfo: {}", serde_json::to_string(&t).unwrap());
    }
}
