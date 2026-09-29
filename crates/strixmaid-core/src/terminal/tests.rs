use std::sync::atomic::AtomicU32;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::config::TerminalConfig;
use crate::session::channel;
use crate::worker::{self, Dispatcher};
use std::collections::HashMap;
use strixmaid_types::rpc::{
    TERM_CLOSE, TERM_OPEN, TERM_RESIZE, TermCloseParams, TermCloseResult, TermOpenParams,
    TermOpenResult, TermResizeParams,
};
use strixmaid_types::{ApiError, ErrorCode};

/// 假 PTY 的 worker 侧那一端：测试往里写 = shell 有输出，丢掉它 = shell 退出。
///
/// # 两个平台拿到的是不同的东西
///
/// Unix 上是 socketpair 的一端，可以先以 `std` 的形式攥在手里、用到时再
/// 注册进 tokio；Windows 上的命名管道没有这种「先建好、以后再注册」的自由度
/// （见下面 `term.open` 处理器里的说明），拿到手的已经是一条注册好的
/// [`IpcChannel`]。两者都实现 `AsyncRead` + `AsyncWrite`，用例里的读写写法一致。
#[cfg(unix)]
type FakePty = tokio::net::UnixStream;
/// 见上。
#[cfg(windows)]
type FakePty = IpcChannel;

/// `ptys` 表里存着的形态。Unix 上存 `std` 的一端（`pty()` 时才注册进 tokio），
/// Windows 上只能存已注册的那一条。
#[cfg(unix)]
type StoredPty = std::os::unix::net::UnixStream;
/// 见上。
#[cfg(windows)]
type StoredPty = IpcChannel;

// ------------------------------------------------------------- 测试脚手架

/// 进程内的假 worker：`term.open` 用一对进程内通道冒充 PTY（worker 侧那一端
/// 留在 `ptys` 里，测试可以往里写来模拟 shell 输出、丢掉它来模拟 shell 退出），
/// `term.close` / `term.resize` 只记账，供断言「发了几次、发给谁」。
/// 进程内 worker 的独占锁。
///
/// 这些用例每个都在**同一个进程里**同时扮演主进程与 worker：socketpair 的两端、
/// `SCM_RIGHTS` 收到的副本、以及每个 `#[tokio::test]` 自己那个用完即弃的 runtime，
/// 全都挤在同一张 fd 表上。并行跑时能观察到一个用例的终端 socket 莫名其妙地
/// 变成「读端已关」（对端仍然打开、写得进去，读却返回 0），随之被判为 shell 退出。
/// 这不是注册表的逻辑问题——`libc::read` 直接读也是 0，socket 在内核里就是那个状态；
/// 根因在本任务范围之外（怀疑是多 runtime 反复创建销毁时 fd 号被回收所致），
/// 尚未查清。
///
/// 真实部署里不存在这个前提：worker 是**另一个进程**，fd 号不共享，主进程一辈子
/// 只有一个 runtime。所以这里用一把锁把「进程内 worker」串起来，而不是去弱化断言。
///
/// Windows 上没有观察到同一现象（句柄值不像 fd 那样「取最小可用号」，
/// 复用窗口小得多），但这把锁照样留着：它约束的是「同一进程里同时演两个角色」
/// 这件事本身，与用的是 fd 还是 `HANDLE` 无关，而且两个平台跑同一套用例
/// 才谈得上比较。
///
/// 用 tokio 的 `Mutex` 而不是 `std` 的：守卫要跨 `await` 持有整个用例；
/// 顺带也不会中毒——一个用例 panic 不该把后面所有用例都拖垮。
static IN_PROCESS_WORKER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 造一条假 PTY：返回（留在假 worker 手里的那一端，随 `term.open` 交给主进程的附件）。
///
/// # Windows 上为什么不能用 `IpcChannel::pair()` 再把一端交出去
///
/// `pair()` 的两端都已经注册在**本进程**的 I/O 完成端口上，而附件是要被
/// 读端用 `DuplicateHandle(DUPLICATE_CLOSE_SOURCE)` 搬走的——那等于在 tokio
/// 背后把它正在用的句柄关掉，此后 tokio 对它的每一次操作都是未定义行为。
///
/// 真 worker 走的是 [`IpcChannel::pair_for_transfer`]：交出去的那一端用
/// `CreateFileW` 直接开，从一开始就不注册，由接收方在它自己的运行时里注册
/// （主进程侧即 [`wrap_stream`] → `IpcChannel::from_client_handle`）。
/// 假 worker 必须走同一条路，否则测到的就不是真实路径。
///
/// Unix 上没有这个约束：`SCM_RIGHTS` 交出去的是 fd，接收方拿到的是一个
/// **新的** fd，与发送方那份互不相干，所以这里仍是原来的 socketpair。
async fn fake_pty() -> (StoredPty, channel::Attachment) {
    #[cfg(unix)]
    {
        let (worker_side, main_side) = std::os::unix::net::UnixStream::pair().unwrap();
        (worker_side, channel::Attachment::from(main_side))
    }
    #[cfg(windows)]
    {
        IpcChannel::pair_for_transfer("faketerm")
            .await
            .expect("建一对终端通道")
    }
}

