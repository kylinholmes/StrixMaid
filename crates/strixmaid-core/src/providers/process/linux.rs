//! Linux 进程枚举后端：直读 `/proc`（`procfs` crate）。
//!
//! # 性能
//!
//! 一次列表要遍历几百到几千个进程。每个进程只读 **`stat` + `cmdline`**（内核线程连 cmdline
//! 都省掉，靠 `PF_KTHREAD` 标志判断）外加一次 `fstat` 取 uid；线程数、RSS、nice、状态、
//! 启动时刻都在 `stat` 里。cgroup / status / environ / fd 只在详情里读。
//! 整个遍历由外壳放在 `spawn_blocking` 里跑。

use std::collections::{BTreeMap, HashSet};
use std::fs;

use procfs::process::{Process, Stat, all_processes};
use strixmaid_types::process::{FdInfo, ProcessDetail, ProcessState, ProcessSummary};
use strixmaid_types::{ApiError, ApiResult};

use super::super::Probe;
use super::super::system::linux::util::meminfo_value;
use super::cpu::CpuSamples;
use super::{Context, cgroup, tty};

/// `/proc/<pid>/stat` 的 `flags` 里的内核线程标志（`include/linux/sched.h`）。
const PF_KTHREAD: u32 = 0x0020_0000;

/// `/proc` 可读即可用。
pub fn probe() -> Probe {
    match fs::read_to_string("/proc/self/stat") {
        Ok(_) => Probe::Available,
        Err(e) => Probe::unavailable(format!("无法读取 /proc/self/stat：{e}")),
    }
}

/// Linux 后端。构造时读一次不随进程变化的常量。
pub struct Backend {
    /// `sysconf(_SC_CLK_TCK)`，几乎总是 100。
    hz: u64,
    page_size: u64,
    /// `/proc/stat` 的 `btime`；读不到为 0，此时 `start_ts` 退化成「开机以来的秒数」。
    boot_time: u64,
}

impl Default for Backend {
    fn default() -> Self {
        Backend::new()
    }
}

impl Backend {
    pub fn new() -> Self {
        Backend {
            hz: procfs::ticks_per_second().max(1),
            page_size: procfs::page_size().max(1),
            boot_time: procfs::boot_time_secs().unwrap_or(0),
        }
    }

    /// `/proc/meminfo` 的 `MemTotal`。
    pub fn mem_total(&self) -> u64 {
        let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
        meminfo_value(&meminfo, "MemTotal").unwrap_or(0)
    }

    /// 遍历 `/proc`，顺带更新 CPU 快照并清理已消失的 pid。
    pub fn list(&self, cpu: &mut CpuSamples, ctx: &Context) -> Vec<ProcessSummary> {
        let mut all: Vec<ProcessSummary> = Vec::with_capacity(512);
        let mut seen: HashSet<u32> = HashSet::with_capacity(512);
        if let Ok(iter) = all_processes() {
            for proc in iter.flatten() {
                let Ok(stat) = proc.stat() else { continue };
                if let Some(s) = self.summarize(&proc, &stat, cpu, ctx) {
                    seen.insert(s.pid);
                    all.push(s);
                }
            }
        }
        cpu.retain_seen(&seen);
        all
    }

    /// 单个进程的详情。
    pub fn detail(
        &self,
        raw_pid: i32,
        cpu: &mut CpuSamples,
        ctx: &Context,
    ) -> ApiResult<ProcessDetail> {
        let pid = raw_pid as u32;
        let not_found = || ApiError::not_found(format!("进程 {pid} 不存在"));
        let proc = Process::new(raw_pid).map_err(|_| not_found())?;
        let stat = proc.stat().map_err(|_| not_found())?;
        let summary = self
            .summarize(&proc, &stat, cpu, ctx)
            .ok_or_else(not_found)?;

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
            .observe(pid, stat.starttime, ticks, ctx.now, self.hz)
            .unwrap_or(0.0);
        let rss_bytes = stat.rss.saturating_mul(self.page_size);
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
            mem_percent: ctx.mem_percent(rss_bytes),
            threads: u32::try_from(stat.num_threads).unwrap_or(0),
            start_ts: self.boot_time as i64 + (stat.starttime / self.hz) as i64,
            nice: stat.nice as i32,
        })
    }
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
    use crate::providers::process::ProcProvider;

    #[test]
    fn 状态映射() {
        assert_eq!(map_state('R'), ProcessState::Running);
        assert_eq!(map_state('D'), ProcessState::DiskSleep);
        assert_eq!(map_state('I'), ProcessState::Idle);
        assert_eq!(map_state('?'), ProcessState::Unknown);
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
    fn 本进程详情_unit_解析一致() {
        let provider = ProcProvider::new();
        let me = std::process::id();
        let d = provider.detail_blocking(me).unwrap();
        // 自己的进程：cwd / environ / fd 都应可读
        assert!(d.cwd.is_some());
        assert!(d.environ.is_some());
        assert!(d.fds.as_ref().is_some_and(|f| !f.is_empty()));

        // unit 必须与直接解析 /proc/self/cgroup 的结果一致
        let raw = fs::read_to_string("/proc/self/cgroup").unwrap();
        let expected_path = cgroup::parse_cgroup_path(&raw);
        assert_eq!(d.cgroup, expected_path);
        let expected_unit = expected_path
            .as_deref()
            .and_then(cgroup::unit_from_cgroup_path);
        assert_eq!(d.unit, expected_unit);
        if let Some(path) = &expected_path
            && path.contains(".service")
        {
            assert!(
                d.unit.is_some(),
                "cgroup {path} 里有 .service，unit 不该为空"
            );
        }
        eprintln!("本进程 cgroup={:?} unit={:?}", d.cgroup, d.unit);
    }
}
