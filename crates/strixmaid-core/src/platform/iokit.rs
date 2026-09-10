//! IOKit 只读 FFI：块设备统计与 GPU 统计的取数通道。
//!
//! 与 helper 的 PAM FFI 同一取向：**手写最小表面**，不为两个采集器引一整套
//! core-foundation 依赖树。只封装本项目用到的读路径：
//!
//! - `IOBlockStorageDriver` 的 `Statistics` 字典（`iostat` 的数据源）；
//! - `IOAccelerator` 的 `PerformanceStatistics` 字典（`ioreg -c IOAccelerator` 可见）。
//!
//! # 内存规则（CF 的 Create/Get 约定）
//!
//! - `IORegistryEntryCreateCFProperties` / `IORegistryEntryCreateCFProperty` 是
//!   **Create**：调用方持有，[`OwnedCf`] 负责 `CFRelease`；
//! - `CFDictionaryGetValue` 是 **Get**：借用，不释放；
//! - `io_object_t` 一律 [`IoObj`] RAII 释放。
//!
//! 全模块无写操作、无特权要求——这些统计对普通用户可读。

#![allow(non_snake_case)]

use std::ffi::{CString, c_char, c_void};

type CFTypeRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFMutableDictionaryRef = *mut c_void;
type CFStringRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFTypeID = usize;
type KernReturn = i32;

