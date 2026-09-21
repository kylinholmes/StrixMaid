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
use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
use windows_sys::Win32::UI::Shell::{
    ILFree, KF_FLAG_DONT_VERIFY, SHFILEINFOW, SHGFI_ICON, SHGFI_ICONLOCATION, SHGFI_LARGEICON,
    SHGFI_PIDL, SHGFI_USEFILEATTRIBUTES, SHGSI_ICON, SHGSI_LARGEICON, SHGetFileInfoW,
    SHGetKnownFolderIDList, SHGetStockIconInfo, SHSTOCKICONID, SHSTOCKICONINFO,
};
use windows_sys::core::GUID;
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
    let icon = extract_icon(exe_path, 0, size)?;
    // SAFETY: icon 由 PrivateExtractIconsW 返回且尚未销毁，借用期不超过本语句。
    unsafe { icon_to_rgba(icon.raw()) }
}

// ===========================================================================
// 文件类型图标
// ===========================================================================

/// 文件类型图标的边长（像素）。
///
/// 与进程图标的 32 不同档：类型图标要撑平铺视图里 68 CSS 像素的格子
/// （`web/src/workspace/TileGrid.tsx`），32 放大到那里明显发糊。64 走
/// [`extract_icon`] 的精确尺寸请求，源文件里常见的 48 / 256 档都能就近缩过来。
pub const FILE_ICON_SIZE: i32 = 64;

/// 按**扩展名**取这一类文件的外壳图标，编码成 PNG。
///
/// 走 `SHGetFileInfoW` + `SHGFI_USEFILEATTRIBUTES`：给外壳一个**虚构的**文件名
/// `strixmaid.<ext>`，它只查注册表的扩展名关联，完全不碰磁盘——这正是本函数
/// 能放在主进程、不经 worker 的原因（不产生任何以调用者身份的文件访问）。
///
/// 分两步而不是直接要 `SHGFI_ICON`：
///
/// 1. 先 `SHGFI_ICONLOCATION` 拿「图标在哪个文件的第几号」，再用
///    [`extract_icon`] 按 [`FILE_ICON_SIZE`] **精确尺寸**提取——`SHGFI_ICON`
///    只有大小两档，具体像素数随系统 DPI 漂移（见模块文档），且大档通常只有
///    32，对 64 的目标尺寸是二次放大。
/// 2. 拿不到位置才回落 `SHGFI_ICON | SHGFI_LARGEICON`：由图标处理器
///    （`IExtractIcon` 的 `Extract` 返回 `S_FALSE` 之外的那类）动态生成的
///    图标没有「文件 + 序号」这种位置可言，只能要现成的 `HICON`。
pub fn file_type_icon_png(ext: &str) -> io::Result<Vec<u8>> {
    // 虚构文件名。扩展名的消毒（拒绝分隔符、`..`、点号）在调用方
    // `providers/fs/icon` 那一层，这里只负责拼接与提取。
    shell_type_icon_png(&format!("strixmaid.{ext}"), FILE_ATTRIBUTE_NORMAL)
}

/// 目录的外壳图标（资源管理器的那只黄色文件夹）。
///
/// 同一个虚构名字换成目录属性即可——`SHGFI_USEFILEATTRIBUTES` 下外壳只看
/// 属性位与扩展名，`FILE_ATTRIBUTE_DIRECTORY` 就是「这是个文件夹」。
pub fn folder_icon_png() -> io::Result<Vec<u8>> {
    shell_type_icon_png("strixmaid", FILE_ATTRIBUTE_DIRECTORY)
}

/// 无扩展名 / 认不出类型的文件的外壳图标（那张白纸）。
pub fn generic_file_icon_png() -> io::Result<Vec<u8>> {
    shell_type_icon_png("strixmaid", FILE_ATTRIBUTE_NORMAL)
}

/// 自动 `ILFree` 的外壳 ID 列表（PIDL）。
struct OwnedPidl(*mut ITEMIDLIST);

impl Drop for OwnedPidl {
    fn drop(&mut self) {
        // SAFETY: 构造时已排除空指针，本类型独占所有权，只释放这一次。
        unsafe { ILFree(self.0) };
    }
}