struct FakeWorker {
    handle: WorkerHandle,
    ptys: Arc<StdMutex<HashMap<u32, StoredPty>>>,
    closes: Arc<StdMutex<Vec<u32>>>,
    resizes: Arc<StdMutex<Vec<(u32, u16, u16)>>>,
}

impl FakeWorker {
    async fn start() -> FakeWorker {
        let ptys: Arc<StdMutex<HashMap<u32, StoredPty>>> = Arc::new(StdMutex::new(HashMap::new()));
        let closes: Arc<StdMutex<Vec<u32>>> = Arc::new(StdMutex::new(Vec::new()));
        let resizes: Arc<StdMutex<Vec<(u32, u16, u16)>>> = Arc::new(StdMutex::new(Vec::new()));
        let next_pid = Arc::new(AtomicU32::new(4000));

        let mut d = Dispatcher::new();
        {
            let ptys = ptys.clone();
            let next_pid = next_pid.clone();
            d.register_fd(
                TERM_OPEN,
                Arc::new(move |params: Value| {
                    let ptys = ptys.clone();
                    let next_pid = next_pid.clone();
                    Box::pin(async move {
                        let req: TermOpenParams = serde_json::from_value(params)
                            .map_err(|e| ApiError::invalid_request(e.to_string()))?;
                        let (worker_side, main_side) = fake_pty().await;
                        let pid = next_pid.fetch_add(1, Ordering::Relaxed);
                        ptys.lock().unwrap().insert(pid, worker_side);
                        let result = TermOpenResult {
                            pid,
                            shell: req.shell.unwrap_or_else(|| "/bin/sh".into()),
                            user: req.user.unwrap_or_else(|| "tester".into()),
                            uid: 1000,
                        };
                        Ok((serde_json::to_value(result).unwrap(), vec![main_side]))
                    })
                        as BoxFuture<'static, Result<(Value, Vec<channel::Attachment>), ApiError>>
                }),
            );
        }
        {
            let closes = closes.clone();
            d.register_fn(TERM_CLOSE, move |params: Value| {
                let closes = closes.clone();
                async move {
                    let p: TermCloseParams = serde_json::from_value(params)
                        .map_err(|e| ApiError::invalid_request(e.to_string()))?;
                    closes.lock().unwrap().push(p.pid);
                    Ok(serde_json::to_value(TermCloseResult {
                        exit: Some(TermExit {
                            code: Some(0),
                            signal: None,
                        }),
                    })
                    .unwrap())
                }
            });
        }
        {
            let resizes = resizes.clone();
            d.register_fn(TERM_RESIZE, move |params: Value| {
                let resizes = resizes.clone();
                async move {
                    let p: TermResizeParams = serde_json::from_value(params)
                        .map_err(|e| ApiError::invalid_request(e.to_string()))?;
                    resizes.lock().unwrap().push((p.pid, p.cols, p.rows));
                    Ok(Value::Null)
                }
            });
        }

        let handle = serve_in_process(d).await;

        FakeWorker {
            handle,
            ptys,
            closes,
            resizes,
        }
    }

