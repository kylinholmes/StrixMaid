//! 进程列表与信号（`docs/design.md` §9.1「进程」组）。
//!
//! 数据全部来自 `/proc`（`procfs` crate），不依赖任何守护进程。
//!
//! # 权限
//!
//! 读进程列表几乎不需要权限，但 `cwd` / `exe` / `environ` / `fd` 只有**同 uid 或 root**
//! 才能读——因此这些字段在 [`ProcessDetail`] 里全是 `Option`，为 `None` 时前端应显示
//! 「需要管理访问」而不是「无」。发信号同理由内核裁决，失败返回
//! [`crate::ErrorCode::PermissionDenied`]。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// 进程状态（`/proc/<pid>/stat` 的第 3 个字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// `R`，可运行或正在运行。
    Running,
    /// `S`，可中断睡眠（绝大多数进程的常态）。
    Sleeping,
    /// `D`，不可中断睡眠（等 IO）。大量 `D` 通常意味着存储出了问题。
    DiskSleep,
    /// `Z`，僵尸，等待父进程 wait。
    Zombie,
    /// `T`，被信号停止。
    Stopped,
    /// `t`，被调试器停止。
    TracingStop,
    /// `X`，已死（极少能被观察到）。
    Dead,
    /// `I`，空闲内核线程。
    Idle,
    /// 其它/未知状态字符。
    #[serde(other)]
    Unknown,
}

/// `GET /api/v1/processes` 的列表项。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProcessSummary {
    /// 进程 id。
    #[schema(example = 1234)]
    pub pid: u32,
    /// 父进程 id。前端用 `pid`/`ppid` 自行拼树（响应本身始终是平铺数组）。
    #[schema(example = 1)]
    pub ppid: u32,
    /// 短名（`/proc/<pid>/comm`），最长 15 字节，会被内核截断。
    #[schema(example = "nginx")]
    pub name: String,
    /// 完整命令行，参数间以空格连接（原始的 NUL 分隔已展开）。
    /// 内核线程无 cmdline，为 `None`——这是区分内核线程与用户进程的可靠依据。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "nginx: worker process")]
    pub cmdline: Option<String>,
    /// 实际 uid。
    #[schema(example = 33)]
    pub uid: u32,
    /// uid 对应的用户名。静态 musl 下 NSS 不可用时可能解析不出，为 `None`
    /// （见 `docs/design.md` §10：NSS 代理是 helper 的 P1 职责）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "www-data")]
    pub user: Option<String>,
    /// 进程状态。
    pub state: ProcessState,
    /// 最近一个采样窗口内的 CPU 占用，**百分比**。单核跑满 = 100.0，多核可超过 100.0。
    ///
    /// 需要两次采样求差，**首次请求可能为 0.0**（还没有前一次样本）。
    #[schema(example = 12.5)]
    pub cpu_percent: f64,
    /// 常驻内存，字节（RSS）。共享页会被重复计入多个进程，**不要求和**。
    #[schema(example = 268_435_456_u64)]
    pub rss_bytes: u64,
    /// 虚拟内存，字节（VSZ）。含未映射实页的地址空间，通常远大于实际占用，仅供参考。
    #[schema(example = 1_073_741_824_u64)]
    pub vms_bytes: u64,
    /// 内存占用比例，**百分比**，= `rss_bytes / MemTotal × 100`。
    #[schema(example = 0.4)]
    pub mem_percent: f64,
    /// 线程数。
    #[schema(example = 4)]
    pub threads: u32,
    /// 进程启动时刻，unix 秒（由 `/proc/<pid>/stat` 的 `starttime` + boot 时间换算）。
    pub start_ts: i64,
    /// nice 值，范围 -20..=19。越小优先级越高。
    #[schema(example = 0)]
    pub nice: i32,
}

