//! 主进程 ↔ helper ↔ worker 的 IPC 协议（`docs/design.md` §10）。
//!
//! # 进程关系
//!
//! ```text
//! 主进程 ──socketpair(fd 3)──▶ strixmaid-helper（每会话一个，持有 PAM 句柄）
//!                                   │ fork + setuid + exec
//!                                   └──socketpair(fd 3)──▶ strixmaid worker
//!                                        └─ 主进程侧的一端经 SCM_RIGHTS 传回主进程
//! ```
//!
//! # 帧格式
//!
//! **u32 大端长度前缀 + JSON**。长度只计 JSON 部分，不含前缀本身；上限
//! [`MAX_FRAME_LEN`]，超过即拒绝（不读取 payload 直接报错）。
//!
//! # 帧头为什么有 `fd_count`
//!
//! 帧头是 **`u32 长度（大端） + u8 fd 个数`**（[`FRAME_HEADER_LEN`] = 5）。
//! fd 本身仍走 `SCM_RIGHTS` 带外通道，帧里只记**这一帧附带了几个**。
//!
//! 记这个数不是为了好看，是因为 `SOCK_STREAM` 上的带外数据有个致命性质：
//! **用普通 `read()` 读过附着了 fd 的那些字节，内核会把 fd 直接丢掉**，
//! 不报错、无痕迹。也就是说只要有一帧可能带 fd，读端就必须对**每一帧**
//! 都用 `recvmsg` 加控制缓冲。有了 `fd_count`，读端还能核对
//! 「说好带 1 个、实际收到 0 个」并当协议错误报出来，而不是拿着一个
//! 半残的终端去调试。
//!
//! 这是**不兼容变更**（`roadmap/03-terminal.md` §8.1）。helper、worker 与主进程
//! 同版本发布，不存在跨版本兼容需求；将来 Agent 与 Server 之间若复用本帧格式，
//! 需要单独版本化。
//!
//! 本模块只提供**同步**（`std::io`）的读写与纯编解码；tokio 版本在
//! `strixmaid-core::session::framing` 里复用这里的 [`encode`] / [`decode`]——
//! 本 crate 按 §3 不依赖 tokio。
//!
//! # 凭据安全（§5.3）
//!
//! [`ToHelper::AuthRespond`] 是整个系统里**唯一**把明文密码序列化的地方——它必须过
//! socketpair 到 helper 才能交给 PAM。为此：
//!
//! - 每个 `value` 都是 [`Zeroizing<String>`]，drop 时擦除；
//! - 编码出的整帧放在 [`Zeroizing<Vec<u8>>`] 里，写完即擦；读入的整帧同样如此；
//! - [`ToHelper`] 的 `Debug` 手写脱敏，`AuthRespond` 只打印条数；
//! - 不实现 `Clone`，避免复制出不受保护的副本。

use std::fmt;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::Zeroizing;

use crate::ApiError;
use crate::auth::{AuthUser, Prompt};

// ===========================================================================
// 常量
// ===========================================================================

/// helper / worker 继承的 IPC socket 所在的 fd 编号。
///
/// 主进程 spawn helper 时把 socketpair 的一端 `dup2` 到 3；helper fork worker 时
/// 同样把 worker 的 socketpair 一端 `dup2` 到 3（覆盖掉继承自 helper 的那个）。
pub const IPC_FD: i32 = 3;

/// 单帧 JSON 部分的最大长度（1 MiB）。
///
/// IPC 消息只有几十到几百字节；这个上限是为了让对端的错误或恶意长度前缀
/// 不会导致无界分配。
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// 帧头字节数：`u32 长度` + `u8 fd 个数`。
pub const FRAME_HEADER_LEN: usize = 5;

/// 一帧最多附带的 fd 个数。
///
/// 目前只有终端用到，且每次恰好 1 个（`roadmap/03-terminal.md` §4.5）。
/// 留 4 是给将来可能的多 fd 场景，同时给读端一个明确的上限——
/// 对端声称要传 200 个 fd 时应当当协议错误拒掉，而不是照着分配控制缓冲。
pub const MAX_FRAME_FDS: usize = 4;

