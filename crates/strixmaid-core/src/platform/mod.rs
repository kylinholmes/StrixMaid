//! 平台原语：某个平台上多个模块共用的底层系统调用封装。
//!
//! 这一层刻意很薄，只放**不含业务判断**的东西：一次 `sysctl`、一次 `getfsstat`、
//! 一个 C 字符数组转 `String`。业务口径（哪些挂载点该采、内存怎么算「可用」）
//! 一律留在调用方。
//!
//! # 为什么只有 macOS
//!
//! Linux 侧不需要这样一层：`/proc` 与 `/sys` 就是文本文件，`procfs` crate 又把
//! 结构化读取包好了，`metrics::collect::linux` 与 `providers::system::linux`
//! 各自 `fs::read_to_string` 即可，没有值得抽取的公共 FFI。
//!
//! macOS 不同——取同一份挂载表要 `getfsstat` 加一串 `unsafe`，
//! [`metrics::collect::macos`](crate::metrics::collect) 与
//! [`providers::system::macos`](crate::providers::system) 都要用；
//! 与其复制两份 `unsafe`，不如在这里写一次、写对。

#[cfg(target_os = "macos")]
pub mod macos;
