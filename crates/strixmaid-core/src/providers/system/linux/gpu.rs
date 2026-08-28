//! 显卡拓扑（roadmap/08 §5.3）：枚举 `/sys/class/drm/card*`。
//!
//! **只描述有哪些卡、什么驱动、多少显存、在哪条 PCI 总线上**——实时指标
//! （`gpu.usage` 等）是另一回事，走 `metrics/collect/linux/gpu.rs`。
//!
//! 拓扑枚举与驱动无关：AMD / Intel / NVIDIA / virtio-gpu、乃至服务器上的 BMC
//! 显示芯片（mgag200 / ast）都会被列出，因为它们都在 `/sys/class/drm` 里、都有
//! PCI vendor:device id。每张卡的 [`GpuSource`] 指明它的**指标**是否可用：
//! `gpu_busy_percent` 可读 → `Sysfs`（典型 amdgpu）；否则 `Unavailable`
//! （NVIDIA 需 NVML，P0 不接，见 §12 Q1；Intel 集显与 BMC 芯片本就没有利用率）。

use std::fs;
use std::path::Path;

use strixmaid_types::system::{GpuInfo, GpuSource};

use super::util::{read_trimmed, read_u64};

const DRM_PATH: &str = "/sys/class/drm";

/// 是不是卡目录本身：`card` + 纯数字。`card0-VGA-1`（连接器）、`renderD128`、
/// `version` 都不是。
pub fn is_card_name(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// 枚举全部显卡。目录不存在（无 DRM 的内核 / 容器）返回空。
pub fn read_gpus() -> Vec<GpuInfo> {
    let Ok(rd) = fs::read_dir(DRM_PATH) else {
        return Vec::new();
    };
    let mut cards: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| is_card_name(n))
        .collect();
    cards.sort();
    cards
        .into_iter()
        .map(|card| read_card(&card, &Path::new(DRM_PATH).join(&card).join("device")))
        .collect()
}

fn read_card(card: &str, device: &Path) -> GpuInfo {
    let source = if device.join("gpu_busy_percent").exists() {
        GpuSource::Sysfs
    } else {
        GpuSource::Unavailable
    };
    GpuInfo {
        card: card.to_owned(),
        model: model_of(device),
        driver: driver_of(device),
        vram_bytes: read_u64(device.join("mem_info_vram_total")),
        bus: bus_of(device),
        source,
    }
}

/// 驱动名 = `device/driver` 符号链接的最后一段。
fn driver_of(device: &Path) -> Option<String> {
    let target = fs::read_link(device.join("driver")).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// PCI 地址 = `device` 符号链接指向的目录名（`0000:62:00.0`）。
/// 平台 / 虚拟设备的名字不是 PCI BDF，退回 `None`。
fn bus_of(device: &Path) -> Option<String> {
    let target = fs::read_link(device).ok()?;
    let name = target.file_name()?.to_string_lossy().into_owned();
    // PCI BDF 形如 `dddd:bb:dd.f`，含两个冒号一个点。
    (name.matches(':').count() == 2 && name.contains('.')).then_some(name)
}

/// 由 PCI `vendor` / `device` id 组一个可读型号串：`厂商名 [vendor:device]`。
/// sysfs 没有营销名，这是最诚实、最便宜的可读标识；认不出厂商就只给 `[v:d]`。
fn model_of(device: &Path) -> Option<String> {
    let vendor = read_hex_id(device.join("vendor"))?;
    let dev = read_hex_id(device.join("device"))?;
    let ids = format!("[{vendor:04x}:{dev:04x}]");
    Some(match pci_vendor_name(vendor) {
        Some(name) => format!("{name} {ids}"),
        None => ids,
    })
}

/// 读 `0x1002` 这类 sysfs 十六进制 id。
fn read_hex_id(path: impl AsRef<Path>) -> Option<u16> {
    let raw = read_trimmed(path)?;
    u16::from_str_radix(raw.trim_start_matches("0x"), 16).ok()
}

/// 常见 GPU 厂商的 PCI vendor id → 名字。只列会出现在 `/sys/class/drm` 里的那些。
fn pci_vendor_name(id: u16) -> Option<&'static str> {
    Some(match id {
        0x1002 => "AMD",
        0x10de => "NVIDIA",
        0x8086 => "Intel",
        0x1a03 => "ASPEED",
        0x102b => "Matrox",
        0x1af4 => "Red Hat", // virtio-gpu
        0x1234 => "QEMU",    // bochs-drm
        0x15ad => "VMware",
        0x1414 => "Microsoft", // Hyper-V
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 卡名判定() {
        assert!(is_card_name("card0"));
        assert!(is_card_name("card12"));
        assert!(!is_card_name("card0-VGA-1"));
        assert!(!is_card_name("renderD128"));
        assert!(!is_card_name("version"));
        assert!(!is_card_name("card"));
    }

    #[test]
    fn 厂商映射() {
        assert_eq!(pci_vendor_name(0x1002), Some("AMD"));
        assert_eq!(pci_vendor_name(0x102b), Some("Matrox"));
        assert_eq!(pci_vendor_name(0xffff), None);
    }

    #[test]
    fn 本机枚举不报错且形状合理() {
        // 本机可能有真卡、可能只有 BMC 显示芯片、也可能在无 DRM 的容器里（空）。
        for g in read_gpus() {
            assert!(is_card_name(&g.card));
            if let Some(b) = &g.bus {
                assert_eq!(b.matches(':').count(), 2, "PCI 地址应形如 dddd:bb:dd.f：{b}");
            }
            if let Some(m) = &g.model {
                assert!(m.contains('['), "型号串应含 [vendor:device]：{m}");
            }
        }
    }
}
