//! 常驻输出泵、背压及幂等关闭。等待始终发生在状态锁之外。

use super::{
    ATTACH_STALL_LIMIT, AttachEvent, CloseReason, READ_CHUNK, Terminal, TerminalClosed,
    TerminalObserver, TerminalRegistry,
};
use crate::store::now_unix;
use std::{
    io,
    sync::{Arc, Weak, atomic::Ordering},
};
use strixmaid_types::rpc::{TERM_CLOSE, TermCloseParams, TermCloseResult};
use tokio::sync::{mpsc::error::TrySendError, oneshot};
use tokio::time::Instant;

/// 真正的关闭动作，幂等。返回 `false` 表示别人已经关过了。
///
/// 顺序上退出状态优先于通知：先把 `term.close` 发去 worker 取回退出状态，
/// 再通知附着方与观察者——反过来的话 `{"t":"exit"}` 里永远带不上退出码。
/// 代价是附着方多等一次 RPC 往返：EOF 路径上 shell 已死、状态现成，几毫秒；
/// 最坏是 DELETE 打在一个不理 SIGHUP 的进程上，等满宽限期到 SIGKILL。
pub(super) async fn shutdown(
    term: &Arc<Terminal>,
    reason: CloseReason,
    observer: Option<&Arc<dyn TerminalObserver>>,
) -> bool {
    // 唯一的裁决点：显式 DELETE 与 shell 退出撞车是**正常**竞态，
    // 输的一方必须安静地返回，尤其不能重发 `term.close`——pid 可能已被复用。
    if term.closed.swap(true, Ordering::AcqRel) {
        return false;
    }

    // 叫醒泵任务。它可能就是当前调用者（EOF 那条路径），那也没关系：
    // 它会在 `select!` 里看到 stop 已就绪，但那时它已经走出循环了。
    term.stop.lock().unwrap_or_else(|e| e.into_inner()).take();

    // 跨进程的等待放在所有锁之外。worker 先走一步是常态（会话登出时 worker
    // 与终端一起拆），那种情况下 PTY 已随 worker 进程消失，退出状态也无从取。
    let params = serde_json::json!(TermCloseParams { pid: term.pid });
    let exit = match term.worker.call(TERM_CLOSE, params).await {
        Ok(v) => {
            let exit = serde_json::from_value::<TermCloseResult>(v)
                .ok()
                .and_then(|r| r.exit);
            tracing::info!(id = %term.id, pid = term.pid, %reason, ?exit, "终端已关闭");
            exit
        }
        Err(e) => {
            // 登出路径上 worker 与终端一起拆，close 打不到是常态，不值得告警；
            // 其余路径上 close 失败意味着**退出码丢了**，必须在默认日志级别下
            // 看得见——12 号方案 §2.1 那个 bug 能活到实测，一半原因是这条日志
            // 原先走 debug，排查时全程无声。
            if matches!(reason, CloseReason::Logout) {
                tracing::debug!(
                    id = %term.id, pid = term.pid, %reason, error = %e,
                    "向 worker 发送 term.close 失败"
                );
            } else {
                tracing::warn!(
                    id = %term.id, pid = term.pid, %reason, error = %e,
                    "向 worker 发送 term.close 失败，退出状态无法取回"
                );
            }
            None
        }
    };

    // 通知并摘掉附着方（锁内只做内存操作）。
    let handle = {
        let mut st = term.lock();
        st.info.attached = false;
        st.attached.take()
    };
    if let Some(h) = handle {
        let _ = h.tx.try_send(AttachEvent::Closed { reason, exit });
    }

    if let Some(obs) = observer {
        let (target_user, target_uid) = {
            let st = term.lock();
            (st.info.user.clone(), st.info.uid)
        };
        obs.on_closed(&TerminalClosed {
            id: term.id.clone(),
            pid: term.pid,
            session_hash: term.session_hash.clone(),
            owner: term.owner.clone(),
            target_user,
            target_uid,
            reason,
            exit,
        });
    }
    true
}

/// 常驻泵：把 socketpair 的输出写进回看缓冲，并转发给当前附着方。
pub(super) async fn pump(
    term: Arc<Terminal>,
    registry: Weak<TerminalRegistry>,
    mut stop: oneshot::Receiver<()>,
) {
    let stream = Arc::clone(&term.stream);
    let mut buf = vec![0u8; READ_CHUNK];
    let reason = loop {
        // 用就绪 API 而不是 `AsyncReadExt::read`：后者要 `&mut IpcChannel`，
        // 而这条 stream 与附着方的写共享同一个 `Arc`（见 [`super::Attachment::write`]）。
        // `readable()` 是取消安全的，被 `select!` 丢掉不会吞字节。
        let ready = tokio::select! {
            // 关闭优先：已经决定要关的终端不必再多等一轮。
            biased;
            _ = &mut stop => return,
            r = stream.readable() => r,
        };
        if let Err(e) = ready {
            tracing::warn!(id = %term.id, pid = term.pid, error = %e, "终端 socket 不可读");
            break CloseReason::Failed;
        }
        match stream.try_read(&mut buf) {
            Ok(0) => break CloseReason::Exited,
            Ok(n) => forward(&term, &buf[..n]).await,
            // 就绪只是提示，假唤醒要接着等。
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            // Windows 的命名管道在对端消失时报 ERROR_BROKEN_PIPE 而不是 0 字节，
            // 协议层上与 EOF 是同一件事（`session/channel.rs` 有同样的判定）。
            // 当成 Failed 会把每一次正常的 shell 退出都记成故障进审计。
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => break CloseReason::Exited,
            Err(e) => {
                tracing::warn!(id = %term.id, pid = term.pid, error = %e, "终端读取失败");
                break CloseReason::Failed;
            }
        }
    };

    match registry.upgrade() {
        Some(reg) => {
            reg.finish(&term, reason).await;
        }
        // 注册表没了（进程在关停）：表已经不存在，直接走关闭动作，观察者也随之无处可寻。
        None => {
            shutdown(&term, reason, None).await;
        }
    }
}

/// 把一段输出写进回看缓冲并转发给附着方（如果有）。
async fn forward(term: &Arc<Terminal>, data: &[u8]) {
    let target = {
        let mut st = term.lock();
        st.scrollback.push(data);
        st.last_active = Instant::now();
        st.info.last_active_ts = now_unix();
        st.attached.as_ref().map(|a| (a.seq, a.tx.clone()))
    };
    let Some((seq, tx)) = target else {
        // 没人看，写进回看缓冲就够了——这正是「断开后 shell 继续跑」的实现。
        return;
    };

    // 快路径：队列有位置时一次同步调用就完事。
    let pending = match tx.try_send(AttachEvent::Data(data.to_vec())) {
        Ok(()) => return,
        Err(TrySendError::Closed(_)) => {
            // 附着方刚 drop，它的 `Drop` 会（或已经）解除附着，这里不重复动作。
            return;
        }
        Err(TrySendError::Full(ev)) => ev,
    };
    match tokio::time::timeout(ATTACH_STALL_LIMIT, tx.send(pending)).await {
        // 慢路径走到这里就是背压在起作用：泵停住 → socketpair 满 → shell 被顶住。
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(_) => {
            // 十秒都塞不进去，这个 WS 已经死了。解除附着让终端回到「无人附着」，
            // 从而重新受空闲回收管辖，见 `ATTACH_STALL_LIMIT`。
            tracing::warn!(
                id = %term.id,
                stall_secs = ATTACH_STALL_LIMIT.as_secs(),
                "附着方长时间不消费，强制解除附着"
            );
            term.finish_attach(seq, CloseReason::Stalled, None);
        }
    }
}
