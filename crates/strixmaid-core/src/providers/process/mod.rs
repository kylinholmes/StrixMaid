//! process provider（id `"proc"`）：进程列表 / 详情 / 信号 / renice，全部直读 `/proc`。
//!
//! # 性能
//!
//! 一次列表要遍历几百到几千个进程。每个进程只读 **`stat` + `cmdline`**（内核线程连 cmdline
//! 都省掉，靠 `PF_KTHREAD` 标志判断）外加一次 `fstat` 取 uid；线程数、RSS、nice、状态、
//! 启动时刻都在 `stat` 里。cgroup / status / environ / fd 只在详情里读。
//! 整个遍历在 `spawn_blocking` 里跑。
//!
//! # CPU%
//!
//! 差分计算见 [`cpu`]：provider 内部持有上一轮 `(pid, starttime) → ticks` 快照，
//! 每次列表 / 详情都更新它，并清理已消失的 pid。**首次调用没有基线，CPU% 为 0.0**。
//!
//! # 权限
//!
//! 读列表几乎不需要权限；`cwd` / `exe` / `environ` / `fd` / `io` 只有同 uid 或 root 能读，
//! 读不到就是 `None`。信号与 renice 由内核裁决，`EPERM` → `PermissionDenied`（可提权重试）。

pub mod cgroup;
pub mod cpu;
pub mod filter;
pub mod tty;
pub mod users;

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use procfs::process::{Process, Stat, all_processes};
use strixmaid_types::process::{
    FdInfo, ProcessDetail, ProcessListQuery, ProcessState, ProcessSummary, SignalName,
};
use strixmaid_types::{ApiError, ApiResult};

use super::system::util::meminfo_value;
use super::{Probe, Provider};
use cpu::CpuSamples;
use users::{UserDb, UserTable};

/// `/proc/<pid>/stat` 的 `flags` 里的内核线程标志（`include/linux/sched.h`）。
const PF_KTHREAD: u32 = 0x0020_0000;

/// 进程 provider。内部是 `Arc`，`Clone` 廉价，便于丢进 `spawn_blocking`。
#[derive(Clone)]
pub struct ProcProvider {
    inner: Arc<Inner>,
}

struct Inner {
    cpu: Mutex<CpuSamples>,
    users: UserDb,
    /// `sysconf(_SC_CLK_TCK)`，几乎总是 100。
    hz: u64,
    page_size: u64,
    /// `/proc/stat` 的 `btime`；读不到为 0，此时 `start_ts` 退化成「开机以来的秒数」。
    boot_time: u64,
}

impl Default for ProcProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcProvider {
    /// 创建 provider；读一次时钟频率、页大小与开机时刻。
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cpu: Mutex::new(CpuSamples::new()),
                users: UserDb::new(),
                hz: procfs::ticks_per_second().max(1),
                page_size: procfs::page_size().max(1),
                boot_time: procfs::boot_time_secs().unwrap_or(0),
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

    /// 同步版列表：遍历 `/proc`、更新 CPU 快照、按查询参数筛选排序。
    pub fn list_blocking(&self, query: &ProcessListQuery) -> Vec<ProcessSummary> {
        let ctx = self.context();
        let mut all: Vec<ProcessSummary> = Vec::with_capacity(512);
        let mut seen: HashSet<u32> = HashSet::with_capacity(512);
        {
            let mut cpu = self.inner.cpu.lock().unwrap_or_else(|e| e.into_inner());
            if let Ok(iter) = all_processes() {
                for proc in iter.flatten() {
                    let Ok(stat) = proc.stat() else { continue };
                    if let Some(s) = self.summarize(&proc, &stat, &mut cpu, &ctx) {
                        seen.insert(s.pid);
                        all.push(s);
                    }
                }
            }
            cpu.retain_seen(&seen);
        }
        filter::apply(all, query, |name| ctx.users.uid_of(name))
    }

