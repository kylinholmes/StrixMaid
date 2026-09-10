//! LogProvider：日志查询 / 单条详情 / boot 列表 / follow 流。
//!
//! 按平台各有一个实现，都是子进程：
//!
//! - Linux：[`journalctl::Journalctl`]，`journalctl -o json`
//!   （`docs/design.md` §4：libsystemd FFI 会毁掉静态构建）；
//! - macOS：[`oslog::OsLog`]，`log show --style ndjson` 与 `log stream`。
//!
//! 非 journald 的 Linux 系统（`/var/log/*.log`）**不实现**，
//! 只留 [`FileLogs`] 空壳证明 trait 容得下它。
//!
//! # 输出量控制
//!
//! 单次查询 `limit` 缺省 [`DEFAULT_LIMIT`]、上限 [`MAX_LIMIT`]；子进程 stdout 用
//! `BufReader::lines` 流式解析，读够就停，绝不把整个输出读进内存。
//!
//! # follow
//!
//! `journalctl -f` 是常驻子进程。同一过滤条件的订阅共享一个子进程；最后一个订阅者
//! [`LogFollow`] drop 时子进程被 kill。

#[cfg(target_os = "linux")]
pub mod journalctl;
#[cfg(target_os = "linux")]
pub mod parse;

#[cfg(target_os = "macos")]
pub mod oslog;

use std::sync::Arc;

use async_trait::async_trait;
use strixmaid_types::log::{
    BootInfo, LogEntry, LogEntryDetail, LogPage, LogQuery, LogUsage, VacuumReq, VacuumResp,
};
use strixmaid_types::{ApiError, ApiResult};
use tokio::sync::broadcast;

use super::{Probe, Provider};

/// `limit` 缺省值。
pub const DEFAULT_LIMIT: u32 = 100;
/// `limit` 上限，越界返回 `InvalidRequest`。
pub const MAX_LIMIT: u32 = 1000;

/// 一个 follow 订阅。drop 即退订；所有订阅者都退了，子进程随之结束。
pub struct LogFollow {
    rx: broadcast::Receiver<Arc<Vec<LogEntry>>>,
    /// 持有共享子进程的引用计数，类型擦除以免 trait 依赖具体实现。
    _lease: Box<dyn std::any::Any + Send + Sync>,
}

impl std::fmt::Debug for LogFollow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogFollow").finish_non_exhaustive()
    }
}

impl LogFollow {
    /// 由实现构造。
    pub fn new(
        rx: broadcast::Receiver<Arc<Vec<LogEntry>>>,
        lease: Box<dyn std::any::Any + Send + Sync>,
    ) -> Self {
        Self { rx, _lease: lease }
    }

    /// 下一批条目（按时间先后）。返回 `None` 表示流已结束（子进程退出）。
    /// 消费太慢被 `Lagged` 时丢掉旧批次继续——follow 只保证「最新」，不保证「不漏」。
    pub async fn next(&mut self) -> Option<Arc<Vec<LogEntry>>> {
        loop {
            match self.rx.recv().await {
                Ok(batch) => return Some(batch),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "logs.follow 消费过慢，丢弃旧批次");
                }
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    }
}

/// 日志能力。
#[async_trait]
pub trait LogProvider: Provider {
    /// 按过滤条件查一页，由新到旧。
    async fn query(&self, q: &LogQuery) -> ApiResult<LogPage>;

    /// 单条全字段详情。游标对不上任何条目时 `NotFound`。
    async fn entry(&self, cursor: &str) -> ApiResult<LogEntryDetail>;

    /// boot 列表，按 `index` 升序（最旧在前，`0` 在最后）。
    async fn boots(&self) -> ApiResult<Vec<BootInfo>>;

    /// 从「现在」开始跟随。`q.cursor` / `q.limit` / `q.since` / `q.until` 被忽略。
    async fn follow(&self, q: &LogQuery) -> ApiResult<LogFollow>;

