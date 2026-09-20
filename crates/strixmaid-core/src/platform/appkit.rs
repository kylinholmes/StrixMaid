//! AppKit 的最小 Objective-C 表面：文件类型 → 系统图标 → PNG。
//!
//! 与 [`super::iokit`] 同一取向——**只声明用得到的那部分**，不引 `objc2` /
//! `objc2-app-kit` 这类封装 crate：本文件全部需求是「几个类、十来个选择子」，
//! 为它拉进一整套 objc 绑定（连同宏与版本适配层）不成比例。代价是每个
//! `objc_msgSend` 都要自己写对签名，这些签名集中在下面一组小函数里，
//! 每个只出现一次。
//!
//! # 这条链路做什么
//!
//! ```text
//! 扩展名 → UTType typeWithFilenameExtension:（macOS 11+）
//!        → NSWorkspace iconForContentType:（macOS 12+，老系统回落 iconForFileType:）
//!        → NSImage 画进 NSBitmapImageRep（固定像素尺寸）
//!        → representationUsingType:NSBitmapImageFileTypePNG → PNG 字节
//! ```
//!
//! PNG 编码交给 AppKit 而不是 `png` crate：那个依赖在本项目里是 Windows 专属
//! （HICON 没有现成编码器），macOS 的 `NSBitmapImageRep` 自带 PNG 输出，
//! 再引一条依赖只是多一份重复。
//!
//! # [`available`] 的判据：有没有窗口服务器连接
//!
//! AppKit 在没有窗口服务器连接的上下文（`launchd` system domain 的守护进程、
//! 无登录会话的 SSH）里行为不确定——轻则给通用图标，重则初始化时卡住。
//! 这正是 [`crate::providers::process::icon::macos`] 模块文档里点名要先验证的
//! 一点，判据用的是它给出的第一个：`CGSessionCopyCurrentDictionary()` 返回
//! 非空。判错的代价不对称（误 false 只是没图标，误 true 可能挂住守护进程），
//! 所以宁严勿宽：拿不到会话字典一律 false。
//!
//! # 线程
//!
//! 调用发生在 `spawn_blocking` 的线程池线程上，不在主线程。这里没有
//! `NSApplication`、没有窗口，只有「画进自备的位图上下文」——Apple 文档明确
//! `NSImage` 可以在多线程使用，绘制目标又是本线程私有的
//! `NSGraphicsContext`，不触碰主线程专属的状态。

use std::ffi::{CStr, CString, c_char, c_void};

/// Objective-C 对象指针。空指针即 `nil`。
type Id = *mut c_void;
/// 选择子。
type Sel = *mut c_void;
type NSInteger = isize;
type NSUInteger = usize;
/// Objective-C 的 `BOOL`：x86_64 上是 `signed char`，arm64 上是 C `bool`，
/// 都是 1 字节按整型寄存器传参，`i8` 两边 ABI 都对。
type ObjcBool = i8;

/// `NSBitmapImageFileTypePNG`。
const PNG_FILE_TYPE: NSUInteger = 4;
/// `NSCompositingOperationSourceOver`。
const SOURCE_OVER: NSUInteger = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}

/// `NSRect` 就是 `CGRect`：四个 `f64`。arm64 上按同构浮点聚合走浮点寄存器、
/// x86_64 上按内存类传栈——把 `objc_msgSend` 转成**具体签名**后由编译器排布，
/// 这正是下面所有调用都先转具体函数类型的原因。
#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Id;
    fn sel_registerName(name: *const c_char) -> Sel;
    /// 真实签名随消息而变，**必须**先转成具体函数类型再调用，
    /// 见各 `msg_*` 包装。
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> *mut c_void;
    fn objc_autoreleasePoolPop(pool: *mut c_void);
}

// 链接 AppKit 是为了 NSWorkspace / NSImage / NSBitmapImageRep 这些类在
// 运行时注册；`NSDeviceRGBColorSpace` 这个 NSString 全局顺带从这里来。
#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {
    static NSDeviceRGBColorSpace: Id;
}

