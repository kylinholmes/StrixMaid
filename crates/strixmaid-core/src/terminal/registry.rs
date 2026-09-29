//! 会话名额预留、终端查询、回收及 worker 接入。

use super::pump::{pump, shutdown};
use super::{
    Attachment, CloseReason, RingBuf, TERMINAL_ID_BYTES, Terminal, TerminalObserver, TerminalOwner,
    TerminalState,
};
use crate::{
    config::TerminalConfig,
    session::{
        WorkerHandle,
        channel::{self, IpcChannel},
    },
    store::now_unix,
};
use rand::Rng as _;
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex as StdMutex, MutexGuard, OnceLock,
    atomic::{AtomicBool, AtomicU64},
};
use strixmaid_types::rpc::{
    TERM_OPEN, TERM_RESIZE, TermOpenParams, TermOpenResult, TermResizeParams,
};
use strixmaid_types::{ApiError, ErrorCode, terminal::TerminalInfo};
use tokio::sync::oneshot;
use tokio::time::Instant;

// ===========================================================================
// TerminalRegistry
// ===========================================================================

/// 注册表的内部表。**只在同步 `Mutex` 下访问，绝不跨 `await` 持有**。
#[derive(Default)]
struct Inner {
    terms: HashMap<String, Arc<Terminal>>,
    /// 会话 → 已通过上限检查但还没插进 `terms` 的数量。
    ///
    /// 没有它就有 TOCTOU：`term.open` 是一次跨进程往返，两个并发的
    /// `POST /terminals` 会双双读到「还差一个到上限」，于是一起建起来。
    /// 上限存在的意义就是挡住跑飞的前端，一个能被并发绕过的上限等于没有。
    reserving: HashMap<String, usize>,
}

impl Inner {
    fn count_for(&self, session_hash: &str) -> usize {
        self.terms
            .values()
            .filter(|t| t.session_hash == session_hash)
            .count()
    }

    fn release(&mut self, session_hash: &str) {
        if let Some(n) = self.reserving.get_mut(session_hash) {
            *n -= 1;
            if *n == 0 {
                self.reserving.remove(session_hash);
            }
        }
    }
}

/// 一个已通过上限检查、尚未落表的名额。`Drop` 即归还——`term.open` 失败、
/// fd 有问题、future 被取消，任何一条路径都不会把名额漏掉。
struct Reservation<'a> {
    registry: &'a TerminalRegistry,
    session_hash: String,
    held: bool,
}

impl Reservation<'_> {
    /// 落表：插入 `terms` 与归还名额必须在**同一把锁**里完成，
    /// 否则中间那一瞬会少算一个，正好够让第 N+1 个终端挤进来。
    fn commit(mut self, term: Arc<Terminal>) {
        let mut inner = self.registry.lock();
        inner.terms.insert(term.id.clone(), term);
        inner.release(&self.session_hash);
        self.held = false;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if self.held {
            self.registry.lock().release(&self.session_hash);
        }
    }
}

/// 主进程持有的全部终端。
///
/// 用 `Arc<TerminalRegistry>` 共享给各 handler；内部可变见 `Inner`。
pub struct TerminalRegistry {
    cfg: TerminalConfig,
    inner: StdMutex<Inner>,
    /// 关闭事件的观察者（宿主启动时装入，用于审计）。见 [`TerminalObserver`]。
    observer: OnceLock<Arc<dyn TerminalObserver>>,
}

impl std::fmt::Debug for TerminalRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalRegistry")
            .field("live", &self.lock().terms.len())
            .finish()
    }
}

impl TerminalRegistry {
    /// 建一个空注册表。
    pub fn new(cfg: TerminalConfig) -> Arc<Self> {
        Arc::new(TerminalRegistry {
            cfg,
            inner: StdMutex::new(Inner::default()),
            observer: OnceLock::new(),
        })
    }

