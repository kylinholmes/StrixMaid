//! CPU 信息：`/proc/cpuinfo` + `/proc/stat` + `/sys/devices/system/{cpu,node}` + cgroup 配额。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use strixmaid_types::system::CpuInfo;

use super::util::{read_trimmed, read_u64};

/// 采集 CPU 信息。任何一项读不到都退化成 `None` / 兜底值，不会失败。
pub fn read_cpu_info() -> CpuInfo {
    let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let parsed = parse_cpuinfo(&cpuinfo);

    // 逻辑核数按 types 文档的定义取 `/proc/stat` 里 `cpuN` 的条数；
    // 读不到再退回 cpuinfo 的 processor 条数，再不行就是 1。
    let logical_cores = fs::read_to_string("/proc/stat")
        .ok()
        .map(|s| count_stat_cpus(&s))
        .filter(|n| *n > 0)
        .or(Some(parsed.logical).filter(|n| *n > 0))
        .unwrap_or(1);

    CpuInfo {
        model: parsed.model.unwrap_or_else(|| "Unknown CPU".to_owned()),
        vendor: parsed.vendor,
        logical_cores,
        physical_cores: parsed.physical,
        numa_nodes: count_numa_nodes(),
        mhz: parsed.mhz.or_else(sysfs_cur_mhz),
        quota_cores: cgroup_cpu_quota(),
    }
}

/// `/proc/cpuinfo` 里能直接解析出的部分。
#[derive(Debug, Default, PartialEq)]
pub struct ParsedCpuInfo {
    pub model: Option<String>,
    pub vendor: Option<String>,
    /// `processor` 块的个数。
    pub logical: u32,
    /// 按 `(physical id, core id)` 去重后的物理核数；字段缺失时为 `None`。
    pub physical: Option<u32>,
    pub mhz: Option<f64>,
}

/// 解析 `/proc/cpuinfo` 文本。x86 与 ARM 的格式差异都在这里处理。
pub fn parse_cpuinfo(raw: &str) -> ParsedCpuInfo {
    let mut out = ParsedCpuInfo::default();
    let mut cores: HashSet<(u32, u32)> = HashSet::new();
    let mut saw_core_fields = false;
    // ARM 没有 model name，靠 implementer/part 解码。
    let mut implementer: Option<u32> = None;
    let mut part: Option<u32> = None;
    // 树莓派等把板子型号写在末尾的全局 `Model` / `Hardware` 行。
    let mut fallback_model: Option<String> = None;

    for block in raw.split("\n\n") {
        let mut physical_id: Option<u32> = None;
        let mut core_id: Option<u32> = None;
        let mut is_cpu_block = false;
        for line in block.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            match key {
                "processor" => is_cpu_block = true,
                "model name" => {
                    if out.model.is_none() && !value.is_empty() {
                        out.model = Some(value.to_owned());
                    }
                }
                "vendor_id" => {
                    if out.vendor.is_none() && !value.is_empty() {
                        out.vendor = Some(value.to_owned());
                    }
                }
                "cpu MHz" => {
                    if out.mhz.is_none() {
                        out.mhz = value.parse().ok();
                    }
                }
                "physical id" => physical_id = value.parse().ok(),
                "core id" => core_id = value.parse().ok(),
                "CPU implementer" => {
                    implementer = implementer.or_else(|| parse_hex(value));
                }
                "CPU part" => part = part.or_else(|| parse_hex(value)),
                "Model" | "Hardware" | "cpu model" | "cpu" | "machine"
                    if fallback_model.is_none() && !value.is_empty() =>
                {
                    fallback_model = Some(value.to_owned());
                }
                _ => {}
            }
        }
        if is_cpu_block {
            out.logical += 1;
            if let (Some(p), Some(c)) = (physical_id, core_id) {
                saw_core_fields = true;
                cores.insert((p, c));
            }
        }
    }

    if saw_core_fields && !cores.is_empty() {
        out.physical = Some(cores.len() as u32);
    }
    if out.model.is_none() {
        out.model = arm_part_name(implementer, part)
            .map(str::to_owned)
            .or(fallback_model);
    }
    if out.vendor.is_none() {
        out.vendor = implementer.and_then(arm_implementer_name).map(str::to_owned);
    }
    out
}

fn parse_hex(value: &str) -> Option<u32> {
    let v = value.trim_start_matches("0x");
    u32::from_str_radix(v, 16).ok()
}

