//! CPU 信息：注册表的型号串 + `GetLogicalProcessorInformationEx` 的拓扑。
//!
//! # 两个数据源，各管一半
//!
//! | 字段 | 来源 |
//! |---|---|
//! | `model` / `vendor` / `mhz` | `HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0` |
//! | `logical_cores` / `physical_cores` / `numa_nodes` / `packages` | `GetLogicalProcessorInformationEx(RelationAll)` |
//!
//! 注册表那一半是 HAL 在启动时写进去的，相当于 Linux 的 `/proc/cpuinfo` 头几行；
//! 拓扑那一半是 Linux `/sys/devices/system/cpu/*/topology` 的等价物。
//! 两者都不需要 WMI，也不起子进程（`design.md` §1）。
//!
//! # 为什么手工按字节解析拓扑缓冲
//!
//! `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` 是**变长**结构体：每一项的 `Size`
//! 由它自己声明，`GroupMask[]` 是尾随数组，`windows-sys` 把四种关系包成一个
//! `union`，按 Rust 类型读要对每种关系各写一次 `unsafe` 转换，还是拿不到尾随数组。
//! 直接按字节走一遍反而更短、更清楚，而且 [`parse_logical_processor_info`]
//! 成了纯函数——可以用固定字节缓冲单测，不依赖本机有几个 NUMA 节点。
//!
//! # 逻辑处理器编号
//!
//! Windows 把逻辑处理器按**处理器组**（每组最多 64 个）编号，超过 64 个逻辑
//! 处理器的机器会有多个组。本模块把它们摊平成全局编号 `组号 × 64 + 组内位号`，
//! 与 Linux 的 `cpuN` 一样是一段连续的、从 0 开始的整数——
//! [`CpuPackage::logical_cores`] 与前端的逻辑处理器视图都按这个编号。
//!
//! # 拿不到的字段
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `quota_cores` | Windows 没有 cgroup。作业对象（Job Object）的 `CpuRateControl` 只约束**本进程树**，且以「周期百分比」而非「几个核」计量，语义对不上；容器（Windows Container）里给的是 `JOB_OBJECT_CPU_RATE_CONTROL_*`，同样不是「可用几个核」。填 `None`，不拿它冒充 |
//! | 物理封装 `id` | Windows 的 `PROCESSOR_RELATIONSHIP` **不带封装 id**（Linux 的 `physical_package_id` 没有对应物），只能按枚举顺序给 0、1、2……详见 [`read_cpu_info`] |

use std::collections::BTreeSet;

use strixmaid_types::system::{CpuInfo, CpuPackage};

use crate::platform::windows::{HKLM, reg_dword, reg_string, reg_subkeys};

/// CPU 描述所在的注册表键（`0` 是第一个逻辑处理器）。
pub const CPU0_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
/// 逻辑处理器一人一个子键的父键。
pub const CPU_ROOT_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor";

// `LOGICAL_PROCESSOR_RELATIONSHIP` 的取值。
const RELATION_PROCESSOR_CORE: u32 = 0;
const RELATION_NUMA_NODE: u32 = 1;
const RELATION_PROCESSOR_PACKAGE: u32 = 3;
const RELATION_GROUP: u32 = 4;

/// 一个处理器组最多容纳多少个逻辑处理器（`KAFFINITY` 的位宽，Windows 固定按 64 算）。
const PROCESSORS_PER_GROUP: u32 = 64;

/// `GROUP_AFFINITY` 的字节大小：`KAFFINITY Mask` + `WORD Group` + `WORD Reserved[3]`。
const GROUP_AFFINITY_SIZE: usize = size_of::<usize>() + 8;
/// `PROCESSOR_GROUP_INFO` 的字节大小：两个 `BYTE` + `BYTE Reserved[38]` + `KAFFINITY`。
const PROCESSOR_GROUP_INFO_SIZE: usize = 40 + size_of::<usize>();
/// 各关系结构体里 `GroupCount`（`WORD`）相对整条记录起始处的偏移。
const GROUP_COUNT_OFFSET: usize = 30;
/// 尾随数组（`GroupMask[]` / `GroupInfo[]`）相对整条记录起始处的偏移。
const TRAILING_ARRAY_OFFSET: usize = 32;

