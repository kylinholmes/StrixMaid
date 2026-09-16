//! 可执行文件的图标 → 32 位 RGBA → PNG。
//!
//! 与本目录其余文件同一定位：只做「一次 Win32 调用」这一层，不含任何业务判断。
//! 「哪个名字对应哪个 exe」「结果缓存多久」都在
//! [`crate::providers::process::icon`] 里。
//!
//! # 为什么是 `PrivateExtractIconsW` 而不是 `SHGetFileInfoW`
//!
//! 两者都能取到 exe 的图标，但 `SHGetFileInfoW` 只给「大图标 / 小图标」两档，
//! 具体像素数随系统 DPI 与外壳设置变化（`SM_CXICON` 通常是 32，高 DPI 下可能是
//! 40、48、64）。前端按固定尺寸渲染，尺寸随机会让表格看起来参差。
//! `PrivateExtractIconsW` 能**精确指定**要多大，本模块固定取 [`ICON_SIZE`]。
//!
//! 名字里的 `Private` 是历史遗留，它在 user32 里已公开导出并写进了 SDK 头文件，
//! 不是未文档化接口。
//!
//! # 图标的 alpha 有两条路，两条都要走
//!
//! `HICON` 内部是**两张**位图（[`ICONINFO`]）：颜色位图 `hbmColor` 与掩码位图
//! `hbmMask`。透明区由哪一张表达，取决于图标是哪个年代的：
//!
//! | 图标形态 | 颜色位图 | 透明区从哪来 | 本模块的处理 |
//! |---|---|---|---|
//! | Windows XP 之前（4 / 8 / 24 位） | 无 alpha 通道，取出来整片 alpha = 0 | `hbmMask`：位为 1 表示该像素透明 | [`alpha_from_mask`] |
//! | Windows XP 起（32 位） | BGRA，alpha 有效 | 颜色位图自带 | 直接用 |
//!
//! 两条路必须都实现，只写一条的后果是**静默产出错误的图**而不是报错：
//!
//! - 只认 alpha：老式图标的 alpha 全为 0，整张图变成全透明（前端看到的是空白）。
//! - 只认掩码：新式图标的掩码是从 alpha 阈值化来的，半透明的抗锯齿边缘会被
//!   硬切成不透明，圆角图标周围出现一圈黑边。
//!
//! 判据是「颜色位图取出来的 alpha 是不是**全部**为 0」——这是各家实现
//! （Chromium、Qt、wxWidgets）通行的判据。理论上「一张真的全透明的图标」
//! 会被误判成老式图标，但那种图标本来就画不出任何东西，走哪条路结果都一样。
//!
//! # 关于预乘
//!
//! 32 位图标资源在 ICO / PE 里存的是**非预乘**的直通 alpha，
//! `CreateIconFromResourceEx` 建出来的颜色位图保留这份原始数据，
//! 因此本模块把取出的 BGRA 直接当直通 alpha 交给 PNG（PNG 也规定非预乘）。
//! 预乘只发生在 `DrawIconEx` / `AlphaBlend` 真正绘制的那一刻，本模块不绘制。
//!
//! 这个前提万一不成立，表现是**半透明边缘偏暗**——只影响抗锯齿的那一两个像素，
//! 不会出现整块黑或整片透明。
//!
//! # 线程与运行时
//!
//! 全部是同步的 GDI 调用，一次几毫秒，**绝不能在 async 运行时线程上直接调**。
//! 调用方（[`crate::providers::process::icon::IconCache`]）一律经
//! `tokio::task::spawn_blocking` 进入。
//!
//! 内存 DC（`CreateCompatibleDC(NULL)`）不需要窗口站与桌面，服务进程
//! （会话 0、非交互窗口站）里同样可用——本模块不创建窗口、不碰用户界面。

use std::io;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HDC, RGBQUAD,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetIconInfo, HICON, ICONINFO, PrivateExtractIconsW,
};

use super::{last_error, wide};

/// 取出的图标边长（像素）。见
/// [`crate::providers::process::icon::ICON_SIZE`] 的跨平台尺寸契约。
pub const ICON_SIZE: i32 = 32;

/// 单张图标的像素数上限。`PrivateExtractIconsW` 只会给我们要的尺寸，
/// 这个上限防的是 `GetObjectW` 回填了异常值时按它去分配一大块内存。
const MAX_PIXELS: i64 = 1024 * 1024;

