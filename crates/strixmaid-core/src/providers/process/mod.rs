//! process provider（id `"proc"`）：进程列表 / 详情 / 信号 / renice。
//!
//! # 结构
//!
//! 本文件是**与平台无关**的外壳：[`ProcProvider`] 的接口形状、`kill(2)` / `setpriority(2)`
//! 这两个 POSIX 写操作、pid 校验，以及 [`cpu`]（CPU% 差分）、[`filter`]（排序 / 过滤 / 树）、
//! [`users`]（uid → 用户名）三个纯逻辑子模块。
//!
//! 「怎么把进程枚举出来」按平台分：
//!
//! | 模块 | 数据源 | 备注 |
//! |---|---|---|
//! | [`linux`] | `/proc`（`procfs` crate） | 目标平台 |
//! | [`macos`] | `libproc`（`proc_listpids` / `proc_pidinfo`） | 开发平台；无 cgroup，`unit` 恒为 `None` |
//!
//! 两个后端产出同一套 DTO（`strixmaid_types::process`），差异由字段的 `Option` 表达，
//! 不新增平台分支到 API 契约里。
//!
//! # CPU%
//!
//! 差分计算见 [`cpu`]：provider 内部持有上一轮 `(pid, starttime) → ticks` 快照，
//! 每次列表 / 详情都更新它，并清理已消失的 pid。**首次调用没有基线，CPU% 为 0.0**。
//! 两个平台的「tick」单位不同（Linux 是 jiffies，macOS 是纳秒），由各自后端连同
//! 对应的 `hz` 一起交给 [`cpu::CpuSamples::observe`]，差分公式本身是共用的。
//!
//! # 权限
//!
//! 读列表几乎不需要权限；`cwd` / `exe` / `environ` / `fd` 只有同 uid 或 root 能读，
//! 读不到就是 `None`。信号与 renice 由内核裁决，`EPERM` → `PermissionDenied`（可提权重试）。

pub mod cpu;
pub mod filter;
pub mod users;

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod tty;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use strixmaid_types::process::{
    ProcessDetail, ProcessListQuery, ProcessSummary, SignalName,
};
use strixmaid_types::{ApiError, ApiResult};

use super::{Probe, Provider};
use cpu::CpuSamples;
use users::{UserDb, UserTable};

/// 进程 provider。内部是 `Arc`，`Clone` 廉价，便于丢进 `spawn_blocking`。
#[derive(Clone)]
pub struct ProcProvider {
    inner: Arc<Inner>,
}

struct Inner {
    cpu: Mutex<CpuSamples>,
    users: UserDb,
    sys: sys::Backend,
}