    /// 取走 worker 侧的 PTY 端。丢掉返回值 = shell 退出（主进程读到 EOF）。
    fn pty(&self, pid: u32) -> FakePty {
        let s = self
            .ptys
            .lock()
            .unwrap()
            .remove(&pid)
            .expect("没有这个 pid 对应的 PTY");
        #[cfg(unix)]
        {
            s.set_nonblocking(true).unwrap();
            FakePty::from_std(s).unwrap()
        }
        // Windows 上存进表里的已经是注册好的通道，直接交出去即可。
        #[cfg(windows)]
        s
    }

    fn closes(&self) -> Vec<u32> {
        self.closes.lock().unwrap().clone()
    }
}

/// 把一个装好方法的分发表接成进程内 worker，返回主进程侧的句柄。
///
/// pid 传 -1：这个 worker 是进程内的，绝不能真去 kill 谁。
/// 附件照样收得到：`IpcChannel::pair()` 在 Windows 上把两端的对端
/// 都设成本进程，`DuplicateHandle` 的源就是自己。
async fn serve_in_process(d: Dispatcher) -> WorkerHandle {
    let (main_side, worker_side) = IpcChannel::pair().unwrap();
    tokio::spawn(async move {
        let _ = worker::serve(worker_side, Arc::new(d)).await;
    });
    WorkerHandle::connect(main_side, -1, None)
        .await
        .expect("进程内 worker 握手失败")
}

fn registry(max_per_session: usize, idle_timeout_secs: u64) -> Arc<TerminalRegistry> {
    TerminalRegistry::new(TerminalConfig {
        idle_timeout_secs,
        max_per_session,
    })
}

fn params() -> TermOpenParams {
    TermOpenParams {
        shell: None,
        user: None,
        cols: 80,
        rows: 24,
    }
}

fn owner() -> TerminalOwner {
    TerminalOwner {
        username: "tester".into(),
        uid: 1000,
        elevated: false,
    }
}

async fn open_one(reg: &Arc<TerminalRegistry>, w: &FakeWorker, session: &str) -> TerminalInfo {
    reg.open(session, owner(), &w.handle, params())
        .await
        .expect("开终端失败")
}

/// 轮询等待某个条件成立；超时即 panic（而不是让断言在竞态下随机失败）。
async fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    for _ in 0..500 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("等待超时: {what}");
}

// ------------------------------------------------------------ 注册表语义

#[tokio::test]
async fn 超过上限的终端被拒且错误码映射为_409() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(2, 1800);

    open_one(&reg, &w, "s1").await;
    open_one(&reg, &w, "s1").await;
    let err = reg
        .open("s1", owner(), &w.handle, params())
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    assert_eq!(err.http_status(), 409);

    // 上限是「每会话」的，别的会话不受影响。
    open_one(&reg, &w, "s2").await;
    assert_eq!(reg.count_for("s1"), 2);
    assert_eq!(reg.count_for("s2"), 1);
}

#[tokio::test]
async fn 并发创建不会突破上限() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(3, 1800);

    // 八个请求同时进来。名额若只在「发 RPC 之前查一次表」，这里会漏进好几个。
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let reg = reg.clone();
        let handle = w.handle.clone();
        tasks.push(tokio::spawn(async move {
            reg.open("s1", owner(), &handle, params()).await
        }));
    }
    let mut ok = 0;
    for t in tasks {
        if t.await.unwrap().is_ok() {
            ok += 1;
        }
    }
    assert_eq!(ok, 3, "成功的创建数必须正好等于上限");
    assert_eq!(reg.count_for("s1"), 3);
}

