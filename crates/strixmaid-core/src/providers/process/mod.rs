//! process provider（id `"proc"`）：进程列表 / 详情 / 信号 / renice。
//!
//! # 结构
//!
//! 本文件是**与平台无关**的外壳：[`ProcProvider`] 的接口形状、`kill(2)` / `setpriority(2)`
//! 这两个 POSIX 写操作、pid 校验，以及 [`cpu`]（CPU% 差分）、[`filter`]（排序 / 过滤 / 树）、
//! [`users`]（uid → 用户名）、[`icon`]（程序图标与其缓存）四个子模块。
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
//!
//! # 图标
//!
//! [`ProcProvider`] 持有一份程序名 → PNG 的缓存（[`icon::IconCache`]），
//! 生命周期随 provider 实例。放在 provider 层而不是路由层，是职责边界的问题：
//! 端点只负责「把字节按 HTTP 语义吐出去」，取图标、解析名字、管生命周期都是
//! provider 的事——端点因此完全不需要知道 GDI 的存在，日后换别的图标来源也
//! 不用动路由。缓存策略与预热调度见 [`icon`] 的模块文档。

pub mod cpu;
pub mod filter;
pub mod icon;
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
    /// 程序名 → 图标 PNG。与 `samples` 各用各的锁：图标那条路要在锁外
    /// `await`，和进程枚举的临界区没有任何交集，共用一把锁只会互相拖住。
    icons: icon::IconCache,
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
                icons: icon::IconCache::new(),
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

    // -----------------------------------------------------------------
    // 图标
    // -----------------------------------------------------------------

    /// `GET /processes/icon/{name}`：按程序名取一张 [`icon::ICON_SIZE`] 见方的 PNG。
    ///
    /// 三类结果各有各的含义，端点直接按它们映射状态码：
    ///
    /// | 情况 | 返回 | HTTP |
    /// |---|---|---|
    /// | 名字不合法（含分隔符 / `..` / 超长） | `invalid_request` | 400 |
    /// | 本平台取不到图标（[`icon::available`] 为假） | `not_found` | 404 |
    /// | 没有同名进程在跑，或取不到它的 exe / 图标 | `not_found` | 404 |
    ///
    /// 非 Windows 平台恒为 404：Linux 上进程与图标没有可靠的对应关系
    /// （图标住在 `.desktop` 与图标主题里，服务器上通常两者都没有），
    /// macOS 的 AppKit 在守护进程上下文里不可靠。见 [`icon`] 的模块文档。
    pub async fn icon_png(&self, name: &str) -> ApiResult<Arc<Vec<u8>>> {
        icon::validate_name(name)?;
        if !icon::available() {
            return Err(ApiError::not_found("本平台无法取得进程图标"));
        }
        self.inner.icons.get(name).await.ok_or_else(|| {
            ApiError::not_found(format!(
                "没有正在运行的 {name}，或者取不到它的可执行文件与图标"
            ))
        })
    }

    /// 起图标的预热与补热后台任务。[`icon::available`] 为假时**一个任务也不起**，
    /// 返回 `None`。
    ///
    /// 任务不阻塞调用方：第一轮预热在后台跑，服务照常立刻开始接受请求。
    /// 之后每 [`icon::REFRESH_INTERVAL`] 补热一轮，间隔与 TTL 的关系见
    /// [`icon`] 的模块文档。
    ///
    /// 返回的句柄没有需要落盘的状态，关停时直接 `abort()` 即可。
    pub fn spawn_icon_warmer(&self) -> Option<tokio::task::JoinHandle<()>> {
        if !icon::available() {
            tracing::debug!("本平台不提供进程图标，不起预热任务");
            return None;
        }
        let this = self.clone();
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(icon::REFRESH_INTERVAL);
            loop {
                // `interval` 的第一个 tick 立即到期：启动后马上预热一轮。
                ticker.tick().await;
                this.warm_icons().await;
            }
        }))
    }

    /// 预热 / 补热一轮：取当前进程表里去重后的程序名，交给缓存。
    async fn warm_icons(&self) {
        let started = Instant::now();
        let this = self.clone();
        let names = match tokio::task::spawn_blocking(move || this.running_names()).await {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!(error = %e, "图标预热：枚举进程失败");
                return;
            }
        };
        let report = self.inner.icons.refresh(names).await;
        tracing::info!(
            names = report.total,
            extracted = report.attempted,
            ok = report.succeeded,
            failed = report.failed,
            cached = self.inner.icons.len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "图标预热完成"
        );
    }

    /// 当前进程表里**去重后**的程序名。
    ///
    /// 几百个进程通常只有几十个不同的名字（十几个标签页的浏览器就是十几个同名
    /// 进程），去重之后才是真正要提取的次数。
    ///
    /// 同时滤掉不合法的名字：内核给出的映像名不该出现分隔符，真出现了也不该
    /// 拿去当文件名用——这里与端点走同一个判据，不另开一套。
    ///
    /// 走 [`Self::list_blocking`] 而不是直接读平台后端，是为了让这个函数保持
    /// 平台无关。副作用是它会推进本实例的 CPU / IO 差分基线——这不影响任何人：
    /// 持有图标缓存的是**主进程**的 provider 实例，而 CPU% 是从**会话 worker**
    /// 里的另一个实例读出来的（见 `routes/processes.rs` 的模块文档）。
    fn running_names(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        self.list_blocking(&ProcessListQuery::default())
            .into_iter()
            .map(|p| p.name)
            .filter(|name| icon::validate_name(name).is_ok())
            .filter(|name| seen.insert(name.clone()))
            .collect()
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
    fn 去重后的程序名远少于进程数() {
        let provider = ProcProvider::new();
        let all = provider.list_blocking(&ProcessListQuery::default());
        let names = provider.running_names();
        assert!(!names.is_empty(), "至少该有本进程自己");
        assert!(
            names.len() <= all.len(),
            "去重后不可能比进程数还多：{} > {}",
            names.len(),
            all.len()
        );
        // 名字全部通过端点那一套消毒，预热与按需走的是同一个判据。
        assert!(names.iter().all(|n| icon::validate_name(n).is_ok()));
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "去重没做干净");
        eprintln!(
            "进程 {} 个，去重后程序名 {} 个",
            all.len(),
            names.len()
        );
    }

    #[tokio::test]
    async fn 图标端点的名字消毒走到了_400() {
        let provider = ProcProvider::new();
        for bad in ["", "../etc/passwd", "C:\\Windows\\explorer.exe", "a/b"] {
            let err = provider.icon_png(bad).await.expect_err("{bad} 应当被拒绝");
            assert_eq!(err.code, ErrorCode::InvalidRequest, "{bad} 的错误码不对");
        }
    }

    #[tokio::test]
    async fn 取不到图标时是_404_而不是报错() {
        let provider = ProcProvider::new();
        let err = provider
            .icon_png("绝不会有这个进程_9f3a1c.exe")
            .await
            .expect_err("不存在的名字应当取不到");
        assert_eq!(err.code, ErrorCode::NotFound);
    }

    /// 真机取一次图标：本平台不支持（macOS / Linux 的空实现）时跳过，不失败。
    #[tokio::test]
    async fn 真机取一张图标并检查是合法_png() {
        if !icon::available() {
            eprintln!("本平台不提供进程图标（icon::available() == false），跳过");
            return;
        }
        let provider = ProcProvider::new();
        // 本进程自己一定在进程表里，名字也一定反查得出路径。
        let Some(me) = std::env::current_exe()
            .ok()
            .and_then(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_owned))
        else {
            eprintln!("取不到当前可执行文件名，跳过");
            return;
        };
        // 候选里加上 explorer.exe：测试二进制本身未必带图标资源。
        for name in [me.as_str(), "explorer.exe"] {
            let Ok(png) = provider.icon_png(name).await else {
                continue;
            };
            assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
            assert_eq!(&png[12..16], b"IHDR");
            let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
            let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
            assert_eq!(
                (w, h),
                (icon::ICON_SIZE, icon::ICON_SIZE),
                "输出尺寸必须是跨平台约定的 {}×{}",
                icon::ICON_SIZE,
                icon::ICON_SIZE
            );
            // 第二次必须命中缓存。
            let again = provider.icon_png(name).await.expect("缓存里应当还在");
            assert!(Arc::ptr_eq(&png, &again), "第二次没有命中缓存");
            return;
        }
        eprintln!("本机没有任何候选程序能取到图标，跳过");
    }

    /// 预热一轮的实测：本机有多少个不同的程序名、耗时多少、取到几张。
    #[tokio::test]
    async fn 预热一轮的实测() {
        if !icon::available() {
            eprintln!("本平台不提供进程图标，跳过预热实测");
            return;
        }
        let provider = ProcProvider::new();
        let names = provider.running_names();
        let t0 = Instant::now();
        let report = provider.inner.icons.refresh(names.clone()).await;
        let elapsed = t0.elapsed();
        eprintln!(
            "预热一轮：{} 个程序名，提取 {} 次，成功 {}，失败 {}，耗时 {:?}",
            report.total, report.attempted, report.succeeded, report.failed, elapsed
        );
        assert_eq!(report.attempted, report.total, "首轮应当全部是新条目");
        assert_eq!(report.succeeded + report.failed, report.attempted);
        // 第二轮：同一批名字，条目全新鲜，应当一个都不重取。
        // 刻意复用第一轮的名单——进程表随时在变，重新枚举一次会混进新出现的
        // 进程，那属于「本来就该热一次」，不是补热策略失效。
        let second = provider.inner.icons.refresh(names).await;
        assert_eq!(second.attempted, 0, "刚热过的条目不该被重取");
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
