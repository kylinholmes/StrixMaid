//! 硬件信息与虚拟化识别：`HKLM\HARDWARE\DESCRIPTION\System\BIOS` + CPUID。
//!
//! # 这个注册表键就是 Windows 上的 DMI
//!
//! Windows 启动时由 HAL 把 SMBIOS（也就是 Linux `/sys/class/dmi/id/` 的数据源）
//! 里的几项抄进 `HKLM\HARDWARE\DESCRIPTION\System\BIOS`，字段一一对应：
//!
//! | `HardwareInfo` | Windows 注册表 | Linux DMI |
//! |---|---|---|
//! | `vendor` | `SystemManufacturer`（退回 `BaseBoardManufacturer`） | `sys_vendor`（退回 `board_vendor`） |
//! | `product` | `SystemProductName`（退回 `BaseBoardProduct`） | `product_name`（退回 `board_name`） |
//! | `bios_version` | `BIOSVersion`，有日期时附上 `BIOSReleaseDate` | `bios_version` |
//! | `serial` | `SystemSerialNumber`（退回 `BaseBoardSerialNumber`） | `product_serial`（需 root） |
//!
//! **与 Linux 的一处差别**：Linux 上 `product_serial` 的 sysfs 权限是 0400，
//! 非特权进程一律读不到；Windows 上这个注册表键对 `Authenticated Users` 可读，
//! 普通权限也能拿到序列号。两边都读不到时一律 `None`，不编。
//!
//! # 厂商占位值
//!
//! 主板厂商经常不填 SMBIOS，留下 `To be filled by O.E.M.`、`System Product Name`
//! 这类占位串（本机实测：`SystemProductName` 就是 `System Product Name`）。
//! [`is_placeholder`] 把它们一律视为「没有」，口径与 Linux 侧 `hardware.rs`
//! 的同名函数一致——显示 `None` 比显示一句厂商忘了改的默认文案有用。
//!
//! # 虚拟化识别：先问 CPU，再问 BIOS
//!
//! 两条证据，按可靠性排序：
//!
//! 1. **CPUID 的 hypervisor 位与厂商叶**（x86 专有）。`CPUID.1:ECX[31]` 为 1 表示
//!    本机跑在某个 hypervisor 之下；此时 `CPUID.0x40000000` 的 EBX/ECX/EDX 是一个
//!    12 字节的 hypervisor 厂商串（`VMwareVMware` / `Microsoft Hv` / `KVMKVMKVM` …）。
//!    这正是 `systemd-detect-virt` 的主判据，**不受厂商是否填了 SMBIOS 影响**。
//! 2. **SMBIOS 的厂商 / 机型串**。CPUID 认不出（或在 ARM64 上根本没有 CPUID）时，
//!    用 `SystemManufacturer` / `SystemProductName` 认常见的几家。
//!
//! 取值词表与 `systemd-detect-virt` 对齐（`vmware` / `oracle` / `kvm` / `qemu` /
//! `microsoft` / `parallels` / `xen` / `bhyve` …）。**物理机返回 `None`**，
//! 不是字符串 `"none"`——这是 DTO 明确要求的。
//!
//! 刻意**不**把 `HKLM\SYSTEM\CurrentControlSet\Services\vmbus` 的存在当成
//! 「跑在 Hyper-V 里」：装了 Hyper-V 角色的**物理宿主机**同样有这个服务，
//! 拿它判定会把宿主机误报成虚拟机。Hyper-V 客户机专属的证据是
//! [`HYPERV_GUEST_KEY`]（只有客户机里的 Integration Services 会建），
//! 它才是 CPUID 之外的兜底。

use strixmaid_types::system::HardwareInfo;

use crate::platform::windows::{HKLM, reg_string};

/// SMBIOS 抄件所在的注册表键。
pub const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";

/// 只有 Hyper-V **客户机**才有的键（宿主机上没有）。
pub const HYPERV_GUEST_KEY: &str = r"SOFTWARE\Microsoft\Virtual Machine\Guest\Parameters";

/// 厂商没填写时 SMBIOS 里常见的占位值，全部视为「没有」。
///
/// 与 Linux 侧 `linux/hardware.rs` 的同名表保持同一口径；Windows 上额外多出
/// `sku`、`system sku`（本机实测 `SystemSKU` 就是 `SKU`）。
const PLACEHOLDERS: &[&str] = &[
    "to be filled by o.e.m.",
    "to be filled by oem",
    "system manufacturer",
    "system product name",
    "system serial number",
    "system version",
    "system sku",
    "sku",
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
    "o.e.m.",
];

/// 是否是厂商未填写的占位值。
pub fn is_placeholder(value: &str) -> bool {
    let lower = value.trim().to_ascii_lowercase();
    lower.is_empty() || PLACEHOLDERS.contains(&lower.as_str())
}

/// 读 BIOS 键的一个值，剔除占位值。
fn bios_field(name: &str) -> Option<String> {
    reg_string(HKLM, BIOS_KEY, name)
        .map(|v| v.trim().to_owned())
        .filter(|v| !is_placeholder(v))
}

