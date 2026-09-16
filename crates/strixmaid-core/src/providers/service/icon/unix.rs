//! 非 Windows 平台的服务图标 —— **尚未实现**，[`available`] 恒为 `false`。
//!
//! Linux 与 macOS 合用这一个文件，而进程图标那边是分开的两个
//! （`providers/process/icon/{linux,macos}.rs`）。理由是这里两个平台的答案**相同**，
//! 而那边不同：进程图标在 macOS 上有 `NSWorkspace::iconForFile:` 这条现成的路，
//! 服务图标没有对应的东西。
//!
//! # 服务的可执行文件本来就没有图标
//!
//! Windows 之所以取得到，是因为服务的 `ImagePath` 指向一个 PE 文件，而 PE 里有
//! 图标资源段。Linux 的 unit `ExecStart=` 指向的是 ELF，macOS 的 launchd
//! `ProgramArguments` 指向的是 Mach-O，两者都不含图形。
//!
//! 两个平台上真正存放图标的地方（freedesktop 图标主题、`.app` 包里的 `.icns`）
//! 都是为**桌面应用**准备的，而服务清一色是没有界面的守护进程：`sshd`、`cron`、
//! `nginx` 在图标主题里没有条目，`com.apple.mDNSResponder` 也没有 `.app` 包。
//! 硬要给就只能回落到「通用可执行文件」那张系统默认图——那等于给每一行都贴一张
//! 与这个服务毫无关系的图，是在展示层编数据。
//!
//! 因此这里不是「还没写」，而是**没有可报的事实**：[`available`] 为 `false`，
//! 按名字与通用图标两个端点一律 404，前端据此留空。
//!
//! 后来人若要实现（例如按 `ExecStart` 的 basename 去匹配同名的 `.desktop` 条目），
//! 只需把下面几个函数写出来，[`super`] 里的缓存与端点一行都不用动。

/// 非 Windows 上恒为 false。见模块文档。
pub fn available() -> bool {
    false
}

/// 服务名 → 可执行文件路径。恒为 `None`，见模块文档。
pub fn binary_path(_name: &str) -> Option<String> {
    None
}

/// 通用图标的来源。恒为 `None`：没有一张属于「服务」这个类别的系统图形可取，
/// 拿通用可执行文件图顶替就是编数据。
pub fn generic_icon_path() -> Option<String> {
    None
}

/// 取一个二进制文件的图标。恒为 `None`，见模块文档。
pub fn icon_png(_binary: &str) -> Option<Vec<u8>> {
    None
}