/// worker 内置 RPC 方法名：探活。
pub const METHOD_PING: &str = "ping";
/// worker 内置 RPC 方法名：报告自身运行身份。
pub const METHOD_WHOAMI: &str = "whoami";

// ===========================================================================
// 错误
// ===========================================================================

/// 帧编解码错误。
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// 底层 IO 失败（包括对端半路关闭导致的 `UnexpectedEof`）。
    #[error("IPC IO 错误: {0}")]
    Io(#[from] io::Error),
    /// 长度前缀超过 [`MAX_FRAME_LEN`]。
    #[error("IPC 帧过大: {len} 字节（上限 {MAX_FRAME_LEN}）")]
    TooLarge {
        /// 对端声明的长度。
        len: u64,
    },
    /// 帧头声明的 fd 个数超过 [`MAX_FRAME_FDS`]。
    #[error("IPC 帧声称附带 {count} 个 fd（上限 {MAX_FRAME_FDS}）")]
    TooManyFds {
        /// 对端声明的个数。
        count: u8,
    },
    /// JSON 编解码失败。
    #[error("IPC JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
    /// 收到的消息类型不符合当前协议阶段的预期。
    #[error("IPC 协议错误: {0}")]
    Protocol(String),
}

/// 帧编解码 Result 别名。
pub type IpcResult<T> = Result<T, IpcError>;

// ===========================================================================
// Zeroizing<String> 的 serde 适配
// ===========================================================================

/// 把 JSON 字符串反序列化成自动擦除的 [`Zeroizing<String>`]。
/// serde 产出的临时 `String` 立即被 move 进 `Zeroizing`，没有第二份拷贝。
pub fn deserialize_zeroizing_string<'de, D>(de: D) -> Result<Zeroizing<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(de).map(Zeroizing::new)
}

/// 把 [`Zeroizing<String>`] 按普通字符串序列化。
///
/// **只用于 IPC 帧**——这是明文密码唯一允许被序列化的路径，且编码结果必须放进
/// [`Zeroizing<Vec<u8>>`]（见 [`encode`]）。
pub fn serialize_zeroizing_string<S>(value: &Zeroizing<String>, ser: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    ser.serialize_str(value.as_str())
}

// ===========================================================================
// 主进程 → helper
// ===========================================================================

/// 对某条 PAM 提示的回应（IPC 版，与 [`crate::auth::PromptResponse`] 同形）。
///
/// 与 HTTP DTO 的区别只在于它**可以序列化**——它要过 socketpair。
/// `Debug` 脱敏，不实现 `Clone`。
#[derive(Serialize, Deserialize)]
pub struct IpcPromptResponse {
    /// 对应 [`Prompt::id`]。
    pub id: u32,
    /// 用户输入的原文，drop 时擦除。
    #[serde(
        serialize_with = "serialize_zeroizing_string",
        deserialize_with = "deserialize_zeroizing_string"
    )]
    pub value: Zeroizing<String>,
}

impl fmt::Debug for IpcPromptResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IpcPromptResponse")
            .field("id", &self.id)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// 主进程发给 helper 的消息。