const KERN_SUCCESS: KernReturn = 0;
/// `kCFStringEncodingUTF8`
const UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`
const CF_NUMBER_SINT64: CFIndex = 4;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
    /// `matching` 被此调用消费（文档明确），不需要也不能再释放。
    fn IOServiceGetMatchingServices(
        port: u32,
        matching: CFDictionaryRef,
        it: *mut u32,
    ) -> KernReturn;
    fn IOIteratorNext(it: u32) -> u32;
    fn IOObjectRelease(obj: u32) -> KernReturn;
    fn IORegistryEntryCreateCFProperties(
        entry: u32,
        props: *mut CFMutableDictionaryRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> KernReturn;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    fn IORegistryEntryGetChildIterator(
        entry: u32,
        plane: *const c_char,
        it: *mut u32,
    ) -> KernReturn;
    fn IOObjectConformsTo(obj: u32, class: *const c_char) -> u32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        s: *const c_char,
        enc: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: CFIndex, enc: u32) -> u8;
    fn CFNumberGetValue(num: CFTypeRef, ty: CFIndex, out: *mut c_void) -> u8;
    fn CFGetTypeID(v: CFTypeRef) -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFDictionaryGetTypeID() -> CFTypeID;
    fn CFRelease(v: CFTypeRef);
}

/// `io_object_t` 的 RAII。
struct IoObj(u32);

impl Drop for IoObj {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

/// 调用方持有的 CF 对象（Create 规则）。
struct OwnedCf(CFTypeRef);

impl Drop for OwnedCf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// 栈上的 CFString key（Create 规则，用完即弃）。
fn cf_str(s: &str) -> Option<OwnedCf> {
    let c = CString::new(s).ok()?;
    let r = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
    if r.is_null() { None } else { Some(OwnedCf(r)) }
}

fn cf_to_i64(v: CFTypeRef) -> Option<i64> {
    if v.is_null() || unsafe { CFGetTypeID(v) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut out: i64 = 0;
    let ok = unsafe { CFNumberGetValue(v, CF_NUMBER_SINT64, (&raw mut out).cast()) };
    (ok != 0).then_some(out)
}

fn cf_to_string(v: CFTypeRef) -> Option<String> {
    if v.is_null() || unsafe { CFGetTypeID(v) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let mut buf = [0_i8; 256];
    let ok = unsafe { CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) };
    if ok == 0 {
        return None;
    }
    let cstr = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    Some(cstr.to_string_lossy().into_owned())
}

/// 一个 IOService 的属性字典快照。
pub struct ServiceProps {
    dict: OwnedCf,
}

impl ServiceProps {
    /// 顶层整数属性。
    pub fn i64(&self, key: &str) -> Option<i64> {
        let k = cf_str(key)?;
        cf_to_i64(unsafe { CFDictionaryGetValue(self.dict.0, k.0) })
    }

    /// 子字典（如 `Statistics` / `PerformanceStatistics`）里的整数。
    pub fn sub_i64(&self, dict_key: &str, key: &str) -> Option<i64> {
        let dk = cf_str(dict_key)?;
        let sub = unsafe { CFDictionaryGetValue(self.dict.0, dk.0) };
        if sub.is_null() || unsafe { CFGetTypeID(sub) } != unsafe { CFDictionaryGetTypeID() } {
            return None;
        }
        let k = cf_str(key)?;
        cf_to_i64(unsafe { CFDictionaryGetValue(sub, k.0) })
    }
}

/// 一个匹配到的服务及其属性；`bsd_name` 来自它在 IOService 平面下第一个
/// 符合 `IOMedia` 的子节点（整盘介质）。
pub struct Service {
    entry: IoObj,
    /// 属性字典。
    pub props: ServiceProps,
}

impl Service {
    /// 第一个符合 `class` 的子节点上的字符串属性（磁盘用它取 `BSD Name`）。
    pub fn child_string(&self, class: &str, key: &str) -> Option<String> {
        let plane = CString::new("IOService").ok()?;
        let class_c = CString::new(class).ok()?;
        let mut it: u32 = 0;
        let kr = unsafe { IORegistryEntryGetChildIterator(self.entry.0, plane.as_ptr(), &mut it) };
        if kr != KERN_SUCCESS {
            return None;
        }
        let it = IoObj(it);
        loop {
            let child = unsafe { IOIteratorNext(it.0) };
            if child == 0 {
                return None;
            }
            let child = IoObj(child);
            if unsafe { IOObjectConformsTo(child.0, class_c.as_ptr()) } != 0 {
                let k = cf_str(key)?;
                let v = unsafe {
                    IORegistryEntryCreateCFProperty(child.0, k.0, std::ptr::null(), 0)
                };
                if v.is_null() {
                    return None;
                }
                let v = OwnedCf(v);
                return cf_to_string(v.0);
            }
        }
    }
}

/// 枚举一个 IOKit 类（含子类）的全部服务并抓取属性。
///
/// 任何一步失败都只是产出更少的项，不报错——探测不到就是没有（design.md §6）。
pub fn services(class: &str) -> Vec<Service> {
    let Ok(class_c) = CString::new(class) else {
        return Vec::new();
    };
    let matching = unsafe { IOServiceMatching(class_c.as_ptr()) };
    if matching.is_null() {
        return Vec::new();
    }
    let mut it: u32 = 0;
    // matching 被消费，无需释放
    if unsafe { IOServiceGetMatchingServices(0, matching, &mut it) } != KERN_SUCCESS {
        return Vec::new();
    }
    let it = IoObj(it);
    let mut out = Vec::new();
    loop {
        let entry = unsafe { IOIteratorNext(it.0) };
        if entry == 0 {
            break;
        }
        let entry = IoObj(entry);
        let mut props: CFMutableDictionaryRef = std::ptr::null_mut();
        let kr = unsafe {
            IORegistryEntryCreateCFProperties(entry.0, &mut props, std::ptr::null(), 0)
        };
        if kr != KERN_SUCCESS || props.is_null() {
            continue;
        }
        out.push(Service {
            entry,
            props: ServiceProps {
                dict: OwnedCf(props),
            },
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟：在 macOS 上枚举块设备驱动应当至少有一个（内置盘），
    /// 且它的 Statistics 里读得出累计字节数。CI 无此硬件时容忍为空。
    #[test]
    fn 枚举块设备统计() {
        let svcs = services("IOBlockStorageDriver");
        for s in &svcs {
            if let Some(v) = s.props.sub_i64("Statistics", "Bytes (Read)") {
                assert!(v >= 0);
                return;
            }
        }
        // 没有任何统计（虚拟机等）不算失败
    }
}