/// `GET /api/v1/processes/{pid}` 的响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProcessDetail {
    /// 列表里已有的那部分字段，JSON 中平铺。
    #[serde(flatten)]
    pub summary: ProcessSummary,
    /// 命令行参数数组（未拼接的原始形式，`argv[0]` 在首位）。内核线程为空数组。
    #[serde(default)]
    pub cmdline_args: Vec<String>,
    /// 可执行文件真实路径（`/proc/<pid>/exe` 的软链目标）。
    /// **无权限或内核线程时为 `None`**；文件已被删除时路径会带 `" (deleted)"` 后缀。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/usr/sbin/nginx")]
    pub exe: Option<String>,
    /// 当前工作目录。无权限时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 有效 uid（`/proc/<pid>/status` 的 `Uid` 第二列）。setuid 程序会与 `summary.uid` 不同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub euid: Option<u32>,
    /// 实际 gid。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gid: Option<u32>,
    /// 控制终端（`/proc/<pid>/stat` 的 `tty_nr` 解出的设备名，如 `pts/0`）。无终端为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    /// cgroup 路径（`/proc/<pid>/cgroup` 的 v2 行）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "/system.slice/nginx.service")]
    pub cgroup: Option<String>,
    /// **所属 systemd unit**，由 `cgroup` 路径反查得出（`docs/design.md` §13 步骤 16）。
    /// 不在任何 unit 下（内核线程、手工 `nohup`）时为 `None`。有值时前端应链到服务详情页。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "nginx.service")]
    pub unit: Option<String>,
    /// 环境变量（⭕ 可选项）。
    ///
    /// **需要同 uid 或 root** 才能读，无权限时为 `None`（而不是空 map）。
    /// 环境变量里常含密钥，前端展示前应默认折叠。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub environ: Option<BTreeMap<String, String>>,
    /// 打开的文件描述符（⭕ 可选项）。无权限时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fds: Option<Vec<FdInfo>>,
    /// 累计读取字节数（`/proc/<pid>/io` 的 `read_bytes`）。无权限时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_read_bytes: Option<u64>,
    /// 累计写入字节数（`write_bytes`）。无权限时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub io_write_bytes: Option<u64>,
}

/// 一个打开的文件描述符。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FdInfo {
    /// fd 编号。
    #[schema(example = 3)]
    pub fd: u32,
    /// 软链目标：文件路径，或 `socket:[12345]` / `pipe:[678]` / `anon_inode:[eventpoll]`。
    #[schema(example = "/var/log/nginx/access.log")]
    pub target: String,
    /// 归类后的类型：`file` / `socket` / `pipe` / `anon_inode` / `dir` / `other`。
    #[schema(example = "file")]
    pub kind: String,
}

/// 允许发送的信号。
///
/// 只开放 `docs/design.md` §9.1 明确列出的三个——不做通用 `kill(2)` 网关，
/// 避免变成任意信号注入的工具。
///
/// 序列化输出为小写（全局约定），同时以 `alias` 接受 design.md 里写的大写形式，
/// 两种拼写都能反序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SignalName {
    /// `SIGTERM`(15)，请求优雅退出。**默认选项。**
    #[serde(alias = "TERM", alias = "SIGTERM")]
    Term,
    /// `SIGKILL`(9)，强杀，进程没有清理机会。前端必须二次确认。
    #[serde(alias = "KILL", alias = "SIGKILL")]
    Kill,
    /// `SIGHUP`(1)，多数守护进程约定为「重载配置」。
    #[serde(alias = "HUP", alias = "SIGHUP")]
    Hup,
}

impl SignalName {
    /// 对应的信号编号（Linux）。
    pub const fn as_i32(self) -> i32 {
        match self {
            Self::Hup => 1,
            Self::Kill => 9,
            Self::Term => 15,
        }
    }
}

/// `POST /api/v1/processes/{pid}/signal` 的请求体。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SignalReq {
    /// 要发送的信号。
    pub signal: SignalName,
}

/// `POST /api/v1/processes/{pid}/renice` 的请求体（⭕ 可选项）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ReniceReq {
    /// 目标 nice 值，范围 -20..=19。**调低（更高优先级）需要 root**，
    /// 非特权用户只能调高，否则返回 [`crate::ErrorCode::PermissionDenied`]。
    #[schema(example = 10)]
    pub nice: i32,
}

/// 进程列表的排序字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProcessSortKey {
    /// 按 pid。
    Pid,
    /// 按短名字典序。
    Name,
    /// 按 CPU 占用。**默认。**
    #[default]
    Cpu,
    /// 按 RSS。
    Mem,
    /// 按用户名。
    User,
    /// 按启动时刻。
    StartTs,
}

/// 排序方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SortOrder {
    /// 升序。
    Asc,
    /// 降序。数值型排序的默认（先看最占资源的）。
    #[default]
    Desc,
}

/// `GET /api/v1/processes` 的查询参数。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ProcessListQuery {
    /// 排序字段，缺省 [`ProcessSortKey::Cpu`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<ProcessSortKey>,
    /// 排序方向，缺省 [`SortOrder::Desc`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<SortOrder>,
    /// 只看某个用户的进程。可传用户名或数字 uid。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "www-data")]
    pub user: Option<String>,
    /// 关键字，对 `name` 与 `cmdline` 做大小写不敏感的子串匹配。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "nginx")]
    pub q: Option<String>,
    /// 树视图。**只影响筛选与排序语义，不改变响应形状**——响应始终是平铺的
    /// [`ProcessSummary`] 数组，树由前端按 `ppid` 拼。
    ///
    /// 为 `true` 时服务端保证：凡命中 `q` / `user` 的进程，其**全部祖先**也一并返回
    /// （否则树会断链），并按父子关系做深度优先排序。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree: Option<bool>,
}