#[tokio::test]
async fn 关闭是幂等的且只向_worker_发一次_term_close() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();

    // 「用户点了 DELETE」与「shell 恰好退出」同时发生，这是正常竞态。
    let (a, b) = tokio::join!(
        reg.finish(&term, CloseReason::Deleted),
        reg.finish(&term, CloseReason::Exited)
    );
    assert!(a ^ b, "只能有一个调用真正执行关闭，实际 a={a} b={b}");
    assert_eq!(
        w.closes(),
        vec![term.pid()],
        "term.close 只能发一次——pid 可能已被复用"
    );
    assert!(reg.list_for("s1").is_empty());

    // 事后再关一次也不能出事。
    assert!(!reg.finish(&term, CloseReason::Deleted).await);
    assert_eq!(w.closes(), vec![term.pid()]);
    // 表里已经没有了，DELETE 走的是 404 而不是第二次关闭。
    let err = reg
        .close("s1", &info.id, CloseReason::Deleted)
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn close_all_for_只关本会话的终端() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let a1 = open_one(&reg, &w, "s1").await;
    let a2 = open_one(&reg, &w, "s1").await;
    let b1 = open_one(&reg, &w, "s2").await;
    let pids: HashMap<String, u32> = ["s1", "s2"]
        .iter()
        .flat_map(|s| reg.list_for(s).into_iter().map(|i| i.id))
        .map(|id| {
            let t = reg.get("s1", &id).or_else(|_| reg.get("s2", &id)).unwrap();
            (id, t.pid())
        })
        .collect();

    assert_eq!(reg.close_all_for("s1", CloseReason::Logout).await, 2);
    assert!(reg.list_for("s1").is_empty());
    assert_eq!(
        reg.list_for("s2")
            .iter()
            .map(|i| i.id.clone())
            .collect::<Vec<_>>(),
        vec![b1.id.clone()],
        "别的会话的终端必须原封不动"
    );

    let mut closed = w.closes();
    closed.sort_unstable();
    let mut expect = vec![pids[&a1.id], pids[&a2.id]];
    expect.sort_unstable();
    assert_eq!(closed, expect, "只该关掉 s1 的两个终端");

    // s2 的终端还能正常用。
    assert!(reg.get("s2", &b1.id).is_ok());
}

#[tokio::test]
async fn 别的会话既看不到也拿不到这个终端() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    assert!(reg.list_for("s2").is_empty());
    let err = reg.get("s2", &info.id).unwrap_err();
    // 不能用 403：那等于告诉对方「这个 id 存在」。
    assert_eq!(err.code, ErrorCode::NotFound);
    assert!(reg.attach("s2", &info.id).is_err());
}

// ---------------------------------------------------------------- 附着

#[tokio::test]
async fn 断开后再附着能拿到之前的全部输出() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();
    let mut pty = w.pty(term.pid());

    let mut first = reg.attach("s1", &info.id).unwrap();
    assert!(term.info().attached);
    pty.write_all(b"hello").await.unwrap();
    assert_eq!(
        first.next().await,
        Some(AttachEvent::Data(b"hello".to_vec()))
    );

    // 断开只解除附着：PTY 与 shell 继续跑，输出继续进回看缓冲。
    drop(first);
    wait_until(|| !term.info().attached, "解除附着").await;
    assert!(reg.get("s1", &info.id).is_ok(), "断开不该关掉终端");

    pty.write_all(b" world").await.unwrap();
    wait_until(
        || term.lock().scrollback.len() == b"hello world".len(),
        "输出进入回看缓冲",
    )
    .await;

    // 重新附着：第一件事必须是**全量**回放，而且是一整块，不能被后到的字节插队。
    let mut again = reg.attach("s1", &info.id).unwrap();
    assert_eq!(
        again.next().await,
        Some(AttachEvent::Data(b"hello world".to_vec()))
    );
    // 回放之后接着收实时输出。
    pty.write_all(b"!").await.unwrap();
    assert_eq!(again.next().await, Some(AttachEvent::Data(b"!".to_vec())));
}