/// 一个**已知文件夹**（桌面、下载、文稿、主目录……）的外壳图标。
///
/// # 为什么不是按路径取
///
/// 「按路径取图标」那条路在 Windows 上是关着的（见
/// `providers/fs/icon` 的文档）：主进程可能以服务身份运行，拿任意路径去问
/// 外壳等于给任何登录用户一个「这个路径存不存在、是什么」的探针。
///
/// 本函数不接受路径，只接受一个**固定枚举**里的 `KNOWNFOLDERID`。调用方
/// 编不出新的取值，因此它问不出任何关于用户文件的事——那条限制仍然成立。
///
/// # `KF_FLAG_DONT_VERIFY`
///
/// 只要这个已知文件夹的**身份**，不要求它在磁盘上真的存在：既省掉一次磁盘
/// 访问，也让服务进程（它自己的配置文件里往往根本没有「下载」这种目录）
/// 照样取得到图标。
///
/// 顺带一提，服务身份解析出来的是**服务账户自己**的那些文件夹，不是登录
/// 用户的——但我们只取图标，而已知文件夹的图标与是谁的无关。
pub fn known_folder_icon_png(folder: &GUID) -> io::Result<Vec<u8>> {
    let _com = ComInit::new();
    warm_up_shell();

    let mut raw: *mut ITEMIDLIST = std::ptr::null_mut();
    // SAFETY: folder 指向一个有效的 GUID；htoken 传空表示当前用户；
    // ppidl 是本函数栈上的输出变量，成功时得到一块需由 ILFree 释放的内存。
    let hr = unsafe {
        SHGetKnownFolderIDList(
            folder,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &raw mut raw,
        )
    };
    if hr < 0 || raw.is_null() {
        return Err(io::Error::other(format!(
            "SHGetKnownFolderIDList 失败：0x{hr:08X}"
        )));
    }
    let pidl = OwnedPidl(raw);

    let mut info = empty_file_info();
    // SAFETY: `SHGFI_PIDL` 下第一个参数是 PIDL 而不是宽字符串（Win32 的这个
    // 形参本身就是个联合），pidl 由 OwnedPidl 看管、活过本次调用；info 是
    // 可写的 SHFILEINFOW，长度如实给出。成功时 info.hIcon 归调用方销毁。
    let ok = unsafe {
        SHGetFileInfoW(
            pidl.0 as *const u16,
            0,
            &raw mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_PIDL | SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ok == 0 || info.hIcon.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "外壳没有给出这个已知文件夹的图标",
        ));
    }
    let icon = OwnedIcon(info.hIcon);
    // SAFETY: icon 有效且尚未销毁，借用期不超过本语句。
    let rgba = unsafe { icon_to_rgba(icon.raw()) }?;
    encode_png(&rgba)
}

/// 一张**库存图标**（`SIID_*`）——磁盘、网络盘这类没有路径可言的东西。
///
/// 与 [`known_folder_icon_png`] 同理：取值来自固定枚举，不接受路径。
pub fn stock_icon_png(siid: SHSTOCKICONID) -> io::Result<Vec<u8>> {
    let _com = ComInit::new();
    warm_up_shell();

    // SAFETY: SHSTOCKICONINFO 是纯 POD（句柄、整数与 u16 数组），全零是合法值。
    let mut sii: SHSTOCKICONINFO = unsafe { std::mem::zeroed() };
    sii.cbSize = std::mem::size_of::<SHSTOCKICONINFO>() as u32;
    // SAFETY: sii 的 cbSize 已按文档填好；成功时 hIcon 归调用方销毁。
    let hr = unsafe { SHGetStockIconInfo(siid, SHGSI_ICON | SHGSI_LARGEICON, &raw mut sii) };
    if hr < 0 || sii.hIcon.is_null() {
        return Err(io::Error::other(format!(
            "SHGetStockIconInfo 失败：0x{hr:08X}"
        )));
    }
    let icon = OwnedIcon(sii.hIcon);
    // SAFETY: 同上。
    let rgba = unsafe { icon_to_rgba(icon.raw()) }?;
    encode_png(&rgba)
}

/// 把「**进程内第一次**取外壳图标」串行化。
///
/// 现象：八条线程各取 50 次图标，其中七条**恰好在各自的第 0 次**调用上拿到
/// `SHGetFileInfoW` 返回 0，之后 393 次全部成功。也就是说失败的不是「并发」，
/// 而是「多条线程同时发起进程里的头一次」——外壳的系统图像列表还在初始化，
/// 跑赢的那条拿到图标，其余的被判失败。
///
/// 图标提取跑在 `spawn_blocking` 的线程池上，而打开一个目录会同时要好几种
/// 类型的图标，正好撞上这个形态。
///
/// [`Once::call_once`] 恰好是需要的语义：第一条线程做，其余线程**阻塞等它做完**
/// 再继续。代价是进程生命期内多一次 `SHGetFileInfoW`。
///
/// 后果值得记一笔：失败会被 `IconCache` 负缓存 5 分钟，而前端
/// （`web/src/workspace/sysicons.ts`）的平台探测一个会话只问一次，收到 404
/// 就把**整个浏览器会话**判定成「本平台不提供系统图标」并全部回落内置图标集。
/// 一次开机时的抖动，换来一整个会话没有系统图标。
///
/// 回归测试：`并发取类型图标不失败`。
fn warm_up_shell() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 结果丢弃：这一次调用只为把外壳的图像列表初始化起来。真失败了也
        // 不该在这里报——调用方自己那次会拿到同样的错误并正常回落。
        let _ = shell_icon("strixmaid", FILE_ATTRIBUTE_NORMAL);
    });
}

