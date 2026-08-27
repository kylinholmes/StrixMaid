//! journald 日志查询（`docs/design.md` §9.1「日志」组）。
//!
//! 数据源是 `journalctl -o json` 子进程（`docs/design.md` §4：libsystemd FFI 会毁掉静态构建）。
//!
//! # 分页模型
//!
//! journald 的游标（`__CURSOR`）是不透明字符串，**唯一且可定位**，比 offset 可靠得多
//! （日志会滚动淘汰，offset 会漂移）。因此列表接口一律用游标翻页，前端配合虚拟滚动。
//!
//! # 权限
//!
//! 能看到多少日志由 **journald ACL** 裁决，不由本程序裁决（`docs/design.md` §1 原则 3）。
//! 不在 `systemd-journal` / `adm` 组的用户只能看到自己的条目，且**不会报错**——
//! 所以前端必须依据 [`crate::capability::UserCapabilities::can_read_journal`] 明确提示，
//! 否则用户会以为日志丢了。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// syslog 优先级，取值与数字 0–7 一一对应（0 最严重）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogPriority {
    /// 0，系统不可用。
    Emerg,
    /// 1，需立即处理。
    Alert,
    /// 2，严重错误。
    Crit,
    /// 3，错误。
    Err,
    /// 4，警告。
    Warning,
    /// 5，正常但值得注意。
    Notice,
    /// 6，一般信息。
    Info,
    /// 7，调试。
    Debug,
}

impl LogPriority {
    /// 对应的 syslog 数字（0–7）。
    pub const fn as_u8(self) -> u8 {
        match self {
            Self::Emerg => 0,
            Self::Alert => 1,
            Self::Crit => 2,
            Self::Err => 3,
            Self::Warning => 4,
            Self::Notice => 5,
            Self::Info => 6,
            Self::Debug => 7,
        }
    }

    /// 从 syslog 数字构造；超出 0–7 返回 `None`。
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Emerg,
            1 => Self::Alert,
            2 => Self::Crit,
            3 => Self::Err,
            4 => Self::Warning,
            5 => Self::Notice,
            6 => Self::Info,
            7 => Self::Debug,
            _ => return None,
        })
    }
}

/// `GET /api/v1/logs` 的查询参数。
///
/// 除 `cursor` / `limit` 外的条件构成一个**过滤器**；翻页时必须原样带上，
/// 否则同一个游标在不同过滤器下的「下一页」不一致。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct LogQuery {
    /// **最低**优先级：只返回严重程度 >= 该值的条目（即数字 <= `priority.as_u8()`）。
    /// 传 `warning` 会同时返回 `emerg`..`warning`。缺省不过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "warning")]
    pub priority: Option<LogPriority>,
    /// 起始时刻（含），unix 秒。缺省不设下界。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = 1_756_252_800_i64)]
    pub since: Option<i64>,
    /// 结束时刻（含），unix 秒。缺省不设上界。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// 按 unit 过滤，需完整 unit 名（对应 `journalctl -u`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "nginx.service")]
    pub unit: Option<String>,
    /// 按 boot 过滤。取值为 [`BootInfo::boot_id`]（32 位 hex），或相对偏移
    /// （`"0"` = 本次启动，`"-1"` = 上一次）。缺省不限制 boot。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "0")]
    pub boot: Option<String>,
    /// 全文关键字，对 `MESSAGE` 做匹配（对应 `journalctl -g`，大小写不敏感）。
    /// 旧版 journalctl 无 `-g` 时由服务端在读出后自行过滤。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "connection refused")]
    pub q: Option<String>,
    /// 翻页游标：从**上一页返回的游标**继续，方向为**由新到旧**（journald 默认的 tail 方向）。
    /// 缺省从最新一条开始。游标本身对应的条目**不重复返回**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// 每页条数。缺省 200，上限由服务端限制（建议 1000）；越界返回
    /// [`crate::ErrorCode::InvalidRequest`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = 200)]
    pub limit: Option<u32>,
}

