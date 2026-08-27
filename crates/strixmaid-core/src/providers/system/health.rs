//! 健康聚合：需重启检测、文件系统容量 / inode 阈值、负载、根文件系统只读。
//!
//! failed units 与 SMART 分别由 service provider 与后续的 SMART 模块补充，
//! 本模块产出的 [`HealthReport::skipped`] 里会如实标出「未检查」。
//!
//! # 本模块不做 I/O
//!
//! 判定规则（阈值、严重级别、版本比较）与平台无关，全部放在这里，靠
//! [`HealthInputs`] 接收采集好的证据；**取证据的那部分按平台分在
//! `linux/health.rs` 与 `macos/health.rs`**。这样两个平台共用同一套判定，
//! 而判定逻辑可以完全用固定输入做单测。

use strixmaid_types::system::{FilesystemInfo, HealthItem, HealthReport, HealthSeverity};

/// 文件系统使用率 / inode 使用率的告警阈值（百分比）。
pub const USAGE_WARNING_PERCENT: f64 = 90.0;
/// 文件系统使用率 / inode 使用率的严重阈值（百分比）。
pub const USAGE_CRITICAL_PERCENT: f64 = 95.0;
/// 1 分钟负载超过「逻辑核数 × 此倍数」时告警。
pub const LOAD_WARNING_FACTOR: f64 = 2.0;

/// 需要重启的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebootReason {
    /// 包管理器留下了 `/run/reboot-required`（Debian / Ubuntu）。
    Marker {
        /// `/run/reboot-required.pkgs` 的内容（触发重启需求的包名列表）。
        packages: Option<String>,
    },
    /// `/boot` 里有比运行中内核更新的内核——通用兜底，覆盖 RHEL / Arch 等没有标记文件的发行版。
    NewerKernel {
        running: String,
        installed: String,
    },
}

/// 纯函数：根据证据判定重启需求。
pub fn reboot_reason(
    marker: bool,
    packages: Option<String>,
    running: &str,
    installed_kernels: &[String],
) -> Option<RebootReason> {
    if marker {
        return Some(RebootReason::Marker { packages });
    }
    let newest = newest_kernel(running, installed_kernels)?;
    Some(RebootReason::NewerKernel {
        running: running.to_owned(),
        installed: newest,
    })
}

/// 在同一「口味」（`-generic` / `-lowlatency` / 无后缀）的候选内核里找比运行中更新的最高版本。
///
/// 口味不同不比较：`6.8.0-71-lowlatency` 比 `6.8.0-71-generic` 字典序大，但装了个
/// 低延迟内核并不意味着需要重启到它。
pub fn newest_kernel(running: &str, candidates: &[String]) -> Option<String> {
    let (run_ver, run_flavour) = split_flavour(running);
    let run_key = version_key(run_ver);
    candidates
        .iter()
        .filter(|c| {
            let (_, flavour) = split_flavour(c);
            flavour == run_flavour
        })
        .filter(|c| version_key(split_flavour(c).0) > run_key)
        .max_by_key(|c| version_key(split_flavour(c).0))
        .cloned()
}

/// `6.8.0-71-generic` → `("6.8.0-71", Some("generic"))`；
/// `5.14.0-427.13.1.el9_4.x86_64` → 整体，口味 `None`（末段含数字，不是口味）。
fn split_flavour(v: &str) -> (&str, Option<&str>) {
    match v.rsplit_once('-') {
        Some((head, tail))
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            (head, Some(tail))
        }
        _ => (v, None),
    }
}

/// 自然排序键：数字段按数值比较，其余按字面比较。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Chunk {
    Num(u64),
    Text(String),
}

fn version_key(v: &str) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut in_digit = false;
    let flush = |buf: &mut String, in_digit: bool, out: &mut Vec<Chunk>| {
        if buf.is_empty() {
            return;
        }
        if in_digit {
            out.push(Chunk::Num(buf.parse().unwrap_or(u64::MAX)));
        } else {
            out.push(Chunk::Text(std::mem::take(buf)));
        }
        buf.clear();
    };
    for c in v.chars() {
        if matches!(c, '.' | '-' | '_') {
            flush(&mut buf, in_digit, &mut out);
            continue;
        }
        if c.is_ascii_digit() != in_digit {
            flush(&mut buf, in_digit, &mut out);
            in_digit = c.is_ascii_digit();
        }
        buf.push(c);
    }
    flush(&mut buf, in_digit, &mut out);
    out
}