/// [`file_type_icon_png`] 一族的共同两步：位置精确提取，回落现成 `HICON`。
fn shell_type_icon_png(fictional_name: &str, attrs: u32) -> io::Result<Vec<u8>> {
    let _com = ComInit::new();
    warm_up_shell();

    // 位置取到了、但那个文件提取失败（比如指向一个已卸载程序留下的路径）时
    // 继续往下走回落，而不是就此放弃。
    if let Some((icon_file, index)) = icon_location(fictional_name, attrs)
        && let Ok(icon) = extract_icon(&icon_file, index, FILE_ICON_SIZE)
    {
        // SAFETY: icon 有效且尚未销毁，借用期不超过本语句。
        let rgba = unsafe { icon_to_rgba(icon.raw()) }?;
        return encode_png(&rgba);
    }

    let icon = shell_icon(fictional_name, attrs)?;
    // SAFETY: 同上。
    let rgba = unsafe { icon_to_rgba(icon.raw()) }?;
    encode_png(&rgba)
}

/// `SHGetFileInfoW` 要求调用线程先初始化 COM（图标处理器是 COM 组件）。
///
/// 调用发生在 `spawn_blocking` 的线程池线程上，那里没人替我们初始化过。
/// `RPC_E_CHANGED_MODE`（线程已按另一种模型初始化）不算失败——COM 已经可用，
/// 只是不该由我们去 `CoUninitialize`。
struct ComInit {
    initialized: bool,
}

impl ComInit {
    fn new() -> Self {
        // SAFETY: 参数按文档——保留参数必须为空，公寓模型 + 关掉 OLE1。
        let hr = unsafe {
            CoInitializeEx(
                std::ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        // S_OK(0) 与 S_FALSE(1)（重复初始化）都要配对的 CoUninitialize；
        // 其余（含 RPC_E_CHANGED_MODE）不要。
        ComInit {
            initialized: hr == 0 || hr == 1,
        }
    }
}

impl Drop for ComInit {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: 与构造里成功的 CoInitializeEx 恰好配对一次。
            unsafe { CoUninitialize() };
        }
    }
}

/// `SHGFI_ICONLOCATION`：这一类文件的图标在哪个文件的第几号资源。
///
/// 序号可以是负数（负的资源 id，`PrivateExtractIconsW` 原样认识），照传。
/// 拿不到位置（处理器动态生成、注册表残缺）返回 `None`，由调用方回落。
fn icon_location(fictional_name: &str, attrs: u32) -> Option<(String, i32)> {
    let wide_name = wide::to_wide(fictional_name);
    let mut info = empty_file_info();
    // SAFETY: wide_name 以 NUL 结尾且在调用期间存活；info 是可写的 SHFILEINFOW，
    // 长度如实给出；SHGFI_USEFILEATTRIBUTES 保证不发生文件访问。
    let ok = unsafe {
        SHGetFileInfoW(
            wide_name.as_ptr(),
            attrs,
            &raw mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_USEFILEATTRIBUTES | SHGFI_ICONLOCATION,
        )
    };
    if ok == 0 {
        return None;
    }
    let path = wide::from_wide_nul(&info.szDisplayName);
    (!path.is_empty()).then_some((path, info.iIcon))
}

/// `SHGFI_ICON | SHGFI_LARGEICON`：直接要一个现成的 `HICON`（尺寸随系统，
/// 通常 32）。回落路径，理由见 [`file_type_icon_png`]。
fn shell_icon(fictional_name: &str, attrs: u32) -> io::Result<OwnedIcon> {
    let wide_name = wide::to_wide(fictional_name);
    let mut info = empty_file_info();
    // SAFETY: 同 `icon_location`；成功时 info.hIcon 是一个**归调用方销毁**的
    // 图标句柄，立即交给 OwnedIcon 接管。
    let ok = unsafe {
        SHGetFileInfoW(
            wide_name.as_ptr(),
            attrs,
            &raw mut info,
            std::mem::size_of::<SHFILEINFOW>() as u32,
            SHGFI_USEFILEATTRIBUTES | SHGFI_ICON | SHGFI_LARGEICON,
        )
    };
    if ok == 0 || info.hIcon.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("外壳没有给出 {fictional_name} 的类型图标"),
        ));
    }
    Ok(OwnedIcon(info.hIcon))
}

