//! worker 侧的终端：`term.open` / `term.resize` / `term.close`
//! （`roadmap/03-terminal.md` §4.1 §4.2 §4.5）。
//!
//! # 为什么终端必须在 worker 里
//!
//! `design.md` §2.2：终端里跑的 shell 就是登录用户本人。worker 已经是那个身份了，
//! 它拉起的 shell 天然继承过来——**内核**来裁决这个 shell 能干什么，
//! 服务端一行授权代码都不用写。反过来，如果终端开在主进程（root / LocalSystem）里，
//! 每一次读写文件、每一次发信号都得由我们自己判断「该不该」，
//! 那正是 `design.md` §5.1 要避免的自建鉴权。
//!
//! # 数据通路：为什么是附件而不是 JSON
//!
//! ```text
//! 主进程 ⇄ IpcChannel ⇄ worker（两个泵）⇄ PTY ⇄ shell
//! ```
//!
//! 终端是字节流，塞进 RPC 的 JSON 帧要付 base64 与转义的代价，还会和别的 RPC 抢
//! 同一条通道的写锁（`worker::FrameWriter` 是串行的）——一个 `cat 大文件` 就能把
//! 整个会话的控制面堵住。所以 `term.open` 只用 RPC 回一个**附件**
//! （`Dispatcher::register_fd`），此后终端字节走它自己那条通道，与控制面彻底分开。
//!
//! # 背压
//!
//! 两个泵都是「读一块、写完再读下一块」，中间没有任何无界缓冲。主进程读得慢
//! → 通道缓冲满 → 泵卡在写上 → 不再读 PTY → PTY 缓冲满 → shell 自己被挡住。
//! 这一路顶回去正是想要的：宁可让 `yes` 慢下来，也不要在 worker 里堆几百 MB
//! 的终端输出。
//!
//! # 身份：worker 不判断「该不该」
//!
//! `TermOpenParams::user` 只有 admin worker 会用；user worker **忽略**它——
//! 它被内核锁死在自己的身份上，`user` 写什么都只能是自己。「这个会话能不能开
//! 别人的终端」由主进程按 `session.elevated` 决定（`roadmap/03-terminal.md` §4.2：
//! 未提权 → 403，根本不会派到 admin worker），worker 这里不复核。
//! 复核会带来两套判断规则，而两套规则迟早会不一致。
//!
//! # 两个平台的实现
//!
//! | | Unix（[`unix`]） | Windows（[`windows`]） |
//! |---|---|---|
//! | 伪终端 | `openpty` + `TIOCSCTTY` | ConPTY（`CreatePseudoConsole`） |
//! | 起 shell | `fork` + `setuid` + `execve` | `CreateProcessAsUserW` + 伪控制台属性 |
//! | 身份切换 | `setgroups`/`setgid`/`setuid` | 目标用户的令牌（由 helper 交来） |
//! | 关整棵进程树 | `killpg(SIGHUP)`（进程组） | 作业对象（Job Object） |
//! | 退出状态 | `waitpid`，可区分 code / signal | `GetExitCodeProcess`，只有 code |
//!
//! 两侧各自实现一个同名同形的 [`TerminalTable`]，本文件只做注册与共享的编解码。
//! 没有把它们抽成 trait：两边的内部状态（pty master fd vs. ConPTY 句柄 + 作业对象）
//! 与生命周期规则差得太远，抽出来的公共部分只剩下方法名。

use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;
use strixmaid_types::rpc::{self, TermOpenParams};
use strixmaid_types::{ApiError, ApiResult};

use super::Dispatcher;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

#[cfg(unix)]
pub use unix::TerminalTable;
#[cfg(windows)]
pub use windows::TerminalTable;

/// 泵一次搬运的字节上限。
pub(crate) const PUMP_BUF: usize = 16 * 1024;

/// `term.close` 里等 shell 咽气的宽限期，超时就强杀。
pub(crate) const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// shell 自行退出后，条目带着退出状态在表里保留多久，等主进程的 `term.close` 来取。
///
/// 主进程在通道读到 EOF 后总会补一次 `term.close`（`terminal/mod.rs` 的 `shutdown`），
/// 正常情况下几毫秒内就来取走了；这个上限只兜「主进程一直不来」的异常路径，
/// 防止条目在表里永久滞留。
pub(crate) const REAPED_LINGER: Duration = Duration::from_secs(30);

/// 表里没有这个 pid。
pub(crate) fn unknown(pid: u32) -> ApiError {
    ApiError::not_found(format!("本 worker 没有 pid 为 {pid} 的终端"))
}

/// 把 JSON 参数解成 `P`。
///
/// 解不出来是**主进程构造错了调用**，不是用户输入有问题，所以报 `internal`；
/// 理由与 `worker::providers::params` 相同，那边是私有的，这里重写一份。
fn decode<P: DeserializeOwned>(method: &'static str, v: Value) -> ApiResult<P> {
    serde_json::from_value(v).map_err(|e| {
        ApiError::internal(format!("worker 无法解析 {method} 的参数")).with_detail(e.to_string())
    })
}

fn encode<R: serde::Serialize>(r: R) -> ApiResult<Value> {
    serde_json::to_value(r)
        .map_err(|e| ApiError::internal("worker 无法序列化结果").with_detail(e.to_string()))
}

/// 把 `term.*` 三个方法注册进分发表，共享同一张终端表。
///
/// `term.open` 走 [`Dispatcher::register_fd`]：它要交出的通道附件必须和结果
/// **在同一帧**里发出去，普通处理器的签名表达不了这件事。
pub fn register(d: &mut Dispatcher) -> TerminalTable {
    let table = TerminalTable::new();

    let t = table.clone();
    d.register_fd(
        rpc::TERM_OPEN,
        Arc::new(move |v: Value| {
            let t = t.clone();
            Box::pin(async move {
                let params: TermOpenParams = decode(rpc::TERM_OPEN, v)?;
                let (result, attachment) = t.open(params).await?;
                Ok((encode(result)?, vec![attachment]))
            })
        }),
    );

    let t = table.clone();
    d.register_fn(rpc::TERM_RESIZE, move |v| {
        let t = t.clone();
        async move {
            t.resize(decode(rpc::TERM_RESIZE, v)?).await?;
            encode(())
        }
    });

    let t = table.clone();
    d.register_fn(rpc::TERM_CLOSE, move |v| {
        let t = t.clone();
        async move {
            let result = t.close(decode(rpc::TERM_CLOSE, v)?).await?;
            encode(result)
        }
    });

    table
}
