//! 注册表只读封装。
//!
//! # 为什么读注册表算「直读内核接口」
//!
//! `design.md` §1 的原则是「信息优先从内核接口直读，不依赖守护进程」。
//! Windows 上与 `/proc`、`/sys`、`/etc/os-release` 同级的东西正是注册表——
//! 系统版本、CPU 型号、BIOS、显卡显存、服务配置全在里面，读它们既不需要
//! WMI（一个 COM 守护进程）也不需要 PowerShell 子进程。
//!
//! # 只读
//!
//! 本模块没有写入函数。唯一需要改注册表的场景（改主机名）走
//! `SetComputerNameExW`，那是系统提供的、会同时更新多处状态的正规入口。
//!
//! # 根键为什么包一层 [`RegRoot`]
//!
//! windows-sys 把 `HKEY` 定义成 `*mut c_void`，于是「根键」这个参数在类型上
//! 长得像裸指针，`clippy::not_unsafe_ptr_arg_deref` 会要求整族读取函数都标成
//! `unsafe fn`。但 `HKEY` 并不是本进程地址空间里的地址：`HKEY_LOCAL_MACHINE`
//! 这类预定义根键是内核约定的**伪句柄常量**，其余的由 `RegOpenKeyExW` 颁发。
//! 传一个伪造的 `HKEY` 进去，advapi32 的反应是返回 `ERROR_INVALID_HANDLE`，
//! 与 Unix 上把一个无效 fd 传给 `read(2)` 同性质——是错误，不是未定义行为。
//!
//! 所以这里的修法不是把安全契约推给调用方（那会让几十处 `reg_string(HKLM, …)`
//! 全都套上 `unsafe`，还会波及 `strixmaid-agent`），而是**让裸指针根本不出现在
//! 签名里**：[`RegRoot`] 的字段是私有的，只能由本模块导出的常量（目前只有
//! [`HKLM`]）得到，模块外的代码无从凭空造一个根键。
//!
//! 与 [`super::token`] 的 `PSID` 形成对照：那边的指针是真的指向本进程内一段
//! 变长结构，`GetSidSubAuthority` 会解引用它，因此那几个函数**确实**必须是
//! `unsafe fn`。两者的区别是「句柄」与「指针」的区别，不是风格差异。

use std::io;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_SUCCESS};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_DWORD, RRF_RT_REG_QWORD, RRF_RT_REG_SZ,
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};

use super::wide::{from_wide, to_wide};
use super::{error_from_code, last_error};

/// 一个注册表根键。见模块文档「根键为什么包一层 `RegRoot`」。
///
/// 只有本模块能构造它，因此凡是拿得到 `RegRoot` 的地方，里面的 `HKEY`
/// 必定是一个预定义的根键伪句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegRoot(HKEY);

// SAFETY: HKEY 是进程内的内核对象编号，跨线程使用是 Win32 的常规做法，
// 理由与本文件里 Key 的两个实现相同。
unsafe impl Send for RegRoot {}
// SAFETY: 本类型是不可变的 Copy 值，不含内部可变性。
unsafe impl Sync for RegRoot {}

impl RegRoot {
    /// 裸根键。给本模块没有封装的一次性 `Reg*W` 调用用（如读二进制值），
    /// 与 [`Key::raw`] 同一用途。
    pub fn raw(self) -> HKEY {
        self.0
    }
}

/// `HKEY_LOCAL_MACHINE`，供调用方无需再 `use` 一次 windows-sys。
pub const HKLM: RegRoot = RegRoot(HKEY_LOCAL_MACHINE);

