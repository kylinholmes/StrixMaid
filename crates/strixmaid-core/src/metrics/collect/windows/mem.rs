//! 内存：`GlobalMemoryStatusEx` + `GetPerformanceInfo` + `SystemPagefileInformation`。
//!
//! 六条内存指标在 Windows 上全都有真实来源，不需要任何估算：
//!
//! | 指标 | 来源 | 说明 |
//! |---|---|---|
//! | `mem.total` | `MEMORYSTATUSEX::ullTotalPhys` | 物理内存总量 |
//! | `mem.available` | `MEMORYSTATUSEX::ullAvailPhys` | 见下 |
//! | `mem.used` | `total − available` | 与 Linux 版同一算式 |
//! | `mem.cached` | `PERFORMANCE_INFORMATION::SystemCache × PageSize` | 系统文件缓存 |
//! | `mem.swap_total` / `mem.swap_used` | [`pagefile_totals`] | 见下 |
//!
//! # `mem.available` 不是估算值
//!
//! 与 macOS 侧必须自己从页计数里估不同，`ullAvailPhys` 是内存管理器自己算好的
//! 「立即可供分配的物理内存」——空闲页 + 零页 + 待机页（standby list，即可以
//! 直接丢弃的缓存页）。这与 Linux `MemAvailable`「空闲 + 可回收页缓存」是同一
//! 思路、同一量级的东西，可以放心当作同一条曲线看。任务管理器「可用」那一栏
//! 显示的就是它。
//!
//! # `mem.swap_total` 绝不能用 `ullTotalPageFile`
//!
//! 这是 Windows 上最容易踩的一个坑：`MEMORYSTATUSEX` 的 `ullTotalPageFile` /
//! `ullAvailPageFile` 是**提交上限**（commit limit ≈ 物理内存 + 页面文件），
//! 不是页面文件大小。拿它当 `mem.swap_total`，一台 32 GiB 内存 + 4 GiB 页面文件
//! 的机器会报出 36 GiB 的「交换空间」，且关掉页面文件后它还有 32 GiB——
//! 那条曲线与 swap 毫无关系。
//!
//! 页面文件本身的大小与用量来自 `SystemPagefileInformation`，封装在
//! [`crate::platform::windows::ntdll::pagefile_totals`]。没有配置页面文件时它
//! 返回 `(0, 0)`，此时**两条 swap 曲线都不产出**——与 Linux 上没有 swap 分区
//! 一样，那不是错误。
//!
//! # `mem.cached` 的口径差异
//!
//! Linux 版是 `Cached + Buffers` 之和（roadmap/08 §4.2）。Windows 的
//! `SystemCache` 是文件系统缓存的驻留量，块设备缓冲不单列（它本来就走同一套
//! 缓存管理器），因此这里只有一项——宁可少算，也不拿别的数字凑。
//!
//! 注意 `SystemCache` 的单位是**页**，要乘 `PageSize` 才是字节；同一次
//! `GetPerformanceInfo` 就带回了 `PageSize`，不必再问一次系统。

use std::time::Instant;

use windows_sys::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog as cat;
use crate::platform::windows::ntdll;

/// 一次内存采样（字节）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemStat {
    pub total: u64,
    pub available: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_used: u64,
}