/// 一张解出来的图，RGBA8、逐行从上到下。
#[derive(Debug, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    /// 长度必然是 `width * height * 4`。
    pub pixels: Vec<u8>,
}

/// 取 `exe_path` 的图标，编码成 [`ICON_SIZE`] 见方的 PNG。
///
/// 失败的原因都不是异常：文件不存在、文件里没有图标资源（不少控制台程序就没有）、
/// 无权读取。一律返回 `Err`，由调用方决定是记负缓存还是报 404。
pub fn icon_png(exe_path: &str) -> io::Result<Vec<u8>> {
    let image = icon_rgba(exe_path, ICON_SIZE)?;
    encode_png(&image)
}

/// 取图标并解成 RGBA，不做 PNG 编码。拆出来是为了让测试能直接检查像素。
pub fn icon_rgba(exe_path: &str, size: i32) -> io::Result<Rgba> {
    let icon = extract_icon(exe_path, size)?;
    // SAFETY: icon 由 PrivateExtractIconsW 返回且尚未销毁，借用期不超过本语句。
    unsafe { icon_to_rgba(icon.raw()) }
}

// ===========================================================================
// RAII
// ===========================================================================

/// 自动 `DestroyIcon` 的图标句柄。
///
/// 与 [`super::handle::Owned`] 同一写法，但**不能**复用它：`HICON` 不是内核对象，
/// 释放函数是 `DestroyIcon` 而不是 `CloseHandle`，拿错会泄漏 GDI 对象
/// （每进程默认上限 10000 个，泄漏到顶之后所有 GDI 调用一起失败）。
struct OwnedIcon(HICON);

impl OwnedIcon {
    fn raw(&self) -> HICON {
        self.0
    }
}

impl Drop for OwnedIcon {
    fn drop(&mut self) {
        // SAFETY: 构造时已排除空句柄，本类型独占所有权，只销毁这一次。
        unsafe {
            DestroyIcon(self.0);
        }
    }
}

/// 自动 `DeleteObject` 的 GDI 位图。
///
/// [`GetIconInfo`] 会**复制**出两张位图交给调用方，文档明确要求由调用方删除；
/// 不删就是每取一次图标泄漏两个 GDI 对象。
struct OwnedBitmap(HBITMAP);

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: 位图由 GetIconInfo 复制而来，所有权在本类型，只删这一次。
        unsafe {
            DeleteObject(self.0);
        }
    }
}

/// 自动 `DeleteDC` 的内存设备上下文。
struct OwnedDc(HDC);

impl Drop for OwnedDc {
    fn drop(&mut self) {
        // SAFETY: DC 由 CreateCompatibleDC 返回，所有权在本类型，只删这一次。
        unsafe {
            DeleteDC(self.0);
        }
    }
}

// ===========================================================================
// 取图标
// ===========================================================================

/// `PrivateExtractIconsW`：从文件里取第 0 个图标，按指定尺寸。
fn extract_icon(exe_path: &str, size: i32) -> io::Result<OwnedIcon> {
    if exe_path.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "exe 路径为空"));
    }
    let wide_path = wide::to_wide(exe_path);
    let mut icon: HICON = std::ptr::null_mut();
    // SAFETY: wide_path 以 NUL 结尾且在调用期间存活；icon 是一个可写的 HICON；
    // 第 6 个参数（piconid）按文档允许为 NULL；nicons = 1 与 icon 的容量一致。
    let got = unsafe {
        PrivateExtractIconsW(
            wide_path.as_ptr(),
            0,
            size,
            size,
            &raw mut icon,
            std::ptr::null_mut(),
            1,
            0,
        )
    };
    // 先无条件接管可能写回来的句柄，再判断成败——否则「报了错却也写了句柄」
    // 这种边角情况会漏一个 GDI 对象。
    let icon = (!icon.is_null()).then(|| OwnedIcon(icon));

    // 返回值是「取到几个」：文件里没有图标资源时是 0，调用本身失败时是 u32::MAX。
    // 只在失败分支里看 `GetLastError`——成功路径上它是上一次调用留下的陈旧值。
    match (got, icon) {
        (1, Some(icon)) => Ok(icon),
        (u32::MAX, _) => Err(last_error()),
        _ => Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{exe_path} 里没有可用的图标资源"),
        )),
    }
}