#[tokio::test]
async fn 新附着顶掉旧附着并给出_replaced() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();
    let mut pty = w.pty(term.pid());

    let mut old = reg.attach("s1", &info.id).unwrap();
    let mut new = reg.attach("s1", &info.id).unwrap();

    assert_eq!(
        old.next().await,
        Some(AttachEvent::Closed {
            reason: CloseReason::Replaced,
            exit: None
        })
    );
    assert_eq!(old.next().await, None, "被顶掉的附着必须彻底结束");

    // 新的那个仍然正常收字节。
    pty.write_all(b"still here").await.unwrap();
    assert_eq!(
        new.next().await,
        Some(AttachEvent::Data(b"still here".to_vec()))
    );

    // 旧附着的 Drop 迟到，也不能把新附着摘掉。
    drop(old);
    assert!(term.info().attached, "迟到的 Drop 摘错了人");
    pty.write_all(b"!").await.unwrap();
    assert_eq!(new.next().await, Some(AttachEvent::Data(b"!".to_vec())));
}

#[tokio::test]
async fn 附着方写入的字节会到达_worker_侧() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();
    let mut pty = w.pty(term.pid());

    let att = reg.attach("s1", &info.id).unwrap();
    att.write(b"echo hi\n").await.unwrap();

    let mut buf = [0u8; 32];
    let n = pty.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"echo hi\n");
}

#[tokio::test]
async fn shell_退出时终端被关闭并通知附着方() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();
    let pty = w.pty(term.pid());
    let mut att = reg.attach("s1", &info.id).unwrap();

    // shell 退出：socketpair 的另一端关闭，主进程读到 EOF。
    drop(pty);

    assert_eq!(
        att.next().await,
        Some(AttachEvent::Closed {
            reason: CloseReason::Exited,
            exit: Some(TermExit {
                code: Some(0),
                signal: None
            })
        }),
        "EOF 路径必须把 worker 报告的退出状态带给附着方"
    );
    assert_eq!(att.next().await, None);
    wait_until(|| reg.list_for("s1").is_empty(), "终端从表里消失").await;
    // 摘表先于 `term.close` 的往返（列表要立刻反映现实），所以这里得等一下 RPC。
    wait_until(|| !w.closes().is_empty(), "term.close 到达 worker").await;
    assert_eq!(
        w.closes(),
        vec![term.pid()],
        "EOF 也要向 worker 发 term.close"
    );
    assert!(term.is_closed());
}

#[tokio::test]
async fn 关闭后附着的_ws_不会永远挂着() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let mut att = reg.attach("s1", &info.id).unwrap();
    reg.close("s1", &info.id, CloseReason::Deleted)
        .await
        .unwrap();
    assert_eq!(
        att.next().await,
        Some(AttachEvent::Closed {
            reason: CloseReason::Deleted,
            exit: Some(TermExit {
                code: Some(0),
                signal: None
            })
        })
    );
    assert_eq!(att.next().await, None);
}

// ------------------------------------------------------------ 空闲与尺寸

// 空闲超时的配置单位是秒，因此下面几个用例只能用真实时间等一秒出头。
// 本来该用 `tokio::time::pause`，但那要给 tokio 打开 `test-util` feature，
// 而本任务不允许改 Cargo.toml——多花一秒钟换不碰别人的文件，值得。
const IDLE_SECS: u64 = 1;
/// 比 [`IDLE_SECS`] 多出的余量，抵消调度抖动。
const IDLE_SLACK: Duration = Duration::from_millis(300);

#[tokio::test]
async fn 空闲超时只回收没有附着的终端() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, IDLE_SECS);
    let idle = open_one(&reg, &w, "s1").await;
    let watched = open_one(&reg, &w, "s1").await;
    let idle_pid = reg.get("s1", &idle.id).unwrap().pid();
    let _att = reg.attach("s1", &watched.id).unwrap();

    // 还没到点，一个都不该动。
    assert_eq!(reg.sweep_idle().await, 0);

    tokio::time::sleep(Duration::from_secs(IDLE_SECS) + IDLE_SLACK).await;
    assert_eq!(reg.sweep_idle().await, 1);
    assert_eq!(w.closes(), vec![idle_pid]);
    assert!(reg.get("s1", &idle.id).is_err());
    assert!(
        reg.get("s1", &watched.id).is_ok(),
        "有 WS 挂着的终端不算空闲"
    );
}