/// 全零的 `SHFILEINFOW`。`windows-sys` 的结构体不带 `Default`，逐字段写一遍
/// 只是把「全零」说得更啰嗦。
fn empty_file_info() -> SHFILEINFOW {
    // SAFETY: SHFILEINFOW 是纯 POD（句柄、整数与 u16 数组），全零是合法值。
    unsafe { std::mem::zeroed() }
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

/// `PrivateExtractIconsW`：从文件里取第 `index` 号图标，按指定尺寸。
///
/// `index` 为负是「负的资源 id」语义（与 `ExtractIconEx` 一致），
/// [`icon_location`] 给出的序号可能就是这种，原样传入。
fn extract_icon(exe_path: &str, index: i32, size: i32) -> io::Result<OwnedIcon> {
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
            index,
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
    fn 常见扩展名取得到类型图标() {
        // .txt 的关联（notepad / 记事本）从 Windows 95 起就在，任何 SKU 都有。
        let png = file_type_icon_png("txt").expect("txt 的类型图标应当取得到");
        let (w, h) = 解析_png_头(&png);
        // 走 ICONLOCATION 精确路径时是 FILE_ICON_SIZE；只在回落路径上才是
        // 系统大图标档。两者都算通过——回落是设计内的路径，不是缺陷。
        assert!(
            (w, h) == (FILE_ICON_SIZE as u32, FILE_ICON_SIZE as u32) || (w == h && w >= 16),
            "类型图标尺寸不合理：{w}×{h}"
        );
    }

    #[test]
    fn 文件夹与通用文件图标() {
        let folder = folder_icon_png().expect("文件夹图标应当取得到");
        assert_eq!(&folder[..4], &[0x89, b'P', b'N', b'G']);
        let generic = generic_file_icon_png().expect("通用文件图标应当取得到");
        assert_eq!(&generic[..4], &[0x89, b'P', b'N', b'G']);
        assert_ne!(folder, generic, "目录属性没有生效：文件夹与文件给了同一张图");
    }

    #[test]
    fn 乱造的扩展名也有回落图标() {
        // 未注册的扩展名走「未知文件」的通用图标，这在 Windows 上是保证的行为：
        // 资源管理器给未知类型画的就是那张白纸。
        let png = file_type_icon_png("绝不会注册这个扩展名9f3a1c")
            .expect("未知扩展名应当回落到通用文件图标");
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
    }

    /// 已知文件夹（下载、桌面……）取得到各自的系统图标。
    ///
    /// 断言两张图**不相同**是承重的：取错路子（比如退回通用文件夹图标）时
    /// 每个文件夹都会是同一只黄文件夹，只断言「取得到」看不出来。
    #[test]
    fn 已知文件夹取得到各自的图标() {
        use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Downloads};

        let desktop = known_folder_icon_png(&FOLDERID_Desktop).expect("桌面图标应当取得到");
        let downloads = known_folder_icon_png(&FOLDERID_Downloads).expect("下载图标应当取得到");
        assert_eq!(&desktop[..4], &[0x89, b'P', b'N', b'G']);
        assert_ne!(desktop, downloads, "桌面与下载不该是同一张图");
    }

    /// 驱动器用外壳的「固定磁盘」图标。
    #[test]
    fn 驱动器取得到固定盘图标() {
        use windows_sys::Win32::UI::Shell::SIID_DRIVEFIXED;

        let png = stock_icon_png(SIID_DRIVEFIXED).expect("固定盘图标应当取得到");
        assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']);
        let folder = folder_icon_png().expect("文件夹图标应当取得到");
        assert_ne!(png, folder, "磁盘不该画成文件夹");
    }

    /// 并发取类型图标不得失败。
    ///
    /// 回归的是这样一个 bug：**进程内第一次** `SHGetFileInfoW(SHGFI_ICON)`
    /// 在多条线程上同时发生时，除了跑赢的那一条，其余全部返回 0。
    /// 实测形态非常干净——八条线程里有七条在**各自的第 0 次**调用上失败，
    /// 之后 393 次全部成功。图标提取跑在 `spawn_blocking` 的线程池上，
    /// 打开一个目录同时要好几种类型的图标，这个形态正好撞上。
    #[test]
    fn 并发取类型图标不失败() {
        let workers: Vec<_> = (0..8)
            .map(|t| {
                std::thread::spawn(move || {
                    (0..50)
                        .filter(|i| {
                            // 三个入口轮着来：通用文件、文件夹、按扩展名，
                            // 免得只把其中一条路热起来就以为好了。
                            match (t + i) % 3 {
                                0 => generic_file_icon_png().is_err(),
                                1 => folder_icon_png().is_err(),
                                _ => file_type_icon_png("txt").is_err(),
                            }
                        })
                        .count()
                })
            })
            .collect();
        let failures: usize = workers.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(failures, 0, "并发下有 {failures}/400 次取图标失败");
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
