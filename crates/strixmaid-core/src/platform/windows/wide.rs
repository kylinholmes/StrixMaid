//! UTF-16 与 Rust 字符串之间的转换。
//!
//! Win32 的 `*W` 系列全部收发以 NUL 结尾的 UTF-16。这里只有三个函数，
//! 但它们出现在几乎每一次调用里，所以边界条件要一次说清：
//!
//! - [`to_wide`] 产出的缓冲**含**结尾 NUL，可直接作为 `PCWSTR` 传出去；
//! - [`from_wide_nul`] 在第一个 NUL 处截断（定长缓冲的常态，例如
//!   `MIB_IF_ROW2::Alias` 是 `[u16; 257]`，其中绝大多数是填充）；
//! - [`from_wide`] 按给定长度整段转换，**不**在 NUL 处停（`LookupAccountSidW`
//!   这类回填长度的 API 用它）。
//!
//! 非法的代理对用 U+FFFD 替换而不是报错：这些字符串是主机名、服务名、
//! 文件路径这类给人看的东西，为一个坏字节让整次采集失败不划算。

/// Rust 字符串 → 以 NUL 结尾的 UTF-16 缓冲。
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 缓冲 → `String`，在第一个 NUL 处截断。
pub fn from_wide_nul(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    from_wide(&buf[..end])
}

/// UTF-16 缓冲 → `String`，整段转换。
pub fn from_wide(buf: &[u16]) -> String {
    String::from_utf16_lossy(buf)
}

/// 从一个以 NUL 结尾的 `*const u16` 读出字符串。
///
/// # Safety
///
/// `ptr` 必须非空，且指向一段以 NUL 结尾、在本次调用期间保持有效的 UTF-16 数据。
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

/// 解析一段 `REG_MULTI_SZ` / `GetLogicalDriveStringsW` 风格的「双 NUL 结尾的
/// NUL 分隔串表」。空串（即连续两个 NUL）表示表结束。
pub fn from_wide_multi(buf: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, c) in buf.iter().enumerate() {
        if *c != 0 {
            continue;
        }
        if i == start {
            // 空串 = 表结束
            break;
        }
        out.push(from_wide(&buf[start..i]));
        start = i + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 往返转换() {
        for s in ["", "C:\\Windows", "服务名", "Ω≈ç√"] {
            let w = to_wide(s);
            assert_eq!(*w.last().unwrap(), 0, "必须以 NUL 结尾");
            assert_eq!(from_wide_nul(&w), s);
        }
    }

    #[test]
    fn 定长缓冲在_nul_处截断() {
        let mut buf = [0u16; 16];
        for (i, c) in "eth0".encode_utf16().enumerate() {
            buf[i] = c;
        }
        assert_eq!(from_wide_nul(&buf), "eth0");
        // 没有 NUL 时整段转换
        let full: Vec<u16> = "abcd".encode_utf16().collect();
        assert_eq!(from_wide_nul(&full), "abcd");
    }

    #[test]
    fn 多串表() {
        // "C:\\\0D:\\\0\0"
        let mut buf: Vec<u16> = Vec::new();
        for s in ["C:\\", "D:\\"] {
            buf.extend(s.encode_utf16());
            buf.push(0);
        }
        buf.push(0);
        assert_eq!(from_wide_multi(&buf), vec!["C:\\", "D:\\"]);
        // 空表
        assert!(from_wide_multi(&[0, 0]).is_empty());
        assert!(from_wide_multi(&[]).is_empty());
    }

    #[test]
    fn 指针转换() {
        let w = to_wide("hello");
        // SAFETY: w 是本函数持有的、以 NUL 结尾的缓冲。
        assert_eq!(unsafe { from_wide_ptr(w.as_ptr()) }, "hello");
        // SAFETY: 空指针分支不解引用。
        assert_eq!(unsafe { from_wide_ptr(std::ptr::null()) }, "");
    }

    /// 坏代理对不能让转换 panic——这些串来自内核与注册表，我们无权要求它们合法。
    #[test]
    fn 非法代理对被替换而不是报错() {
        let bad = [0xD800u16, 0x0041];
        assert!(from_wide(&bad).contains('A'));
    }
}
