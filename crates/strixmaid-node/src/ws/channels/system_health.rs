//! `system.health` 频道：健康报告的变更推送（roadmap/04 §B.2）。
//!
//! # 事件源留在主进程
//!
//! 健康报告的内容（磁盘容量、inode、负载、需重启、failed units）对所有本地
//! 用户都是同一份事实，与谁在看无关——与 `services.changed` 同一判断。
//! 主进程每 30 秒重算一次，只有**显著变化**（条目集合按 `id + target +
//! severity` 有增删或级别变化）才广播整份报告；`detail` 文本的抖动（比如
//! 磁盘用量数字每轮微变）不打扰订阅者。
//!
//! # failed units 并入
//!
//! `HostProvider::health()` 的 `skipped` 里标着 `systemd`——failed units 归
//! service provider。本频道把两者合起来：有 service provider 时按
//! `state = failed` 计数生成 `unit.failed` 条目，并把 `systemd` 从 `skipped`
//! 摘掉。REST 的 `GET /system/health` 仍是 worker 里那份未合并的报告；
//! 前端要完整结论应订阅本频道。

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use serde_json::Value;
use strixmaid_core::providers::service::ServiceProvider;
use strixmaid_core::providers::system::HostProvider;
use strixmaid_types::ApiError;
use strixmaid_types::service::{UnitActiveState, UnitListQuery};
use strixmaid_types::system::{HealthItem, HealthReport, HealthSeverity};
use strixmaid_types::ws::WsChannel;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::ws::hub::{ChannelEvent, ChannelSource, ChannelStream, SubscribeContext};

/// `system.health` 频道源。持有最新报告的 watch 接收端。
pub struct SystemHealth {
    rx: watch::Receiver<Arc<HealthReport>>,
}

impl SystemHealth {
    /// 起后台重算任务。返回的 `JoinHandle` 由宿主在关停时 abort。
    pub fn start(
        host: HostProvider,
        service: Option<Arc<dyn ServiceProvider>>,
        interval: Duration,
    ) -> (SystemHealth, JoinHandle<()>) {
        // 初始占位：`ts == 0` 表示「第一轮还没算完」，订阅时不推它。
        let (tx, rx) = watch::channel(Arc::new(empty_report()));
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                ticker.tick().await;
                let Some(report) = compute(&host, service.as_ref()).await else {
                    continue;
                };
                let significant = {
                    let prev = tx.borrow();
                    prev.ts == 0 || significant_change(&prev, &report)
                };
                if significant {
                    tx.send_replace(Arc::new(report));
                }
            }
        });
        (SystemHealth { rx }, task)
    }

    /// 从现成的接收端构造（测试用：不起重算任务）。
    #[cfg(test)]
    fn with_receiver(rx: watch::Receiver<Arc<HealthReport>>) -> SystemHealth {
        SystemHealth { rx }
    }
}

fn empty_report() -> HealthReport {
    HealthReport {
        ts: 0,
        status: HealthSeverity::Ok,
        items: Vec::new(),
        skipped: Vec::new(),
    }
}

/// 采一轮：主机健康 + failed units。失败返回 `None`，保留上一份报告。
async fn compute(
    host: &HostProvider,
    service: Option<&Arc<dyn ServiceProvider>>,
) -> Option<HealthReport> {
    let mut report = match host.health().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "健康重算失败，本轮跳过");
            return None;
        }
    };
    if let Some(svc) = service {
        let q = UnitListQuery {
            state: Some(UnitActiveState::Failed),
            ..UnitListQuery::default()
        };
        match svc.list_units(&q).await {
            Ok(units) => {
                merge_failed_units(&mut report, units.iter().map(|u| u.name.as_str()));
            }
            Err(e) => {
                tracing::debug!(error = %e, "failed units 计数失败，本轮报告不含 unit.failed");
            }
        }
    }
    Some(report)
}