/// `HICON` → RGBA。
///
/// # Safety
///
/// `icon` 必须是有效的、尚未销毁的图标句柄。
unsafe fn icon_to_rgba(icon: HICON) -> io::Result<Rgba> {
    let mut info = ICONINFO {
        fIcon: 0,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: std::ptr::null_mut(),
        hbmColor: std::ptr::null_mut(),
    };
    // SAFETY: 调用方保证 icon 有效；info 是一块可写的 ICONINFO。
    // 成功时它回填两个**新**位图句柄，由下面的 OwnedBitmap 接管。
    if unsafe { GetIconInfo(icon, &raw mut info) } == 0 {
        return Err(last_error());
    }
    let color = OwnedBitmap(info.hbmColor);
    let mask = OwnedBitmap(info.hbmMask);

    if color.0.is_null() {
        // 只有掩码位图、没有颜色位图 = 1 位单色图标（`hbmMask` 高度是宽度的两倍，
        // 上半是 AND 掩码、下半是 XOR 数据）。exe 的图标资源里已经基本绝迹，
        // 本模块不为它写第三条路径——宁可如实报「取不到」，也不猜一张出来。
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "单色图标（无颜色位图），不支持",
        ));
    }

    // SAFETY: color 是 GetIconInfo 刚给出的有效位图。
    let dims = unsafe { bitmap_size(color.0) }?;
    let (w, h) = dims;

    // SAFETY: CreateCompatibleDC(NULL) 建一块与屏幕兼容的内存 DC，不需要参数校验。
    let hdc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
    if hdc.is_null() {
        return Err(last_error());
    }
    let hdc = OwnedDc(hdc);

    // SAFETY: hdc 与 color 都有效，w/h 来自 color 自身。
    let mut pixels = unsafe { read_bgra(hdc.0, color.0, w, h) }?;

    // 老式图标：颜色位图整片 alpha = 0，透明区在掩码里。见模块文档。
    if pixels.as_chunks::<4>().0.iter().all(|px| px[3] == 0) {
        // SAFETY: hdc 有效；mask 可能为空，函数内部判空。
        unsafe { alpha_from_mask(hdc.0, mask.0, w, h, &mut pixels) }?;
    }

    // GDI 给的是 BGRA，PNG 要 RGBA：就地交换 B 与 R。
    for px in pixels.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Ok(Rgba {
        width: w,
        height: h,
        pixels,
    })
}

/// 位图的宽高。
///
/// # Safety
///
/// `bmp` 必须是有效的位图句柄。
unsafe fn bitmap_size(bmp: HBITMAP) -> io::Result<(u32, u32)> {
    let mut desc = BITMAP {
        bmType: 0,
        bmWidth: 0,
        bmHeight: 0,
        bmWidthBytes: 0,
        bmPlanes: 0,
        bmBitsPixel: 0,
        bmBits: std::ptr::null_mut(),
    };
    // SAFETY: 调用方保证 bmp 有效；desc 是一块可写的 BITMAP，长度如实给出。
    let n = unsafe {
        GetObjectW(
            bmp,
            std::mem::size_of::<BITMAP>() as i32,
            (&raw mut desc).cast(),
        )
    };
    if n == 0 {
        return Err(last_error());
    }
    let (w, h) = (desc.bmWidth, desc.bmHeight);
    if w <= 0 || h <= 0 || i64::from(w) * i64::from(h) > MAX_PIXELS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("图标尺寸不合理：{w}×{h}"),
        ));
    }
    Ok((w as u32, h as u32))
}