// 只为让 `UTType` 类被注册进 objc 运行时，没有要直接调用的 C 符号。
#[link(name = "UniformTypeIdentifiers", kind = "framework")]
unsafe extern "C" {}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// 有窗口服务器连接时返回当前登录会话的属性字典，否则 NULL。
    fn CGSessionCopyCurrentDictionary() -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    // 签名与 [`super::iokit`] 里的声明保持一致（`*const`）：同名外部符号
    // 声明不一致会触发 `clashing_extern_declarations`。
    fn CFRelease(cf: *const c_void);
}

// ===========================================================================
// objc_msgSend 的具体签名
// ===========================================================================

/// 把 `objc_msgSend` 转成具体签名。每个签名只在一处使用，转错会在
/// 对应那一条消息上立刻表现出来，而不是感染整个文件。
macro_rules! msg_send {
    ($ret:ty, $($arg:ty),*) => {{
        // SAFETY 由调用处承担：转出的签名必须与消息的真实签名一致。
        #[allow(clippy::missing_transmute_annotations)]
        std::mem::transmute::<unsafe extern "C" fn(), unsafe extern "C" fn(Id, Sel $(, $arg)*) -> $ret>(
            objc_msgSend as unsafe extern "C" fn(),
        )
    }};
}

unsafe fn msg_id(obj: Id, sel: Sel) -> Id {
    unsafe { msg_send!(Id,)(obj, sel) }
}

unsafe fn msg_id_cstr(obj: Id, sel: Sel, arg: *const c_char) -> Id {
    unsafe { msg_send!(Id, *const c_char)(obj, sel, arg) }
}

unsafe fn msg_id_id(obj: Id, sel: Sel, arg: Id) -> Id {
    unsafe { msg_send!(Id, Id)(obj, sel, arg) }
}

unsafe fn msg_void(obj: Id, sel: Sel) {
    unsafe { msg_send!((),)(obj, sel) }
}

unsafe fn msg_void_id(obj: Id, sel: Sel, arg: Id) {
    unsafe { msg_send!((), Id)(obj, sel, arg) }
}

unsafe fn msg_bool_sel(obj: Id, sel: Sel, arg: Sel) -> ObjcBool {
    unsafe { msg_send!(ObjcBool, Sel)(obj, sel, arg) }
}

unsafe fn msg_usize(obj: Id, sel: Sel) -> NSUInteger {
    unsafe { msg_send!(NSUInteger,)(obj, sel) }
}

unsafe fn msg_ptr(obj: Id, sel: Sel) -> *const u8 {
    unsafe { msg_send!(*const u8,)(obj, sel) }
}

/// `initWithBitmapDataPlanes:pixelsWide:pixelsHigh:bitsPerSample:samplesPerPixel:hasAlpha:isPlanar:colorSpaceName:bytesPerRow:bitsPerPixel:`
#[allow(clippy::too_many_arguments)]
unsafe fn msg_init_bitmap(
    obj: Id,
    sel: Sel,
    planes: *mut *mut u8,
    wide: NSInteger,
    high: NSInteger,
    bps: NSInteger,
    spp: NSInteger,
    alpha: ObjcBool,
    planar: ObjcBool,
    color_space: Id,
    bytes_per_row: NSInteger,
    bits_per_pixel: NSInteger,
) -> Id {
    unsafe {
        msg_send!(
            Id,
            *mut *mut u8,
            NSInteger,
            NSInteger,
            NSInteger,
            NSInteger,
            ObjcBool,
            ObjcBool,
            Id,
            NSInteger,
            NSInteger
        )(
            obj,
            sel,
            planes,
            wide,
            high,
            bps,
            spp,
            alpha,
            planar,
            color_space,
            bytes_per_row,
            bits_per_pixel,
        )
    }
}

/// `drawInRect:fromRect:operation:fraction:`
unsafe fn msg_draw_in_rect(
    obj: Id,
    sel: Sel,
    rect: CGRect,
    from: CGRect,
    op: NSUInteger,
    fraction: f64,
) {
    unsafe { msg_send!((), CGRect, CGRect, NSUInteger, f64)(obj, sel, rect, from, op, fraction) }
}

/// `representationUsingType:properties:`
unsafe fn msg_id_usize_id(obj: Id, sel: Sel, ty: NSUInteger, props: Id) -> Id {
    unsafe { msg_send!(Id, NSUInteger, Id)(obj, sel, ty, props) }
}

