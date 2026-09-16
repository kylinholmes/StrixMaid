//! 句柄的 RAII 与两个易错点。
//!
//! # `INVALID_HANDLE_VALUE` 不是空句柄
//!
//! Win32 里「失败」有两种返回：`CreateFileW` 系列返回
//! `INVALID_HANDLE_VALUE`（`-1`），`OpenProcess` / `CreateEventW` 系列返回
//! **空**句柄。把两者混为一谈的后果是对 `-1` 调 `CloseHandle`——那是**当前进程的
//! 伪句柄**，在某些调试器配置下会直接触发异常。因此 [`Owned::new`] 两种都挡。
//!
//! # 为什么不用 `std::os::windows::io::OwnedHandle`
//!
//! 标准库那个只接受「非空」句柄，且 `Drop` 就是 `CloseHandle`——语义上正合适，
//! 本模块也确实用它承载**要交给别人**的句柄（见 [`OwnedHandleExt`]）。
//! 但它的构造要求 `unsafe` 且不接受 `INVALID_HANDLE_VALUE`，
//! 而我们大量的 Win32 调用恰好会返回它。[`Owned`] 是那层「先判失败、再接管」
//! 的薄封装，让每个调用点少写三行。

use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle, RawHandle};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

/// `INVALID_HANDLE_VALUE`，写成函数避免各处重复 `as` 转换。
pub const fn invalid_handle() -> HANDLE {
    INVALID_HANDLE_VALUE
}

/// 关一个句柄，忽略结果。
///
/// # Safety
///
/// `h` 必须是一个本进程拥有、尚未关闭的句柄。
pub unsafe fn close_handle(h: HANDLE) {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        return;
    }
    // SAFETY: 调用方保证句柄有效且只关一次。
    unsafe {
        CloseHandle(h);
    }
}

/// 自动关闭的句柄。
#[derive(Debug)]
pub struct Owned(HANDLE);

// SAFETY: 句柄是内核对象的进程内编号，跨线程使用是 Win32 的常规做法；
// 本类型不提供内部可变性，所有权唯一。
unsafe impl Send for Owned {}
// SAFETY: 同上；`&Owned` 只能读出句柄值，真正的并发安全由被指向的内核对象保证。
unsafe impl Sync for Owned {}

impl Owned {
    /// 接管一个刚由 Win32 返回的句柄。空句柄与 `INVALID_HANDLE_VALUE` 都视为失败，
    /// 此时取当前的 Win32 错误码返回。
    ///
    /// # Safety
    ///
    /// `h` 必须是刚由 Win32 返回、尚无其它持有者的句柄。
    pub unsafe fn new(h: HANDLE) -> io::Result<Owned> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            return Err(super::last_error());
        }
        Ok(Owned(h))
    }

    /// 裸句柄。只在调用期间借用，不转移所有权。
    pub fn raw(&self) -> HANDLE {
        self.0
    }

    /// 放弃所有权，把裸句柄交出去（调用方负责关闭）。
    pub fn into_raw(self) -> HANDLE {
        let h = self.0;
        std::mem::forget(self);
        h
    }

    /// 转成标准库的 [`OwnedHandle`]，以便交给 `std` / `tokio` 的 `From*RawHandle`。
    pub fn into_std(self) -> OwnedHandle {
        // SAFETY: `new` 已排除空与 INVALID_HANDLE_VALUE，且所有权在此转移，
        // `into_raw` 之后本类型不再释放它。
        unsafe { OwnedHandle::from_raw_handle(self.into_raw() as RawHandle) }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: 构造时已保证句柄有效，且本类型独占所有权，只关这一次。
        unsafe { close_handle(self.0) };
    }
}

/// 给标准库的 [`OwnedHandle`] 补一个「取裸 HANDLE」的便捷方法。
///
/// `OwnedHandle` 只给 `as_raw_handle() -> RawHandle`（`*mut c_void`），
/// 而 `windows-sys` 的签名要 `HANDLE`。两者在布局上就是同一个东西，
/// 但每个调用点都写一次 `as` 转换既啰嗦又容易写反方向。
pub trait OwnedHandleExt {
    /// 借出裸句柄，所有权不变。
    fn win32(&self) -> HANDLE;
}

impl OwnedHandleExt for OwnedHandle {
    fn win32(&self) -> HANDLE {
        use std::os::windows::io::AsRawHandle as _;
        self.as_raw_handle() as HANDLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{CreateEventW, GetCurrentProcess};

    #[test]
    fn 空句柄与无效句柄都当失败() {
        // SAFETY: 传的就是两个非法值，函数只做判断不解引用。
        assert!(unsafe { Owned::new(std::ptr::null_mut()) }.is_err());
        // SAFETY: 同上。
        assert!(unsafe { Owned::new(invalid_handle()) }.is_err());
    }

    #[test]
    fn 接管与释放() {
        // SAFETY: 四个参数按文档给出：无安全描述符、手动重置、初始无信号、匿名。
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        // SAFETY: h 刚由 CreateEventW 返回，尚无其它持有者。
        let owned = unsafe { Owned::new(h) }.expect("CreateEventW 应当成功");
        assert!(!owned.raw().is_null());
        // into_std 之后由 std 负责关闭，不会二次释放
        let std_handle = owned.into_std();
        assert!(!std_handle.win32().is_null());
    }

    #[test]
    fn 伪句柄不会被关掉() {
        // GetCurrentProcess 返回的是伪句柄 (-1)，关它是错的。
        // SAFETY: 无参数、无副作用。
        let pseudo = unsafe { GetCurrentProcess() };
        // SAFETY: close_handle 对 INVALID_HANDLE_VALUE 直接返回，不会真的去关。
        unsafe { close_handle(pseudo) };
        // 走到这里没崩就说明挡住了；再用一次伪句柄证明它还有效。
        // SAFETY: 无参数、无副作用。
        assert_eq!(unsafe { GetCurrentProcess() }, pseudo);
    }
}