/// 从 `GetLogicalProcessorInformationEx` 解出的拓扑。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Topology {
    /// 全部逻辑处理器的全局编号，升序去重。
    pub logical: Vec<u32>,
    /// 物理核数（`RelationProcessorCore` 的条数）。
    pub physical_cores: u32,
    /// NUMA 节点数（`RelationNumaNode` 的条数）。
    pub numa_nodes: u32,
    /// 每个物理封装（`RelationProcessorPackage`）含哪些逻辑处理器，按枚举序。
    pub packages: Vec<Vec<u32>>,
}

/// 解析 `GetLogicalProcessorInformationEx(RelationAll)` 回填的缓冲。
///
/// 纯函数：缓冲怎么来的不关心，畸形数据一律**停在出错处**并返回已解析到的部分，
/// 不 panic、不越界。
pub fn parse_logical_processor_info(buf: &[u8]) -> Topology {
    let mut out = Topology::default();
    let mut logical: BTreeSet<u32> = BTreeSet::new();
    // 没有任何 RelationProcessorCore 时（理论上不会，防御性）用组信息兜底。
    let mut from_groups: BTreeSet<u32> = BTreeSet::new();
    let mut at = 0usize;

    while at + 8 <= buf.len() {
        let relationship = u32_at(buf, at);
        let size = u32_at(buf, at + 4) as usize;
        // Size 必须至少覆盖头部，且整条记录落在缓冲内，否则这份数据已经不可信。
        if size < 8 || at + size > buf.len() {
            break;
        }
        let record = &buf[at..at + size];
        match relationship {
            RELATION_PROCESSOR_CORE => {
                out.physical_cores += 1;
                logical.extend(group_masks(record));
            }
            RELATION_NUMA_NODE => out.numa_nodes += 1,
            RELATION_PROCESSOR_PACKAGE => {
                let mut cores: Vec<u32> = group_masks(record).into_iter().collect();
                cores.sort_unstable();
                out.packages.push(cores);
            }
            RELATION_GROUP => from_groups.extend(group_infos(record)),
            // RelationCache 与将来新增的关系：跳过，靠 Size 走到下一条。
            _ => {}
        }
        at += size;
    }

    if logical.is_empty() {
        logical = from_groups;
    }
    out.logical = logical.into_iter().collect();
    out
}

/// 读 `PROCESSOR_RELATIONSHIP` / `NUMA_NODE_RELATIONSHIP` 尾随的 `GROUP_AFFINITY[]`，
/// 摊平成全局逻辑处理器编号。
///
/// 两种结构体在这一点上布局一致：`GroupCount` 在偏移 30、数组在偏移 32。
fn group_masks(record: &[u8]) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    if record.len() < TRAILING_ARRAY_OFFSET {
        return out;
    }
    let count = u16_at(record, GROUP_COUNT_OFFSET) as usize;
    for i in 0..count {
        let at = TRAILING_ARRAY_OFFSET + i * GROUP_AFFINITY_SIZE;
        if at + GROUP_AFFINITY_SIZE > record.len() {
            break;
        }
        let mask = usize_at(record, at);
        let group = u16_at(record, at + size_of::<usize>()) as u32;
        out.extend(mask_to_ids(mask, group));
    }
    out
}