#[tokio::test]
async fn 有输出的终端不会被判为空闲() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, IDLE_SECS);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();
    let mut pty = w.pty(term.pid());

    // 睡过大半个空闲窗口之后来一次输出，计时必须从这次输出重新起算。
    tokio::time::sleep(Duration::from_secs(IDLE_SECS) - Duration::from_millis(400)).await;
    pty.write_all(b"tick").await.unwrap();
    wait_until(|| term.lock().scrollback.len() == 4, "输出到达").await;

    tokio::time::sleep(Duration::from_millis(600)).await;
    // 从创建算已经超过 1 秒了，但从最近一次输出算还没到。
    assert_eq!(reg.sweep_idle().await, 0, "有输出的终端被误判为空闲");
    assert!(w.closes().is_empty());

    tokio::time::sleep(Duration::from_secs(IDLE_SECS) + IDLE_SLACK).await;
    assert_eq!(reg.sweep_idle().await, 1);
    assert_eq!(w.closes(), vec![term.pid()]);
}

#[tokio::test]
async fn 空闲超时为零表示关闭回收() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 0);
    open_one(&reg, &w, "s1").await;
    // 若把 0 当字面值用，「空闲了 0 秒 >= 0 秒」立刻成立，这里会被扫掉。
    assert_eq!(reg.sweep_idle().await, 0);
    assert_eq!(reg.count_for("s1"), 1);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(reg.sweep_idle().await, 0);
    assert!(w.closes().is_empty());
}

#[tokio::test]
async fn resize_先落到_worker_再更新本地记录() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let info = open_one(&reg, &w, "s1").await;
    let pid = reg.get("s1", &info.id).unwrap().pid();

    reg.resize("s1", &info.id, 220, 50).await.unwrap();
    assert_eq!(*w.resizes.lock().unwrap(), vec![(pid, 220, 50)]);
    let after = reg.list_for("s1").remove(0);
    assert_eq!((after.cols, after.rows), (220, 50));

    // 0 是非法尺寸，不该白跑一趟 RPC。
    let err = reg.resize("s1", &info.id, 0, 50).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert_eq!(w.resizes.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn 列表只含本会话且带上附着状态() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let a = open_one(&reg, &w, "s1").await;
    let b = open_one(&reg, &w, "s1").await;
    open_one(&reg, &w, "s2").await;

    let list = reg.list_for("s1");
    assert_eq!(list.len(), 2);
    assert!(list.iter().all(|i| !i.attached));
    assert!(list.iter().any(|i| i.id == a.id));
    assert!(list.iter().any(|i| i.id == b.id));

    let _att = reg.attach("s1", &a.id).unwrap();
    let list = reg.list_for("s1");
    let a_info = list.iter().find(|i| i.id == a.id).unwrap();
    let b_info = list.iter().find(|i| i.id == b.id).unwrap();
    assert!(a_info.attached);
    assert!(!b_info.attached);
}

#[tokio::test]
async fn 尺寸为零的创建请求不会打扰_worker() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let err = reg
        .open(
            "s1",
            owner(),
            &w.handle,
            TermOpenParams {
                shell: None,
                user: None,
                cols: 0,
                rows: 24,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::InvalidRequest);
    assert!(w.ptys.lock().unwrap().is_empty(), "不该开出 PTY");
    // 被拒的请求不能占住名额。
    assert_eq!(reg.count_for("s1"), 0);
    open_one(&reg, &w, "s1").await;
}

#[tokio::test]
async fn 终端_id_是随机且不重复的() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(8, 1800);
    let mut ids = std::collections::HashSet::new();
    for _ in 0..8 {
        let info = open_one(&reg, &w, "s1").await;
        assert_eq!(info.id.len(), TERMINAL_ID_BYTES * 2);
        assert!(ids.insert(info.id), "终端 id 重复");
    }
}