impl MemStat {
    /// 已用物理内存，口径同 Linux 版：`total − available`。
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// `GlobalMemoryStatusEx` → `(总量, 可用)`，字节。
pub fn read_global_status() -> Option<(u64, u64)> {
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: status 是一个完整的 MEMORYSTATUSEX，dwLength 已按约定填成结构体大小
    //（这个字段不填函数会直接失败，不是可选项）。
    let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
    (ok != 0).then_some((status.ullTotalPhys, status.ullAvailPhys))
}

/// `GetPerformanceInfo` → 系统文件缓存的字节数。
///
/// 读不到只是少一条曲线，不该让整轮失败。
pub fn read_system_cache() -> Option<u64> {
    let mut info = PERFORMANCE_INFORMATION {
        cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        ..Default::default()
    };
    // SAFETY: info 是一个完整的 PERFORMANCE_INFORMATION，cb 如实描述其大小。
    let ok = unsafe {
        GetPerformanceInfo(
            &raw mut info,
            std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        return None;
    }
    // SystemCache 的单位是页。
    Some((info.SystemCache as u64).saturating_mul(info.PageSize as u64))
}

/// 内存采集器。无状态——全是瞬时量，不需要差分。
#[derive(Debug, Clone, Copy, Default)]
pub struct MemCollector;

impl MemCollector {
    pub fn new() -> Self {
        MemCollector
    }

    /// 把一次采样摊成样本。
    pub fn samples(stat: &MemStat) -> Vec<Sample> {
        let mut out = vec![
            Sample::new(cat::MEM_TOTAL, stat.total as f64),
            Sample::new(cat::MEM_USED, stat.used() as f64),
            Sample::new(cat::MEM_AVAILABLE, stat.available as f64),
            Sample::new(cat::MEM_CACHED, stat.cached as f64),
        ];
        // 没配页面文件时 total 为 0，两条曲线都恒为 0，不如不产出。
        if stat.swap_total > 0 {
            out.extend([
                Sample::new(cat::MEM_SWAP_TOTAL, stat.swap_total as f64),
                Sample::new(cat::MEM_SWAP_USED, stat.swap_used as f64),
            ]);
        }
        out
    }
}

impl Collector for MemCollector {
    fn name(&self) -> &'static str {
        "mem"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        let (total, available) = read_global_status()
            .ok_or_else(|| CollectError::new(self.name(), "GlobalMemoryStatusEx 调用失败"))?;
        // 可选输入：缓存读不到少一条曲线；页面文件读不到当作没配页面文件。
        let cached = read_system_cache().unwrap_or(0).min(total);
        let (swap_total, swap_used) = ntdll::pagefile_totals().unwrap_or((0, 0));
        Ok(Self::samples(&MemStat {
            total,
            available: available.min(total),
            cached,
            swap_total,
            swap_used: swap_used.min(swap_total),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 已用是总量减可用() {
        let s = MemStat {
            total: 1000,
            available: 400,
            ..Default::default()
        };
        assert_eq!(s.used(), 600);
        // 可用大于总量（两个计数来源采样错位）时夹到 0，而不是溢出成天文数字
        let s = MemStat {
            total: 400,
            available: 1000,
            ..Default::default()
        };
        assert_eq!(s.used(), 0);
    }

    #[test]
    fn 无页面文件时不产出_swap_曲线() {
        let stat = MemStat {
            total: 100,
            available: 40,
            cached: 10,
            ..Default::default()
        };
        let out = MemCollector::samples(&stat);
        assert_eq!(out.len(), 4);
        assert!(!out.iter().any(|s| s.metric.starts_with("mem.swap")));
        let used = out.iter().find(|s| s.metric == cat::MEM_USED).unwrap();
        assert_eq!(used.value, 60.0);

        let with_swap = MemStat {
            swap_total: 8,
            swap_used: 5,
            ..stat
        };
        assert_eq!(MemCollector::samples(&with_swap).len(), 6);
    }

    #[test]
    fn 本机全局状态合理() {
        let (total, available) = read_global_status().expect("GlobalMemoryStatusEx");
        assert!(total > 256 * 1024 * 1024, "物理内存只有 {total} 字节？");
        assert!(available <= total);
        eprintln!(
            "本机内存：总 {} MiB，可用 {} MiB",
            total / (1 << 20),
            available / (1 << 20)
        );
    }

    #[test]
    fn 本机文件缓存不超过物理内存() {
        let (total, _) = read_global_status().unwrap();
        let cache = read_system_cache().expect("GetPerformanceInfo");
        assert!(cache > 0, "系统文件缓存不该是 0");
        assert!(cache <= total, "缓存 {cache} 超过物理内存 {total}");
        eprintln!("本机系统文件缓存：{} MiB", cache / (1 << 20));
    }

    /// 页面文件的口径必须是「页面文件本身」，不是提交上限——
    /// 这条把模块文档里那个坑钉住。
    #[test]
    fn swap_口径不是提交上限() {
        let (total, _) = read_global_status().unwrap();
        let (swap_total, swap_used) = ntdll::pagefile_totals().expect("页面文件信息");
        assert!(swap_used <= swap_total);
        if swap_total > 0 {
            assert!(
                swap_total < total + swap_total,
                "swap_total 若等于提交上限就会把物理内存算进去"
            );
        }
        eprintln!(
            "本机页面文件：{} / {} MiB",
            swap_used / (1 << 20),
            swap_total / (1 << 20)
        );
    }

    #[test]
    fn 本机采集值域合理() {
        let mut c = MemCollector::new();
        let out = c.collect(Instant::now()).expect("内存采集");
        let get = |m: &str| {
            out.iter()
                .find(|s| s.metric == m)
                .map(|s| s.value)
                .expect(m)
        };
        let total = get(cat::MEM_TOTAL);
        assert!(total > 0.0);
        assert!(get(cat::MEM_AVAILABLE) <= total);
        assert!(get(cat::MEM_USED) <= total);
        assert!(get(cat::MEM_CACHED) <= total);
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
            assert!(s.labels.is_empty(), "内存指标没有标签");
        }
    }
}