/// 读一个 `REG_SZ` / `REG_EXPAND_SZ` 值。
///
/// 值不存在、类型不符、或读取被拒时返回 `None`——注册表里缺一项是常态
/// （不同 Windows 版本的键位并不一致），调用方一律按「这一项没有」处理。
pub fn reg_string(root: RegRoot, subkey: &str, value: &str) -> Option<String> {
    let sub = to_wide(subkey);
    let val = to_wide(value);
    let mut len: u32 = 0;

    // 第一次只问长度（字节数，含结尾 NUL）。
    // SAFETY: 两个路径串以 NUL 结尾；pvdata 为空时函数只回填 pcbdata。
    let rc = unsafe {
        RegGetValueW(
            root.0,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut len,
        )
    };
    if rc != ERROR_SUCCESS || len < 2 {
        return None;
    }

    let mut buf = vec![0u16; len as usize / 2 + 1];
    let mut cap = (buf.len() * 2) as u32;
    // SAFETY: buf 有 cap 字节可写，cap 如实描述其大小。
    let rc = unsafe {
        RegGetValueW(
            root.0,
            sub.as_ptr(),
            val.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast::<core::ffi::c_void>(),
            &raw mut cap,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    // 回填的是字节数；去掉结尾 NUL 再转。
    let chars = (cap as usize / 2).min(buf.len());
    let s = from_wide(&buf[..chars]);
    let s = s.trim_end_matches('\0').to_owned();
    (!s.is_empty()).then_some(s)
}

/// 读一个 `REG_DWORD` 值。
pub fn reg_dword(root: RegRoot, subkey: &str, value: &str) -> Option<u32> {
    reg_scalar::<u32>(root, subkey, value, RRF_RT_REG_DWORD)
}

/// 读一个 `REG_QWORD` 值。
pub fn reg_qword(root: RegRoot, subkey: &str, value: &str) -> Option<u64> {
    reg_scalar::<u64>(root, subkey, value, RRF_RT_REG_QWORD)
}

fn reg_scalar<T: Copy + Default>(root: RegRoot, subkey: &str, value: &str, flags: u32) -> Option<T> {
    let sub = to_wide(subkey);
    let val = to_wide(value);
    let mut out = T::default();
    let mut len = std::mem::size_of::<T>() as u32;
    // SAFETY: out 是一个完整的 T，len 如实描述其大小；两个路径串以 NUL 结尾。
    let rc = unsafe {
        RegGetValueW(
            root.0,
            sub.as_ptr(),
            val.as_ptr(),
            flags,
            std::ptr::null_mut(),
            (&raw mut out).cast::<core::ffi::c_void>(),
            &raw mut len,
        )
    };
    (rc == ERROR_SUCCESS && len as usize == std::mem::size_of::<T>()).then_some(out)
}

/// 列出一个键下的全部子键名。
///
/// 键不存在或无权限时返回空表（不是错误）——与 Linux 侧 `read_dir` 失败时
/// 返回空列表的处理一致。
pub fn reg_subkeys(root: RegRoot, subkey: &str) -> Vec<String> {
    let Ok(key) = open_read(root, subkey) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        // 注册表键名上限 255 个字符（MSDN），多给一个 NUL 的位置。
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: name 有 len 个 u16 可写；其余输出参数按文档允许为空。
        let rc = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                name.as_mut_ptr(),
                &raw mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        match rc {
            ERROR_SUCCESS => out.push(from_wide(&name[..len as usize])),
            // 名字超过 256 字符：跳过它继续，不让一个畸形键名截断整次枚举。
            ERROR_MORE_DATA => {}
            // ERROR_NO_MORE_ITEMS 或任何错误都表示到此为止。
            _ => break,
        }
        index += 1;
    }
    out
}

/// 打开一个只读键。返回的 [`Key`] 析构时 `RegCloseKey`。
pub fn open_read(root: RegRoot, subkey: &str) -> io::Result<Key> {
    let sub = to_wide(subkey);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: sub 以 NUL 结尾；phkresult 指向本栈帧上的可写变量。
    let rc = unsafe { RegOpenKeyExW(root.0, sub.as_ptr(), 0, KEY_READ, &raw mut key) };
    if rc != ERROR_SUCCESS {
        return Err(if rc == ERROR_SUCCESS {
            last_error()
        } else {
            error_from_code(rc)
        });
    }
    Ok(Key(key))
}

/// 一个打开的注册表键，析构时关闭。
#[derive(Debug)]
pub struct Key(HKEY);

// SAFETY: HKEY 是进程内的内核对象编号，跨线程使用是 Win32 的常规做法。
unsafe impl Send for Key {}
// SAFETY: 本类型只暴露只读操作，且不含内部可变性。
unsafe impl Sync for Key {}

impl Key {
    /// 裸句柄，仅在调用期间借用。
    pub fn raw(&self) -> HKEY {
        self.0
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: 构造成功才有实例，只关一次。
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CURRENT_VERSION: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

    #[test]
    fn 读系统版本串() {
        // 任何 Windows 上都有这一项。
        let name = reg_string(HKLM, CURRENT_VERSION, "ProductName")
            .expect("ProductName 在任何 Windows 上都存在");
        assert!(!name.is_empty());
        assert!(!name.contains('\0'), "结尾 NUL 必须去掉：{name:?}");
        eprintln!("本机 ProductName = {name}");
    }

    #[test]
    fn 读数字值() {
        // CurrentMajorVersionNumber 是 Win10+ 才有的 DWORD；没有就跳过。
        if let Some(major) = reg_dword(HKLM, CURRENT_VERSION, "CurrentMajorVersionNumber") {
            assert!(major >= 6, "主版本号异常：{major}");
        }
    }

    #[test]
    fn 缺失的键与值都返回_none() {
        assert_eq!(reg_string(HKLM, r"SOFTWARE\没有这个键", "x"), None);
        assert_eq!(reg_string(HKLM, CURRENT_VERSION, "没有这个值"), None);
        assert_eq!(reg_dword(HKLM, CURRENT_VERSION, "没有这个值"), None);
        // 类型不符：ProductName 是 REG_SZ，按 DWORD 读必须失败而不是给个垃圾值
        assert_eq!(reg_dword(HKLM, CURRENT_VERSION, "ProductName"), None);
    }

    #[test]
    fn 枚举子键() {
        // 服务表：任何 Windows 上都有几百个子键。
        let services = reg_subkeys(HKLM, r"SYSTEM\CurrentControlSet\Services");
        assert!(services.len() > 50, "只枚举到 {} 个服务", services.len());
        assert!(services.iter().all(|s| !s.is_empty()));
        // 不存在的键给空表而不是 panic
        assert!(reg_subkeys(HKLM, r"SOFTWARE\没有这个键").is_empty());
    }
}
