//! DMI 硬件信息（`/sys/class/dmi/id/`），ARM 板子回落到设备树 `model`。
//!
//! `product_serial` / `board_serial` 只有 root 能读（sysfs 权限 0400）——非特权时
//! 静默为 `None`，不报错。

use std::fs;
use std::path::Path;

use strixmaid_types::system::HardwareInfo;

use super::util::read_trimmed;

const DMI_DIR: &str = "/sys/class/dmi/id";

/// 厂商没填写时 BIOS 里常见的占位值，全部视为「没有」。
const PLACEHOLDERS: &[&str] = &[
    "to be filled by o.e.m.",
    "to be filled by oem",
    "system manufacturer",
    "system product name",
    "system serial number",
    "system version",
    "default string",
    "not specified",
    "not applicable",
    "not available",
    "none",
    "n/a",
    "unknown",
    "undefined",
    "empty",
    "0123456789",
    "oem",
];

/// 读一个 DMI 字段，剔除占位值。
fn dmi_field(name: &str) -> Option<String> {
    read_trimmed(Path::new(DMI_DIR).join(name)).filter(|v| !is_placeholder(v))
}

/// 是否是厂商未填写的占位值。
pub fn is_placeholder(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.is_empty() || PLACEHOLDERS.contains(&lower.as_str())
}

/// 读取硬件信息。容器与多数 ARM 板子上 DMI 不存在；一个字段都没有时返回 `None`。
pub fn read_hardware() -> Option<HardwareInfo> {
    let mut hw = HardwareInfo::default();
    if Path::new(DMI_DIR).is_dir() {
        hw.vendor = dmi_field("sys_vendor").or_else(|| dmi_field("board_vendor"));
        // 一些主板厂商把型号只写在 board_name 里，product_name 留空或占位。
        hw.product = dmi_field("product_name").or_else(|| dmi_field("board_name"));
        hw.bios_version = dmi_field("bios_version");
        hw.serial = dmi_field("product_serial").or_else(|| dmi_field("board_serial"));
    }
    if hw.product.is_none() {
        hw.product = device_tree_model();
    }
    if hw == HardwareInfo::default() {
        None
    } else {
        Some(hw)
    }
}

/// `/proc/device-tree/model`（ARM / RISC-V）。内容以 NUL 结尾，要去掉。
fn device_tree_model() -> Option<String> {
    let raw = fs::read("/proc/device-tree/model").ok()?;
    let s = String::from_utf8_lossy(&raw);
    let s = s.trim_end_matches('\0').trim();
    if s.is_empty() || is_placeholder(s) {
        None
    } else {
        Some(s.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 占位值识别() {
        assert!(is_placeholder("To Be Filled By O.E.M."));
        assert!(is_placeholder("Default string"));
        assert!(is_placeholder(""));
        assert!(!is_placeholder("PowerEdge R7625"));
    }

    #[test]
    fn 本机读取不_panic() {
        let _ = read_hardware();
    }
}
