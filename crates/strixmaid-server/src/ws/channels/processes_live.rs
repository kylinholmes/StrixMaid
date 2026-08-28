//! `processes.live` 频道：进程列表按间隔全量推送，**在订阅者的 user worker 里跑**
//! （roadmap/04 §B.3）。
//!
//! # 为什么走 worker
//!
//! 进程列表本身对所有本地用户可见（读 `/proc` 不需要权限），但 CPU% 是
//! **差分状态**——基线存在 `ProcProvider` 实例里，而该实例随 worker 生命周期
//! 存在（`worker/providers.rs` 的模块文档）。走会话自己的 worker，REST 的
//! `GET /processes` 与本频道共享同一份基线，两边的 CPU% 数字才对得上。
//!
//! # 参数校验在这一侧
//!
//! `interval_secs` / `limit` 的边界在本文件校验并回填缺省——只有这里能带着
//! 订阅方的 `id` 回一帧 `err`；worker 侧对收到的值只做使用不做复核
//! （值不合法最多让那个用户自己的 worker 多干活，不是安全问题）。
//!
//! # 退订
//!
//! 与 `logs.follow` 相同：`Receiver` 一 drop 即退订，worker 里的推送任务随之
//! 结束，无需清理代码。

use std::sync::Arc;

use futures::stream::{self, StreamExt};
use serde_json::Value;
use strixmaid_types::ApiError;
use strixmaid_types::rpc::{self, ProcLiveParams};
use strixmaid_types::ws::WsChannel;

use crate::auth::AuthState;
use crate::ws::hub::{ChannelEvent, ChannelSource, ChannelStream, SubscribeContext};

/// `processes.live` 频道源。不持有 provider，只持有找 worker 的路径。
pub struct ProcessesLive {
    auth: Arc<AuthState>,
}

impl ProcessesLive {
    /// 绑定到认证状态：订阅时按会话取该用户的 user worker。
    pub fn new(auth: Arc<AuthState>) -> Self {
        ProcessesLive { auth }
    }
}

/// 校验并回填缺省（roadmap/04 §B.3）：`interval_secs` 允许 2–10 缺省 3，
/// `limit` 允许 1–500 缺省 100。回填后随订阅下发，worker 不再猜缺省值。
fn validated(params: Option<Value>) -> Result<ProcLiveParams, ApiError> {
    let mut q: ProcLiveParams = match params {
        None | Some(Value::Null) => ProcLiveParams::default(),
        Some(v) => serde_json::from_value(v).map_err(|e| {
            ApiError::invalid_request("processes.live 的订阅参数不合法")
                .with_detail(e.to_string())
        })?,
    };
    let interval = q
        .interval_secs
        .unwrap_or(rpc::PROC_LIVE_DEFAULT_INTERVAL_SECS);
    if !(rpc::PROC_LIVE_MIN_INTERVAL_SECS..=rpc::PROC_LIVE_MAX_INTERVAL_SECS).contains(&interval) {
        return Err(ApiError::invalid_request(format!(
            "interval_secs 允许 {}–{}，收到 {interval}",
            rpc::PROC_LIVE_MIN_INTERVAL_SECS,
            rpc::PROC_LIVE_MAX_INTERVAL_SECS
        )));
    }
    let limit = q.limit.unwrap_or(rpc::PROC_LIVE_DEFAULT_LIMIT);
    if limit == 0 || limit > rpc::PROC_LIVE_MAX_LIMIT {
        return Err(ApiError::invalid_request(format!(
            "limit 允许 1–{}，收到 {limit}",
            rpc::PROC_LIVE_MAX_LIMIT
        )));
    }
    q.interval_secs = Some(interval);
    q.limit = Some(limit);
    Ok(q)
}

impl ChannelSource for ProcessesLive {
    fn name(&self) -> &'static str {
        WsChannel::ProcessesLive.as_str()
    }

    fn subscribe(
        &self,
        params: Option<Value>,
        ctx: &SubscribeContext,
    ) -> Result<ChannelStream, ApiError> {
        let q = validated(params)?;
        let params = serde_json::to_value(&q).map_err(|e| {
            ApiError::internal("无法序列化 processes.live 的订阅参数").with_detail(e.to_string())
        })?;

        // 同 `logs.follow`：trait 方法是同步的，取 worker 与发订阅帧用
        // `block_in_place` 桥接，只在订阅那一刻发生一次。
        let auth = Arc::clone(&self.auth);
        let session = ctx.session.clone();
        let rx = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(super::worker_subscribe(
                &auth,
                &session,
                rpc::PROC_LIVE,
                params,
            ))
        })?;

        Ok(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|frame| (ChannelEvent::Data(frame), rx))
        })
        .boxed())
    }
}

#[cfg(test)]
mod tests {
    use strixmaid_types::ErrorCode;

    use super::*;

    #[test]
    fn 缺省回填() {
        let q = validated(None).unwrap();
        assert_eq!(q.interval_secs, Some(rpc::PROC_LIVE_DEFAULT_INTERVAL_SECS));
        assert_eq!(q.limit, Some(rpc::PROC_LIVE_DEFAULT_LIMIT));

        let q = validated(Some(serde_json::json!({ "sort": "mem", "limit": 500 }))).unwrap();
        assert_eq!(q.limit, Some(500), "上限本身是合法值");
        assert!(q.query.sort.is_some(), "平铺的筛选字段要透传");
    }

    #[test]
    fn 越界参数报_invalid_request() {
        for bad in [
            serde_json::json!({ "interval_secs": 1 }),
            serde_json::json!({ "interval_secs": 11 }),
            serde_json::json!({ "limit": 0 }),
            serde_json::json!({ "limit": 10000 }),
        ] {
            let err = validated(Some(bad.clone())).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRequest, "{bad}");
        }
        let err = validated(Some(serde_json::json!({ "limit": "abc" }))).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }
}