    /// 同步版详情。
    pub fn detail_blocking(&self, pid: u32) -> ApiResult<ProcessDetail> {
        let raw_pid = checked_pid(pid)?;
        let not_found = || ApiError::not_found(format!("进程 {pid} 不存在"));
        let proc = Process::new(raw_pid).map_err(|_| not_found())?;
        let stat = proc.stat().map_err(|_| not_found())?;
        let ctx = self.context();
        let summary = {
            let mut cpu = self.inner.cpu.lock().unwrap_or_else(|e| e.into_inner());
            self.summarize(&proc, &stat, &mut cpu, &ctx)
                .ok_or_else(not_found)?
        };

        let status = proc.status().ok();
        let cgroup = fs::read_to_string(format!("/proc/{pid}/cgroup"))
            .ok()
            .and_then(|raw| cgroup::parse_cgroup_path(&raw));
        let unit = cgroup.as_deref().and_then(cgroup::unit_from_cgroup_path);
        let io = proc.io().ok();

        Ok(ProcessDetail {
            summary,
            cmdline_args: proc.cmdline().unwrap_or_default(),
            exe: proc.exe().ok().map(|p| p.to_string_lossy().into_owned()),
            cwd: proc.cwd().ok().map(|p| p.to_string_lossy().into_owned()),
            euid: status.as_ref().map(|s| s.euid),
            gid: status.as_ref().map(|s| s.rgid),
            tty: tty::tty_name(stat.tty_nr),
            cgroup,
            unit,
            environ: proc.environ().ok().map(|m| {
                m.into_iter()
                    .map(|(k, v)| {
                        (
                            k.to_string_lossy().into_owned(),
                            v.to_string_lossy().into_owned(),
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            }),
            fds: read_fds(pid),
            io_read_bytes: io.as_ref().map(|i| i.read_bytes),
            io_write_bytes: io.as_ref().map(|i| i.write_bytes),
        })
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
            other => ApiError::internal(format!("向进程 {pid} 发送 {sig} 失败")).with_detail(other.to_string()),
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

    /// 一次列表 / 详情共用的上下文：用户表快照、MemTotal、采样时刻。
    fn context(&self) -> Context {
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        Context {
            users: self.inner.users.snapshot(),
            mem_total: meminfo_value(&meminfo, "MemTotal").unwrap_or(0),
            now: Instant::now(),
        }
    }

    /// 把一个进程的 `stat` 转成 [`ProcessSummary`]，同时更新 CPU 快照。
    fn summarize(
        &self,
        proc: &Process,
        stat: &Stat,
        cpu: &mut CpuSamples,
        ctx: &Context,
    ) -> Option<ProcessSummary> {
        let pid = u32::try_from(stat.pid).ok().filter(|p| *p > 0)?;
        let uid = proc.uid().ok()?;
        let kernel_thread = stat.flags & PF_KTHREAD != 0;
        let cmdline = if kernel_thread {
            None
        } else {
            proc.cmdline()
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| v.join(" "))
        };
        let ticks = stat.utime.saturating_add(stat.stime);
        let cpu_percent = cpu
            .observe(pid, stat.starttime, ticks, ctx.now, self.inner.hz)
            .unwrap_or(0.0);
        let rss_bytes = stat.rss.saturating_mul(self.inner.page_size);
        let mem_percent = if ctx.mem_total > 0 {
            ((rss_bytes as f64 / ctx.mem_total as f64 * 100.0) * 100.0).round() / 100.0
        } else {
            0.0
        };
        Some(ProcessSummary {
            pid,
            ppid: u32::try_from(stat.ppid).unwrap_or(0),
            name: stat.comm.clone(),
            cmdline,
            uid,
            user: ctx.users.name_of(uid).map(str::to_owned),
            state: map_state(stat.state),
            cpu_percent,
            rss_bytes,
            vms_bytes: stat.vsize,
            mem_percent,
            threads: u32::try_from(stat.num_threads).unwrap_or(0),
            start_ts: self.inner.boot_time as i64 + (stat.starttime / self.inner.hz) as i64,
            nice: stat.nice as i32,
        })
    }
}

#[async_trait]
impl Provider for ProcProvider {
    fn id(&self) -> &'static str {
        "proc"
    }

    async fn probe(&self) -> Probe {
        match fs::read_to_string("/proc/self/stat") {
            Ok(_) => Probe::Available,
            Err(e) => Probe::unavailable(format!("无法读取 /proc/self/stat：{e}")),
        }
    }
}

struct Context {
    users: Arc<UserTable>,
    mem_total: u64,
    now: Instant,
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

/// `/proc/<pid>/stat` 第 3 字段 → [`ProcessState`]。
pub fn map_state(c: char) -> ProcessState {
    match c {
        'R' => ProcessState::Running,
        'S' => ProcessState::Sleeping,
        'D' => ProcessState::DiskSleep,
        'Z' => ProcessState::Zombie,
        'T' => ProcessState::Stopped,
        't' => ProcessState::TracingStop,
        'X' | 'x' => ProcessState::Dead,
        'I' => ProcessState::Idle,
        _ => ProcessState::Unknown,
    }
}

/// 读 `/proc/<pid>/fd`。目录打不开（无权限）→ `None`；单个 fd 在读取间隙被关闭则跳过。
fn read_fds(pid: u32) -> Option<Vec<FdInfo>> {
    let dir = fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut out: Vec<FdInfo> = dir
        .flatten()
        .filter_map(|e| {
            let fd: u32 = e.file_name().to_str()?.parse().ok()?;
            let target = fs::read_link(e.path()).ok()?;
            let target = target.to_string_lossy().into_owned();
            let kind = classify_fd(&target, &e.path());
            Some(FdInfo {
                fd,
                target,
                kind: kind.to_owned(),
            })
        })
        .collect();
    out.sort_by_key(|f| f.fd);
    Some(out)
}

/// 按软链目标归类 fd：`file` / `socket` / `pipe` / `anon_inode` / `dir` / `other`。
fn classify_fd(target: &str, link_path: &std::path::Path) -> &'static str {
    if target.starts_with("socket:") {
        "socket"
    } else if target.starts_with("pipe:") {
        "pipe"
    } else if target.starts_with("anon_inode:") {
        "anon_inode"
    } else if target.starts_with('/') {
        // metadata 跟随软链：目标是目录就是 dir
        match fs::metadata(link_path) {
            Ok(m) if m.is_dir() => "dir",
            _ => "file",
        }
    } else {
        "other"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::ErrorCode;

    #[test]
    fn 状态映射() {
        assert_eq!(map_state('R'), ProcessState::Running);
        assert_eq!(map_state('D'), ProcessState::DiskSleep);
        assert_eq!(map_state('I'), ProcessState::Idle);
        assert_eq!(map_state('?'), ProcessState::Unknown);
    }

    #[test]
    fn pid_校验() {
        assert!(checked_pid(0).is_err());
        assert!(checked_pid(u32::MAX).is_err());
        assert_eq!(checked_pid(1).unwrap(), 1);
    }

    #[test]
    fn fd_归类() {
        let p = std::path::Path::new("/nonexistent");
        assert_eq!(classify_fd("socket:[123]", p), "socket");
        assert_eq!(classify_fd("pipe:[9]", p), "pipe");
        assert_eq!(classify_fd("anon_inode:[eventpoll]", p), "anon_inode");
        assert_eq!(classify_fd("/var/log/x.log", p), "file");
        assert_eq!(classify_fd("weird", p), "other");
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

        // 性能：目标 500 进程 < 50ms，按比例放宽到本机的进程数；上限 2s 只是防止彻底退化。
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
    fn 本进程详情_unit_解析一致() {
        let provider = ProcProvider::new();
        let me = std::process::id();
        let d = provider.detail_blocking(me).unwrap();
        assert_eq!(d.summary.pid, me);
        assert!(!d.cmdline_args.is_empty());
        // 自己的进程：cwd / exe / environ / fd 都应可读
        assert!(d.exe.is_some());
        assert!(d.cwd.is_some());
        assert!(d.environ.is_some());
        assert!(d.fds.as_ref().is_some_and(|f| !f.is_empty()));
        assert_eq!(d.euid, Some(unsafe { libc::geteuid() }));

        // unit 必须与直接解析 /proc/self/cgroup 的结果一致；
        // 只要本进程在某个 .service/.scope 下（ssh.service、user@N.service/…、session-N.scope），unit 就非空。
        let raw = fs::read_to_string("/proc/self/cgroup").unwrap();
        let expected_path = cgroup::parse_cgroup_path(&raw);
        assert_eq!(d.cgroup, expected_path);
        let expected_unit = expected_path.as_deref().and_then(cgroup::unit_from_cgroup_path);
        assert_eq!(d.unit, expected_unit);
        if let Some(path) = &expected_path
            && path.contains(".service")
        {
            assert!(d.unit.is_some(), "cgroup {path} 里有 .service，unit 不该为空");
        }
        eprintln!("本进程 cgroup={:?} unit={:?}", d.cgroup, d.unit);
    }

    #[test]
    fn 不存在的进程() {
        let provider = ProcProvider::new();
        // pid_max 默认 4194304；用一个远超它的合法 i32
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
        assert_eq!(provider.signal(1, SignalName::Kill).unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(provider.signal(0, SignalName::Term).unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(provider.renice(std::process::id(), 40).unwrap_err().code, ErrorCode::InvalidRequest);
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
                q: Some("strixmaid".into()),
                tree: Some(true),
                ..Default::default()
            })
            .await
            .unwrap();
        // 树模式下命中项的祖先也在，故列表里必然有 pid 1 或某个根
        assert!(!list.is_empty());
        let me = std::process::id();
        let d = p.detail(me).await.unwrap();
        assert_eq!(d.summary.pid, me);
    }
}