/// 把 failed units 并入报告（§B.2）：`skipped` 里的 `systemd` 摘掉（这一项
/// 本轮确实检查了），有 failed 时生成一条 `unit.failed`，总体结论跟着最高
/// 严重级别走，条目保持按严重级别降序。
fn merge_failed_units<'a>(report: &mut HealthReport, names: impl Iterator<Item = &'a str>) {
    report.skipped.retain(|s| s != "systemd");
    let names: Vec<&str> = names.collect();
    if !names.is_empty() {
        let shown: Vec<&str> = names.iter().copied().take(10).collect();
        let mut detail = shown.join(", ");
        if names.len() > shown.len() {
            detail.push_str(&format!(" 等共 {} 个", names.len()));
        }
        report.items.push(HealthItem {
            id: "unit.failed".into(),
            severity: HealthSeverity::Warning,
            title: format!("{} 个 unit 处于 failed 状态", names.len()),
            detail: Some(detail),
            target: None,
        });
    }
    report.status = report
        .items
        .iter()
        .map(|i| i.severity)
        .max()
        .unwrap_or(HealthSeverity::Ok);
    report.items.sort_by_key(|i| std::cmp::Reverse(i.severity));
}

/// 显著变化 = 条目集合（`id + target + severity`）有增删或级别变化，
/// 或 `skipped` 变了。只有 `detail` / `title` 文本不同**不算**（§B.4）。
fn significant_change(a: &HealthReport, b: &HealthReport) -> bool {
    fn keys(r: &HealthReport) -> std::collections::BTreeSet<(&str, Option<&str>, HealthSeverity)> {
        r.items
            .iter()
            .map(|i| (i.id.as_str(), i.target.as_deref(), i.severity))
            .collect()
    }
    keys(a) != keys(b) || a.skipped != b.skipped
}

impl ChannelSource for SystemHealth {
    fn name(&self) -> &'static str {
        WsChannel::SystemHealth.as_str()
    }

    /// 忽略参数与 `ctx`：健康是全局事实，与谁在看无关。
    fn subscribe(
        &self,
        _params: Option<Value>,
        _ctx: &SubscribeContext,
    ) -> Result<ChannelStream, ApiError> {
        let mut rx = self.rx.clone();
        // 订阅时立即推当前报告（§B.2）；`borrow_and_update` 顺带把当前版本标为
        // 已读，`changed()` 不会为同一份再醒一次。首轮尚未算完时没有可推的。
        let current = rx.borrow_and_update().clone();
        let initial = (current.ts != 0)
            .then(|| serde_json::to_value(&*current).ok())
            .flatten()
            .map(ChannelEvent::Data);
        let live = stream::unfold(rx, |mut rx| async move {
            rx.changed().await.ok()?;
            let report = rx.borrow_and_update().clone();
            Some((serde_json::to_value(&*report).ok().map(ChannelEvent::Data), rx))
        })
        .filter_map(futures::future::ready);
        Ok(stream::iter(initial).chain(live).boxed())
    }

    /// `req`：返回当前报告（可能是 `ts == 0` 的空占位）。
    fn request(&self, _params: Option<Value>) -> BoxFuture<'static, Result<Value, ApiError>> {
        let current = self.rx.borrow().clone();
        Box::pin(async move {
            serde_json::to_value(&*current)
                .map_err(|e| ApiError::internal("序列化健康报告失败").with_detail(e.to_string()))
        })
    }
}

#[cfg(test)]
mod tests {
    use strixmaid_core::session::ClientMeta;
    use strixmaid_types::auth::AuthUser;

    use super::*;

    fn item(id: &str, target: Option<&str>, severity: HealthSeverity, detail: &str) -> HealthItem {
        HealthItem {
            id: id.into(),
            severity,
            title: format!("{id} 的标题"),
            detail: Some(detail.into()),
            target: target.map(str::to_owned),
        }
    }

    fn report(ts: i64, items: Vec<HealthItem>) -> HealthReport {
        let status = items
            .iter()
            .map(|i| i.severity)
            .max()
            .unwrap_or(HealthSeverity::Ok);
        HealthReport {
            ts,
            status,
            items,
            skipped: vec!["smart".into()],
        }
    }

    #[test]
    fn 只有_detail_不同不算显著变化() {
        let a = report(1, vec![item("disk.usage", Some("/var"), HealthSeverity::Warning, "86%")]);
        let b = report(2, vec![item("disk.usage", Some("/var"), HealthSeverity::Warning, "87%")]);
        assert!(!significant_change(&a, &b), "detail 抖动不该触发广播");
    }