    /// 磁盘占用与本机支持的清理方式。占用测不到时 `bytes` 为 `None`，不报错。
    async fn usage(&self) -> ApiResult<LogUsage>;

    /// 清理日志。请求必须恰好指定一种方式（[`validate_vacuum`]），
    /// 且该方式在 [`LogUsage::modes`] 里——否则 `InvalidRequest`。
    /// 权限由底层工具裁决（journald 文件属主 / macOS root），被拒映射
    /// `PermissionDenied` + `can_retry_elevated`。
    async fn vacuum(&self, req: &VacuumReq) -> ApiResult<VacuumResp>;
}

/// 校验清理请求：三个字段恰好给一个。返回给了哪一个。
pub fn validate_vacuum(req: &VacuumReq) -> ApiResult<strixmaid_types::log::VacuumMode> {
    use strixmaid_types::log::VacuumMode;
    let picked = [
        req.keep_secs.map(|_| VacuumMode::KeepDuration),
        req.max_bytes.map(|_| VacuumMode::MaxSize),
        // erase_all: false 视同没给,不许拿 false 当占位
        req.erase_all.filter(|v| *v).map(|_| VacuumMode::EraseAll),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    match picked.as_slice() {
        [one] => Ok(*one),
        [] => Err(ApiError::invalid_request(
            "keep_secs / max_bytes / erase_all 必须恰好给一个",
        )),
        _ => Err(ApiError::invalid_request(
            "keep_secs / max_bytes / erase_all 只能给一个",
        )),
    }
}

/// 解析 `journalctl` 输出里的人类可读容量（`56.0M` / `1.5G` / `823B`），二进制底。
/// 认不出返回 `None`——上层把「测不到」如实传下去，不编数字。
pub fn parse_human_size(token: &str) -> Option<u64> {
    let t = token.trim().trim_end_matches('B');
    let (num, mult) = match t.chars().last()? {
        'K' => (&t[..t.len() - 1], 1024f64),
        'M' => (&t[..t.len() - 1], 1024f64 * 1024.0),
        'G' => (&t[..t.len() - 1], 1024f64 * 1024.0 * 1024.0),
        'T' => (&t[..t.len() - 1], 1024f64 * 1024.0 * 1024.0 * 1024.0),
        c if c.is_ascii_digit() || c == '.' => (t, 1.0),
        _ => return None,
    };
    let v: f64 = num.trim().parse().ok()?;
    (v >= 0.0).then_some((v * mult) as u64)
}

/// 归一化 `limit`：缺省 [`DEFAULT_LIMIT`]，`0` 或超过 [`MAX_LIMIT`] 报 400。
pub fn normalize_limit(limit: Option<u32>) -> ApiResult<usize> {
    match limit {
        None => Ok(DEFAULT_LIMIT as usize),
        Some(0) => Err(ApiError::invalid_request("limit 不能为 0")),
        Some(n) if n > MAX_LIMIT => Err(ApiError::invalid_request(format!(
            "limit 超过上限 {MAX_LIMIT}"
        ))),
        Some(n) => Ok(n as usize),
    }
}

/// 选择 log provider：`journalctl` 可用则用之，否则 `None`（capabilities 的 `journal` 为 `false`）。
#[cfg(target_os = "linux")]
pub async fn pick_log_provider() -> Option<Arc<dyn LogProvider>> {
    let j = journalctl::Journalctl::new();
    match j.probe().await {
        Probe::Unavailable { reason } => {
            tracing::warn!(reason, "journalctl 不可用，日志能力关闭");
            None
        }
        probe => {
            tracing::info!(?probe, "log provider: journalctl");
            Some(Arc::new(j))
        }
    }
}

/// 选择 log provider：macOS 上是 `log(1)` 统一日志。
#[cfg(target_os = "macos")]
pub async fn pick_log_provider() -> Option<Arc<dyn LogProvider>> {
    let l = oslog::OsLog::new();
    match l.probe().await {
        Probe::Unavailable { reason } => {
            tracing::warn!(reason, "log show 不可用，日志能力关闭");
            None
        }
        probe => {
            tracing::info!(?probe, "log provider: oslog");
            Some(Arc::new(l))
        }
    }
}

/// 非 journald 系统的纯文件日志（`/var/log/*.log`）——**空壳，未实现**。
///
/// 只在 Linux 上编译：macOS 的日志全在统一日志里，没有这条降级路径。
///
/// 留在这里是为了证明 [`LogProvider`] 的接口容得下它：游标可以是 `file:offset`，
/// follow 可以是 inotify。P0 不做（`docs/design.md` §1：除 systemd 外默认什么都没有，
/// 但 journald 本身是 systemd 的一部分）。
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct FileLogs;

#[cfg(target_os = "linux")]
#[async_trait]
impl Provider for FileLogs {
    fn id(&self) -> &'static str {
        "file-logs"
    }

    async fn probe(&self) -> Probe {
        Probe::unavailable("纯文件日志 provider 尚未实现")
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl LogProvider for FileLogs {
    async fn query(&self, _q: &LogQuery) -> ApiResult<LogPage> {
        // todo!: 扫描 /var/log/{syslog,messages}，游标为 "<inode>:<offset>"。
        todo!("FileLogs::query 未实现")
    }

    async fn entry(&self, _cursor: &str) -> ApiResult<LogEntryDetail> {
        // todo!: 按 "<inode>:<offset>" 定位并读一行。
        todo!("FileLogs::entry 未实现")
    }

    async fn boots(&self) -> ApiResult<Vec<BootInfo>> {
        // todo!: 没有 boot 概念，可按 /var/log/wtmp 或 `-- MARK --` 切分。
        todo!("FileLogs::boots 未实现")
    }

    async fn follow(&self, _q: &LogQuery) -> ApiResult<LogFollow> {
        // todo!: inotify 监听追加。
        todo!("FileLogs::follow 未实现")
    }

    async fn usage(&self) -> ApiResult<LogUsage> {
        // todo!: du /var/log/*.log。
        todo!("FileLogs::usage 未实现")
    }

    async fn vacuum(&self, _req: &VacuumReq) -> ApiResult<VacuumResp> {
        // todo!: logrotate 才是正道,这里最多做截断。
        todo!("FileLogs::vacuum 未实现")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 容量解析() {
        assert_eq!(parse_human_size("823B"), Some(823));
        assert_eq!(parse_human_size("56.0M"), Some(58_720_256));
        assert_eq!(parse_human_size("1.5G"), Some(1_610_612_736));
        assert_eq!(parse_human_size("16K"), Some(16_384));
        assert_eq!(parse_human_size("0"), Some(0));
        assert_eq!(parse_human_size("garbage"), None);
        assert_eq!(parse_human_size(""), None);
    }

    #[test]
    fn 清理请求校验() {
        use strixmaid_types::log::VacuumMode;
        let ok = validate_vacuum(&VacuumReq {
            keep_secs: Some(86_400),
            ..Default::default()
        });
        assert_eq!(ok.unwrap(), VacuumMode::KeepDuration);
        assert_eq!(
            validate_vacuum(&VacuumReq {
                erase_all: Some(true),
                ..Default::default()
            })
            .unwrap(),
            VacuumMode::EraseAll
        );
        // 什么都不给 / 给两个 / erase_all=false 都不行
        assert!(validate_vacuum(&VacuumReq::default()).is_err());
        assert!(
            validate_vacuum(&VacuumReq {
                keep_secs: Some(1),
                max_bytes: Some(1),
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            validate_vacuum(&VacuumReq {
                erase_all: Some(false),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn limit_normalization() {
        assert_eq!(normalize_limit(None).unwrap(), DEFAULT_LIMIT as usize);
        assert_eq!(normalize_limit(Some(1000)).unwrap(), 1000);
        assert!(normalize_limit(Some(0)).is_err());
        assert!(normalize_limit(Some(1001)).is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn file_logs_probe_is_unavailable_not_panic() {
        assert!(!FileLogs.probe().await.is_available());
    }
}
