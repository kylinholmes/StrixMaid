//! 虚拟化 / 容器识别 —— 自己做，不调 `systemd-detect-virt`。
//!
//! 证据来源（全部是读文件，无需特权）：
//!
//! | 证据 | 说明 |
//! |---|---|
//! | `/run/systemd/container` | 容器运行时留下的标记（docker / podman / lxc / systemd-nspawn） |
//! | `/.dockerenv` / `/run/.containerenv` | docker / podman 的经典标记文件 |
//! | `/proc/1/cgroup` | 无 cgroup namespace 时能看到 `docker-*` / `libpod-*` / `lxc/` 等路径 |
//! | `/proc/sys/kernel/osrelease` | 含 `microsoft` / `WSL` → WSL |
//! | `/sys/hypervisor/type` | Xen |
//! | `/sys/class/dmi/id/{sys_vendor,product_name,bios_vendor,board_vendor}` | 各家虚拟机的 DMI 指纹 |
//! | `/proc/cpuinfo` 的 `hypervisor` flag | 有 hypervisor 但认不出是谁 → `vm-other` |
//!
//! 返回值与 `systemd-detect-virt` 的取值一致（`kvm` / `vmware` / `oracle` / `microsoft` /
//! `xen` / `docker` / `podman` / `lxc` / `wsl` / `vm-other` / `container-other` …）。
//! 容器优先于虚拟机：docker 跑在 KVM 上时报告 `docker`，与 systemd 的行为一致。

use std::fs;
use std::path::Path;

use super::util::read_trimmed;

/// 一次识别所需的全部原始证据。字段全是 `Option` / `bool`，读不到就是「没有这项证据」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VirtEvidence {
    /// `/sys/class/dmi/id/sys_vendor`
    pub dmi_sys_vendor: Option<String>,
    /// `/sys/class/dmi/id/product_name`
    pub dmi_product_name: Option<String>,
    /// `/sys/class/dmi/id/bios_vendor`
    pub dmi_bios_vendor: Option<String>,
    /// `/sys/class/dmi/id/board_vendor`
    pub dmi_board_vendor: Option<String>,
    /// `/proc/cpuinfo` 里是否有 `hypervisor` flag（x86 专有）。
    pub cpu_hypervisor_flag: bool,
    /// `/sys/hypervisor/type`（Xen 才有）。
    pub hypervisor_type: Option<String>,
    /// `/proc/1/cgroup` 原文。
    pub pid1_cgroup: Option<String>,
    /// `/run/systemd/container` 内容。
    pub systemd_container: Option<String>,
    /// `/.dockerenv` 是否存在。
    pub dockerenv: bool,
    /// `/run/.containerenv` 是否存在。
    pub containerenv: bool,
    /// `/proc/sys/kernel/osrelease`。
    pub osrelease: Option<String>,
}

impl VirtEvidence {
    /// 从本机收集证据。任何一项读不到都不影响其它项。
    pub fn collect() -> Self {
        let dmi = |name: &str| read_trimmed(format!("/sys/class/dmi/id/{name}"));
        Self {
            dmi_sys_vendor: dmi("sys_vendor"),
            dmi_product_name: dmi("product_name"),
            dmi_bios_vendor: dmi("bios_vendor"),
            dmi_board_vendor: dmi("board_vendor"),
            cpu_hypervisor_flag: cpuinfo_has_hypervisor_flag(),
            hypervisor_type: read_trimmed("/sys/hypervisor/type"),
            pid1_cgroup: fs::read_to_string("/proc/1/cgroup").ok(),
            systemd_container: read_trimmed("/run/systemd/container"),
            dockerenv: Path::new("/.dockerenv").exists(),
            containerenv: Path::new("/run/.containerenv").exists(),
            osrelease: read_trimmed("/proc/sys/kernel/osrelease"),
        }
    }
}

/// 识别本机的虚拟化类型。物理机返回 `None`。
pub fn detect_virtualization() -> Option<String> {
    detect(&VirtEvidence::collect()).map(str::to_owned)
}

/// 纯函数：根据证据判定。物理机返回 `None`。
pub fn detect(ev: &VirtEvidence) -> Option<&'static str> {
    if let Some(container) = detect_container(ev) {
        return Some(container);
    }
    if ev
        .osrelease
        .as_deref()
        .is_some_and(|r| r.to_ascii_lowercase().contains("microsoft") || r.contains("WSL"))
    {
        return Some("wsl");
    }
    detect_vm(ev)
}