/// 读取硬件信息。一个字段都没有时返回 `None`（与 Linux 侧一致）。
pub fn read_hardware() -> Option<HardwareInfo> {
    let hw = HardwareInfo {
        vendor: bios_field("SystemManufacturer").or_else(|| bios_field("BaseBoardManufacturer")),
        product: bios_field("SystemProductName").or_else(|| bios_field("BaseBoardProduct")),
        bios_version: bios_version(),
        serial: bios_field("SystemSerialNumber").or_else(|| bios_field("BaseBoardSerialNumber")),
    };
    (hw != HardwareInfo::default()).then_some(hw)
}

/// BIOS 版本串。
///
/// OEM 主板的 `BIOSVersion` 常常只是 `0802` 这种四位数字，单看没有意义；
/// `BIOSReleaseDate` 能读到时附在后面组成 `0802 (12/01/2022)`。日期**原样附上**，
/// 不重新格式化——它是 SMBIOS 里的字面值（多为 `MM/DD/YYYY`），改写格式反而
/// 可能在某些固件上把日月弄反。
fn bios_version() -> Option<String> {
    let version = bios_field("BIOSVersion")?;
    match bios_field("BIOSReleaseDate") {
        Some(date) => Some(format!("{version} ({date})")),
        None => Some(version),
    }
}

/// 虚拟化识别，见模块文档。物理机返回 `None`。
pub fn detect_virtualization() -> Option<String> {
    if let Some(v) = cpuid_hypervisor_vendor().and_then(|s| virt_from_cpuid_vendor(&s)) {
        return Some(v.to_owned());
    }
    if let Some(v) = virt_from_smbios(
        bios_field("SystemManufacturer").as_deref().unwrap_or(""),
        bios_field("SystemProductName").as_deref().unwrap_or(""),
    ) {
        return Some(v.to_owned());
    }
    // 前两条都认不出（或在 ARM64 上根本没有 CPUID 可问）时的最后一条证据：
    // 这个键只有 Hyper-V **客户机**里的 Integration Services 会建，宿主机上没有。
    crate::platform::windows::registry::open_read(HKLM, HYPERV_GUEST_KEY)
        .is_ok()
        .then(|| "microsoft".to_owned())
}

/// CPUID `0x40000000` 的 hypervisor 厂商串 → `systemd-detect-virt` 的取值。
///
/// 纯函数，可用固定输入单测。认不出返回 `None`（不编一个泛化值——
/// 调用方还有 SMBIOS 那条路可走）。
pub fn virt_from_cpuid_vendor(vendor: &str) -> Option<&'static str> {
    let v = vendor.trim_matches('\0').trim();
    Some(match v {
        "VMwareVMware" => "vmware",
        "Microsoft Hv" => "microsoft",
        "KVMKVMKVM" => "kvm",
        "XenVMMXenVMM" => "xen",
        "VBoxVBoxVBox" => "oracle",
        "TCGTCGTCGTCG" => "qemu",
        "bhyve bhyve" => "bhyve",
        "ACRNACRNACRN" => "acrn",
        "QNXQVMBSQG" => "qnx",
        // Parallels 有两种写法，取决于字节序被怎么读
        "prl hyperv" | " lrpepyh vr" | "lrpepyh vr" => "parallels",
        _ => return None,
    })
}

/// SMBIOS 的厂商 / 机型串 → `systemd-detect-virt` 的取值。纯函数。
pub fn virt_from_smbios(manufacturer: &str, product: &str) -> Option<&'static str> {
    let joined = format!("{manufacturer} {product}").to_ascii_lowercase();
    // Hyper-V 客户机的 SMBIOS 是「Microsoft Corporation」+「Virtual Machine」。
    // 只认 Microsoft 是不够的——Surface 这类真机的厂商也是 Microsoft Corporation。
    if joined.contains("microsoft") && joined.contains("virtual machine") {
        return Some("microsoft");
    }
    for (needle, name) in [
        ("vmware", "vmware"),
        ("virtualbox", "oracle"),
        ("innotek", "oracle"),
        ("qemu", "qemu"),
        ("kvm", "kvm"),
        ("parallels", "parallels"),
        ("xen", "xen"),
        ("bochs", "bochs"),
        ("bhyve", "bhyve"),
        ("google compute engine", "kvm"),
        ("amazon ec2", "amazon"),
    ] {
        if joined.contains(needle) {
            return Some(name);
        }
    }
    None
}

