//! macOS 的进程图标 —— **尚未实现**，[`available`] 恒为 `false`。
//!
//! 本文件是留给后续实现的接手点。缓存、负缓存、并发合并、预热与补热全部在
//! [`super`] 里，是平台无关的；实现 macOS 只需要把下面两个函数写出来，
//! **不需要动 [`super`] 里的任何一行**。
//!
//! # 怎么取图标
//!
//! macOS 的图标不在可执行文件里，而在应用包（`.app`）的 `Contents/Resources`
//! 下，由 `Info.plist` 的 `CFBundleIconFile` 指定；同时还有一层「文档类型 /
//! UTI → 图标」的映射。**不要自己去解 `.icns`**，系统有现成的：
//!
//! 1. 进程的可执行路径由 `proc_pidpath` 取得（[`crate::providers::process::macos`]
//!    已经在用它），得到的是 `Foo.app/Contents/MacOS/Foo` 这样的路径；
//! 2. 向上找到最近的 `.app` 目录——`NSWorkspace` 对 bundle 路径给出的是应用
//!    图标，对 `Contents/MacOS/Foo` 这个可执行文件给出的是通用的「Unix 可执行
//!    文件」图标，两者差别很大；
//! 3. `[[NSWorkspace sharedWorkspace] iconForFile:path]` 得到 `NSImage`；
//! 4. `NSImage` → `NSBitmapImageRep` → `representationUsingType:NSBitmapImageFileTypePNG`
//!    得到 PNG 字节。
//!
//! 尺寸要缩放到 [`super::ICON_SIZE`]：`iconForFile:` 返回的 `NSImage` 里通常
//! 有多个分辨率的表示（16 到 1024 都有），取最接近的那个再缩放。
//!
//! # 依赖的代价要先想清楚
//!
//! 上面全是 Objective-C 运行时调用。本项目在 macOS 上已经有
//! [`crate::platform::iokit`] 那种「只声明用得到的那部分 C 表面」的做法，
//! 但 `NSWorkspace` 是 Objective-C 类而不是 C 函数，要么引一个
//! `objc2` / `objc2-app-kit` 这样的 crate，要么自己写 `objc_msgSend` 的调用。
//! 引入依赖前请在 `Cargo.toml` 的注释里写明理由，与 `png` 那条一样。
//!
//! # [`available`] 要考虑无窗口的服务进程
//!
//! 这是本文件最需要先验证的一点：`NSWorkspace` 属于 AppKit，而 AppKit 在
//! **没有窗口服务器连接**的上下文（`launchd` 的 system domain 守护进程、
//! 没有登录会话的 SSH 环境）里行为不确定——轻则返回通用图标，重则在初始化
//! 时卡住或崩溃。StrixMaid 的主进程正是这样一个守护进程。
//!
//! 因此 `available()` **不能**直接返回 true。可行的判据（实现时择一并写明）：
//!
//! - 检查当前进程是否有窗口服务器连接（`CGSessionCopyCurrentDictionary`
//!   返回非空，或 `[NSApplication sharedApplication]` 能安全创建）；
//! - 或者干脆只在「取图标的动作发生在登录用户的 worker 里」这一前提下开启。
//!
//! 判错的代价不对称：误判为 false 只是没有图标，误判为 true 可能让守护进程挂住。

use std::path::Path;

/// macOS 上暂时恒为 `false`：见模块文档「[`available`] 要考虑无窗口的服务进程」。
///
/// 返回 `false` 时 [`super`] 完全不起预热与补热任务，端点直接 404。
pub fn available() -> bool {
    false
}

/// 尚未实现，恒为 `None`。
///
/// 实现时用 `exe`（`proc_pidpath` 给出的可执行路径，向上找 `.app`），
/// `name` 在 macOS 上用不到——保留它是因为 Linux 那条路只有名字可用，
/// 见 [`super`] 的模块文档。
pub fn icon_png(_name: &str, _exe: Option<&Path>) -> Option<Vec<u8>> {
    None
}
