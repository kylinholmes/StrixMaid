//! 健康证据的 Linux 取证：需重启检测与负载读取。
//!
//! 判定规则在 [`super::super::health`]，本文件只负责把证据从 `/proc` 与 `/boot` 里读出来。

use std::fs;
use std::path::Path;

use super::super::health::{RebootReason, reboot_reason};
use super::util::read_trimmed;

/// 从本机采集证据并判定是否需要重启。
pub fn detect_reboot_required() -> Option<RebootReason> {
    let marker = ["/run/reboot-required", "/var/run/reboot-required"]
        .iter()
        .any(|p| Path::new(p).exists());
    let packages = read_trimmed("/run/reboot-required.pkgs");
    let running = read_trimmed("/proc/sys/kernel/osrelease").unwrap_or_default();
    let installed = boot_kernel_versions();
    reboot_reason(marker, packages, &running, &installed)
}

/// 扫描 `/boot/vmlinuz-*`，返回版本串列表（去掉 `vmlinuz-` 前缀）。
///
/// 跳过 `vmlinuz-linux`（Arch，不带版本）与 `0-rescue-*`（RHEL 救援内核）。
pub fn boot_kernel_versions() -> Vec<String> {
    let Ok(entries) = fs::read_dir("/boot") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_str()?;
            let version = name.strip_prefix("vmlinuz-")?;
            is_kernel_version(version).then(|| version.to_owned())
        })
        .collect()
}

/// 看起来像内核版本串（至少以数字开头且含 `.`，不是 rescue）。
fn is_kernel_version(v: &str) -> bool {
    v.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && v.contains('.')
        && !v.contains("rescue")
        && !v.ends_with(".old")
}

/// `/proc/loadavg` 的 1 分钟负载。
pub fn read_load1() -> Option<f64> {
    read_trimmed("/proc/loadavg")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 内核文件名过滤() {
        assert!(is_kernel_version("6.8.0-71-generic"));
        assert!(!is_kernel_version("linux"));
        assert!(!is_kernel_version(
            "0-rescue-0f38c197b7d54696a5601541413a560e"
        ));
        assert!(!is_kernel_version("6.8.0-71-generic.old"));
    }

    #[test]
    fn 本机采集不_panic() {
        let _ = detect_reboot_required();
        let _ = read_load1();
    }
}