    #[test]
    fn 级别变化与条目增删都算() {
        let base = report(1, vec![item("disk.usage", Some("/var"), HealthSeverity::Warning, "86%")]);
        let escalated = report(2, vec![item("disk.usage", Some("/var"), HealthSeverity::Critical, "96%")]);
        assert!(significant_change(&base, &escalated));

        let added = report(2, vec![
            item("disk.usage", Some("/var"), HealthSeverity::Warning, "86%"),
            item("disk.usage", Some("/data"), HealthSeverity::Warning, "85%"),
        ]);
        assert!(significant_change(&base, &added), "同 id 不同 target 是新条目");

        let cleared = report(2, vec![]);
        assert!(significant_change(&base, &cleared));
    }

    #[test]
    fn failed_units_并入并抬高结论() {
        let mut r = HealthReport {
            ts: 1,
            status: HealthSeverity::Ok,
            items: Vec::new(),
            skipped: vec!["systemd".into(), "smart".into()],
        };
        merge_failed_units(&mut r, ["a.service", "b.service"].into_iter());
        assert_eq!(r.skipped, vec!["smart".to_string()], "systemd 已检查，要摘掉");
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].id, "unit.failed");
        assert_eq!(r.status, HealthSeverity::Warning);
        assert!(r.items[0].detail.as_deref().unwrap().contains("a.service"));

        // 没有 failed 时只摘 skipped，不加条目、不改结论。
        let mut clean = HealthReport {
            ts: 1,
            status: HealthSeverity::Critical,
            items: vec![item("fs.read_only", Some("/"), HealthSeverity::Critical, "ro")],
            skipped: vec!["systemd".into()],
        };
        merge_failed_units(&mut clean, std::iter::empty());
        assert!(clean.skipped.is_empty());
        assert_eq!(clean.items.len(), 1);
        assert_eq!(clean.status, HealthSeverity::Critical, "已有的更高结论不受影响");
    }

    fn ctx() -> SubscribeContext {
        SubscribeContext {
            session: strixmaid_core::session::Session {
                token_hash: "h".into(),
                node: "local".into(),
                user: AuthUser {
                    uid: 1000,
                    gid: 1000,
                    username: "tester".into(),
                    groups: vec![],
                },
                elevated: false,
                elevated_ts: None,
                authed_ts: 0,
                created_ts: 0,
                last_active_ts: 0,
                meta: ClientMeta {
                    user_agent: None,
                    remote_addr: None,
                },
                session_opened: false,
            },
        }
    }

    #[tokio::test]
    async fn 订阅先收当前报告_再收变更() {
        let first = Arc::new(report(10, vec![]));
        let (tx, rx) = watch::channel(first);
        let src = SystemHealth::with_receiver(rx);

        let mut stream = src.subscribe(None, &ctx()).unwrap();
        let ChannelEvent::Data(initial) =
            tokio::time::timeout(Duration::from_secs(1), stream.next())
                .await
                .expect("应立即收到当前报告")
                .unwrap()
        else {
            panic!("首帧应是 data")
        };
        assert_eq!(initial["ts"], 10);

        tx.send_replace(Arc::new(report(
            20,
            vec![item("unit.failed", None, HealthSeverity::Warning, "x")],
        )));
        let ChannelEvent::Data(next) = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("变更要推下来")
            .unwrap()
        else {
            panic!("应是 data")
        };
        assert_eq!(next["ts"], 20);
        assert_eq!(next["status"], "warning");
    }

    #[tokio::test]
    async fn 首轮未完成时订阅不收占位帧() {
        let (tx, rx) = watch::channel(Arc::new(empty_report()));
        let src = SystemHealth::with_receiver(rx);
        let mut stream = src.subscribe(None, &ctx()).unwrap();
        // 占位（ts == 0）不该被推；直到第一份真实报告出现。
        tx.send_replace(Arc::new(report(30, vec![])));
        let ChannelEvent::Data(first) = tokio::time::timeout(Duration::from_secs(1), stream.next())
            .await
            .expect("第一份真实报告要到")
            .unwrap()
        else {
            panic!("应是 data")
        };
        assert_eq!(first["ts"], 30);
    }
}
