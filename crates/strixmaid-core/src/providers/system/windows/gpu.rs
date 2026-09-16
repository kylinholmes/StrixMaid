//! 显卡拓扑（roadmap/08 §5.3）：枚举显示适配器类的注册表键。
//!
//! **只描述有哪些卡、什么驱动、多少显存**——实时指标（`gpu.usage` 等）走
//! `metrics/collect/windows/gpu.rs` 的 PDH 计数器，是另一回事。
//!
//! # 数据源
//!
//! `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}`
//! 是「显示适配器」设备安装类，每装一个显示驱动就多一个四位数字子键
//! （`0000`、`0001`……）。这是 Windows 上与 `/sys/class/drm/card*` 同级的东西：
//! 由内核的即插即用管理器维护，不需要 WMI、不需要 DXGI（那要起 COM）。
//!
//! | `GpuInfo` 字段 | 注册表值 |
//! |---|---|
//! | `model` | `DriverDesc`（营销名，如 `AMD Radeon R7 350 Series`） |
//! | `vram_bytes` | `HardwareInformation.qwMemorySize`（退回 32 位的 `…MemorySize`） |
//! | `driver` | `ProviderName` + `DriverVersion` |
//! | `bus` | **没有**，见下 |
//!
//! # 枚举出来的不都是「显卡」
//!
//! 远程桌面的 `Microsoft Remote Display Adapter`、各种远控软件的虚拟显示器
//! （IDD 驱动）都注册在同一个类下，本机实测就有两个。**照列不过滤**——
//! 与 Linux 侧「`/sys/class/drm` 里的 virtio-gpu、BMC 显示芯片都会被列出」
//! 同一口径：拓扑枚举与驱动无关，能不能取到指标由 `source` 说明。
//! 这类虚拟适配器没有 `qwMemorySize`，`vram_bytes` 自然是 `None`。
//!
//! # `bus` 为什么是 `None`
//!
//! DTO 的 `bus` 要的是 PCI 地址（`0000:62:00.0` 形态的 BDF）。显示类键里**没有
//! 这个信息**——只有 `MatchingDeviceId`（`PCI\VEN_1002&DEV_683F&SUBSYS_…`），
//! 那是「厂商 id + 设备 id」，不是总线上的位置；同一型号的两张卡它完全一样。
//! BDF 在 `HKLM\SYSTEM\CurrentControlSet\Enum\PCI\…\LocationInformation` 里，
//! 但那是一句本地化的自然语言（`PCI bus 1, device 0, function 0`），
//! 要把它凑成 `0000:01:00.0` 得先假定语言。**宁可留 `None`**，
//! 不拿 `VEN:DEV` 冒充一个地址——那是两种不同的东西。
//!
//! `VEN`/`DEV` 没有浪费：`DriverDesc` 读不到时用它拼出与 Linux 侧同形状的
//! 型号串 `AMD [1002:683f]`（见 [`model_from_device_id`]）。
//!
//! # `card` 命名与指标标签的一致性
//!
//! Linux 用 DRM 的 `card0`；Windows 没有这个命名，这里用**注册表子键号**
//! 拼成 `gpu0` / `gpu1` / `gpu2`。指标侧从 PDH 实例名
//! （`…_phys_0_eng_0_engtype_3D`）里取 `phys_N` 分组，**两者未必对得上**——
//! `phys_N` 是 WDDM 的物理适配器序号，与设备安装类的子键号是两套编号。
//! 这一点已在交付报告里单独标出，需要与指标侧统一后再定。
//!
//! # `source` 取值的说明
//!
//! [`GpuSource`] 只有 `Sysfs` / `Nvml` / `Unavailable` 三个取值，全是 Linux 的
//! 概念。Windows 上 GPU 指标既不走 sysfs 也不走 NVML，而是 WDDM 暴露的 PDH
//! 计数器（`\GPU Engine(*)`、`\GPU Adapter Memory(*)`）。这里按**语义**选：
//!
//! - PDH 的 GPU 计数器在本机可用 → [`GpuSource::Sysfs`]，含义是「这张卡的
//!   `gpu.*` 指标可用，走平台原生的只读接口」——这正是前端拿这个字段做的判断；
//! - 计数器不存在（Win10 1709 以前、或 Server Core 没装显示驱动）→
//!   [`GpuSource::Unavailable`]。
//!
//! 报 `Unavailable` 会让前端把明明采得到的曲线藏起来，那比枚举名不贴切更糟。
//! 枚举值本身建议加一个 `Pdh`（或把 `Sysfs` 改名成与平台无关的 `Native`），
//! 但那要改 `strixmaid-types`，已在交付报告里提出，本模块不擅自改。

use strixmaid_types::system::{GpuInfo, GpuSource};

use crate::platform::windows::{HKLM, reg_dword, reg_qword, reg_string, reg_subkeys};