///
/// 一个 helper 进程的生命周期内消息顺序固定为：
/// `AuthStart` → (`AuthRespond`)* → [`SpawnWorker`] → … → `CloseSession`。
/// helper 在 `AuthFail` / `Error` 之后会自行退出。
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToHelper {
    /// 开始一次 PAM 认证对话。helper 随后回 [`FromHelper::Prompts`]（若 PAM 需要交互）
    /// 或直接回 [`FromHelper::AuthOk`] / [`FromHelper::AuthFail`]。
    AuthStart {
        /// PAM 服务名，对应 `/etc/pam.d/<service>`。
        service: String,
        /// 要认证的系统用户名。
        username: String,
        /// 主二进制路径，用于 `exec <worker_exe> worker`。
        /// 为 `None` 时 helper 取自身所在目录下的 `strixmaid`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_exe: Option<String>,
        /// 允许提权的组（`session.elevate_groups`）。
        ///
        /// 由主进程在认证一开始就下发，helper **不读配置文件**——它是 setuid 组件，
        /// 少一条读文件的路径就少一处可被做手脚的地方（`roadmap/01` §4.8）。
        /// helper 在 `SpawnWorker { as_root: true }` 时用它做权威判断。
        #[serde(default)]
        elevate_groups: Vec<String>,
        /// 设置为 `PAM_RHOST` 的来源地址，供 PAM 模块记日志 / 限速。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rhost: Option<String>,
    },
    /// 回应上一轮 [`FromHelper::Prompts`]。
    AuthRespond {
        /// 对所有需要输入的提示的回应；`Info` / `Error` 类提示不需要回应。
        responses: Vec<IpcPromptResponse>,
    },
    /// 用已认证的身份 fork + setuid + exec 一个 worker。
    ///
    /// helper 回 [`FromHelper::WorkerSpawned`]，**紧接着一帧**带 `SCM_RIGHTS` 的
    /// 单字节消息传递 worker socketpair 的主进程侧 fd。
    SpawnWorker {
        /// 是否调用 `pam_open_session`（§5.4：这是 `--user` unit 支持的前提）。
        /// 失败时降级继续，结果反映在 `WorkerSpawned::session_opened`。
        open_session: bool,
        /// 为 `true` 时**不切换身份**、以 root 运行 worker（admin worker，§2.2）。
        /// 要求 helper 自身是 root，否则 helper 回 [`FromHelper::Error`]。
        as_root: bool,
    },
    /// 关闭 PAM 会话并退出 helper。helper 回 [`FromHelper::SessionClosed`] 后退出。
    /// 直接关闭 socket 等价于本消息。
    CloseSession,
}

impl fmt::Debug for ToHelper {
    /// 手写：`AuthRespond` 只打印条数，绝不打印内容。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToHelper::AuthStart {
                service,
                username,
                worker_exe,
                rhost,
                elevate_groups,
            } => f
                .debug_struct("AuthStart")
                .field("service", service)
                .field("username", username)
                .field("worker_exe", worker_exe)
                .field("rhost", rhost)
                // 组名不是敏感信息，而排查「为什么提权被拒」时最先要看的就是它
                .field("elevate_groups", elevate_groups)
                .finish(),
            ToHelper::AuthRespond { responses } => f
                .debug_struct("AuthRespond")
                .field("responses", &format_args!("<{} redacted>", responses.len()))
                .finish(),
            ToHelper::SpawnWorker {
                open_session,
                as_root,
            } => f
                .debug_struct("SpawnWorker")
                .field("open_session", open_session)
                .field("as_root", as_root)
                .finish(),
            ToHelper::CloseSession => f.write_str("CloseSession"),
        }
    }
}

// ===========================================================================
// helper → 主进程
// ===========================================================================

/// helper 发给主进程的消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromHelper {
    /// PAM 需要用户回答。主进程把它原样透传给浏览器，再以 [`ToHelper::AuthRespond`] 回来。
    Prompts {
        /// 本轮提示，`id` 从 0 重新编号。
        prompts: Vec<Prompt>,
    },
    /// 认证 + 账户检查通过。
    AuthOk {
        /// 认证到的系统身份（`PAM_USER` 可能被模块改写，以这里为准）。
        user: AuthUser,
    },
    /// 认证失败。helper 发出本消息后退出。
    AuthFail {
        /// PAM 错误描述（`pam_strerror`），不含任何凭据。
        reason: String,
    },
    /// worker 已 fork + exec。**下一帧**是带 `SCM_RIGHTS` 的 fd 传递帧。
    WorkerSpawned {
        /// worker pid。主进程用它在登出时终止 worker。
        pid: i32,
        /// worker 实际运行的 uid（`as_root` 时为 0）。
        uid: u32,
        /// `pam_open_session` 是否成功。非 root 环境下 pam_systemd 等会失败，
        /// 此时降级继续、用户级 unit 不可用。
        session_opened: bool,
        /// `session_opened == false` 时的原因，供日志与能力探测。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_error: Option<String>,
    },
    /// PAM 会话已关闭，helper 即将退出。
    SessionClosed,
    /// 协议或系统错误（如非 root 却收到 `as_root`）。
    Error {
        /// 人类可读说明，不含凭据。
        message: String,
    },
}

