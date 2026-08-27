//! helper 的日志：只写 stderr（交由 journald 收集），**只记事件、不记内容**。
//!
//! 本模块刻意没有「打印任意结构体」的入口——helper 进程里流经的东西包括明文密码，
//! 任何 `{:?}` 都是隐患。调用方只能传一句描述事件的话。

use std::io::Write;

/// 记录一条事件。加进程 pid 前缀，便于在 journald 里区分并发的多个 helper。
pub fn event(what: &str) {
    let stderr = std::io::stderr();
    let mut lock = stderr.lock();
    // 写 stderr 失败（如被关闭）没有任何补救手段，忽略。
    let _ = writeln!(lock, "strixmaid-helper[{}]: {what}", std::process::id());
}