/// 读 `GROUP_RELATIONSHIP` 尾随的 `PROCESSOR_GROUP_INFO[]` 里的活动处理器掩码。
fn group_infos(record: &[u8]) -> BTreeSet<u32> {
    let mut out = BTreeSet::new();
    if record.len() < TRAILING_ARRAY_OFFSET {
        return out;
    }
    // GROUP_RELATIONSHIP：MaximumGroupCount @8、ActiveGroupCount @10。
    let active = u16_at(record, 10) as usize;
    for i in 0..active {
        let at = TRAILING_ARRAY_OFFSET + i * PROCESSOR_GROUP_INFO_SIZE;
        if at + PROCESSOR_GROUP_INFO_SIZE > record.len() {
            break;
        }
        let mask = usize_at(record, at + 40);
        out.extend(mask_to_ids(mask, i as u32));
    }
    out
}

/// 亲和掩码 → 全局逻辑处理器编号（`组号 × 64 + 位号`）。
fn mask_to_ids(mask: usize, group: u32) -> impl Iterator<Item = u32> {
    let base = group * PROCESSORS_PER_GROUP;
    (0..usize::BITS)
        .filter(move |bit| (mask >> bit) & 1 == 1)
        .map(move |bit| base + bit)
}

fn u16_at(buf: &[u8], at: usize) -> u16 {
    buf.get(at..at + 2)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u16::from_ne_bytes)
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_ne_bytes)
}

fn usize_at(buf: &[u8], at: usize) -> usize {
    buf.get(at..at + size_of::<usize>())
        .and_then(|b| b.try_into().ok())
        .map_or(0, usize::from_ne_bytes)
}

/// 调一次 `GetLogicalProcessorInformationEx(RelationAll)`，返回原始缓冲。
///
/// 失败返回 `None`——调用方退化成注册表 / `GetActiveProcessorCount` 的核数，
/// 少的只是物理核与 NUMA 这两项。
pub fn query_logical_processor_info() -> Option<Vec<u8>> {
    use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows_sys::Win32::System::SystemInformation::{
        GetLogicalProcessorInformationEx, RelationAll, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
    };

    let mut len: u32 = 0;
    // SAFETY: 缓冲传空指针时该 API 只回填所需字节数并返回 FALSE
    // （`GetLastError` 为 ERROR_INSUFFICIENT_BUFFER），这是文档规定的用法。
    unsafe {
        GetLogicalProcessorInformationEx(RelationAll, std::ptr::null_mut(), &raw mut len);
    }
    if len == 0 {
        return None;
    }
    if crate::platform::windows::last_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
    {
        // 不是「缓冲太小」而是别的错误：这次调用本身失败了。
        return None;
    }

    // 缓冲要按结构体对齐（里面有 KAFFINITY / 指针宽度的字段）：用一个
    // SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX 的 Vec 占位，再按字节读。
    let units = len as usize / size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>() + 1;
    let mut aligned: Vec<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX> =
        vec![SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX::default(); units];
    let mut cap = (units * size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()) as u32;
    // SAFETY: aligned 有 cap 字节可写且按该结构体对齐，cap 如实描述其大小。
    let ok = unsafe {
        GetLogicalProcessorInformationEx(RelationAll, aligned.as_mut_ptr(), &raw mut cap)
    };
    if ok == 0 {
        return None;
    }
    let bytes = (cap as usize).min(units * size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>());
    // SAFETY: aligned 至少有 bytes 个已初始化字节（刚由内核回填），
    // 且 Vec 在本函数返回前一直存活，复制出来后不再引用它。
    let raw = unsafe { std::slice::from_raw_parts(aligned.as_ptr().cast::<u8>(), bytes) };
    Some(raw.to_vec())
}

/// `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)`：跨处理器组的逻辑处理器总数。
fn active_processor_count() -> Option<u32> {
    use windows_sys::Win32::System::Threading::{ALL_PROCESSOR_GROUPS, GetActiveProcessorCount};
    // SAFETY: 无指针参数；失败返回 0。
    let n = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
    (n > 0).then_some(n)
}