// ===========================================================================
// 主进程 ↔ worker
// ===========================================================================

/// 主进程发给 worker 的消息。
///
/// RPC 采用「方法名 + JSON 参数」的开放形式，worker 端按名字分发，provider 后续以
/// 注册的方式挂进来，不需要改本枚举。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToWorker {
    /// 一次 RPC 调用。worker 以同 `id` 的 [`FromWorker::Result`] 或
    /// [`FromWorker::Error`] 回应；调用可以并发在途，回应顺序不保证。
    Call {
        /// 主进程分配的调用序号。
        id: u64,
        /// 方法名，如 [`METHOD_PING`] / [`METHOD_WHOAMI`] / `service.action`。
        method: String,
        /// 参数，按方法各自约定。
        #[serde(default)]
        params: serde_json::Value,
    },
    /// 建立一个订阅（`roadmap/01-worker-execution.md` §4.4）。
    ///
    /// worker 为它起一个**独立的任务**，把流的每一项以同 `id` 的
    /// [`FromWorker::Event`] 送回；流结束或建立失败时发一帧 [`FromWorker::End`]。
    /// `id` 与 [`ToWorker::Call::id`] 共用同一个序号空间——同一条连接上
    /// 一个 `id` 只对应一件事，主进程的等待表因此不必区分两种键。
    Subscribe {
        /// 主进程分配的订阅序号。
        id: u64,
        /// 频道名，如 [`crate::rpc::LOG_FOLLOW`]。
        channel: String,
        /// 参数，按频道各自约定。
        #[serde(default)]
        params: serde_json::Value,
    },
    /// 取消一个订阅。worker 结束对应的任务，**不再**为它发 `End`——
    /// 主进程发出本消息时就已经把订阅从等待表里摘掉了，再回一帧只会是噪音。
    ///
    /// 对未知或已结束的 `id` 是无操作，重复发送安全。
    Unsubscribe {
        /// 对应 [`ToWorker::Subscribe::id`]。
        id: u64,
    },
    /// 请求 worker 退出。worker 处理完在途调用后关闭 socket。
    Shutdown,
}

/// worker 发给主进程的消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromWorker {
    /// worker 启动后的第一帧，用于主进程确认它确实以预期身份跑起来了。
    Hello {
        /// worker pid。
        pid: i32,
        /// 实际 uid。
        uid: u32,
        /// 实际 gid。
        gid: u32,
    },
    /// RPC 成功。
    Result {
        /// 对应 [`ToWorker::Call::id`]。
        id: u64,
        /// 返回值。
        value: serde_json::Value,
    },
    /// RPC 失败。
    Error {
        /// 对应 [`ToWorker::Call::id`]。
        id: u64,
        /// 错误——与 HTTP 层同一形状，主进程可以原样透传给浏览器。
        error: ApiError,
    },
    /// 订阅流的一项。
    Event {
        /// 对应 [`ToWorker::Subscribe::id`]。
        id: u64,
        /// 一帧数据，形状由频道约定。
        data: serde_json::Value,
    },
    /// 订阅结束。此后该 `id` 不会再有任何帧。
    ///
    /// `error` 为 `None` 表示流自然走完（follow 的子进程退出、采集器停止等）；
    /// 为 `Some` 表示订阅**建立失败**或中途出错（频道不存在、provider 不可用、
    /// polkit 拒绝……），错误与 HTTP 层同一形状，可原样上报。
    End {
        /// 对应 [`ToWorker::Subscribe::id`]。
        id: u64,
        /// 结束原因；正常结束为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ApiError>,
    },
}