impl Default for ProcProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcProvider {
    /// 创建 provider。后端在这里读一次常量（时钟频率、页大小、开机时刻等）。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cpu: Mutex::new(CpuSamples::new()),
                users: UserDb::new(),
                sys: sys::Backend::new(),
            }),
        }
    }

    /// 是否已有一轮 CPU 快照（否则下一次列表的 CPU% 全为 0）。
    pub fn has_baseline(&self) -> bool {
        self.inner
            .cpu
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_baseline()
    }

    /// `GET /processes`。
    pub async fn list(&self, query: ProcessListQuery) -> ApiResult<Vec<ProcessSummary>> {
        let this = self.clone();
        blocking(move || this.list_blocking(&query)).await
    }

    /// `GET /processes/{pid}`。
    pub async fn detail(&self, pid: u32) -> ApiResult<ProcessDetail> {
        let this = self.clone();
        blocking(move || this.detail_blocking(pid)).await?
    }

    /// 同步版列表：枚举进程、更新 CPU 快照、按查询参数筛选排序。
    pub fn list_blocking(&self, query: &ProcessListQuery) -> Vec<ProcessSummary> {
        let ctx = self.context();
        let all = {
            let mut cpu = self.inner.cpu.lock().unwrap_or_else(|e| e.into_inner());
            self.inner.sys.list(&mut cpu, &ctx)
        };
        filter::apply(all, query, |name| ctx.users.uid_of(name))
    }

    /// 同步版详情。
    pub fn detail_blocking(&self, pid: u32) -> ApiResult<ProcessDetail> {
        let raw_pid = checked_pid(pid)?;
        let ctx = self.context();
        let mut cpu = self.inner.cpu.lock().unwrap_or_else(|e| e.into_inner());
        self.inner.sys.detail(raw_pid, &mut cpu, &ctx)
    }

    /// `POST /processes/{pid}/signal`：`kill(2)`。
    pub fn signal(&self, pid: u32, signal: SignalName) -> ApiResult<()> {
        let raw_pid = checked_pid(pid)?;
        if raw_pid == 1 {
            return Err(ApiError::invalid_request("不允许向 PID 1（init）发送信号"));
        }
        let sig = match signal {
            SignalName::Term => Signal::SIGTERM,
            SignalName::Kill => Signal::SIGKILL,
            SignalName::Hup => Signal::SIGHUP,
        };
        kill(Pid::from_raw(raw_pid), sig).map_err(|e| match e {
            Errno::ESRCH => ApiError::not_found(format!("进程 {pid} 不存在")),
            Errno::EPERM => ApiError::permission_denied(format!(
                "内核拒绝向进程 {pid} 发送 {sig}：不是该进程的属主"
            ))
            .with_detail(e.to_string())
            .retry_elevated(),
            other => ApiError::internal(format!("向进程 {pid} 发送 {sig} 失败"))
                .with_detail(other.to_string()),
        })
    }

    /// `POST /processes/{pid}/renice`：`setpriority(2)`。
    pub fn renice(&self, pid: u32, nice: i32) -> ApiResult<()> {
        if !(-20..=19).contains(&nice) {
            return Err(ApiError::invalid_request("nice 值必须在 -20..=19 之间"));
        }
        let raw_pid = checked_pid(pid)?;
        // SAFETY: setpriority 只读参数，无内存副作用。
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, raw_pid as libc::id_t, nice) };
        if rc == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        Err(match e.raw_os_error() {
            Some(libc::ESRCH) => ApiError::not_found(format!("进程 {pid} 不存在")),
            Some(libc::EACCES) | Some(libc::EPERM) => ApiError::permission_denied(format!(
                "内核拒绝调整进程 {pid} 的优先级：调低 nice 值（提高优先级）需要 root，且只能操作自己的进程"
            ))
            .with_detail(e.to_string())
            .retry_elevated(),
            _ => ApiError::internal(format!("调整进程 {pid} 优先级失败")).with_detail(e.to_string()),
        })
    }

    /// 一次列表 / 详情共用的上下文：用户表快照、内存总量、采样时刻。
    fn context(&self) -> Context {
        Context {
            users: self.inner.users.snapshot(),
            mem_total: self.inner.sys.mem_total(),
            now: Instant::now(),
        }
    }
}

#[async_trait]
impl Provider for ProcProvider {
    fn id(&self) -> &'static str {
        "proc"
    }

    async fn probe(&self) -> Probe {
        sys::probe()
    }
}

/// 一次枚举共用的上下文，由外壳构造、交给平台后端。
pub struct Context {
    pub users: Arc<UserTable>,
    /// 物理内存总量，用于算 `mem_percent`；读不到为 0（此时 `mem_percent` 恒为 0）。
    pub mem_total: u64,
    pub now: Instant,
}

impl Context {
    /// RSS 占物理内存的百分比，保留两位小数。
    pub fn mem_percent(&self, rss_bytes: u64) -> f64 {
        if self.mem_total == 0 {
            return 0.0;
        }
        ((rss_bytes as f64 / self.mem_total as f64 * 100.0) * 100.0).round() / 100.0
    }
}

/// 在阻塞线程池里跑一段同步采集。
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal("进程采集任务异常终止").with_detail(e.to_string()))
}