/// 采集 CPU 信息。任何一项读不到都退化成 `None` / 兜底值，不会失败。
pub fn read_cpu_info() -> CpuInfo {
    let topo = query_logical_processor_info()
        .map(|buf| parse_logical_processor_info(&buf))
        .unwrap_or_default();

    let logical_cores = if topo.logical.is_empty() {
        // 拓扑取不到：先问内核要个总数，再退到「注册表里有几个 CentralProcessor 子键」
        // （HAL 给每个逻辑处理器建一个），最后兜底 1。
        active_processor_count()
            .or_else(|| {
                let n = reg_subkeys(HKLM, CPU_ROOT_KEY)
                    .iter()
                    .filter(|k| k.bytes().all(|b| b.is_ascii_digit()))
                    .count() as u32;
                (n > 0).then_some(n)
            })
            .unwrap_or(1)
    } else {
        topo.logical.len() as u32
    };

    CpuInfo {
        model: reg_string(HKLM, CPU0_KEY, "ProcessorNameString")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Unknown CPU".to_owned()),
        vendor: reg_string(HKLM, CPU0_KEY, "VendorIdentifier"),
        logical_cores,
        physical_cores: (topo.physical_cores > 0).then_some(topo.physical_cores),
        numa_nodes: (topo.numa_nodes > 0).then_some(topo.numa_nodes),
        // `~MHz` 是 HAL 启动时测出的**标称**主频，不随调频变化。
        // DTO 说这一项「会随调频变化，仅供展示」，Windows 上它其实是恒定值——
        // 实时主频要走 `\Processor Information(*)\% Processor Performance`，
        // 那是指标的事，不在静态信息里。
        mhz: reg_dword(HKLM, CPU0_KEY, "~MHz")
            .filter(|v| *v > 0)
            .map(f64::from),
        // 见模块文档「拿不到的字段」
        quota_cores: None,
        packages: to_packages(&topo, logical_cores),
    }
}