/// [`METHOD_WHOAMI`] 的返回值：worker 眼中的自己。
///
/// 用来端到端证明「worker 以登录用户身份运行」——每个字段都直接来自内核或 exec 时的环境。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct WhoAmI {
    /// `getpid()`。
    pub pid: i32,
    /// `getuid()`。
    pub uid: u32,
    /// `geteuid()`。
    pub euid: u32,
    /// `getgid()`。
    pub gid: u32,
    /// `getegid()`。
    pub egid: u32,
    /// `getgroups()`。
    #[serde(default)]
    pub groups: Vec<u32>,
    /// 当前工作目录。
    pub cwd: String,
    /// 环境变量 `USER`（由 helper 在 exec 前设置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 环境变量 `HOME`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
}

// ===========================================================================
// 帧编解码（纯函数 + 同步 IO）
// ===========================================================================

/// 把消息编码成完整的一帧（长度前缀 + JSON）。
///
/// 返回值放在 [`Zeroizing`] 里：帧可能含明文密码，调用方写完后 drop 即擦除。
pub fn encode<T: Serialize + ?Sized>(msg: &T) -> IpcResult<Zeroizing<Vec<u8>>> {
    encode_with_fds(msg, 0)
}

/// 同 [`encode`]，并在帧头声明这一帧附带 `fd_count` 个 fd。
///
/// fd 本身由调用方经 `SCM_RIGHTS` 与本帧**一起**发出（见
/// `strixmaid-core::session::framing`）。这里只负责把个数写进帧头，
/// 让读端知道要不要去控制缓冲里取。
pub fn encode_with_fds<T: Serialize + ?Sized>(
    msg: &T,
    fd_count: u8,
) -> IpcResult<Zeroizing<Vec<u8>>> {
    if fd_count as usize > MAX_FRAME_FDS {
        return Err(IpcError::TooManyFds { count: fd_count });
    }
    // 先把 JSON 写进一个 Zeroizing 缓冲，再拼前缀，全程不出现未受保护的中间副本。
    let mut frame = Zeroizing::new(Vec::with_capacity(128));
    frame.extend_from_slice(&[0, 0, 0, 0, 0]);
    serde_json::to_writer(&mut *frame, msg)?;
    let len = frame.len() - FRAME_HEADER_LEN;
    if len > MAX_FRAME_LEN {
        return Err(IpcError::TooLarge { len: len as u64 });
    }
    frame[..4].copy_from_slice(&(len as u32).to_be_bytes());
    frame[4] = fd_count;
    Ok(frame)
}

/// 把一帧的 JSON 部分解码成消息。
pub fn decode<T: DeserializeOwned>(payload: &[u8]) -> IpcResult<T> {
    Ok(serde_json::from_slice(payload)?)
}

/// 解析帧头，返回 `(payload 长度, fd 个数)` 并校验两个上限。
pub fn parse_header(header: [u8; FRAME_HEADER_LEN]) -> IpcResult<(usize, u8)> {
    let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err(IpcError::TooLarge { len: len as u64 });
    }
    let fd_count = header[4];
    if fd_count as usize > MAX_FRAME_FDS {
        return Err(IpcError::TooManyFds { count: fd_count });
    }
    Ok((len, fd_count))
}