// ------------------------------------------------------ 真 PTY 端到端

/// 起一个进程内的**真** worker：RPC 分发、附件传递、双泵、PTY 全是真实现，
/// 只是 worker 逻辑跑在本进程里而不是另一个进程。
async fn start_real_worker() -> WorkerHandle {
    let mut d = Dispatcher::new();
    let _table = crate::worker::terminal::register(&mut d);
    serve_in_process(d).await
}

/// A 期回归（`roadmap/12-workspace.md` §4.9）：**真 PTY** 下 shell 自行退出后，
/// 附着方必须在几秒内收到带真实退出码的关闭事件，终端同时从列表消失。
///
/// 必须用真 PTY 而不是上面的假 worker：这个 bug 的藏身处正是
/// 「registry ⇄ 通道 ⇄ worker 双泵 ⇄ PTY」这段从没被端到端跑过的链路——
/// Windows 上 shell 退出并不会让 ConPTY 关闭输出管道，EOF 要等有人关掉
/// 伪控制台才出现，而假 worker（drop 即 EOF）天生测不到这件事。
#[tokio::test]
async fn 真_pty_里_shell_退出后立刻收到退出码且终端出表() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = start_real_worker().await;
    let reg = registry(4, 1800);

    // Unix 上固定用 /bin/sh（理由同 `worker::terminal::unix` 的测试：
    // 不跟开发者的 rc 文件绑定）；Windows 上用默认 shell（%COMSPEC%，即 cmd.exe）。
    #[cfg(unix)]
    let shell = Some("/bin/sh".to_string());
    #[cfg(windows)]
    let shell = None;
    let info = match reg
        .open(
            "s1",
            owner(),
            &w,
            TermOpenParams {
                shell,
                user: None,
                cols: 80,
                rows: 24,
            },
        )
        .await
    {
        Ok(i) => i,
        // ConPTY 需要 Windows 10 1809+，与 worker 侧测试一样跳过而不是失败。
        Err(e) => {
            eprintln!("跳过：本机开不出真终端（{}）", e.message);
            return;
        }
    };
    // `TerminalInfo.pid`（A 期一并加的）必须就是 worker 里 shell 的 pid——
    // C 期的 cwd 兜底轮询要拿它查 `/api/v1/processes/{pid}`。
    assert!(info.pid > 0);
    assert_eq!(info.pid, reg.get("s1", &info.id).unwrap().pid());
    let mut att = reg.attach("s1", &info.id).unwrap();

    // 等 shell 打出第一个字节（横幅或提示符），确认它真的起来了——
    // 在 shell 就绪之前发 exit，测的就不是「运行中的 shell 退出」这条路径。
    let deadline = Instant::now() + Duration::from_secs(15);
    match tokio::time::timeout_at(deadline, att.next()).await {
        Ok(Some(AttachEvent::Data(_))) => {}
        Ok(other) => panic!("shell 输出之前附着就结束了: {other:?}"),
        Err(_) => panic!("15 秒内没等到 shell 的任何输出"),
    }

    att.write(if cfg!(windows) {
        b"exit 42\r\n" as &[u8]
    } else {
        b"exit 42\n"
    })
    .await
    .unwrap();

    // 核心断言：几秒内（远小于 worker 侧 30 秒的兜底清理）收到 Exited + 42。
    // 超时即 `roadmap/12-workspace.md` §2.1 实测到的退出信号 bug。
    let deadline = Instant::now() + Duration::from_secs(10);
    let (reason, exit) = loop {
        match tokio::time::timeout_at(deadline, att.next()).await {
            // exit 命令的回显等尾巴输出，读掉继续等。
            Ok(Some(AttachEvent::Data(_))) => continue,
            Ok(Some(AttachEvent::Closed { reason, exit })) => break (reason, exit),
            Ok(None) => panic!("附着结束却没有收到 Closed 事件"),
            Err(_) => {
                panic!("shell 已退出，但 10 秒内附着方没有收到关闭事件——退出信号丢了")
            }
        }
    };
    assert_eq!(reason, CloseReason::Exited, "原因必须是 exited");
    assert_eq!(
        exit,
        Some(TermExit {
            code: Some(42),
            signal: None
        }),
        "必须带上 shell 真实的退出码 42"
    );
    assert_eq!(att.next().await, None);
    wait_until(|| reg.list_for("s1").is_empty(), "终端从列表消失").await;
}