/// pid 必须在 `1..=i32::MAX`：0 与负数对 `kill` / `setpriority` 有「进程组 / 全部进程」的特殊语义，绝不能放过去。
fn checked_pid(pid: u32) -> ApiResult<i32> {
    i32::try_from(pid)
        .ok()
        .filter(|p| *p >= 1)
        .ok_or_else(|| ApiError::invalid_request(format!("非法的 pid：{pid}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::ErrorCode;

    #[test]
    fn pid_校验() {
        assert!(checked_pid(0).is_err());
        assert!(checked_pid(u32::MAX).is_err());
        assert_eq!(checked_pid(1).unwrap(), 1);
    }

    #[test]
    fn 内存百分比() {
        let ctx = Context {
            users: Arc::new(UserTable::default()),
            mem_total: 1000,
            now: Instant::now(),
        };
        assert_eq!(ctx.mem_percent(125), 12.5);
        assert_eq!(ctx.mem_percent(0), 0.0);
        let zero = Context { mem_total: 0, ..ctx };
        assert_eq!(zero.mem_percent(500), 0.0, "读不到 MemTotal 时不该除以 0");
    }

    #[test]
    fn 本机列表_找到自己_且性能可接受() {
        let provider = ProcProvider::new();
        let me = std::process::id();

        let t0 = Instant::now();
        let first = provider.list_blocking(&ProcessListQuery::default());
        let first_elapsed = t0.elapsed();
        assert!(!first.is_empty());
        let mine = first.iter().find(|p| p.pid == me).expect("列表里必须有本进程");
        assert!(mine.cmdline.is_some(), "本进程不是内核线程，必须有 cmdline");
        assert_eq!(mine.cpu_percent, 0.0, "首次调用没有基线");
        assert!(mine.threads >= 1);
        assert!(mine.rss_bytes > 0);
        assert!(mine.start_ts > 0);
        assert!(provider.has_baseline());

        // 第二轮：有基线，CPU% 是差分值（可能仍为 0，但不再是「无基线」）
        std::thread::sleep(cpu::MIN_INTERVAL);
        let t1 = Instant::now();
        let second = provider.list_blocking(&ProcessListQuery::default());
        let second_elapsed = t1.elapsed();
        assert!(second.iter().any(|p| p.pid == me));

        let per_proc = second_elapsed.as_secs_f64() / second.len().max(1) as f64;
        eprintln!(
            "进程列表：{} 个进程，首轮 {:?}，次轮 {:?}（每进程 {:.1}µs）",
            second.len(),
            first_elapsed,
            second_elapsed,
            per_proc * 1e6
        );
        assert!(second_elapsed.as_secs() < 2, "进程列表耗时异常：{second_elapsed:?}");
    }

    #[test]
    fn 本进程详情() {
        let provider = ProcProvider::new();
        let me = std::process::id();
        let d = provider.detail_blocking(me).unwrap();
        assert_eq!(d.summary.pid, me);
        assert!(!d.cmdline_args.is_empty());
        assert!(d.exe.is_some(), "自己的 exe 总该读得到");
        // SAFETY: geteuid 无副作用。
        assert_eq!(d.euid, Some(unsafe { libc::geteuid() }));
    }

    #[test]
    fn 不存在的进程() {
        let provider = ProcProvider::new();
        // Linux 的 pid_max 默认 4194304，macOS 更小；用一个远超两者的合法 i32
        let err = provider.detail_blocking(2_000_000_000).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let err = provider.signal(2_000_000_000, SignalName::Term).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        let err = provider.renice(2_000_000_000, 5).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    #[test]
    fn 参数校验() {
        let provider = ProcProvider::new();
        assert_eq!(
            provider.signal(1, SignalName::Kill).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            provider.signal(0, SignalName::Term).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
        assert_eq!(
            provider.renice(std::process::id(), 40).unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn 非_root_无法_renice_pid_1() {
        // SAFETY: geteuid 无副作用。
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let provider = ProcProvider::new();
        let err = provider.renice(1, 10).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        assert!(err.can_retry_elevated);
    }

    #[tokio::test]
    async fn async_接口与探测() {
        let p = ProcProvider::new();
        assert_eq!(p.id(), "proc");
        assert_eq!(p.probe().await, Probe::Available);
        let list = p
            .list(ProcessListQuery {
                tree: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!list.is_empty());
        let me = std::process::id();
        let d = p.detail(me).await.unwrap();
        assert_eq!(d.summary.pid, me);
    }
}