/// 同步读取一帧的 JSON 部分。
///
/// - 对端在帧边界上干净地关闭 → `Ok(None)`；
/// - 读到一半断开 → `Err(Io(UnexpectedEof))`；
/// - 长度超限 → `Err(TooLarge)`，且**不会**去读 payload。
pub fn read_frame<R: Read + ?Sized>(r: &mut R) -> IpcResult<Option<Zeroizing<Vec<u8>>>> {
    let mut header = [0u8; FRAME_HEADER_LEN];
    if !read_exact_or_eof(r, &mut header)? {
        return Ok(None);
    }
    // 同步读端（helper）不接收 fd：它只**发送** worker 的 fd 给主进程。
    // 真收到带 fd 的帧说明协议用错了，宁可报错也不要把 fd 静默丢掉。
    let (len, fd_count) = parse_header(header)?;
    if fd_count > 0 {
        return Err(IpcError::Protocol(format!(
            "同步读端收到声称附带 {fd_count} 个 fd 的帧，但它不具备接收能力"
        )));
    }
    let mut payload = Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut payload)?;
    Ok(Some(payload))
}

/// 同步写出一帧（`frame` 必须是 [`encode`] 的输出）。
pub fn write_frame<W: Write + ?Sized>(w: &mut W, frame: &[u8]) -> IpcResult<()> {
    w.write_all(frame)?;
    w.flush()?;
    Ok(())
}

/// 同步读一条消息；对端干净关闭时返回 `Ok(None)`。
pub fn read_msg<R: Read + ?Sized, T: DeserializeOwned>(r: &mut R) -> IpcResult<Option<T>> {
    match read_frame(r)? {
        None => Ok(None),
        Some(payload) => decode(&payload).map(Some),
    }
}

/// 同步写一条消息。编码缓冲写完即擦除。
pub fn write_msg<W: Write + ?Sized, T: Serialize + ?Sized>(w: &mut W, msg: &T) -> IpcResult<()> {
    let frame = encode(msg)?;
    write_frame(w, &frame)
}

/// 读满 `buf`；若在读到第一个字节之前就遇到 EOF，返回 `Ok(false)`。
fn read_exact_or_eof<R: Read + ?Sized>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "IPC 帧头未读完对端即关闭",
                ));
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