/// 显示适配器设备安装类的 GUID 键。
pub const DISPLAY_CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

/// 探测 GPU 指标可用性时用的 PDH 计数器，按可靠性排序。
///
/// `GPU Adapter Memory` 每个适配器一个实例，**空闲时也在**；
/// `GPU Engine` 的实例是按进程建的，整机没有 GPU 负载时可能一个都没有。
/// 因此先问前者。
const GPU_COUNTER_PATHS: &[&str] = &[
    r"\GPU Adapter Memory(*)\Dedicated Usage",
    r"\GPU Engine(*)\Utilization Percentage",
];

/// 探测 PDH 计数器时的两轮采样间隔。这里只关心「计数器对象在不在」，
/// 不取值，所以给最小间隔。
const PROBE_GAP: std::time::Duration = std::time::Duration::from_millis(1);

/// 是不是显示类下的适配器子键：四位数字。`Configuration` / `Properties`
/// 这两个同级键不是适配器。
pub fn adapter_key_index(name: &str) -> Option<u32> {
    (name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit()))
        .then(|| name.parse().ok())
        .flatten()
}

/// 枚举全部显示适配器，按子键号排序。类键不存在时返回空表。
pub fn read_gpus() -> Vec<GpuInfo> {
    let source = if gpu_counters_available() {
        GpuSource::Sysfs
    } else {
        GpuSource::Unavailable
    };

    let mut indices: Vec<u32> = reg_subkeys(HKLM, DISPLAY_CLASS_KEY)
        .iter()
        .filter_map(|n| adapter_key_index(n))
        .collect();
    indices.sort_unstable();
    indices
        .into_iter()
        .map(|i| read_adapter(i, source))
        .collect()
}

fn read_adapter(index: u32, source: GpuSource) -> GpuInfo {
    let key = format!("{DISPLAY_CLASS_KEY}\\{index:04}");
    let device_id = reg_string(HKLM, &key, "MatchingDeviceId");
    GpuInfo {
        card: format!("gpu{index}"),
        model: reg_string(HKLM, &key, "DriverDesc")
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            // 驱动没写描述串时，退回与 Linux 同形状的 `厂商 [ven:dev]`。
            .or_else(|| device_id.as_deref().and_then(model_from_device_id)),
        driver: driver_string(&key),
        vram_bytes: vram_bytes(&key),
        // 见模块文档「`bus` 为什么是 `None`」
        bus: None,
        source,
    }
}

/// 驱动标识：`ProviderName`，能读到版本时附上。
///
/// Windows 没有 Linux 那种「内核模块名」（`amdgpu` / `i915`）。用户可见的对应物
/// 是驱动提供商与版本号，设备管理器显示的也是这两项。
fn driver_string(key: &str) -> Option<String> {
    let provider = reg_string(HKLM, key, "ProviderName")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    let version = reg_string(HKLM, key, "DriverVersion")
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    match (provider, version) {
        (Some(p), Some(v)) => Some(format!("{p} {v}")),
        (Some(p), None) => Some(p),
        (None, v) => v,
    }
}

/// 显存字节数。
///
/// `HardwareInformation.qwMemorySize` 是 `REG_QWORD`（Win8 起）；更老的驱动只写
/// 32 位的 `HardwareInformation.MemorySize`，那个在 4 GiB 以上的卡上会截断，
/// 所以只作退路。两个都没有（虚拟显示适配器、WDDM 之前的驱动）时为 `None`。
fn vram_bytes(key: &str) -> Option<u64> {
    reg_qword(HKLM, key, "HardwareInformation.qwMemorySize")
        .or_else(|| reg_dword(HKLM, key, "HardwareInformation.MemorySize").map(u64::from))
        .filter(|v| *v > 0)
}

/// `PCI\VEN_1002&DEV_683F&SUBSYS_35001787` → `(0x1002, 0x683f)`。
///
/// 非 PCI 的设备 id（`Root\OrayIddDriver`、`RdpIdd_IndirectDisplay` 这类
/// 软件适配器）返回 `None`。纯函数。
pub fn parse_pci_ids(device_id: &str) -> Option<(u16, u16)> {
    let upper = device_id.to_ascii_uppercase();
    let ven = hex_after(&upper, "VEN_")?;
    let dev = hex_after(&upper, "DEV_")?;
    Some((ven, dev))
}

/// 取 `needle` 之后紧跟的 4 位十六进制数。
fn hex_after(haystack: &str, needle: &str) -> Option<u16> {
    let rest = haystack.split_once(needle)?.1;
    let digits: String = rest.chars().take(4).collect();
    (digits.len() == 4)
        .then(|| u16::from_str_radix(&digits, 16).ok())
        .flatten()
}