/// 容器判定。
fn detect_container(ev: &VirtEvidence) -> Option<&'static str> {
    if let Some(kind) = ev.systemd_container.as_deref() {
        return Some(match kind {
            "docker" => "docker",
            "podman" => "podman",
            "lxc" | "lxc-libvirt" => "lxc",
            "systemd-nspawn" => "systemd-nspawn",
            "oci" => "container-other",
            _ => "container-other",
        });
    }
    if ev.dockerenv {
        return Some("docker");
    }
    if ev.containerenv {
        return Some("podman");
    }
    if let Some(cgroup) = ev.pid1_cgroup.as_deref() {
        // 只看路径部分（`:` 之后），避免 v1 的控制器名误匹配。
        for line in cgroup.lines() {
            let path = line.rsplit_once(':').map(|(_, p)| p).unwrap_or(line);
            if path.contains("/docker/") || path.contains("/docker-") {
                return Some("docker");
            }
            if path.contains("libpod-") || path.contains("/podman") {
                return Some("podman");
            }
            if path.contains("/lxc/") || path.contains("lxc.payload") {
                return Some("lxc");
            }
            if path.contains("/machine.slice/machine-") {
                return Some("systemd-nspawn");
            }
        }
    }
    None
}

/// 虚拟机判定。
fn detect_vm(ev: &VirtEvidence) -> Option<&'static str> {
    if ev
        .hypervisor_type
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("xen"))
    {
        return Some("xen");
    }

    // 把四个 DMI 字段拼成一个小写串做子串匹配：各家虚拟机把指纹放的位置不完全一样
    // （VirtualBox 在 product_name，Xen 在 sys_vendor，QEMU 两处都有）。
    let dmi = [
        ev.dmi_sys_vendor.as_deref(),
        ev.dmi_product_name.as_deref(),
        ev.dmi_bios_vendor.as_deref(),
        ev.dmi_board_vendor.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(str::to_ascii_lowercase)
    .collect::<Vec<_>>()
    .join("|");
    let product = ev
        .dmi_product_name
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();

    let has = |needle: &str| dmi.contains(needle);

    if has("qemu") || has("kvm") {
        return Some("kvm");
    }
    if has("vmware") {
        return Some("vmware");
    }
    // 只认 VirtualBox 自己的指纹：物理 Oracle 服务器的 sys_vendor 也是 "Oracle Corporation"。
    if has("virtualbox") || has("innotek") {
        return Some("oracle");
    }
    if has("xen") {
        return Some("xen");
    }
    // Hyper-V：sys_vendor 是 Microsoft Corporation，但 Surface 也是——必须看 product_name。
    if has("microsoft corporation") && product.contains("virtual machine") {
        return Some("microsoft");
    }
    if has("parallels") {
        return Some("parallels");
    }
    if has("bhyve") {
        return Some("bhyve");
    }
    if has("apple virtualization") {
        return Some("apple");
    }
    if has("bochs") {
        return Some("bochs");
    }
    // 裸金属 EC2（*.metal）的 DMI 同样写 Amazon EC2，靠 hypervisor flag 区分。
    if has("amazon ec2") && ev.cpu_hypervisor_flag {
        return Some("amazon");
    }
    if has("google") && product.contains("compute engine") {
        return Some("google");
    }
    if ev.cpu_hypervisor_flag {
        return Some("vm-other");
    }
    None
}