/// ARM `CPU implementer` → 厂商名。
fn arm_implementer_name(id: u32) -> Option<&'static str> {
    Some(match id {
        0x41 => "ARM",
        0x42 => "Broadcom",
        0x43 => "Cavium",
        0x48 => "HiSilicon",
        0x4e => "NVIDIA",
        0x50 => "Applied Micro",
        0x51 => "Qualcomm",
        0x53 => "Samsung",
        0x61 => "Apple",
        0x66 => "Faraday",
        0x69 => "Intel",
        0x70 => "Phytium",
        0xc0 => "Ampere",
        _ => return None,
    })
}

/// 常见 ARM 核心的 `(implementer, part)` → 名字。查不到时返回 `None`，
/// 调用方再退回 `Model` / `Hardware` 行。
fn arm_part_name(implementer: Option<u32>, part: Option<u32>) -> Option<&'static str> {
    Some(match (implementer?, part?) {
        (0x41, 0xd03) => "ARM Cortex-A53",
        (0x41, 0xd04) => "ARM Cortex-A35",
        (0x41, 0xd05) => "ARM Cortex-A55",
        (0x41, 0xd07) => "ARM Cortex-A57",
        (0x41, 0xd08) => "ARM Cortex-A72",
        (0x41, 0xd09) => "ARM Cortex-A73",
        (0x41, 0xd0a) => "ARM Cortex-A75",
        (0x41, 0xd0b) => "ARM Cortex-A76",
        (0x41, 0xd0c) => "ARM Neoverse-N1",
        (0x41, 0xd0d) => "ARM Cortex-A77",
        (0x41, 0xd40) => "ARM Neoverse-V1",
        (0x41, 0xd41) => "ARM Cortex-A78",
        (0x41, 0xd42) => "ARM Cortex-A78AE",
        (0x41, 0xd44) => "ARM Cortex-X1",
        (0x41, 0xd46) => "ARM Cortex-A510",
        (0x41, 0xd47) => "ARM Cortex-A710",
        (0x41, 0xd48) => "ARM Cortex-X2",
        (0x41, 0xd49) => "ARM Neoverse-N2",
        (0x41, 0xd4a) => "ARM Neoverse-E1",
        (0x41, 0xd4f) => "ARM Neoverse-V2",
        (0x41, 0xd80) => "ARM Cortex-A520",
        (0x41, 0xd81) => "ARM Cortex-A720",
        (0x48, 0xd01) => "HiSilicon Kunpeng-920",
        (0x70, 0x662) => "Phytium FTC662",
        (0x70, 0x663) => "Phytium FTC663",
        (0xc0, 0xac3) => "Ampere-1",
        _ => return None,
    })
}

/// `/proc/stat` 里 `cpuN` 行的条数（不含汇总的 `cpu` 行）。
pub fn count_stat_cpus(stat: &str) -> u32 {
    stat.lines()
        .filter(|l| {
            l.starts_with("cpu") && l.as_bytes().get(3).is_some_and(u8::is_ascii_digit)
        })
        .count() as u32
}

/// NUMA 节点数：`/sys/devices/system/node/node*` 的个数。
fn count_numa_nodes() -> Option<u32> {
    let entries = fs::read_dir("/sys/devices/system/node").ok()?;
    let n = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("node") && name[4..].chars().all(|c| c.is_ascii_digit())
        })
        .count() as u32;
    (n > 0).then_some(n)
}

/// 没有 `cpu MHz` 行（ARM）时从 cpufreq 取当前频率（单位 kHz）。
fn sysfs_cur_mhz() -> Option<f64> {
    read_u64("/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq").map(|khz| khz as f64 / 1000.0)
}

/// cgroup CPU 配额换算的可用核数。
///
/// v2：沿本进程 cgroup 路径逐级向上读 `cpu.max`，取最严格的一层（配额对子树生效）；
/// 容器内 cgroup namespace 的根就是容器自己的 cgroup，`/sys/fs/cgroup/cpu.max` 即配额。
/// v1：`/sys/fs/cgroup/cpu/cpu.cfs_quota_us` / `cpu.cfs_period_us`。
/// 无配额返回 `None`。
fn cgroup_cpu_quota() -> Option<f64> {
    let root = Path::new("/sys/fs/cgroup");
    let own = fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|raw| crate::providers::process::cgroup::parse_cgroup_path(&raw))
        .unwrap_or_else(|| "/".to_owned());

    let mut best: Option<f64> = None;
    let mut dir: PathBuf = root.join(own.trim_start_matches('/'));
    loop {
        if let Some(cores) = read_trimmed(dir.join("cpu.max")).and_then(|s| parse_cpu_max(&s)) {
            best = Some(best.map_or(cores, |b: f64| b.min(cores)));
        }
        if dir == root || !dir.pop() || !dir.starts_with(root) {
            break;
        }
    }
    if best.is_some() {
        return best;
    }

    // cgroup v1
    let quota: i64 = read_trimmed(root.join("cpu/cpu.cfs_quota_us"))?.parse().ok()?;
    let period: i64 = read_trimmed(root.join("cpu/cpu.cfs_period_us"))?.parse().ok()?;
    (quota > 0 && period > 0).then(|| quota as f64 / period as f64)
}

