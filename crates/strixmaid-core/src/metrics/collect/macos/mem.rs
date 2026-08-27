//! 内存：mach `host_statistics64(HOST_VM_INFO64)` + `sysctl hw.memsize` / `vm.swapusage`。
//!
//! # `mem.available` 是估算值
//!
//! Linux 的 `MemAvailable` 是内核自己算好的一个字段——它知道哪些页可回收、回收要付多大
//! 代价。macOS 没有等价物，只能由我们从页计数里估：
//!
//! ```text
//! available ≈ (free + purgeable + external) × 页大小
//! ```
//!
//! - `free`：完全空闲页；
//! - `purgeable`：应用显式标记为「内存紧张时可以直接丢弃」的页；
//! - `external`：文件backing 的页，也就是 macOS 版的 page cache，可回写后回收。
//!
//! 这与 Linux `MemAvailable`「空闲 + 可回收页缓存」的思路一致，但**没有**扣除
//! 内核为保证不 OOM 而必须留下的水位线，因此系统性偏乐观。用于观察趋势足够，
//! 不要拿它做容量告警的绝对阈值。
//!
//! # 不产出的指标
//!
//! `mem.buffers`（块设备缓冲）与 `mem.dirty`（等待写回的脏页）在 XNU 里没有
//! 单独统计，宁可不产出，也不拿别的数字冒充。

use std::time::Instant;

use super::{CollectError, Collector, Sample};
use crate::metrics::catalog as cat;
use crate::platform::macos::{page_size, sysctl_scalar};

/// 一次内存采样（字节）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemStat {
    pub total: u64,
    pub available: u64,
    pub free: u64,
    pub cached: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub swap_used: u64,
}

/// mach 的页计数，尚未换算成字节。分出来是为了让换算逻辑可以脱离 FFI 单测。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmPages {
    pub free: u64,
    pub purgeable: u64,
    pub external: u64,
}

impl VmPages {
    /// 换算成字节并与 `total` 合成。`available` 夹在 `0..=total`——
    /// 页计数与 `hw.memsize` 来自两个不同的内核子系统，采样存在时间差，
    /// 极端情况下相加可能略微超过物理内存，让它溢出会使 `used` 变成负数。
    pub fn to_stat(self, total: u64, page: u64, swap: SwapStat) -> MemStat {
        let available = (self.free + self.purgeable + self.external)
            .saturating_mul(page)
            .min(total);
        MemStat {
            total,
            available,
            free: self.free.saturating_mul(page),
            cached: self.external.saturating_mul(page),
            swap_total: swap.total,
            swap_free: swap.free,
            swap_used: swap.used,
        }
    }
}

/// `vm.swapusage` 的三个数值（字节）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SwapStat {
    pub total: u64,
    pub free: u64,
    pub used: u64,
}

/// 读一次 `host_statistics64(HOST_VM_INFO64)`。
pub fn read_vm_pages() -> Option<VmPages> {
    let mut vm = std::mem::MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: 缓冲区是一整个 vm_statistics64，count 如实描述其 integer_t 个数；
    // 内核只按 flavor 约定的布局写入，不会越界。
    let rc = unsafe {
        libc::host_statistics64(
            mach2::mach_init::mach_host_self(),
            libc::HOST_VM_INFO64,
            vm.as_mut_ptr().cast::<libc::integer_t>(),
            &raw mut count,
        )
    };
    if rc != libc::KERN_SUCCESS {
        return None;
    }
    // SAFETY: 调用成功即已按 flavor 布局初始化；结构体本身是 zeroed 的，
    // 内核没写到的尾部字段读出来是 0，我们也不用它们。
    let vm = unsafe { vm.assume_init() };
    // vm_statistics64 是 repr(packed(8))，逐字段取值（Copy）而非取引用。
    Some(VmPages {
        free: u64::from(vm.free_count),
        purgeable: u64::from(vm.purgeable_count),
        external: u64::from(vm.external_page_count),
    })
}

/// 读 `vm.swapusage`。没有配置交换区时全为 0（不是错误）。
pub fn read_swap() -> SwapStat {
    let Some(x) = sysctl_scalar::<libc::xsw_usage>("vm.swapusage") else {
        return SwapStat::default();
    };
    SwapStat {
        total: x.xsu_total,
        free: x.xsu_avail,
        used: x.xsu_used,
    }
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
            Sample::new(cat::MEM_AVAILABLE, stat.available as f64),
            Sample::new(
                cat::MEM_USED,
                stat.total.saturating_sub(stat.available) as f64,
            ),
            Sample::new(cat::MEM_FREE, stat.free as f64),
            Sample::new(cat::MEM_CACHED, stat.cached as f64),
        ];
        // 没配交换区时 total 为 0，三条曲线都恒为 0，不如不产出。
        if stat.swap_total > 0 {
            out.extend([
                Sample::new(cat::MEM_SWAP_TOTAL, stat.swap_total as f64),
                Sample::new(cat::MEM_SWAP_FREE, stat.swap_free as f64),
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
        let pages = read_vm_pages().ok_or_else(|| {
            CollectError::new(self.name(), "host_statistics64(HOST_VM_INFO64) 调用失败")
        })?;
        let total = sysctl_scalar::<u64>("hw.memsize")
            .ok_or_else(|| CollectError::new(self.name(), "读不到 hw.memsize"))?;
        Ok(Self::samples(&pages.to_stat(
            total,
            page_size(),
            read_swap(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 页数换算与夹取() {
        let pages = VmPages {
            free: 10,
            purgeable: 5,
            external: 25,
        };
        let s = pages.to_stat(1000 * 4096, 4096, SwapStat::default());
        assert_eq!(s.available, 40 * 4096);
        assert_eq!(s.free, 10 * 4096);
        assert_eq!(s.cached, 25 * 4096);

        // 页计数之和超过物理内存时夹到 total，避免 used 变成负数
        let s = pages.to_stat(20 * 4096, 4096, SwapStat::default());
        assert_eq!(s.available, 20 * 4096);
        assert_eq!(s.total.saturating_sub(s.available), 0);
    }

    #[test]
    fn 无交换区时不产出_swap_曲线() {
        let stat = MemStat {
            total: 100,
            available: 40,
            ..Default::default()
        };
        let out = MemCollector::samples(&stat);
        assert_eq!(out.len(), 5);
        assert!(!out.iter().any(|s| s.metric.starts_with("mem.swap")));
        let used = out.iter().find(|s| s.metric == cat::MEM_USED).unwrap();
        assert_eq!(used.value, 60.0);

        let with_swap = MemStat {
            swap_total: 8,
            swap_free: 3,
            swap_used: 5,
            ..stat
        };
        assert_eq!(MemCollector::samples(&with_swap).len(), 8);
    }

    #[test]
    fn 本机采集值域合理() {
        let mut c = MemCollector::new();
        let out = c.collect(Instant::now()).expect("mach 内存统计");
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
        assert!(get(cat::MEM_FREE) <= total);
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
        }
        // XNU 没有这两个概念，不能凭空产出
        for absent in [cat::MEM_BUFFERS, cat::MEM_DIRTY] {
            assert!(
                !out.iter().any(|s| s.metric == absent),
                "{absent} 在 macOS 上不该存在"
            );
        }
    }
}