unsafe fn cls(name: &CStr) -> Option<Id> {
    // SAFETY: name 以 NUL 结尾。
    let c = unsafe { objc_getClass(name.as_ptr()) };
    (!c.is_null()).then_some(c)
}

unsafe fn sel(name: &CStr) -> Sel {
    // SAFETY: name 以 NUL 结尾。sel_registerName 从不失败。
    unsafe { sel_registerName(name.as_ptr()) }
}

/// 作用域内的 autorelease pool：本文件的消息大量返回 autoreleased 对象，
/// 没有 pool 它们会积到线程退出才释放。
struct Pool(*mut c_void);

impl Pool {
    fn new() -> Self {
        // SAFETY: push/pop 严格配对，见 Drop。
        Pool(unsafe { objc_autoreleasePoolPush() })
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // SAFETY: 恰好弹出构造时压入的那一层。
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

// ===========================================================================
// 对外表面
// ===========================================================================

/// 本进程有没有窗口服务器连接。判据与代价见模块文档。
pub fn available() -> bool {
    // SAFETY: 无参调用；返回的 CFDictionaryRef 归我们释放（Copy 规则）。
    let dict = unsafe { CGSessionCopyCurrentDictionary() };
    if dict.is_null() {
        return false;
    }
    // SAFETY: dict 非空且归我们所有，只释放这一次。
    unsafe { CFRelease(dict) };
    true
}

/// 按扩展名取这一类文件的系统图标，`size`×`size` 像素的 PNG。
///
/// 取不到（扩展名带 NUL、系统给不出图——正常路径上几乎不发生：未知类型
/// 也有「白纸」图标）返回 `None`。**调用方必须先用 [`available`] 把守**，
/// 见模块文档；扩展名的字符消毒也在调用方（`providers/fs/icon`）。
pub fn file_type_icon_png(ext: &str, size: u32) -> Option<Vec<u8>> {
    let c_ext = CString::new(ext).ok()?;
    let _pool = Pool::new();
    // SAFETY: 整条链路的对象生命周期由 pool 与显式 release 管理，
    // 各消息的签名与选择子一一对应，见各处注释。
    unsafe {
        let ns_ext = ns_string(&c_ext)?;
        let image = image_for_uttype(sel(c"typeWithFilenameExtension:"), ns_ext, ns_ext)?;
        render_png(image, size)
    }
}

/// 目录的系统图标（Finder 的那只蓝色文件夹）。UTI `public.folder`。
pub fn folder_icon_png(size: u32) -> Option<Vec<u8>> {
    icon_for_identifier(c"public.folder", c"'fldr'", size)
}

/// 无扩展名 / 认不出类型的文件的系统图标（白纸）。UTI `public.data`。
pub fn generic_file_icon_png(size: u32) -> Option<Vec<u8>> {
    icon_for_identifier(c"public.data", c"'docu'", size)
}

/// 按**真实路径**取图标：`iconForFile:`。
///
/// 与按类型那三条不同，这条会读路径指向的东西（bundle 的 `Info.plist`
/// 与图标文件、自定义文件夹图标），给的是 Finder 里那个**具体条目**的图
/// ——`.app` 显示应用自己的图标而不是文件夹，符号链接解析到目标。
/// 路径不存在或不可读时 AppKit 给通用图标，不报错；身份与展示范围的
/// 把关在调用方（`providers/fs/icon` 与 `routes/files.rs`）。
pub fn file_icon_png(path: &str, size: u32) -> Option<Vec<u8>> {
    let c_path = CString::new(path).ok()?;
    let _pool = Pool::new();
    // SAFETY: 同 file_type_icon_png。
    unsafe {
        let ns_path = ns_string(&c_path)?;
        let workspace = shared_workspace()?;
        let image = msg_id_id(workspace, sel(c"iconForFile:"), ns_path);
        if image.is_null() {
            return None;
        }
        render_png(image, size)
    }
}

/// 按固定 UTI 取图标（文件夹 / 通用文件这类**身份已知**的类型）。
/// `hfs_fallback` 是老系统（没有 UTType）用的 HFS 类型码串，如 `'fldr'`。
fn icon_for_identifier(identifier: &CStr, hfs_fallback: &CStr, size: u32) -> Option<Vec<u8>> {
    let _pool = Pool::new();
    // SAFETY: 同 file_type_icon_png。
    unsafe {
        let ns_id = ns_string(identifier)?;
        let ns_hfs = ns_string(hfs_fallback)?;
        let image = image_for_uttype(sel(c"typeWithIdentifier:"), ns_id, ns_hfs)?;
        render_png(image, size)
    }
}

unsafe fn ns_string(s: &CStr) -> Option<Id> {
    // SAFETY: s 以 NUL 结尾；返回值 autoreleased，归当前 pool。
    let ns = unsafe { msg_id_cstr(cls(c"NSString")?, sel(c"stringWithUTF8String:"), s.as_ptr()) };
    (!ns.is_null()).then_some(ns)
}

unsafe fn shared_workspace() -> Option<Id> {
    // SAFETY: 类与选择子都存在于 AppKit。
    let ws = unsafe { msg_id(cls(c"NSWorkspace")?, sel(c"sharedWorkspace")) };
    (!ws.is_null()).then_some(ws)
}

/// UTType 路线（macOS 12+）取 `NSImage`；老系统回落到废弃但仍在的
/// `iconForFileType:`（参数用 `fallback_arg`：扩展名或 HFS 类型码串）。
///
/// 先 `respondsToSelector:` 再发消息——对老系统直接发未知选择子是崩溃，
/// 不是错误返回。
unsafe fn image_for_uttype(uttype_sel: Sel, uttype_arg: Id, fallback_arg: Id) -> Option<Id> {
    unsafe {
        let workspace = shared_workspace()?;
        let icon_for_content_type = sel(c"iconForContentType:");
        let mut image: Id = std::ptr::null_mut();
        if msg_bool_sel(workspace, sel(c"respondsToSelector:"), icon_for_content_type) != 0
            && let Some(uttype_cls) = cls(c"UTType")
        {
            let uttype = msg_id_id(uttype_cls, uttype_sel, uttype_arg);
            if !uttype.is_null() {
                image = msg_id_id(workspace, icon_for_content_type, uttype);
            }
        }
        if image.is_null() {
            image = msg_id_id(workspace, sel(c"iconForFileType:"), fallback_arg);
        }
        (!image.is_null()).then_some(image)
    }
}

/// 把 `NSImage` 画进 `size`×`size` 的位图并编码成 PNG。
///
/// # Safety
///
/// `image` 必须是有效的 `NSImage`。
unsafe fn render_png(image: Id, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let px = size as NSInteger;
        // alloc/init 出来的 rep 归我们释放；后面所有提前返回都要先走 release，
        // 因此从这里起收敛到单一出口。
        let rep = msg_init_bitmap(
            msg_id(cls(c"NSBitmapImageRep")?, sel(c"alloc")),
            sel(
                c"initWithBitmapDataPlanes:pixelsWide:pixelsHigh:bitsPerSample:samplesPerPixel:hasAlpha:isPlanar:colorSpaceName:bytesPerRow:bitsPerPixel:",
            ),
            std::ptr::null_mut(), // 让 rep 自己分配像素缓冲
            px,
            px,
            8,    // 每样本 8 位
            4,    // RGBA 四样本
            1,    // hasAlpha
            0,    // isPlanar：交错存储
            NSDeviceRGBColorSpace,
            0, // bytesPerRow / bitsPerPixel 都让它自己算
            0,
        );
        if rep.is_null() {
            return None;
        }

        let out = draw_and_encode(image, rep, size);
        msg_void(rep, sel(c"release"));
        out
    }
}

/// `render_png` 的中段：画 + 编码。拆出来是为了让 `rep` 的 release
/// 在唯一出口处成对，不随提前返回散落。
///
/// # Safety
///
/// `image` 与 `rep` 必须有效。
unsafe fn draw_and_encode(image: Id, rep: Id, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let ctx_cls = cls(c"NSGraphicsContext")?;
        let ctx = msg_id_id(ctx_cls, sel(c"graphicsContextWithBitmapImageRep:"), rep);
        if ctx.is_null() {
            return None;
        }

        // 当前上下文是线程局部的类状态：save / set / restore 必须严格配对，
        // 否则污染的是本线程**下一次**走到这里的调用。
        msg_void(ctx_cls, sel(c"saveGraphicsState"));
        msg_void_id(ctx_cls, sel(c"setCurrentContext:"), ctx);
        let side = f64::from(size);
        msg_draw_in_rect(
            image,
            sel(c"drawInRect:fromRect:operation:fraction:"),
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: side,
                    height: side,
                },
            },
            // fromRect 全零 = 整张源图。
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: 0.0,
                    height: 0.0,
                },
            },
            SOURCE_OVER,
            1.0,
        );
        msg_void(ctx_cls, sel(c"restoreGraphicsState"));

        let props = msg_id(cls(c"NSDictionary")?, sel(c"dictionary"));
        let data = msg_id_usize_id(
            rep,
            sel(c"representationUsingType:properties:"),
            PNG_FILE_TYPE,
            props,
        );
        if data.is_null() {
            return None;
        }
        let len = msg_usize(data, sel(c"length"));
        let bytes = msg_ptr(data, sel(c"bytes"));
        if bytes.is_null() || len == 0 {
            return None;
        }
        // data 是 autoreleased 的，趁 pool 还没弹先拷出来。
        Some(std::slice::from_raw_parts(bytes, len).to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PNG 头：8 字节签名 + IHDR 里的宽高。
    fn 解析_png_头(png: &[u8]) -> (u32, u32) {
        assert!(png.len() > 24, "PNG 太短：{} 字节", png.len());
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(&png[12..16], b"IHDR");
        (
            u32::from_be_bytes([png[16], png[17], png[18], png[19]]),
            u32::from_be_bytes([png[20], png[21], png[22], png[23]]),
        )
    }

    #[test]
    fn 常见扩展名取得到类型图标() {
        if !available() {
            eprintln!("本环境没有窗口服务器连接（CI / SSH），跳过");
            return;
        }
        let png = file_type_icon_png("txt", 64).expect("txt 的类型图标应当取得到");
        assert_eq!(解析_png_头(&png), (64, 64), "尺寸不是请求的大小");
    }

    #[test]
    fn 乱造的扩展名也有回落图标() {
        if !available() {
            eprintln!("本环境没有窗口服务器连接（CI / SSH），跳过");
            return;
        }
        // 未注册的扩展名 UTType 给动态类型（dyn.*），图标是通用文档那张白纸。
        let png = file_type_icon_png("绝不会注册这个扩展名9f3a1c", 32)
            .expect("未知扩展名应当回落到通用文档图标");
        assert_eq!(解析_png_头(&png), (32, 32));
    }

    #[test]
    fn 文件夹与通用文件图标() {
        if !available() {
            eprintln!("本环境没有窗口服务器连接（CI / SSH），跳过");
            return;
        }
        for (名字, png) in [
            ("文件夹", folder_icon_png(64)),
            ("通用文件", generic_file_icon_png(64)),
        ] {
            let png = png.unwrap_or_else(|| panic!("{名字}图标应当取得到"));
            assert_eq!(解析_png_头(&png), (64, 64), "{名字}图标尺寸不对");
        }
    }

    #[test]
    fn 按路径取_bundle_的图标() {
        if !available() {
            eprintln!("本环境没有窗口服务器连接（CI / SSH），跳过");
            return;
        }
        // Finder 在任何 macOS 上都在。它的图标与通用文件夹不同——
        // 这正是「按路径」区别于「按类型」的全部意义，必须断言出来。
        let finder = file_icon_png("/System/Library/CoreServices/Finder.app", 64)
            .expect("Finder 的图标应当取得到");
        assert_eq!(解析_png_头(&finder), (64, 64));
        let folder = folder_icon_png(64).expect("文件夹图标应当取得到");
        assert_ne!(finder, folder, ".app 给的还是通用文件夹，说明走错了路");
        // 不存在的路径 AppKit 给通用图标而不是报错——如实接受，这不是错误。
        assert!(file_icon_png("/绝不存在/的路径.app", 32).is_some());
    }

    #[test]
    fn 含_nul_的输入直接拒绝() {
        assert!(file_type_icon_png("tx\0t", 32).is_none());
        assert!(file_icon_png("/tmp/\0x", 32).is_none());
    }

    #[test]
    fn 能力探测不崩溃且可重复() {
        // 两次结果必须一致：这个判据没有随时间波动的成分。
        assert_eq!(available(), available());
    }
}