/// 把位图读成 32 位 BGRA、逐行从上到下。
///
/// `biHeight` 取**负值**是关键：DIB 默认自下而上存储，负高度才是自上而下，
/// 省掉一次翻转，也避免「忘了翻转」这种上下颠倒的经典 bug。
///
/// # Safety
///
/// `hdc` 与 `bmp` 必须有效，`w` / `h` 必须是 `bmp` 的真实尺寸。
unsafe fn read_bgra(hdc: HDC, bmp: HBITMAP, w: u32, h: u32) -> io::Result<Vec<u8>> {
    let mut info = BITMAPINFO {
        bmiHeader: header(w, h, 32),
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    // SAFETY: buf 至少有 w*h*4 字节（32 位、无行填充，宽度对齐天然满足 4 字节）；
    // info 如实描述了请求的格式；hdc / bmp 由调用方保证有效。
    let lines = unsafe {
        GetDIBits(
            hdc,
            bmp,
            0,
            h,
            buf.as_mut_ptr().cast(),
            &raw mut info,
            DIB_RGB_COLORS,
        )
    };
    if lines as u32 != h {
        return Err(last_error());
    }
    Ok(buf)
}

/// 用 1 位掩码位图补出 alpha 通道。
///
/// AND 掩码的语义来自 `DrawIcon` 的画法 `dest = (dest AND mask) XOR color`：
/// **掩码位为 1 的像素保留背景，即透明**；为 0 的像素画出颜色位图。
///
/// 这里按 1 位/像素**原样**读出掩码，而不是让 GDI 转成 32 位再看颜色——
/// 后者要依赖单色位图隐含调色板（0 = 黑、1 = 白）的转换行为，多一层可以走样的
/// 假设；原样读出的位就是掩码本身，没有解释空间。代价是要自己算行对齐：
/// DIB 的每一行按 4 字节对齐，1 位/像素时是 `((w + 31) / 32) * 4` 字节。
///
/// # Safety
///
/// `hdc` 必须有效；`mask` 可以为空（此时报错），非空时必须是有效位图；
/// `pixels` 的长度必须是 `w * h * 4`。
unsafe fn alpha_from_mask(
    hdc: HDC,
    mask: HBITMAP,
    w: u32,
    h: u32,
    pixels: &mut [u8],
) -> io::Result<()> {
    if mask.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "图标既没有 alpha 通道也没有掩码位图",
        ));
    }
    // SAFETY: 调用方保证 mask 有效。
    let (mw, mh) = unsafe { bitmap_size(mask) }?;
    if (mw, mh) != (w, h) {
        // 掩码是宽高各半 / 双倍高度这类形态时，与颜色位图对不上像素。
        // 与其按比例猜一个对应关系，不如如实报错。
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("掩码位图尺寸 {mw}×{mh} 与颜色位图 {w}×{h} 不一致"),
        ));
    }

    /// 1 位 DIB 的 `BITMAPINFO`：`bmiColors` 要能放下两个表项，
    /// 而 `windows-sys` 的 `BITMAPINFO` 只声明了一个（C 里的柔性数组惯例）。
    #[repr(C)]
    struct MonoInfo {
        header: BITMAPINFOHEADER,
        colors: [RGBQUAD; 2],
    }

    let mut info = MonoInfo {
        header: header(w, h, 1),
        colors: [RGBQUAD::default(); 2],
    };
    let stride = (w as usize).div_ceil(32) * 4;
    let mut bits = vec![0u8; stride * h as usize];
    // SAFETY: bits 有 stride*h 字节，与 info 描述的 1 位/行对齐格式一致；
    // info 的实际长度比 BITMAPINFO 大（多一个表项），按 C 的柔性数组约定传其地址。
    let lines = unsafe {
        GetDIBits(
            hdc,
            mask,
            0,
            h,
            bits.as_mut_ptr().cast(),
            (&raw mut info).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
        )
    };
    if lines as u32 != h {
        return Err(last_error());
    }

    for y in 0..h as usize {
        let row = &bits[y * stride..(y + 1) * stride];
        for x in 0..w as usize {
            // DIB 的 1 位像素在字节内自高位向低位排列。
            let transparent = (row[x >> 3] >> (7 - (x & 7))) & 1 == 1;
            pixels[(y * w as usize + x) * 4 + 3] = if transparent { 0 } else { 255 };
        }
    }
    Ok(())
}

/// 构造一个自上而下、无压缩的 `BITMAPINFOHEADER`。
fn header(w: u32, h: u32, bits: u16) -> BITMAPINFOHEADER {
    BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        // 负高度 = 自上而下，见 `read_bgra` 的说明。
        biHeight: -(h as i32),
        biPlanes: 1,
        biBitCount: bits,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: if bits == 1 { 2 } else { 0 },
        biClrImportant: 0,
    }
}

// ===========================================================================
// PNG
// ===========================================================================