/// 由 PCI id 组一个可读型号串：`AMD [1002:683f]`，形状与 Linux 侧一致。
/// 认不出厂商就只给 `[ven:dev]`；不是 PCI 设备则 `None`。
pub fn model_from_device_id(device_id: &str) -> Option<String> {
    let (ven, dev) = parse_pci_ids(device_id)?;
    let ids = format!("[{ven:04x}:{dev:04x}]");
    Some(match pci_vendor_name(ven) {
        Some(name) => format!("{name} {ids}"),
        None => ids,
    })
}

/// 常见 GPU 厂商的 PCI vendor id → 名字。与 Linux 侧 `gpu.rs` 同一张表。
fn pci_vendor_name(id: u16) -> Option<&'static str> {
    Some(match id {
        0x1002 => "AMD",
        0x10de => "NVIDIA",
        0x8086 => "Intel",
        0x1a03 => "ASPEED",
        0x102b => "Matrox",
        0x1af4 => "Red Hat",
        0x1234 => "QEMU",
        0x15ad => "VMware",
        0x1414 => "Microsoft",
        _ => return None,
    })
}

/// 本机的 WDDM GPU 性能计数器在不在，见模块文档「`source` 取值的说明」。
fn gpu_counters_available() -> bool {
    GPU_COUNTER_PATHS
        .iter()
        .any(|p| crate::platform::windows::pdh::query_wildcard(p, PROBE_GAP).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 适配器子键判定() {
        assert_eq!(adapter_key_index("0000"), Some(0));
        assert_eq!(adapter_key_index("0002"), Some(2));
        assert_eq!(adapter_key_index("0013"), Some(13));
        // 同级的非适配器键
        assert_eq!(adapter_key_index("Configuration"), None);
        assert_eq!(adapter_key_index("Properties"), None);
        // 位数不对
        assert_eq!(adapter_key_index("000"), None);
        assert_eq!(adapter_key_index("00000"), None);
        assert_eq!(adapter_key_index(""), None);
    }

    #[test]
    fn 设备_id_解析() {
        assert_eq!(
            parse_pci_ids(r"PCI\VEN_1002&DEV_683F&SUBSYS_35001787"),
            Some((0x1002, 0x683f))
        );
        // 小写也要认
        assert_eq!(
            parse_pci_ids(r"pci\ven_10de&dev_1c8d"),
            Some((0x10de, 0x1c8d))
        );
        // 软件适配器不是 PCI 设备
        assert_eq!(parse_pci_ids("RdpIdd_IndirectDisplay"), None);
        assert_eq!(parse_pci_ids(r"Root\OrayIddDriver"), None);
        assert_eq!(parse_pci_ids(""), None);
        // 只有 VEN 没有 DEV
        assert_eq!(parse_pci_ids(r"PCI\VEN_8086"), None);
        // 位数不足
        assert_eq!(parse_pci_ids(r"PCI\VEN_100&DEV_683F"), None);
    }

    #[test]
    fn 型号串兜底() {
        assert_eq!(
            model_from_device_id(r"PCI\VEN_1002&DEV_683F&SUBSYS_35001787").as_deref(),
            Some("AMD [1002:683f]")
        );
        assert_eq!(
            model_from_device_id(r"PCI\VEN_10DE&DEV_2504").as_deref(),
            Some("NVIDIA [10de:2504]")
        );
        // 认不出的厂商只给 id
        assert_eq!(
            model_from_device_id(r"PCI\VEN_FFFF&DEV_0001").as_deref(),
            Some("[ffff:0001]")
        );
        assert_eq!(model_from_device_id("RdpIdd_IndirectDisplay"), None);
    }

    #[test]
    fn 本机显卡枚举() {
        let gpus = read_gpus();
        // 没有显示适配器的机器是存在的（Server Core），只在非空时断言形状。
        for g in &gpus {
            assert!(g.card.starts_with("gpu"), "卡名：{}", g.card);
            assert!(
                g.card[3..].chars().all(|c| c.is_ascii_digit()),
                "卡名后缀必须是数字：{}",
                g.card
            );
            assert_eq!(g.bus, None, "注册表里没有 PCI 地址，见模块文档");
            if let Some(v) = g.vram_bytes {
                assert!(v > 0);
                assert!(v < 1 << 40, "显存 {v} 字节，明显不对");
            }
            if let Some(m) = &g.model {
                assert!(!m.is_empty());
            }
        }
        // 卡名不能重复
        let mut names: Vec<&str> = gpus.iter().map(|g| g.card.as_str()).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "卡名重复了");
        eprintln!("本机 GPU：{}", serde_json::to_string(&gpus).unwrap());
    }

    #[test]
    fn 指标可用性探测不_panic() {
        let available = gpu_counters_available();
        eprintln!("本机 WDDM GPU 计数器可用：{available}");
    }
}