/// 问 CPUID 要 hypervisor 厂商串。不在 hypervisor 里、或不是 x86 时返回 `None`。
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn cpuid_hypervisor_vendor() -> Option<String> {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::__cpuid;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::__cpuid;

    /// `CPUID.1:ECX` 的 hypervisor-present 位。
    const HYPERVISOR_PRESENT: u32 = 1 << 31;
    /// hypervisor 厂商叶。
    const HYPERVISOR_VENDOR_LEAF: u32 = 0x4000_0000;

    // `__cpuid` 是安全函数：CPUID 在任何 x86 / x86_64 上都存在，
    // 无副作用、不访存，因此这里不需要 unsafe。
    let basic = __cpuid(1);
    if basic.ecx & HYPERVISOR_PRESENT == 0 {
        return None;
    }
    // 0x40000000 只在 hypervisor 位为 1 时才有意义，上面刚确认过；
    // 物理机上执行它也没有危害，只是结果无意义。
    let leaf = __cpuid(HYPERVISOR_VENDOR_LEAF);
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&leaf.ebx.to_le_bytes());
    bytes[4..8].copy_from_slice(&leaf.ecx.to_le_bytes());
    bytes[8..12].copy_from_slice(&leaf.edx.to_le_bytes());
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// 非 x86 平台（Windows on ARM）没有 CPUID，只能靠 SMBIOS 与注册表。
#[cfg(not(any(target_arch = "x86", target_arch = "x86_64")))]
fn cpuid_hypervisor_vendor() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 占位值识别() {
        assert!(is_placeholder("To be filled by O.E.M."));
        assert!(is_placeholder("System Product Name"));
        assert!(is_placeholder("SKU"));
        assert!(is_placeholder("Default string"));
        assert!(is_placeholder("   "));
        assert!(is_placeholder(""));
        assert!(!is_placeholder("ASUSTeK COMPUTER INC."));
        assert!(!is_placeholder("PowerEdge R650"));
    }

    #[test]
    fn cpuid_厂商串映射() {
        assert_eq!(virt_from_cpuid_vendor("VMwareVMware"), Some("vmware"));
        assert_eq!(virt_from_cpuid_vendor("Microsoft Hv"), Some("microsoft"));
        // KVM 的串只有 9 个字符，后面补 NUL
        assert_eq!(virt_from_cpuid_vendor("KVMKVMKVM\0\0\0"), Some("kvm"));
        assert_eq!(virt_from_cpuid_vendor("XenVMMXenVMM"), Some("xen"));
        assert_eq!(virt_from_cpuid_vendor("VBoxVBoxVBox"), Some("oracle"));
        assert_eq!(virt_from_cpuid_vendor("TCGTCGTCGTCG"), Some("qemu"));
        // 认不出就是认不出，不给泛化值
        assert_eq!(virt_from_cpuid_vendor("GenuineIntel"), None);
        assert_eq!(virt_from_cpuid_vendor(""), None);
    }

    #[test]
    fn smbios_串映射() {
        assert_eq!(virt_from_smbios("VMware, Inc.", "VMware7,1"), Some("vmware"));
        assert_eq!(
            virt_from_smbios("innotek GmbH", "VirtualBox"),
            Some("oracle")
        );
        assert_eq!(virt_from_smbios("QEMU", "Standard PC (Q35 + ICH9)"), Some("qemu"));
        assert_eq!(
            virt_from_smbios("Microsoft Corporation", "Virtual Machine"),
            Some("microsoft")
        );
        assert_eq!(
            virt_from_smbios("Parallels International", "Parallels Virtual Platform"),
            Some("parallels")
        );
        assert_eq!(virt_from_smbios("Xen", "HVM domU"), Some("xen"));
        // 真机不能误报
        assert_eq!(virt_from_smbios("ASUS", "System Product Name"), None);
        assert_eq!(virt_from_smbios("Dell Inc.", "PowerEdge R650"), None);
        assert_eq!(virt_from_smbios("", ""), None);
        // Surface 这类真机厂商也是 Microsoft，光有厂商名不能算
        assert_eq!(
            virt_from_smbios("Microsoft Corporation", "Surface Laptop 5"),
            None,
            "只认厂商名会把 Surface 误报成 Hyper-V 客户机"
        );
    }

    #[test]
    fn 本机硬件信息() {
        // 物理机、虚拟机都能跑：只断言「有值时不是占位串」。
        let hw = read_hardware();
        if let Some(hw) = &hw {
            for v in [&hw.vendor, &hw.product, &hw.bios_version, &hw.serial]
                .into_iter()
                .flatten()
            {
                assert!(!v.is_empty());
                assert!(!is_placeholder(v), "占位值没被过滤：{v}");
            }
        }
        eprintln!("本机 HardwareInfo：{hw:?}");
    }

    #[test]
    fn 本机虚拟化识别不误报() {
        let v = detect_virtualization();
        if let Some(v) = &v {
            assert!(
                [
                    "vmware",
                    "oracle",
                    "kvm",
                    "qemu",
                    "microsoft",
                    "parallels",
                    "xen",
                    "bochs",
                    "bhyve",
                    "acrn",
                    "qnx",
                    "amazon",
                ]
                .contains(&v.as_str()),
                "未知的虚拟化标识：{v}（取值必须与 systemd-detect-virt 对齐）"
            );
        }
        eprintln!("本机虚拟化：{v:?}（None 表示物理机）");
    }
}