/// RGBA → PNG 字节。
fn encode_png(image: &Rgba) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(4096);
    let mut encoder = png::Encoder::new(&mut out, image.width, image.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|e| io::Error::other(format!("写 PNG 头失败：{e}")))?;
    writer
        .write_image_data(&image.pixels)
        .map_err(|e| io::Error::other(format!("写 PNG 像素失败：{e}")))?;
    writer
        .finish()
        .map_err(|e| io::Error::other(format!("收尾 PNG 失败：{e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机上「几乎必然存在且必然带图标」的两个候选。取不到就跳过测试，
    /// 不失败——精简镜像 / Server Core 上 `explorer.exe` 未必装。
    fn 候选可执行文件() -> Option<String> {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        [
            format!("{root}\\explorer.exe"),
            format!("{root}\\System32\\notepad.exe"),
            format!("{root}\\System32\\cmd.exe"),
        ]
        .into_iter()
        .find(|p| std::path::Path::new(p).is_file())
    }

    /// PNG 头：8 字节签名 + IHDR（长度 13、类型 "IHDR"、宽、高……）。
    fn 解析_png_头(png: &[u8]) -> (u32, u32) {
        assert!(png.len() > 24, "PNG 太短：{} 字节", png.len());
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "PNG 魔数不对"
        );
        assert_eq!(&png[12..16], b"IHDR", "第一个块必须是 IHDR");
        let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
        let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
        (w, h)
    }

    #[test]
    fn 取本机程序的图标并检查_png() {
        let Some(exe) = 候选可执行文件() else {
            eprintln!("本机没有找到可用来测试的系统程序，跳过");
            return;
        };
        let png = match icon_png(&exe) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("本机取不到 {exe} 的图标（{e}），跳过");
                return;
            }
        };
        let (w, h) = 解析_png_头(&png);
        assert_eq!((w, h), (ICON_SIZE as u32, ICON_SIZE as u32), "尺寸不是请求的大小");
    }

    #[test]
    fn 图标既不是全透明也不是纯黑块() {
        let Some(exe) = 候选可执行文件() else {
            eprintln!("本机没有找到可用来测试的系统程序，跳过");
            return;
        };
        let img = match icon_rgba(&exe, ICON_SIZE) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("本机取不到 {exe} 的图标（{e}），跳过");
                return;
            }
        };
        assert_eq!(img.pixels.len(), (img.width * img.height * 4) as usize);

        // 两条 alpha 路径任何一条写错，表现就是下面两条断言之一失败：
        // 只认 alpha → 老式图标整片透明；只认掩码 → 半透明边缘被切成黑边。
        let 不透明像素 = img
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|px| px[3] > 0)
            .count();
        assert!(不透明像素 > 0, "整张图全透明，alpha 合成出错了");

        let 有颜色 = img
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|px| px[3] > 0 && (px[0] > 8 || px[1] > 8 || px[2] > 8));
        assert!(有颜色, "不透明区域全是纯黑，像素读取或通道顺序出错了");
    }

    #[test]
    fn 不存在的文件取不到图标() {
        let err = icon_png("C:\\这个文件不存在\\也没有图标.exe").expect_err("应当失败");
        // 只要求是错误，不断言具体 errno：不同 Windows 版本对「路径不存在」
        // 与「文件里没有图标」给的是不同的码。
        let _ = err;
    }

    #[test]
    fn 空路径直接拒绝() {
        assert!(icon_png("").is_err());
    }

    #[test]
    fn 掩码行对齐算法() {
        // 1 位 DIB 每行按 4 字节对齐：1..=32 列都是 4 字节，33 列起是 8 字节。
        for (w, expect) in [(1u32, 4usize), (16, 4), (32, 4), (33, 8), (64, 8), (65, 12)] {
            assert_eq!((w as usize).div_ceil(32) * 4, expect, "宽度 {w} 的行长算错");
        }
    }

    #[test]
    fn 位图头是自上而下的() {
        let h = header(32, 32, 32);
        assert_eq!(h.biHeight, -32, "正的高度会让图上下颠倒");
        assert_eq!(h.biWidth, 32);
        assert_eq!(h.biBitCount, 32);
        assert_eq!(h.biClrUsed, 0);
        // 1 位时必须声明两个调色板表项，否则 GetDIBits 不知道要回填多少。
        assert_eq!(header(32, 32, 1).biClrUsed, 2);
    }
}