/// 一次健康评估所需的全部输入。全部由调用方采集，本模块不做 I/O，便于测试。
#[derive(Debug, Clone, Default)]
pub struct HealthInputs {
    pub ts: i64,
    pub filesystems: Vec<FilesystemInfo>,
    /// 1 分钟负载均值。
    pub load1: Option<f64>,
    pub logical_cores: u32,
    pub reboot: Option<RebootReason>,
    /// 本轮**没有检查**的项，原样进 [`HealthReport::skipped`]。
    ///
    /// 由平台侧填写而不是写死在这里：Linux 跳过的是 `systemd`（failed units 归
    /// service provider）与 `smart`；macOS 上没有 systemd 这个概念，报它只会误导
    /// 前端把「未检查」显示成一项待补的能力。
    pub skipped: Vec<String>,
}

/// 生成健康报告。条目按严重级别降序。
pub fn build_report(inputs: &HealthInputs) -> HealthReport {
    let mut items: Vec<HealthItem> = Vec::new();

    if let Some(reason) = &inputs.reboot {
        items.push(reboot_item(reason));
    }

    for fs in &inputs.filesystems {
        if fs.mount_point == "/" && fs.read_only {
            items.push(HealthItem {
                id: "fs.read_only".into(),
                severity: HealthSeverity::Critical,
                title: "根文件系统处于只读状态".into(),
                detail: Some(format!("{} ({}) 以 ro 挂载，通常意味着磁盘或文件系统出错", fs.device, fs.fs_type)),
                target: Some("/".into()),
            });
        }
        if fs.total_bytes > 0 {
            let pct = fs.used_bytes as f64 / fs.total_bytes as f64 * 100.0;
            if let Some(severity) = usage_severity(pct) {
                items.push(HealthItem {
                    id: "disk.usage".into(),
                    severity,
                    title: format!("{} 已使用 {pct:.1}%", fs.mount_point),
                    detail: Some(format!(
                        "{} ({})：已用 {} / 共 {}，剩余可用 {}",
                        fs.device,
                        fs.fs_type,
                        human_bytes(fs.used_bytes),
                        human_bytes(fs.total_bytes),
                        human_bytes(fs.available_bytes)
                    )),
                    target: Some(fs.mount_point.clone()),
                });
            }
        }
        if let (Some(total), Some(used)) = (fs.inodes_total, fs.inodes_used)
            && total > 0
        {
            let pct = used as f64 / total as f64 * 100.0;
            if let Some(severity) = usage_severity(pct) {
                items.push(HealthItem {
                    id: "disk.inodes".into(),
                    severity,
                    title: format!("{} 的 inode 已使用 {pct:.1}%", fs.mount_point),
                    detail: Some(format!("{used} / {total}；inode 耗尽后即使有剩余空间也无法创建文件")),
                    target: Some(fs.mount_point.clone()),
                });
            }
        }
    }

    if let Some(load1) = inputs.load1
        && inputs.logical_cores > 0
    {
        let limit = inputs.logical_cores as f64 * LOAD_WARNING_FACTOR;
        if load1 > limit {
            items.push(HealthItem {
                id: "load.high".into(),
                severity: HealthSeverity::Warning,
                title: format!("1 分钟负载 {load1:.2} 超过逻辑核数的 {LOAD_WARNING_FACTOR:.0} 倍"),
                detail: Some(format!("逻辑核 {}，阈值 {limit:.0}", inputs.logical_cores)),
                target: None,
            });
        }
    }

    items.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.id.cmp(&b.id)));
    let status = items
        .iter()
        .map(|i| i.severity)
        .max()
        .unwrap_or(HealthSeverity::Ok);

    HealthReport {
        ts: inputs.ts,
        status,
        items,
        skipped: inputs.skipped.clone(),
    }
}

fn reboot_item(reason: &RebootReason) -> HealthItem {
    let detail = match reason {
        RebootReason::Marker { packages } => Some(match packages {
            Some(p) => format!("包管理器标记了需要重启（/run/reboot-required）；相关包：{}", p.split_whitespace().collect::<Vec<_>>().join(", ")),
            None => "包管理器标记了需要重启（/run/reboot-required）".to_owned(),
        }),
        RebootReason::NewerKernel { running, installed } => {
            Some(format!("运行中内核 {running}，已安装更新的内核 {installed}"))
        }
    };
    HealthItem {
        id: "reboot.required".into(),
        severity: HealthSeverity::Warning,
        title: "系统需要重启".into(),
        detail,
        target: None,
    }
}

fn usage_severity(percent: f64) -> Option<HealthSeverity> {
    if percent >= USAGE_CRITICAL_PERCENT {
        Some(HealthSeverity::Critical)
    } else if percent > USAGE_WARNING_PERCENT {
        Some(HealthSeverity::Warning)
    } else {
        None
    }
}