// -------------------------------------------------------------- 观察者

#[derive(Default)]
struct RecordingObserver {
    events: StdMutex<Vec<TerminalClosed>>,
}

impl TerminalObserver for RecordingObserver {
    fn on_closed(&self, event: &TerminalClosed) {
        self.events.lock().unwrap().push(event.clone());
    }
}

impl RecordingObserver {
    fn install(reg: &TerminalRegistry) -> Arc<RecordingObserver> {
        let obs = Arc::new(RecordingObserver::default());
        reg.set_observer(obs.clone());
        obs
    }

    fn events(&self) -> Vec<TerminalClosed> {
        self.events.lock().unwrap().clone()
    }
}

/// 三种非 REST 的关闭（shell 自退、空闲、登出）都要产生恰好一次回调，
/// 事件里带原因、归属与退出状态——这是 roadmap/03 §7 审计验收的地基。
#[tokio::test]
async fn shell_退出_空闲_登出都回调观察者() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, IDLE_SECS);
    let obs = RecordingObserver::install(&reg);

    // shell 自行退出（EOF）。
    let exited = open_one(&reg, &w, "s1").await;
    let pty = w.pty(reg.get("s1", &exited.id).unwrap().pid());
    drop(pty);
    wait_until(|| !obs.events().is_empty(), "EOF 触发回调").await;

    // 空闲回收。
    let idle = open_one(&reg, &w, "s1").await;
    tokio::time::sleep(Duration::from_secs(IDLE_SECS) + IDLE_SLACK).await;
    assert_eq!(reg.sweep_idle().await, 1);

    // 会话登出。
    let logout = open_one(&reg, &w, "s2").await;
    assert_eq!(reg.close_all_for("s2", CloseReason::Logout).await, 1);

    let events = obs.events();
    assert_eq!(events.len(), 3, "每次真正的关闭恰好一次回调");
    let by_id = |id: &str| {
        events
            .iter()
            .find(|e| e.id == id)
            .unwrap_or_else(|| panic!("没有 {id} 的关闭事件"))
    };

    let e = by_id(&exited.id);
    assert_eq!(e.reason, CloseReason::Exited);
    assert_eq!(
        e.exit,
        Some(TermExit {
            code: Some(0),
            signal: None
        })
    );
    assert_eq!(e.owner, owner());
    assert_eq!(e.session_hash, "s1");

    assert_eq!(by_id(&idle.id).reason, CloseReason::Idle);
    assert_eq!(by_id(&logout.id).reason, CloseReason::Logout);
    assert_eq!(by_id(&logout.id).session_hash, "s2");
}

/// REST 的 DELETE 同样回调（跳不跳过 `deleted` 是宿主的决定，core 只报事实），
/// 且幂等裁决输掉的一方不产生第二次回调。
#[tokio::test]
async fn delete_只回调一次() {
    let _serial = IN_PROCESS_WORKER.lock().await;
    let w = FakeWorker::start().await;
    let reg = registry(4, 1800);
    let obs = RecordingObserver::install(&reg);
    let info = open_one(&reg, &w, "s1").await;
    let term = reg.get("s1", &info.id).unwrap();

    let (a, b) = tokio::join!(
        reg.finish(&term, CloseReason::Deleted),
        reg.finish(&term, CloseReason::Exited)
    );
    assert!(a ^ b);
    let events = obs.events();
    assert_eq!(events.len(), 1, "输掉裁决的一方不得再回调");
    assert_eq!(events[0].pid, term.pid());
}