// ===========================================================================
// 单元测试
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PromptStyle;
    use std::io::Cursor;

    #[test]
    fn 帧编解码往返() {
        let msg = FromHelper::Prompts {
            prompts: vec![Prompt {
                id: 0,
                style: PromptStyle::Prompt,
                text: "Password: ".into(),
            }],
        };
        let frame = encode(&msg).unwrap();
        // 帧头 = JSON 长度 + fd 个数
        let (len, fds) = parse_header(frame[..FRAME_HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!(len, frame.len() - FRAME_HEADER_LEN);
        assert_eq!(fds, 0, "普通消息不带 fd");

        let mut cur = Cursor::new(frame.to_vec());
        let back: FromHelper = read_msg(&mut cur).unwrap().unwrap();
        assert_eq!(back, msg);
        // 流耗尽 → 干净 EOF
        assert!(read_msg::<_, FromHelper>(&mut cur).unwrap().is_none());
    }

    #[test]
    fn 多帧连续读写() {
        let mut buf = Vec::new();
        write_msg(
            &mut buf,
            &ToWorker::Call {
                id: 1,
                method: METHOD_PING.into(),
                params: serde_json::Value::Null,
            },
        )
        .unwrap();
        write_msg(&mut buf, &ToWorker::Shutdown).unwrap();

        let mut cur = Cursor::new(buf);
        let a: ToWorker = read_msg(&mut cur).unwrap().unwrap();
        let b: ToWorker = read_msg(&mut cur).unwrap().unwrap();
        assert!(matches!(a, ToWorker::Call { id: 1, .. }));
        assert_eq!(b, ToWorker::Shutdown);
        assert!(read_frame(&mut cur).unwrap().is_none());
    }

    #[test]
    fn 零长度帧被帧层接受但_json_层拒绝() {
        // 帧头 5 字节：长度 0、fd 个数 0
        let mut cur = Cursor::new(vec![0, 0, 0, 0, 0]);
        let payload = read_frame(&mut cur).unwrap().unwrap();
        assert!(payload.is_empty());
        assert!(decode::<FromHelper>(&payload).is_err());
    }

    #[test]
    fn 超长帧在读_payload_前被拒绝() {
        let mut too_big = (MAX_FRAME_LEN as u32 + 1).to_be_bytes().to_vec();
        too_big.push(0); // fd_count
        // 故意只给帧头不给 payload：若实现试图读 payload 会得到 UnexpectedEof 而不是 TooLarge。
        let mut cur = Cursor::new(too_big);
        match read_frame(&mut cur) {
            Err(IpcError::TooLarge { len }) => assert_eq!(len, MAX_FRAME_LEN as u64 + 1),
            other => panic!("应为 TooLarge，实际 {other:?}"),
        }
        // 恰好等于上限则允许
        let mut at_limit = (MAX_FRAME_LEN as u32).to_be_bytes().to_vec();
        at_limit.push(0);
        assert_eq!(
            parse_header(at_limit.try_into().unwrap()).unwrap(),
            (MAX_FRAME_LEN, 0)
        );
    }

    #[test]
    fn 帧头记录_fd_个数() {
        let frame = encode_with_fds(&FromWorker::Hello { pid: 1, uid: 0, gid: 0 }, 1).unwrap();
        let header: [u8; FRAME_HEADER_LEN] = frame[..FRAME_HEADER_LEN].try_into().unwrap();
        let (len, fds) = parse_header(header).unwrap();
        assert_eq!(fds, 1);
        assert_eq!(len, frame.len() - FRAME_HEADER_LEN);
        // 不带 fd 的普通 encode 记 0
        let plain = encode(&FromWorker::Hello { pid: 1, uid: 0, gid: 0 }).unwrap();
        assert_eq!(plain[4], 0);
        // 内容一致，只差帧头那一个字节
        assert_eq!(&plain[FRAME_HEADER_LEN..], &frame[FRAME_HEADER_LEN..]);
    }

    #[test]
    fn fd_个数越界被拒() {
        let err = encode_with_fds(&FromWorker::Hello { pid: 1, uid: 0, gid: 0 }, 99).unwrap_err();
        assert!(matches!(err, IpcError::TooManyFds { count: 99 }), "{err:?}");

        let mut header = [0u8; FRAME_HEADER_LEN];
        header[4] = 99;
        assert!(matches!(
            parse_header(header),
            Err(IpcError::TooManyFds { count: 99 })
        ));
    }

    /// 同步读端（helper）不具备接收 fd 的能力，收到这种帧要报错而不是把 fd 丢掉。
    #[test]
    fn 同步读端拒绝带_fd_的帧() {
        let frame = encode_with_fds(&FromWorker::Hello { pid: 1, uid: 0, gid: 0 }, 1).unwrap();
        let mut cur = Cursor::new(frame.to_vec());
        match read_frame(&mut cur) {
            Err(IpcError::Protocol(msg)) => assert!(msg.contains("fd"), "{msg}"),
            other => panic!("应为 Protocol，实际 {other:?}"),
        }
    }

    #[test]
    fn 半帧断开报_unexpected_eof() {
        // 声明 10 字节只给 3 字节（帧头 5 字节：长度 4 + fd 个数 1）
        let mut cur = Cursor::new(vec![0, 0, 0, 10, 0, b'{', b'}', b' ']);
        match read_frame(&mut cur) {
            Err(IpcError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("应为 UnexpectedEof，实际 {other:?}"),
        }
        // 帧头读了一半
        let mut cur = Cursor::new(vec![0, 0]);
        match read_frame(&mut cur) {
            Err(IpcError::Io(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("应为 UnexpectedEof，实际 {other:?}"),
        }
    }

    #[test]
    fn auth_respond_可序列化但_debug_脱敏() {
        let msg = ToHelper::AuthRespond {
            responses: vec![IpcPromptResponse {
                id: 0,
                value: Zeroizing::new("hunter2".into()),
            }],
        };
        let dbg = format!("{msg:?}");
        assert!(!dbg.contains("hunter2"), "Debug 泄露了明文: {dbg}");
        assert!(dbg.contains("redacted"));

        // 线格式必须带明文（helper 要拿去给 PAM），但只走 Zeroizing 缓冲
        let frame = encode(&msg).unwrap();
        let json = std::str::from_utf8(&frame[FRAME_HEADER_LEN..]).unwrap();
        assert!(json.contains("hunter2"));
        let back: ToHelper = decode(&frame[FRAME_HEADER_LEN..]).unwrap();
        match back {
            ToHelper::AuthRespond { responses } => {
                assert_eq!(responses.len(), 1);
                assert_eq!(responses[0].id, 0);
                assert_eq!(responses[0].value.as_str(), "hunter2");
            }
            other => panic!("类型不对: {other:?}"),
        }
    }

    #[test]
    fn 线格式使用_type_判别字段() {
        let frame = encode(&ToHelper::CloseSession).unwrap();
        assert_eq!(&frame[FRAME_HEADER_LEN..], br#"{"type":"close_session"}"#);
        let frame = encode(&FromHelper::SessionClosed).unwrap();
        assert_eq!(&frame[FRAME_HEADER_LEN..], br#"{"type":"session_closed"}"#);
        let frame = encode(&ToHelper::SpawnWorker {
            open_session: true,
            as_root: false,
        })
        .unwrap();
        assert_eq!(
            &frame[FRAME_HEADER_LEN..],
            br#"{"type":"spawn_worker","open_session":true,"as_root":false}"#
        );
    }

    #[test]
    fn 订阅消息往返且_end_省略_error() {
        let sub = ToWorker::Subscribe {
            id: 3,
            channel: crate::rpc::LOG_FOLLOW.into(),
            params: serde_json::json!({ "unit": "nginx.service" }),
        };
        let frame = encode(&sub).unwrap();
        assert_eq!(decode::<ToWorker>(&frame[FRAME_HEADER_LEN..]).unwrap(), sub);

        let frame = encode(&ToWorker::Unsubscribe { id: 3 }).unwrap();
        assert_eq!(&frame[FRAME_HEADER_LEN..], br#"{"type":"unsubscribe","id":3}"#);

        let ev = FromWorker::Event {
            id: 3,
            data: serde_json::json!([{ "message": "hi" }]),
        };
        assert_eq!(decode::<FromWorker>(&encode(&ev).unwrap()[FRAME_HEADER_LEN..]).unwrap(), ev);

        // 正常结束不带 error 字段，省得每帧都拖一个 null
        let frame = encode(&FromWorker::End { id: 3, error: None }).unwrap();
        assert_eq!(&frame[FRAME_HEADER_LEN..], br#"{"type":"end","id":3}"#);
        assert_eq!(
            decode::<FromWorker>(&frame[FRAME_HEADER_LEN..]).unwrap(),
            FromWorker::End { id: 3, error: None }
        );

        let failed = FromWorker::End {
            id: 3,
            error: Some(ApiError::not_found("没有这个频道")),
        };
        assert_eq!(
            decode::<FromWorker>(&encode(&failed).unwrap()[FRAME_HEADER_LEN..]).unwrap(),
            failed
        );
    }

    #[test]
    fn worker_消息往返() {
        let err = FromWorker::Error {
            id: 7,
            error: ApiError::not_found("没有这个方法"),
        };
        let frame = encode(&err).unwrap();
        let back: FromWorker = decode(&frame[FRAME_HEADER_LEN..]).unwrap();
        assert_eq!(back, err);

        let who = WhoAmI {
            pid: 1,
            uid: 1000,
            euid: 1000,
            gid: 1000,
            egid: 1000,
            groups: vec![1000, 27],
            cwd: "/home/alice".into(),
            user: Some("alice".into()),
            home: Some("/home/alice".into()),
        };
        let v = serde_json::to_value(&who).unwrap();
        let back: WhoAmI = serde_json::from_value(v).unwrap();
        assert_eq!(back, who);
    }
}
