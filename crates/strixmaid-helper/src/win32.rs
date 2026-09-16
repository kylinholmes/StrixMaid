//! Windows 侧公用的 Win32 薄封装：宽字符串、错误码、句柄、随机数。
//!
//! # helper 为什么要自带一份
//!
//! `strixmaid-core` 里已经有形状几乎相同的 `platform::windows::{wide, handle}`，
//! 但 helper **不依赖 core**（见 `Cargo.toml` 的依赖表）。helper 是整条认证链上
//! 权限最高的进程：它持有登录令牌、能以任意已认证身份创建进程。依赖越少，
//! 能被第三方代码做手脚的面就越小，也越容易在审计时一眼读完。代价是这几十行
//! 要重写一遍，收益是 helper 的依赖树只有 `serde` / `zeroize` / `windows-sys`。
//!
//! 这里只收两个 Windows 模块（[`crate::auth::windows`] 与
//! [`crate::spawn::windows`]）都要用的东西；只有一处用到的留在各自模块里。
//!
//! # 与 Unix 的对应
//!
//! Unix 侧没有对应文件：那边的系统调用经 `libc` / `nix` 直接可用，
//! 字符串是字节串、错误是 `errno`，不需要转换层。Windows 的 `*W` API 全部收发
//! UTF-16、错误在线程局部的 `GetLastError` 里、句柄要显式关闭，这三件事
//! 每次调用都躲不开，所以值得收在一处。

use std::ffi::c_void;
use std::os::windows::io::{FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Security::Cryptography::ProcessPrng;

/// Rust 字符串 → 以 NUL 结尾的 UTF-16 缓冲，可直接当 `PCWSTR` 传出去。
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 缓冲 → `String`，整段转换。
///
/// 非法代理对用 U+FFFD 替换而不是报错：这些串来自内核与注册表，
/// 没有理由要求它们一定合法，而为一个坏字节让整次登录失败不划算。
pub fn from_wide(buf: &[u16]) -> String {
    String::from_utf16_lossy(buf)
}

/// 从一个以 NUL 结尾的 `*const u16` 读出字符串。
///
/// # Safety
///
/// `ptr` 为空时返回空串；非空时必须指向一段以 NUL 结尾、在本次调用期间
/// 保持有效的 UTF-16 数据。
pub unsafe fn from_wide_ptr(ptr: *const u16) -> String {
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0usize;
    // SAFETY: 调用方保证 ptr 指向以 NUL 结尾的缓冲，因此这个循环必然停下。
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: 上面刚数出 len 个非 NUL 的 u16，这段内存有效。
    from_wide(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// 当前线程的 `GetLastError`。
pub fn last_error_code() -> u32 {
    // SAFETY: GetLastError 无参数、无副作用，只读线程局部的错误码。
    unsafe { GetLastError() }
}

/// `GetLastError` 的人类可读形式（由 `std` 去查系统的消息表）。
pub fn last_error_text() -> String {
    std::io::Error::from_raw_os_error(last_error_code() as i32).to_string()
}

/// 把一个刚从 Win32 拿到的裸句柄装进 [`OwnedHandle`]（drop 时 `CloseHandle`）。
///
/// 两种失败形态都在这里挡掉：`NULL`（`CreateNamedPipeW` 之外的大多数 API）
/// 与 `INVALID_HANDLE_VALUE`（文件 / 管道系 API）。这一步不能省——`OwnedHandle`
/// 用空指针做 niche 优化，把 `NULL` 塞进去是未定义行为。
///
/// # Safety
///
/// `raw` 必须是本进程刚取得、尚无其它所有者的句柄；调用成功后所有权转移给
/// 返回的 [`OwnedHandle`]，调用方不得再自行关闭它。
pub unsafe fn own(raw: HANDLE) -> Option<OwnedHandle> {
    if raw.is_null() || raw == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: 调用方保证句柄有效且独占；上面已排除 NULL 与 INVALID_HANDLE_VALUE。
    Some(unsafe { OwnedHandle::from_raw_handle(raw.cast::<c_void>()) })
}

/// 一段密码学强度的随机字节的十六进制形式，用于命名管道的名字。
///
/// 用 `ProcessPrng`（bcryptprimitives.dll）而不是拉一个 `rand` 依赖进来：
/// 它是 Windows 自己的用户态 PRNG 入口，无需初始化、按文档**不会失败**，
/// 恰好是「只要几十位随机量」这种场合的最小解。
///
/// 随机量是安全性的一部分：worker 通道的管道名猜不到，冒名去连的那条路
/// 就不成立（另一半保障见 [`crate::spawn::windows`] 里对 SDDL 的说明）。
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // SAFETY: buf 有 bytes 个可写字节，长度如实给出；ProcessPrng 只写这块缓冲。
    unsafe {
        ProcessPrng(buf.as_mut_ptr(), buf.len());
    }
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        // 手写十六进制：为这一处引 `hex` crate 不值得。
        out.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(b & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 宽字符串往返() {
        for s in ["", "C:\\Windows", "用户名", "alice@contoso.com"] {
            let w = to_wide(s);
            assert_eq!(*w.last().unwrap(), 0, "必须以 NUL 结尾");
            // SAFETY: w 是本函数持有的、以 NUL 结尾的缓冲。
            assert_eq!(unsafe { from_wide_ptr(w.as_ptr()) }, s);
        }
        // SAFETY: 空指针分支不解引用。
        assert_eq!(unsafe { from_wide_ptr(std::ptr::null()) }, "");
    }

    #[test]
    fn 空句柄与无效句柄都当失败() {
        // SAFETY: 两个值都不是真句柄，函数在解引用之前就返回 None。
        assert!(unsafe { own(std::ptr::null_mut()) }.is_none());
        // SAFETY: 同上。
        assert!(unsafe { own(INVALID_HANDLE_VALUE) }.is_none());
    }

    #[test]
    fn 随机十六进制长度正确且不重复() {
        let a = random_hex(16);
        let b = random_hex(16);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "两次取值相同说明 PRNG 没在工作");
    }
}