/// 一条日志（列表视图所需的字段）。
///
/// # 时间精度
///
/// journald 的 `__REALTIME_TIMESTAMP` 是**微秒级 epoch**，这里拆成两个字段：
/// [`Self::ts`]（unix 秒，遵循全局约定）+ [`Self::us`]（秒内微秒偏移）。
///
/// 日志是全局「时间戳一律 i64 秒」约定的**唯一例外**：一个服务吐 stack trace 时
/// 同一秒内几十条是常态，只有秒精度的话时间列会显示成一片相同的值。
///
/// `us` **只服务于展示**；排序与定位仍然靠 [`Self::cursor`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    /// journald 游标（`__CURSOR`），不透明字符串。用于翻页、以及
    /// `GET /api/v1/logs/entry/{cursor}` 取全字段详情（作为路径参数需 URL 编码）。
    #[schema(example = "s=1a2b;i=3c4d;b=5e6f;m=7089;t=90ab;x=cdef")]
    pub cursor: String,
    /// 记录时刻，unix 秒（`__REALTIME_TIMESTAMP / 1_000_000`）。
    #[schema(example = 1_756_252_800_i64)]
    pub ts: i64,
    /// 秒内微秒偏移，范围 `0..1_000_000`（`__REALTIME_TIMESTAMP % 1_000_000`）。
    ///
    /// 完整时刻 = `ts` 秒 + `us` 微秒。仅用于展示（毫秒/微秒列），
    /// **不要拿它排序**——同一微秒内仍可能有多条，顺序以数组顺序与 `cursor` 为准。
    #[schema(example = 481_237_u32)]
    pub us: u32,
    /// 优先级。原始记录缺 `PRIORITY` 时按 [`LogPriority::Info`] 归一。
    pub priority: LogPriority,
    /// 消息正文（`MESSAGE`）。二进制消息会被转成有损的可打印形式。
    #[schema(example = "Started A high performance web server.")]
    pub message: String,
    /// 来源 unit（`_SYSTEMD_UNIT`，内核消息回落到 `UNIT`）。内核与早期用户态消息为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "nginx.service")]
    pub unit: Option<String>,
    /// syslog 标识（`SYSLOG_IDENTIFIER`），通常是可执行文件名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "nginx")]
    pub identifier: Option<String>,
    /// 产生消息的进程 pid（`_PID`）。内核消息为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 产生消息的进程 uid（`_UID`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<u32>,
    /// 主机名（`_HOSTNAME`）。多节点汇聚时用于区分来源。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// 所属 boot id（`_BOOT_ID`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_id: Option<String>,
    /// 传输方式（`_TRANSPORT`）：`journal` / `syslog` / `kernel` / `stdout` / `audit`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "stdout")]
    pub transport: Option<String>,
}

/// `GET /api/v1/logs/entry/{cursor}` 的响应体：单条日志的**全字段**详情。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogEntryDetail {
    /// 已结构化的常用字段，JSON 中平铺。
    #[serde(flatten)]
    pub entry: LogEntry,
    /// journald 的**全部**原始字段，键为 `_PID` / `_CMDLINE` / `CODE_FILE` 等。
    /// 二进制值会被跳过或做有损转换。键集合完全取决于写日志的程序，前端按 k-v 表格展示即可。
    #[serde(default)]
    #[schema(value_type = Object)]
    pub fields: BTreeMap<String, String>,
}

/// `GET /api/v1/logs` 的响应体：一页日志 + 游标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogPage {
    /// 本页条目，**由新到旧**排序。
    #[serde(default)]
    pub entries: Vec<LogEntry>,
    /// 继续向**更旧**方向翻页的游标（= 本页最后一条的 `cursor`）。
    /// 为 `None` 表示已到最旧一条。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// 向**更新**方向翻页的游标（= 本页第一条的 `cursor`）。
    /// 为 `None` 表示本页为空。用于「回到更新的日志」与增量刷新。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_cursor: Option<String>,
}

/// `GET /api/v1/logs/boots` 的列表项（对应 `journalctl --list-boots`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BootInfo {
    /// 相对序号：`0` = 本次启动，`-1` = 上一次，依此类推。可直接作为 [`LogQuery::boot`] 的值。
    #[schema(example = 0)]
    pub index: i32,
    /// 32 位 hex 的 boot id。跨重启稳定，比 `index` 可靠（`index` 会随新启动而整体平移）。
    #[schema(example = "4d3a2b1c5e6f70819a2b3c4d5e6f7081")]
    pub boot_id: String,
    /// 该 boot 内第一条日志的时刻。
    pub first_ts: i64,
    /// 该 boot 内最后一条日志的时刻。对当前 boot 即「刚刚」。
    pub last_ts: i64,
}
