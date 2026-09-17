//! `logs.follow` 频道：journald 实时追加，**在订阅者自己的 user worker 里跑**。
//!
//! 订阅参数就是 `LogQuery`（priority / unit / q / boot …），每帧 `d` 是 `Vec<LogEntry>`。
//!
//! # 为什么必须绕一趟 worker
//!
//! 改造之前这里直接持有主进程的 `LogProvider`，`journalctl -f` 因此以**主进程的
//! 身份**运行：主进程是 root 时所有人都能看到全系统日志，主进程不是 root 时谁都
//! 只能看到主进程属主的日志。两种结果都不对——日志的可见范围应当随登录用户走。
//!
//! journald 用 ACL 裁决「这个进程能读哪些日志」，裁决对象是执行 `journalctl` 的
//! 那个进程的 uid。所以唯一正确的做法是把 follow 子进程放进该会话的 user worker
//! （uid = 登录用户），也就是 `WorkerHandle::subscribe(rpc::LOG_FOLLOW, ..)`。
//! 服务端在这里**没有任何权限判断**，它只是把订阅交给正确的执行者
//! （`design.md` §5.1）。
//!
//! # 退订
//!
//! `WorkerHandle::subscribe` 返回的 `Receiver` 一 drop 就自动退订，worker 里的
//! `journalctl -f` 子进程随之结束。hub 在 `unsub` / 连接断开时会丢弃整条
//! [`ChannelStream`]，`Receiver` 是这条流的状态，因此一起被丢弃——不需要在这里
//! 写任何清理代码，也就不会有「某条路径忘了清理」的漏洞。

use std::sync::Arc;

use futures::stream::{self, StreamExt};
use serde_json::Value;
use strixmaid_types::log::LogQuery;
use strixmaid_types::ws::WsChannel;
use strixmaid_types::{ApiError, rpc};

use crate::auth::AuthState;
use crate::ws::hub::{ChannelEvent, ChannelSource, ChannelStream, SubscribeContext};

/// `logs.follow` 频道源。它不持有 provider，只持有找 worker 的路径。
pub struct LogsFollow {
    auth: Arc<AuthState>,
}

impl LogsFollow {
    /// 绑定到认证状态：订阅时按会话取该用户的 user worker。
    pub fn new(auth: Arc<AuthState>) -> Self {
        LogsFollow { auth }
    }
}

impl ChannelSource for LogsFollow {
    fn name(&self) -> &'static str {
        WsChannel::LogsFollow.as_str()
    }

    fn subscribe(
        &self,
        params: Option<Value>,
        ctx: &SubscribeContext,
    ) -> Result<ChannelStream, ApiError> {
        let query: LogQuery = match params {
            None | Some(Value::Null) => LogQuery::default(),
            Some(v) => serde_json::from_value(v).map_err(|e| {
                ApiError::invalid_request("logs.follow 的订阅参数不合法").with_detail(e.to_string())
            })?,
        };
        let params = serde_json::to_value(&query).map_err(|e| {
            ApiError::internal("无法序列化 logs.follow 的订阅参数").with_detail(e.to_string())
        })?;

        // trait 方法是同步的，而取 worker 与发订阅帧都是 async：用 `block_in_place`
        // 桥接。这只在订阅那一刻发生一次（一次查表 + 写一帧），不在热路径上，
        // 与 `services.changed` 的做法一致。
        let auth = Arc::clone(&self.auth);
        let session = ctx.session.clone();
        let rx = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(super::worker_subscribe(
                &auth,
                &session,
                rpc::LOG_FOLLOW,
                params,
            ))
        })?;

        // 每一项是 worker 送来的一批 `LogEntry`，原样当作一帧 `data`。
        Ok(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|batch| (ChannelEvent::Data(batch), rx))
        })
        .boxed())
    }
}