/// `/proc/cpuinfo` 是否有 `hypervisor` flag。只看第一个 CPU 的 `flags` 行。
fn cpuinfo_has_hypervisor_flag() -> bool {
    let Ok(raw) = fs::read_to_string("/proc/cpuinfo") else {
        return false;
    };
    raw.lines()
        .find(|l| l.starts_with("flags"))
        .and_then(|l| l.split_once(':'))
        .is_some_and(|(_, flags)| flags.split_whitespace().any(|f| f == "hypervisor"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare_metal() -> VirtEvidence {
        VirtEvidence {
            dmi_sys_vendor: Some("Dell Inc.".into()),
            dmi_product_name: Some("PowerEdge R7625".into()),
            dmi_bios_vendor: Some("Dell Inc.".into()),
            dmi_board_vendor: Some("Dell Inc.".into()),
            pid1_cgroup: Some("0::/init.scope\n".into()),
            osrelease: Some("6.8.0-71-generic".into()),
            ..Default::default()
        }
    }

    #[test]
    fn 物理机为_none() {
        assert_eq!(detect(&bare_metal()), None);
        assert_eq!(detect(&VirtEvidence::default()), None);
    }

    #[test]
    fn kvm_qemu() {
        let ev = VirtEvidence {
            dmi_sys_vendor: Some("QEMU".into()),
            dmi_product_name: Some("Standard PC (Q35 + ICH9, 2009)".into()),
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&ev), Some("kvm"));
        let ev = VirtEvidence {
            dmi_sys_vendor: Some("Red Hat".into()),
            dmi_product_name: Some("KVM".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&ev), Some("kvm"));
    }

    #[test]
    fn vmware_virtualbox_hyperv_xen() {
        let vmware = VirtEvidence {
            dmi_sys_vendor: Some("VMware, Inc.".into()),
            dmi_product_name: Some("VMware Virtual Platform".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&vmware), Some("vmware"));

        let vbox = VirtEvidence {
            dmi_sys_vendor: Some("innotek GmbH".into()),
            dmi_product_name: Some("VirtualBox".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&vbox), Some("oracle"));

        // 物理 Oracle 服务器不能被误判成 VirtualBox
        let sun = VirtEvidence {
            dmi_sys_vendor: Some("Oracle Corporation".into()),
            dmi_product_name: Some("SUN FIRE X4170 M2 SERVER".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&sun), None);

        let hyperv = VirtEvidence {
            dmi_sys_vendor: Some("Microsoft Corporation".into()),
            dmi_product_name: Some("Virtual Machine".into()),
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&hyperv), Some("microsoft"));

        // Surface 也是 Microsoft Corporation，但不是虚拟机
        let surface = VirtEvidence {
            dmi_sys_vendor: Some("Microsoft Corporation".into()),
            dmi_product_name: Some("Surface Pro 7".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&surface), None);

        let xen = VirtEvidence {
            dmi_sys_vendor: Some("Xen".into()),
            dmi_product_name: Some("HVM domU".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&xen), Some("xen"));
        let xen_pv = VirtEvidence {
            hypervisor_type: Some("xen".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&xen_pv), Some("xen"));
    }

    #[test]
    fn 云厂商() {
        let ec2 = VirtEvidence {
            dmi_sys_vendor: Some("Amazon EC2".into()),
            dmi_product_name: Some("t3.micro".into()),
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&ec2), Some("amazon"));
        // 裸金属 EC2 没有 hypervisor flag
        let metal = VirtEvidence {
            cpu_hypervisor_flag: false,
            ..ec2
        };
        assert_eq!(detect(&metal), None);

        let gce = VirtEvidence {
            dmi_sys_vendor: Some("Google".into()),
            dmi_product_name: Some("Google Compute Engine".into()),
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&gce), Some("google"));
    }

    #[test]
    fn 认不出的_hypervisor_为_vm_other() {
        let ev = VirtEvidence {
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&ev), Some("vm-other"));
    }

    #[test]
    fn 容器优先于虚拟机() {
        let docker = VirtEvidence {
            dockerenv: true,
            dmi_sys_vendor: Some("QEMU".into()),
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&docker), Some("docker"));

        let podman = VirtEvidence {
            containerenv: true,
            ..bare_metal()
        };
        assert_eq!(detect(&podman), Some("podman"));

        let by_cgroup = VirtEvidence {
            pid1_cgroup: Some(
                "0::/system.slice/docker-0123456789abcdef.scope\n".into(),
            ),
            ..bare_metal()
        };
        assert_eq!(detect(&by_cgroup), Some("docker"));

        let libpod = VirtEvidence {
            pid1_cgroup: Some("0::/user.slice/user-1000.slice/user@1000.service/user.slice/libpod-abc.scope/container\n".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&libpod), Some("podman"));

        let lxc = VirtEvidence {
            pid1_cgroup: Some("12:pids:/lxc/web1\n0::/lxc.payload.web1/init.scope\n".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&lxc), Some("lxc"));

        let nspawn = VirtEvidence {
            systemd_container: Some("systemd-nspawn".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&nspawn), Some("systemd-nspawn"));

        let marker = VirtEvidence {
            systemd_container: Some("lxc".into()),
            ..bare_metal()
        };
        assert_eq!(detect(&marker), Some("lxc"));
    }

    #[test]
    fn wsl() {
        let ev = VirtEvidence {
            osrelease: Some("5.15.153.1-microsoft-standard-WSL2".into()),
            dmi_sys_vendor: None,
            dmi_product_name: None,
            dmi_bios_vendor: None,
            dmi_board_vendor: None,
            cpu_hypervisor_flag: true,
            ..bare_metal()
        };
        assert_eq!(detect(&ev), Some("wsl"));
    }

    #[test]
    fn 本机证据可收集() {
        // 只要求不 panic；结果取决于机器。
        let _ = detect_virtualization();
    }
}
