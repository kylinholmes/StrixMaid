//! 健康证据的 Windows 取证。
//!
//! 判定规则在 [`super::super::health`]，本文件只负责取证。Windows 上能取到的
//! 证据比 Linux 少一项——**没有负载均值**。
//!
//! # 为什么 `load1` 是 `None`
//!
//! 「负载均值」是 Unix 内核维护的一个指数滑动平均（可运行 + 不可中断睡眠的
//! 任务数）。NT 内核**根本不统计这个量**，也没有任何近似物：
//! `\System\Processor Queue Length` 是**瞬时**队列长度，既不是均值、也不含
//! 等待 IO 的线程，拿它去和「逻辑核数 × 2」比会得出完全不同的结论。
//! 因此填 `None`，`build_report` 会跳过 `load.high` 这一条。
//!
//! # 「需要重启」在 Windows 上怎么判
//!
//! Windows 没有 `/run/reboot-required` 那样的标记文件，对应物是两个**注册表键
//! 是否存在**：
//!
//! | 键 | 谁建的 |
//! |---|---|
//! | `…\Component Based Servicing\RebootPending` | CBS（组件服务栈），装/卸系统组件、累积更新后建 |
//! | `…\WindowsUpdate\Auto Update\RebootRequired` | Windows Update 装完补丁后建 |
//!
//! 刻意**不**把 `Session Manager` 的 `PendingFileRenameOperations` 算进来：
//! 那个值在装了任何用到 `MoveFileEx(..., DELAY_UNTIL_REBOOT)` 的软件之后都会
//! 出现，绝大多数并不真的需要重启，算进来会让「需要重启」长期常亮。
//!
//! # 为什么用 [`RebootReason::Marker`]
//!
//! [`RebootReason`] 只有两个变体：`Marker`（包管理器留了标记）与
//! `NewerKernel`（装了比运行中更新的内核）。Windows 上不存在「装了新内核但还没
//! 重启到它」这种可独立观测的状态——系统更新是原子的，装完就等重启，
//! 所以 `NewerKernel` 对不上。
//!
//! `Marker` 的语义正是「包管理器标记了需要重启」，而 **Windows Update 就是
//! Windows 的包管理器**，它建的 `RebootRequired` 键就是那个标记，一一对应。
//! 因此用 `Marker`，并把触发的键名放进 `packages` 字段，让详情文本能说清
//! 到底是谁要求重启的。
//!
//! ⚠ 已知瑕疵：`build_report` 生成的详情文本里写死了
//! 「包管理器标记了需要重启（/run/reboot-required）」这句 Linux 路径。
//! 这是平台无关层的措辞问题，改它要动共享文件，已在交付报告里提出。

use super::super::health::RebootReason;
use crate::platform::windows::HKLM;
use crate::platform::windows::registry::open_read;

/// CBS 的待重启标记键。
pub const CBS_REBOOT_PENDING: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Component Based Servicing\RebootPending";
/// Windows Update 的待重启标记键。
pub const WU_REBOOT_REQUIRED: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\WindowsUpdate\Auto Update\RebootRequired";

/// 从本机采集证据并判定是否需要重启。
///
/// 两个标记键都不存在时返回 `None`（不需要重启）。
pub fn detect_reboot_required() -> Option<RebootReason> {
    let markers = reboot_markers();
    (!markers.is_empty()).then(|| RebootReason::Marker {
        // `build_report` 会把这个串按空白切开再用 `, ` 连起来，所以这里每一项
        // 本身不能含空格。
        packages: Some(markers.join(" ")),
    })
}

/// 哪些待重启标记存在。返回的每一项都是**不含空格**的标识，见上。
fn reboot_markers() -> Vec<&'static str> {
    [
        ("Component-Based-Servicing", CBS_REBOOT_PENDING),
        ("Windows-Update", WU_REBOOT_REQUIRED),
    ]
    .into_iter()
    .filter(|(_, key)| open_read(HKLM, key).is_ok())
    .map(|(name, _)| name)
    .collect()
}

/// 1 分钟负载均值。Windows 上恒为 `None`，原因见模块文档。
pub fn read_load1() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 负载恒为_none() {
        assert_eq!(read_load1(), None, "NT 内核不统计负载均值，不能编一个");
    }

    #[test]
    fn 标记名不含空格() {
        // `build_report` 按空白切分 `packages`，含空格的项会被切碎。
        for (name, _) in [
            ("Component-Based-Servicing", CBS_REBOOT_PENDING),
            ("Windows-Update", WU_REBOOT_REQUIRED),
        ] {
            assert!(!name.contains(' '), "标记名 {name} 含空格");
        }
    }

    #[test]
    fn 本机重启检测不_panic() {
        // 本机可能待重启、也可能不待重启，只断言取值形状。
        match detect_reboot_required() {
            Some(RebootReason::Marker { packages }) => {
                let p = packages.expect("有标记时必须说明是哪一个");
                assert!(
                    p.split_whitespace()
                        .all(|m| m == "Component-Based-Servicing" || m == "Windows-Update"),
                    "未知的标记：{p}"
                );
                eprintln!("本机待重启，标记：{p}");
            }
            Some(other) => panic!("Windows 上只该用 Marker，实际 {other:?}"),
            None => eprintln!("本机不需要重启"),
        }
    }
}