/// 物理封装 → 逻辑处理器（roadmap/08 §5.2）。
///
/// Windows 的 `PROCESSOR_RELATIONSHIP` 里**没有封装 id**，只有「这是第几条
/// 封装记录」。因此 `id` 按枚举顺序给 0、1、2……单路机器上永远是 0，与 Linux
/// 的 `physical_package_id` 在绝大多数场景下取值一致；多路机器上它是一个
/// 稳定的序号而不是固件写死的编号，这一点与 Linux 有别。
///
/// 一条封装记录都没有时退化为**一个** id 为 0 的封装含全部逻辑核——
/// 与 Linux 侧读不到 topology 时的退化一致，面板至少还能画成一块。
fn to_packages(topo: &Topology, logical_cores: u32) -> Vec<CpuPackage> {
    if topo.packages.is_empty() {
        return vec![CpuPackage {
            id: 0,
            logical_cores: (0..logical_cores).collect(),
        }];
    }
    topo.packages
        .iter()
        .enumerate()
        .map(|(i, cores)| CpuPackage {
            id: i as u32,
            logical_cores: cores.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼一条 `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX` 记录：
    /// 头部（Relationship + Size）+ 30 字节到 `GroupCount` 的填充 + 尾随数组。
    fn record(relationship: u32, group_masks: &[(usize, u16)]) -> Vec<u8> {
        let size = TRAILING_ARRAY_OFFSET + group_masks.len() * GROUP_AFFINITY_SIZE;
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(&relationship.to_ne_bytes());
        b.extend_from_slice(&(size as u32).to_ne_bytes());
        b.resize(GROUP_COUNT_OFFSET, 0);
        b.extend_from_slice(&(group_masks.len() as u16).to_ne_bytes());
        for (mask, group) in group_masks {
            b.extend_from_slice(&mask.to_ne_bytes());
            b.extend_from_slice(&group.to_ne_bytes());
            b.extend_from_slice(&[0u8; 6]); // Reserved[3]
        }
        assert_eq!(b.len(), size);
        b
    }

    /// 四核八线程、单封装、单 NUMA 节点的典型缓冲。
    fn quad_core_buffer() -> Vec<u8> {
        let mut b = Vec::new();
        // 四个物理核，每核两个逻辑处理器（超线程）
        for core in 0..4u32 {
            let mask = 0b11usize << (core * 2);
            b.extend(record(RELATION_PROCESSOR_CORE, &[(mask, 0)]));
        }
        b.extend(record(RELATION_NUMA_NODE, &[(0xff, 0)]));
        b.extend(record(RELATION_PROCESSOR_PACKAGE, &[(0xff, 0)]));
        b
    }

    #[test]
    fn 解析四核八线程() {
        let topo = parse_logical_processor_info(&quad_core_buffer());
        assert_eq!(topo.physical_cores, 4);
        assert_eq!(topo.numa_nodes, 1);
        assert_eq!(topo.logical, (0..8).collect::<Vec<u32>>());
        assert_eq!(topo.packages, vec![(0..8).collect::<Vec<u32>>()]);
    }

    #[test]
    fn 双封装双_numa() {
        let mut b = Vec::new();
        for core in 0..2u32 {
            b.extend(record(RELATION_PROCESSOR_CORE, &[(1usize << core, 0)]));
        }
        for core in 2..4u32 {
            b.extend(record(RELATION_PROCESSOR_CORE, &[(1usize << core, 0)]));
        }
        b.extend(record(RELATION_NUMA_NODE, &[(0b0011, 0)]));
        b.extend(record(RELATION_NUMA_NODE, &[(0b1100, 0)]));
        b.extend(record(RELATION_PROCESSOR_PACKAGE, &[(0b0011, 0)]));
        b.extend(record(RELATION_PROCESSOR_PACKAGE, &[(0b1100, 0)]));

        let topo = parse_logical_processor_info(&b);
        assert_eq!(topo.physical_cores, 4);
        assert_eq!(topo.numa_nodes, 2);
        assert_eq!(topo.logical, vec![0, 1, 2, 3]);
        assert_eq!(topo.packages, vec![vec![0, 1], vec![2, 3]]);

        let pkgs = to_packages(&topo, 4);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].id, 0);
        assert_eq!(pkgs[1].id, 1);
        assert_eq!(pkgs[1].logical_cores, vec![2, 3]);
    }

    /// 超过 64 个逻辑处理器的机器有多个处理器组，编号要摊平成 `组 × 64 + 位`。
    #[test]
    fn 多处理器组摊平成全局编号() {
        let mut b = Vec::new();
        b.extend(record(RELATION_PROCESSOR_CORE, &[(0b1, 0)]));
        b.extend(record(RELATION_PROCESSOR_CORE, &[(0b1, 1)]));
        b.extend(record(RELATION_PROCESSOR_CORE, &[(0b10, 1)]));
        let topo = parse_logical_processor_info(&b);
        assert_eq!(topo.logical, vec![0, 64, 65]);
        assert_eq!(topo.physical_cores, 3);
    }

    /// 只有 `RelationGroup` 时靠组信息兜底算逻辑核。
    #[test]
    fn 无核记录时用组信息兜底() {
        let size = TRAILING_ARRAY_OFFSET + PROCESSOR_GROUP_INFO_SIZE;
        let mut b = Vec::with_capacity(size);
        b.extend_from_slice(&RELATION_GROUP.to_ne_bytes());
        b.extend_from_slice(&(size as u32).to_ne_bytes());
        b.extend_from_slice(&1u16.to_ne_bytes()); // MaximumGroupCount @8
        b.extend_from_slice(&1u16.to_ne_bytes()); // ActiveGroupCount @10
        b.resize(TRAILING_ARRAY_OFFSET, 0);
        b.push(4); // MaximumProcessorCount
        b.push(4); // ActiveProcessorCount
        b.resize(TRAILING_ARRAY_OFFSET + 40, 0); // Reserved[38]
        b.extend_from_slice(&0b1111usize.to_ne_bytes());
        assert_eq!(b.len(), size);

        let topo = parse_logical_processor_info(&b);
        assert_eq!(topo.logical, vec![0, 1, 2, 3]);
        assert_eq!(topo.physical_cores, 0, "组信息不说有几个物理核");
    }

    #[test]
    fn 畸形缓冲不_panic() {
        // 空缓冲
        assert_eq!(parse_logical_processor_info(&[]), Topology::default());
        // Size 为 0：必须停下，不能死循环
        let zero_size = [0u8; 16];
        assert_eq!(parse_logical_processor_info(&zero_size), Topology::default());
        // Size 超出缓冲：停在这里，返回已解析的部分
        let mut b = quad_core_buffer();
        b.extend_from_slice(&RELATION_PROCESSOR_CORE.to_ne_bytes());
        b.extend_from_slice(&9999u32.to_ne_bytes());
        let topo = parse_logical_processor_info(&b);
        assert_eq!(topo.physical_cores, 4, "越界那条不算数，前面的照常");
        // 半条头部
        let topo = parse_logical_processor_info(&[1, 2, 3]);
        assert_eq!(topo, Topology::default());
        // GroupCount 声称有 100 组，实际数据只有一组：读到哪算哪
        let mut b = record(RELATION_PROCESSOR_CORE, &[(0b11, 0)]);
        b[GROUP_COUNT_OFFSET] = 100;
        let topo = parse_logical_processor_info(&b);
        assert_eq!(topo.logical, vec![0, 1]);
    }

    #[test]
    fn 无封装记录时退化为单封装() {
        let topo = Topology {
            logical: vec![0, 1, 2, 3],
            physical_cores: 4,
            numa_nodes: 1,
            packages: Vec::new(),
        };
        let pkgs = to_packages(&topo, 4);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].id, 0);
        assert_eq!(pkgs[0].logical_cores, vec![0, 1, 2, 3]);
    }

    #[test]
    fn 本机拓扑() {
        let buf = query_logical_processor_info().expect("GetLogicalProcessorInformationEx 应可用");
        assert!(!buf.is_empty());
        let topo = parse_logical_processor_info(&buf);
        assert!(!topo.logical.is_empty(), "至少有一个逻辑处理器");
        assert!(topo.physical_cores >= 1);
        assert!(
            topo.physical_cores as usize <= topo.logical.len(),
            "物理核 {} 不该多于逻辑核 {}",
            topo.physical_cores,
            topo.logical.len()
        );
        assert!(topo.numa_nodes >= 1, "任何机器至少一个 NUMA 节点");
        assert!(!topo.packages.is_empty(), "至少一个物理封装");
        // 逻辑处理器编号必须升序且无重复
        assert!(topo.logical.windows(2).all(|w| w[0] < w[1]));
        eprintln!(
            "本机拓扑：逻辑核 {:?}，物理核 {}，NUMA {}，封装 {:?}",
            topo.logical.len(),
            topo.physical_cores,
            topo.numa_nodes,
            topo.packages
        );
    }

    #[test]
    fn 本机_cpu_信息() {
        let cpu = read_cpu_info();
        assert!(!cpu.model.is_empty() && cpu.model != "Unknown CPU", "型号：{}", cpu.model);
        assert!(cpu.logical_cores >= 1);
        assert!(cpu.physical_cores.is_some_and(|n| n >= 1));
        assert!(cpu.numa_nodes.is_some_and(|n| n >= 1));
        assert_eq!(cpu.quota_cores, None, "Windows 没有 cgroup 配额");
        assert!(!cpu.packages.is_empty());
        // 各封装的逻辑核加起来不该超过总数
        let in_packages: usize = cpu.packages.iter().map(|p| p.logical_cores.len()).sum();
        assert!(in_packages <= cpu.logical_cores as usize * 2, "封装里的核数离谱：{in_packages}");
        eprintln!("本机 CpuInfo：{}", serde_json::to_string(&cpu).unwrap());
    }
}