    /// 装入关闭事件的观察者。只在宿主启动时调用一次，重复装入被忽略。
    pub fn set_observer(&self, observer: Arc<dyn TerminalObserver>) {
        if self.observer.set(observer).is_err() {
            tracing::warn!("终端观察者已装入过，重复调用被忽略");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 生效的配置。
    pub fn config(&self) -> &TerminalConfig {
        &self.cfg
    }

    /// 开一个终端：向 `worker` 发 `term.open`，接管它交回的 fd，起泵任务。
    ///
    /// **以谁的身份跑由「发给哪个 worker」决定**（见 `strixmaid_types::rpc::TermOpenParams`
    /// 的注释）。本模块不做鉴权判断——`user` 是否需要提权由调用方在选 worker 时决定，
    /// 这里再判一次就是第二套鉴权，正是 `design.md` §5.1 要避免的。
    pub async fn open(
        self: &Arc<Self>,
        session_hash: &str,
        owner: TerminalOwner,
        worker: &WorkerHandle,
        params: TermOpenParams,
    ) -> Result<TerminalInfo, ApiError> {
        if params.cols == 0 || params.rows == 0 {
            return Err(ApiError::invalid_request("终端尺寸的行列数都必须大于 0"));
        }
        // 先占名额再发 RPC：反过来的话，超限时已经在 worker 里 fork 出了一个 shell，
        // 还得再发一次 `term.close` 去收拾。
        let slot = self.reserve(session_hash)?;

        let (cols, rows) = (params.cols, params.rows);
        let value = serde_json::to_value(&params)
            .map_err(|e| ApiError::internal(format!("term.open 参数序列化失败: {e}")))?;
        let (result, fds) = worker.call_with_fds(TERM_OPEN, value).await?;
        let result: TermOpenResult = serde_json::from_value(result)
            .map_err(|e| ApiError::internal(format!("term.open 响应格式错误: {e}")))?;

        // fd 数量不对就是协议错。`fds` 在这个作用域结束时全部关闭，不会泄漏；
        // worker 侧的 shell 会因为 socketpair 断开而收到 EOF 自行退出。
        let mut fds = fds;
        if fds.len() != 1 {
            return Err(ApiError::internal(format!(
                "term.open 应附带 1 个附件，实际 {}",
                fds.len()
            )));
        }
        let stream = wrap_stream(fds.remove(0))?;

        let now = now_unix();
        let id = random_hex(TERMINAL_ID_BYTES);
        let info = TerminalInfo {
            id: id.clone(),
            shell: result.shell,
            user: result.user,
            uid: result.uid,
            pid: result.pid,
            cols,
            rows,
            created_ts: now,
            last_active_ts: now,
            attached: false,
        };
        let (stop_tx, stop_rx) = oneshot::channel();
        let term = Arc::new(Terminal {
            id,
            session_hash: session_hash.to_string(),
            owner,
            pid: result.pid,
            worker: worker.clone(),
            stream: Arc::new(stream),
            state: StdMutex::new(TerminalState {
                info: info.clone(),
                scrollback: RingBuf::new(),
                attached: None,
                last_active: Instant::now(),
            }),
            closed: AtomicBool::new(false),
            next_seq: AtomicU64::new(1),
            stop: StdMutex::new(Some(stop_tx)),
        });

        slot.commit(term.clone());
        // 泵只持有注册表的 `Weak`：宿主放掉注册表时（进程关停）泵不该反过来
        // 把它吊着不放，那会让一堆终端连同 worker 句柄一起活到进程结束。
        tokio::spawn(pump(term, Arc::downgrade(self), stop_rx));

        tracing::info!(
            id = %info.id,
            pid = result.pid,
            user = %info.user,
            shell = %info.shell,
            "终端已创建"
        );
        Ok(info)
    }

    /// 占一个名额。超限时返回 [`ErrorCode::Conflict`]（→ HTTP 409，
    /// `roadmap/03-terminal.md` §7 的验收标准）。
    fn reserve(&self, session_hash: &str) -> Result<Reservation<'_>, ApiError> {
        let mut inner = self.lock();
        let live = inner.count_for(session_hash);
        let reserving = inner.reserving.get(session_hash).copied().unwrap_or(0);
        if live + reserving >= self.cfg.max_per_session {
            return Err(ApiError::new(
                ErrorCode::Conflict,
                format!(
                    "本会话的终端数已达上限 {}，请先关闭一个",
                    self.cfg.max_per_session
                ),
            ));
        }
        *inner.reserving.entry(session_hash.to_string()).or_insert(0) += 1;
        Ok(Reservation {
            registry: self,
            session_hash: session_hash.to_string(),
            held: true,
        })
    }

    /// 按 id 取本会话的终端。
    ///
    /// `session_hash` 不匹配时返回 [`ErrorCode::NotFound`] 而不是 403：
    /// 别的会话不该能通过错误码的差异**探测出某个 id 是否存在**。
    pub fn get(&self, session_hash: &str, id: &str) -> Result<Arc<Terminal>, ApiError> {
        self.lock()
            .terms
            .get(id)
            .filter(|t| t.session_hash == session_hash)
            .cloned()
            .ok_or_else(|| ApiError::not_found("终端不存在或已关闭"))
    }

    /// 本会话的全部终端，按创建时间排序。
    pub fn list_for(&self, session_hash: &str) -> Vec<TerminalInfo> {
        let terms: Vec<Arc<Terminal>> = self
            .lock()
            .terms
            .values()
            .filter(|t| t.session_hash == session_hash)
            .cloned()
            .collect();
        // 取 `info()` 要拿每个终端自己的锁，因此先放掉注册表的锁再取——
        // 两把锁嵌套是死锁的经典配方，即便当前顺序安全也不留这个隐患。
        let mut out: Vec<TerminalInfo> = terms.iter().map(|t| t.info()).collect();
        out.sort_by(|a, b| a.created_ts.cmp(&b.created_ts).then(a.id.cmp(&b.id)));
        out
    }

    /// 本会话当前的终端数。
    pub fn count_for(&self, session_hash: &str) -> usize {
        self.lock().count_for(session_hash)
    }

    /// 附着一个 WS。
    pub fn attach(&self, session_hash: &str, id: &str) -> Result<Attachment, ApiError> {
        Ok(self.get(session_hash, id)?.attach())
    }

    /// 改窗口大小。
    pub async fn resize(
        &self,
        session_hash: &str,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), ApiError> {
        if cols == 0 || rows == 0 {
            return Err(ApiError::invalid_request("终端尺寸的行列数都必须大于 0"));
        }
        let term = self.get(session_hash, id)?;
        let params = serde_json::to_value(TermResizeParams {
            pid: term.pid,
            cols,
            rows,
        })
        .map_err(|e| ApiError::internal(format!("term.resize 参数序列化失败: {e}")))?;
        // 先等 worker 真的 ioctl 成功再改本地记录：反过来会让 `GET /terminals`
        // 报告一个 PTY 上并不成立的尺寸。
        term.worker.call(TERM_RESIZE, params).await?;
        let mut st = term.lock();
        st.info.cols = cols;
        st.info.rows = rows;
        Ok(())
    }

    /// 关闭一个终端（`DELETE /terminals/{id}`）。
    pub async fn close(
        &self,
        session_hash: &str,
        id: &str,
        reason: CloseReason,
    ) -> Result<(), ApiError> {
        let term = self.get(session_hash, id)?;
        self.finish(&term, reason).await;
        Ok(())
    }

    /// 关闭一个会话的**全部**终端，返回实际关掉的个数。
    ///
    /// 会话登出与会话超时都要调它（`roadmap/03-terminal.md` §4.3）：终端跑的是
    /// 用户身份的 shell，会话都没了还留着它，等于留下一个没有主人的登录态。
    pub async fn close_all_for(&self, session_hash: &str, reason: CloseReason) -> usize {
        // 先在锁内把它们从表里全部摘走，再逐个关。摘表这一步必须是原子的，
        // 否则一个并发的 `POST /terminals` 会在登出中途插进来一个新终端。
        let doomed: Vec<Arc<Terminal>> = {
            let mut inner = self.lock();
            let ids: Vec<String> = inner
                .terms
                .values()
                .filter(|t| t.session_hash == session_hash)
                .map(|t| t.id.clone())
                .collect();
            ids.iter().filter_map(|id| inner.terms.remove(id)).collect()
        };
        let mut closed = 0;
        for term in doomed {
            if shutdown(&term, reason, self.observer.get()).await {
                closed += 1;
            }
        }
        closed
    }

    /// 回收空闲终端，返回关掉的个数（`roadmap/03-terminal.md` §4.3「空闲」）。
    ///
    /// `idle_timeout_secs = 0` 视为**关闭空闲回收**：字面理解「0 秒即空闲」
    /// 会让每个新终端在下一次扫描时立刻被关掉，那不是任何人想要的配置。
    pub async fn sweep_idle(&self) -> usize {
        let limit = self.cfg.idle_timeout();
        if limit.is_zero() {
            return 0;
        }
        let now = Instant::now();
        let doomed: Vec<Arc<Terminal>> = {
            let candidates: Vec<Arc<Terminal>> = self.lock().terms.values().cloned().collect();
            // 判定要拿每个终端自己的锁，所以放掉注册表的锁之后再筛。
            let ids: Vec<String> = candidates
                .iter()
                .filter(|t| t.idle_for(now).is_some_and(|idle| idle >= limit))
                .map(|t| t.id.clone())
                .collect();
            let mut inner = self.lock();
            ids.iter().filter_map(|id| inner.terms.remove(id)).collect()
        };
        let mut closed = 0;
        for term in doomed {
            tracing::info!(id = %term.id, pid = term.pid, "终端空闲超时，关闭");
            if shutdown(&term, CloseReason::Idle, self.observer.get()).await {
                closed += 1;
            }
        }
        closed
    }

    /// 摘表 + 关闭。返回 `true` 表示本次调用是真正执行关闭的那一个。
    pub(super) async fn finish(&self, term: &Arc<Terminal>, reason: CloseReason) -> bool {
        // 先摘表：`GET /terminals` 立刻不再列出它，不必等 worker 应答。
        self.lock().terms.remove(&term.id);
        shutdown(term, reason, self.observer.get()).await
    }
}

/// 把 worker 交回的附件变成一条可用的通道。
fn wrap_stream(attachment: channel::Attachment) -> Result<IpcChannel, ApiError> {
    #[cfg(unix)]
    {
        IpcChannel::from_owned_fd(attachment)
            .map_err(|e| ApiError::internal(format!("终端通道注册到 tokio 失败: {e}")))
    }
    // SAFETY: 这是 worker 用 `IpcChannel::pair_for_transfer` 建的客户端一端，
    // 带 `FILE_FLAG_OVERLAPPED`，刚由 `DuplicateHandle` 搬进本进程，
    // 尚未注册到任何 I/O 完成端口。
    #[cfg(windows)]
    unsafe {
        IpcChannel::from_client_handle(attachment)
            .map_err(|e| ApiError::internal(format!("终端通道注册到 tokio 失败: {e}")))
    }
}

/// 随机 hex。用 `rand::rng()`（CSPRNG，OS 熵播种）而不是任何计数器：
/// 终端 id 出现在 URL 里，可枚举的 id 会把「猜 id」变成一条攻击面。
fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}
