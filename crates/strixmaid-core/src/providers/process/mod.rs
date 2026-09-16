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
//! | [`linux`] | `/proc`（`procfs` crate） | |
//! | [`macos`] | `libproc`（`proc_listpids` / `proc_pidinfo`） | 无 cgroup，`unit` 恒为 `None` |
//! | [`windows`] | `NtQuerySystemInformation(SystemProcessInformation)` | 一次调用取全表；`cgroup` 恒为 `None`，`unit` 由服务的 pid 反查 |
//!
//! 三个后端产出同一套 DTO（`strixmaid_types::process`），差异由字段的 `Option` 表达，
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
pub mod io;
pub mod users;

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod tty;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
/// 信号与优先级的 POSIX 实现，Linux 与 macOS 共用。
///
/// 放在这里而不是各自的后端文件里：`kill(2)` 与 `setpriority(2)` 是 POSIX 的
/// 东西，两个后端一个字都不会写得不一样，复制两份只会多一处可以走样的地方。
#[cfg(unix)]
pub mod posix;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(target_os = "macos")]
use macos as sys;
#[cfg(windows)]
use windows as sys;

/// 「发信号 / 改优先级」这两个写操作的平台实现。
///
/// 与 [`sys`]（枚举进程的后端）分开：枚举方式是每个平台各写一套的，
/// 而这两个写操作在 Linux 与 macOS 上完全相同。
#[cfg(unix)]
use posix as signals;
#[cfg(windows)]
use windows as signals;

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use strixmaid_types::process::{
    ProcessDetail, ProcessListQuery, ProcessSummary, SignalName,
};
use strixmaid_types::{ApiError, ApiResult};

use super::{Probe, Provider};
use cpu::CpuSamples;
use io::IoSamples;
use users::{UserDb, UserTable};

/// 进程 provider。内部是 `Arc`，`Clone` 廉价，便于丢进 `spawn_blocking`。
#[derive(Clone)]
pub struct ProcProvider {
    inner: Arc<Inner>,
}

struct Inner {
    /// CPU 与 IO 的差分基线放同一把锁下:一轮列表对两者的读改是同一临界区。
    samples: Mutex<(CpuSamples, IoSamples)>,
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
                samples: Mutex::new((CpuSamples::new(), IoSamples::new())),
                users: UserDb::new(),
                sys: sys::Backend::new(),
            }),
        }
    }

    /// 是否已有一轮 CPU 快照（否则下一次列表的 CPU% 全为 0）。
    pub fn has_baseline(&self) -> bool {
        self.inner
            .samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .0
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
            let mut guard = self.inner.samples.lock().unwrap_or_else(|e| e.into_inner());
            let (cpu, io) = &mut *guard;
            self.inner.sys.list(cpu, io, &ctx)
        };
        filter::apply(all, query, |name| ctx.users.uid_of(name))
    }

    /// 同步版详情。
    pub fn detail_blocking(&self, pid: u32) -> ApiResult<ProcessDetail> {
        let raw_pid = checked_pid(pid)?;
        let ctx = self.context();
        let mut guard = self.inner.samples.lock().unwrap_or_else(|e| e.into_inner());
        let (cpu, io) = &mut *guard;
        self.inner.sys.detail(raw_pid, cpu, io, &ctx)
    }

    /// `POST /processes/{pid}/signal`。
    ///
    /// Unix 上是 `kill(2)`；Windows 没有信号，映射见
    /// [`windows::send_signal`](sys::send_signal)。
    pub fn signal(&self, pid: u32, signal: SignalName) -> ApiResult<()> {
        let raw_pid = checked_pid(pid)?;
        if raw_pid == 1 {
            return Err(ApiError::invalid_request("不允许向 PID 1（init）发送信号"));
        }
        signals::send_signal(pid, raw_pid, signal)
    }

    /// `POST /processes/{pid}/renice`。
    ///
    /// Unix 上是 `setpriority(2)`；Windows 上映射成优先级类，见
    /// [`windows::set_nice`](sys::set_nice)。
    pub fn renice(&self, pid: u32, nice: i32) -> ApiResult<()> {
        if !(-20..=19).contains(&nice) {
            return Err(ApiError::invalid_request("nice 值必须在 -20..=19 之间"));
        }
        let raw_pid = checked_pid(pid)?;
        signals::set_nice(pid, raw_pid, nice)
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
        // euid 的第二来源按平台不同：Unix 上是 `geteuid(2)`；Windows 没有
        // 「有效用户」这一层，一个进程只有一张主令牌，因此 euid 恒等于 uid
        // （详见 `providers/process/windows.rs` 的同名断言）。
        #[cfg(unix)]
        // SAFETY: geteuid 无副作用。
        assert_eq!(d.euid, Some(unsafe { libc::geteuid() }));
        #[cfg(windows)]
        assert_eq!(d.euid, Some(d.summary.uid), "Windows 只有一张主令牌");
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

    #[cfg(unix)]
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

    /// 上一条在 Windows 上的等价物。
    ///
    /// pid 1 在 Windows 上不是 init，也不保证存在；这里要证明的那件事
    /// ——「权限不够时如实报 `PermissionDenied` 且标明可提权重试」——对应的是
    /// 「普通令牌打不开 LocalSystem 名下的进程」（`OpenProcess` 报
    /// `ERROR_ACCESS_DENIED`，见 `windows::write_error`）。
    ///
    /// 令牌已提升时跳过：那时 `SetPriorityClass` 会真的成功，
    /// 在开发机上把某个系统进程的优先级类改掉，测试不该留下这种副作用。
    #[cfg(windows)]
    #[test]
    fn 非管理员无法_renice_系统账户的进程() {
        use strixmaid_types::process::ProcessListQuery;

        if crate::platform::windows::token::is_elevated() {
            eprintln!("当前令牌已提升，跳过：此时这一步会真的改掉系统进程的优先级类");
            return;
        }
        let provider = ProcProvider::new();
        // uid 0 = LocalSystem；也包含「打不开因而身份未知」的进程，
        // 那些正是本用例要的对象。0 与 4 由 `reject_system_pid` 另行挡下。
        let Some(victim) = provider
            .list_blocking(&ProcessListQuery::default())
            .into_iter()
            .find(|p| p.uid == 0 && p.pid != 0 && p.pid != 4)
        else {
            eprintln!("列表里没有 LocalSystem 名下的进程，跳过");
            return;
        };
        let err = provider.renice(victim.pid, 10).unwrap_err();
        if err.code == ErrorCode::NotFound {
            eprintln!("进程 {} 在两次调用之间退出了，跳过", victim.pid);
            return;
        }
        assert_eq!(err.code, ErrorCode::PermissionDenied, "{err:?}");
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