/// 解析 `cpu.max` 的 `"<quota> <period>"`；`max` 表示无限制。
pub fn parse_cpu_max(s: &str) -> Option<f64> {
    let mut it = s.split_whitespace();
    let quota = it.next()?;
    if quota == "max" {
        return None;
    }
    let quota: f64 = quota.parse().ok()?;
    let period: f64 = it.next().unwrap_or("100000").parse().ok()?;
    (quota > 0.0 && period > 0.0).then(|| quota / period)
}

#[cfg(test)]
mod tests {
    use super::*;

    const X86: &str = "processor\t: 0\nvendor_id\t: AuthenticAMD\ncpu family\t: 25\nmodel name\t: AMD EPYC 9374F 32-Core Processor\ncpu MHz\t\t: 2794.700\nphysical id\t: 0\ncore id\t\t: 0\nflags\t\t: fpu vme\n\nprocessor\t: 1\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD EPYC 9374F 32-Core Processor\ncpu MHz\t\t: 3000.000\nphysical id\t: 0\ncore id\t\t: 1\n\nprocessor\t: 2\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD EPYC 9374F 32-Core Processor\nphysical id\t: 0\ncore id\t\t: 0\n\nprocessor\t: 3\nvendor_id\t: AuthenticAMD\nmodel name\t: AMD EPYC 9374F 32-Core Processor\nphysical id\t: 1\ncore id\t\t: 0\n";

    #[test]
    fn 解析_x86_cpuinfo() {
        let p = parse_cpuinfo(X86);
        assert_eq!(p.model.as_deref(), Some("AMD EPYC 9374F 32-Core Processor"));
        assert_eq!(p.vendor.as_deref(), Some("AuthenticAMD"));
        assert_eq!(p.logical, 4);
        // (0,0) (0,1) (1,0) → 3 个物理核；processor 2 是 processor 0 的超线程
        assert_eq!(p.physical, Some(3));
        assert_eq!(p.mhz, Some(2794.7));
    }

    const ARM: &str = "processor\t: 0\nBogoMIPS\t: 108.00\nFeatures\t: fp asimd\nCPU implementer\t: 0x41\nCPU architecture: 8\nCPU variant\t: 0x0\nCPU part\t: 0xd08\nCPU revision\t: 3\n\nprocessor\t: 1\nCPU implementer\t: 0x41\nCPU part\t: 0xd08\n\nHardware\t: BCM2835\nRevision\t: c03114\nModel\t\t: Raspberry Pi 4 Model B Rev 1.4\n";

    #[test]
    fn 解析_arm_cpuinfo() {
        let p = parse_cpuinfo(ARM);
        assert_eq!(p.model.as_deref(), Some("ARM Cortex-A72"));
        assert_eq!(p.vendor.as_deref(), Some("ARM"));
        assert_eq!(p.logical, 2);
        assert_eq!(p.physical, None, "ARM 没有 physical id / core id，不能瞎猜");
        assert_eq!(p.mhz, None);
    }

    #[test]
    fn 未知_arm_核心退回_model_行() {
        let raw = "processor\t: 0\nCPU implementer\t: 0x99\nCPU part\t: 0x001\n\nModel\t\t: Some Board\n";
        let p = parse_cpuinfo(raw);
        assert_eq!(p.model.as_deref(), Some("Some Board"));
        assert_eq!(p.vendor, None);
    }

    #[test]
    fn 统计_proc_stat_的_cpu_行() {
        let stat = "cpu  1 2 3\ncpu0 1 2 3\ncpu1 1 2 3\ncpu12 1 2 3\nintr 5\nctxt 6\n";
        assert_eq!(count_stat_cpus(stat), 3);
        assert_eq!(count_stat_cpus(""), 0);
    }

    #[test]
    fn 解析_cpu_max() {
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("200000 100000"), Some(2.0));
        assert_eq!(parse_cpu_max("50000 100000"), Some(0.5));
        assert_eq!(parse_cpu_max("garbage"), None);
    }

    #[test]
    fn 本机采集() {
        let c = read_cpu_info();
        assert!(c.logical_cores >= 1);
        assert!(!c.model.is_empty());
    }
}
