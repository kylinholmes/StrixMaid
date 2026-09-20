//! 平台原语：某个平台上多个模块共用的底层系统调用封装。
//!
//! 这一层刻意很薄，只放**不含业务判断**的东西：一次 `sysctl`、一次 `getfsstat`、
//! 一次 `RegGetValueW`、一个 C 字符数组转 `String`。业务口径（哪些挂载点该采、
//! 内存怎么算「可用」）一律留在调用方。
//!
//! # 为什么 Linux 没有这一层
//!
//! `/proc` 与 `/sys` 就是文本文件，`procfs` crate 又把结构化读取包好了，
//! `metrics::collect::linux` 与 `providers::system::linux` 各自
//! `fs::read_to_string` 即可，没有值得抽取的公共 FFI。
//!
//! macOS 与 Windows 不同——它们的每一次取数都是一串 `unsafe`，且**同一个原语
//! 被多个模块共用**：
//!
//! | 平台 | 共用原语 | 谁在用 |
//! |---|---|---|
//! | macOS | `sysctl` / `getfsstat` | `metrics::collect::macos`、`providers::system::macos` |
//! | macOS | IOKit 只读属性 | 磁盘与 GPU 两个采集器 |
//! | macOS | AppKit 的类型图标（objc_msgSend） | 文件类型图标；进程图标的 macOS 侧日后也走它 |
//! | Windows | 注册表、宽字符、句柄 RAII | 几乎所有 Windows 侧模块 |
//! | Windows | `NtQuerySystemInformation` | 进程 provider、CPU / 负载采集器 |
//! | Windows | 卷与物理盘枚举 | 文件系统与磁盘采集器、`providers::system::windows` |
//! | Windows | 令牌与 SID | 进程 provider、能力探测、会话层 |
//!
//! 与其把同一串 `unsafe` 复制两遍，不如在这里写一次、写对。

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub mod iokit;

#[cfg(target_os = "macos")]
pub mod appkit;

#[cfg(windows)]
pub mod windows;