/// 简单的人类可读字节数（详情文本用）。
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(mount: &str, total: u64, used: u64, inodes: Option<(u64, u64)>, ro: bool) -> FilesystemInfo {
        FilesystemInfo {
            mount_point: mount.into(),
            device: "/dev/sda1".into(),
            fs_type: "ext4".into(),
            total_bytes: total,
            used_bytes: used,
            available_bytes: total - used,
            inodes_total: inodes.map(|i| i.0),
            inodes_used: inodes.map(|i| i.1),
            read_only: ro,
        }
    }

    #[test]
    fn 标记文件优先() {
        let r = reboot_reason(true, Some("linux-image-generic".into()), "6.8.0-71-generic", &[]);
        assert_eq!(
            r,
            Some(RebootReason::Marker {
                packages: Some("linux-image-generic".into())
            })
        );
    }

    #[test]
    fn 更新的内核触发兜底() {
        let installed = vec!["6.8.0-71-generic".to_owned(), "6.8.0-138-generic".to_owned()];
        let r = reboot_reason(false, None, "6.8.0-71-generic", &installed);
        assert_eq!(
            r,
            Some(RebootReason::NewerKernel {
                running: "6.8.0-71-generic".into(),
                installed: "6.8.0-138-generic".into()
            })
        );
        // 运行中已是最新
        assert_eq!(reboot_reason(false, None, "6.8.0-138-generic", &installed), None);
        // 只装了更旧的
        assert_eq!(reboot_reason(false, None, "6.9.0-1-generic", &installed), None);
        // /boot 读不到
        assert_eq!(reboot_reason(false, None, "6.8.0-71-generic", &[]), None);
    }

    #[test]
    fn 不同口味不比较() {
        let installed = vec!["6.8.0-71-lowlatency".to_owned(), "6.8.0-71-generic".to_owned()];
        assert_eq!(reboot_reason(false, None, "6.8.0-71-generic", &installed), None);
    }

    #[test]
    fn rhel_风格版本自然排序() {
        let installed = vec![
            "5.14.0-427.13.1.el9_4.x86_64".to_owned(),
            "5.14.0-427.20.1.el9_4.x86_64".to_owned(),
            "5.14.0-427.9.1.el9_4.x86_64".to_owned(),
        ];
        assert_eq!(
            newest_kernel("5.14.0-427.13.1.el9_4.x86_64", &installed).as_deref(),
            Some("5.14.0-427.20.1.el9_4.x86_64")
        );
        // 字典序会把 "9" 排在 "20" 后面，自然排序不会
        assert_eq!(newest_kernel("5.14.0-427.20.1.el9_4.x86_64", &installed), None);
    }

    #[test]
    fn 报告条目与严重级别() {
        let inputs = HealthInputs {
            ts: 1,
            filesystems: vec![
                fs("/", 100, 50, Some((100, 10)), false),
                fs("/var", 100, 92, None, false),
                fs("/data", 100, 96, Some((100, 99)), false),
            ],
            load1: Some(10.0),
            logical_cores: 4,
            reboot: Some(RebootReason::Marker { packages: None }),
            skipped: vec!["systemd".into(), "smart".into()],
        };
        let r = build_report(&inputs);
        assert_eq!(r.status, HealthSeverity::Critical);
        assert_eq!(r.skipped, vec!["systemd", "smart"], "未检查项原样透传");
        let ids: Vec<(&str, Option<&str>, HealthSeverity)> = r
            .items
            .iter()
            .map(|i| (i.id.as_str(), i.target.as_deref(), i.severity))
            .collect();
        // 降序：critical 在前
        assert_eq!(ids[0].2, HealthSeverity::Critical);
        assert!(ids.contains(&("disk.usage", Some("/data"), HealthSeverity::Critical)));
        assert!(ids.contains(&("disk.inodes", Some("/data"), HealthSeverity::Critical)));
        assert!(ids.contains(&("disk.usage", Some("/var"), HealthSeverity::Warning)));
        assert!(ids.contains(&("reboot.required", None, HealthSeverity::Warning)));
        assert!(ids.contains(&("load.high", None, HealthSeverity::Warning)));
        assert!(!ids.iter().any(|i| i.1 == Some("/")), "/ 只用了 50%，不该出现");
        assert_eq!(r.items.len(), 5);
    }

    #[test]
    fn 一切正常时为空列表() {
        let inputs = HealthInputs {
            ts: 1,
            filesystems: vec![fs("/", 100, 10, Some((100, 1)), false)],
            load1: Some(1.0),
            logical_cores: 4,
            reboot: None,
            skipped: Vec::new(),
        };
        let r = build_report(&inputs);
        assert_eq!(r.status, HealthSeverity::Ok);
        assert!(r.items.is_empty());
        assert!(r.skipped.is_empty());
    }

    #[test]
    fn 根只读为_critical() {
        let inputs = HealthInputs {
            filesystems: vec![fs("/", 100, 10, None, true)],
            ..Default::default()
        };
        let r = build_report(&inputs);
        assert_eq!(r.items[0].id, "fs.read_only");
        assert_eq!(r.status, HealthSeverity::Critical);
    }
}
