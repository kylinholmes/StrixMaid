//! 文件类型图标：按**扩展名**取一张系统真图标的 PNG（roadmap/12 §4.7、§8 未决 8）。
//!
//! 内置图标集（`web/src/assets/icons/`）把常见类型盖住了，这条路提供的是
//! 「Word 文件显示系统里真正的 Word 图标」这一档：前端优先取系统图标，
//! 404 时回落内置集。
//!
//! # key 是扩展名，不是路径
//!
//! 这不是实现便利，是本端点能放在**主进程、不经 worker** 的前提
//! （`/files/*` 其余端点全部经 worker，理由见 `routes/files.rs`）：
//!
//! - 类型图标是**操作系统的属性**（注册表关联 / UTType 数据库），与登录用户
//!   无关，也不产生任何对用户文件的访问——Windows 用 `SHGFI_USEFILEATTRIBUTES`
//!   查一个虚构文件名，macOS 查的是 UTType 数据库，两边都不碰磁盘上的真实文件。
//!   没有文件访问，就没有「以谁的身份访问」的问题。
//! - 按类型取才有缓存价值：300 个文件的目录大概 8 种类型 → 8 个请求（§4.7），
//!   按路径取则每个文件一发、且没有任何两次命中同一个 key。
//!
//! 唯一的按路径例外是 macOS 的 bundle（[`FileTypeIcons::path_icon_png`]）：
//! `.app` 的图标是**这一个应用**的，类型层面不存在。前端只对「名字带扩展名的
//! 目录 / 符号链接」走这条路，量级是每个应用一次而不是每个文件一次。
//!
//! # 平台
//!
//! | 平台 | 实现 | [`available`] |
//! |---|---|---|
//! | Windows | `SHGetFileInfoW`（[`crate::platform::windows::icon::file_type_icon_png`]） | 恒 true |
//! | macOS | `NSWorkspace`（[`crate::platform::appkit`]） | 有窗口服务器连接才 true |
//! | Linux | 无（服务器常态没有图标主题，见 §8 未决 3） | 恒 false |
//!
//! `available` 为 false 时端点一律 404，前端探测一次后整个会话不再来问
//! （`web/src/workspace/sysicons.ts`）。
//!
//! # 缓存
//!
//! 本体是 [`IconCache`]，与进程 / 服务图标同一套（TTL、负缓存、容量、
//! single-flight），只换 extractor——§4.7 明文要求「照搬，不另发明」。
//! key 统一转小写：`PDF` 与 `pdf` 是同一类，两个 key 会把同一张图存两份。
//! 不预热：类型集合事先不可知（随用户浏览的目录而定），predict 不如 lazy。

use std::sync::Arc;

use strixmaid_types::{ApiError, ApiResult};

use crate::providers::process::icon::IconCache;

/// 输出图标的边长（像素）。
///
/// 与进程图标的 32 不同档：类型图标要撑平铺视图 68 CSS 像素的格子，
/// 32 放大到那里明显发糊。Windows 侧的常量与此对齐
/// （[`crate::platform::windows::icon::FILE_ICON_SIZE`]，那边的回落路径可能
/// 给出系统大图标档的尺寸，见其文档——PNG 自带尺寸，前端按 CSS 定尺渲染）。
pub const FILE_ICON_SIZE: u32 = 64;

/// 扩展名的长度上限（字节）。真实扩展名极少超过 10；这个上限挡的是
/// 拿超长串试探的请求。
const MAX_EXT_LEN: usize = 32;

/// 路径的长度上限（字节）。`PATH_MAX` 一档，宽裕即可。
const MAX_PATH_LEN: usize = 4096;

/// 保留 key：目录的系统图标。`$` 被 [`validate_ext`] 拒绝，真实扩展名
/// 撞不上保留名——这不是巧合，是 `$` 进拒绝清单的理由。
pub const DIR_KEY: &str = "$dir";

/// 保留 key：无扩展名 / 认不出类型的文件的系统图标。
pub const GENERIC_FILE_KEY: &str = "$file";

// ---------------------------------------------------------------------------
// 快速访问栏的保留 key（roadmap/12 §8 未决 3 的 Windows 部分）
// ---------------------------------------------------------------------------
//
// 左栏的「主目录 / 桌面 / 下载 / …… / 驱动器」在 Windows 上原先一律回落内置
// 图标集，理由写在 `web/src/workspace/sysicons.ts` 的 `iconKeysOf`：拿 `$dir`
// 去取会把它们全画成同一只黄文件夹，反而丢信息。
//
// 现在给它们各开一个保留 key。**仍然不接受路径**——取值是下面这张固定的表，
// 调用方编不出新的，所以本端点「不碰用户文件、可以留在主进程」的前提一行
// 不改（见本模块开头的「key 是扩展名，不是路径」）。

/// 保留 key：登录用户的主目录。
pub const HOME_KEY: &str = "$home";
/// 保留 key：桌面。
pub const DESKTOP_KEY: &str = "$desktop";
/// 保留 key：文稿 / 文档。
pub const DOCUMENTS_KEY: &str = "$documents";
/// 保留 key：下载。
pub const DOWNLOADS_KEY: &str = "$downloads";
/// 保留 key：图片。
pub const PICTURES_KEY: &str = "$pictures";
/// 保留 key：音乐。
pub const MUSIC_KEY: &str = "$music";
/// 保留 key：视频 / 影片。
pub const VIDEOS_KEY: &str = "$videos";
/// 保留 key：公共。
pub const PUBLIC_KEY: &str = "$public";
/// 保留 key：「此电脑」/ 全部驱动器这个虚拟根。
pub const COMPUTER_KEY: &str = "$computer";
/// 保留 key：一块固定磁盘。
pub const DRIVE_KEY: &str = "$drive";

/// 全部保留 key。`$` 被 [`validate_ext`] 拒绝，真实扩展名撞不上它们。
///
/// 前端的同名表在 `web/src/workspace/sysicons.ts`，两边必须一致——
/// 多一个少一个只会表现为「那一项没有图标」，不会报错。
pub const RESERVED_KEYS: &[&str] = &[
    DIR_KEY,
    GENERIC_FILE_KEY,
    HOME_KEY,
    DESKTOP_KEY,
    DOCUMENTS_KEY,
    DOWNLOADS_KEY,
    PICTURES_KEY,
    MUSIC_KEY,
    VIDEOS_KEY,
    PUBLIC_KEY,
    COMPUTER_KEY,
    DRIVE_KEY,
];

/// 本平台 / 本运行环境能不能取文件类型图标。
pub fn available() -> bool {
    sys_available()
}

/// 校验端点收到的扩展名。**不含**前导点：`pdf`，不是 `.pdf`。
///
/// 这道闸是承重的：Windows 侧会把它拼进 `strixmaid.<ext>` 这个虚构文件名交给
/// 外壳，macOS 侧交给 `UTType`。拒绝点号既挡 `..`，也保证「一个 key 恰好
/// 一段扩展名」；分隔符、Windows 文件名非法字符与控制字符照
/// [`crate::providers::process::icon::validate_name`] 的清单拒绝。
pub fn validate_ext(ext: &str) -> ApiResult<()> {
    if ext.is_empty() {
        return Err(ApiError::invalid_request("扩展名不能为空"));
    }
    if ext.len() > MAX_EXT_LEN {
        return Err(ApiError::invalid_request(format!(
            "扩展名过长（{} 字节，上限 {MAX_EXT_LEN}）",
            ext.len()
        )));
    }
    if let Some(bad) = ext.chars().find(|c| {
        // `$` 不是 Windows 的非法字符，挡它是为了给 [`DIR_KEY`] /
        // [`GENERIC_FILE_KEY`] 留出撞不上真实扩展名的保留名字空间。
        matches!(
            c,
            '.' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '$'
        ) || c.is_control()
            || c.is_whitespace()
    }) {
        return Err(ApiError::invalid_request(format!(
            "扩展名含非法字符：{bad:?}"
        )));
    }
    Ok(())
}

/// 文件类型图标的缓存。挂在 [`crate::providers::fs`] 对应的路由状态上，
/// 生命周期随进程。`Clone` 廉价（内部是 `Arc`），克隆出来的共用同一份缓存。
///
/// 两份 [`IconCache`]，key 语义不同（与进程 / 服务图标的分立同一道理）：
/// `types` 按小写扩展名（外加两个保留 key），`paths` 按绝对路径——后者只在
/// macOS 上有货（`.app` 这类 bundle 的真实图标，见 [`Self::path_icon_png`]）。
#[derive(Clone)]
pub struct FileTypeIcons {
    icons: Arc<IconCache>,
    paths: Arc<IconCache>,
}

impl Default for FileTypeIcons {
    fn default() -> Self {
        Self::new()
    }
}

impl FileTypeIcons {
    /// 建一份空缓存。key 是**小写**扩展名，见模块文档。
    pub fn new() -> Self {
        FileTypeIcons {
            icons: Arc::new(IconCache::with_extractor(extract)),
            paths: Arc::new(IconCache::with_extractor(extract_by_path)),
        }
    }

    /// `GET /files/icon/{ext}`：这一类文件的系统图标。
    ///
    /// `ext` 还接受两个保留 key：[`DIR_KEY`]（文件夹）与 [`GENERIC_FILE_KEY`]
    /// （无扩展名的文件）。`$` 在扩展名消毒里被拒绝，保留名撞不上真实类型。
    ///
    /// | 情况 | 返回 | HTTP |
    /// |---|---|---|
    /// | 扩展名不合法（含点号 / 分隔符 / `$` / 超长） | `invalid_request` | 400 |
    /// | 本平台不提供（[`available`] 为假，Linux 恒如此） | `not_found` | 404 |
    /// | 系统给不出这一类的图标 | `not_found` | 404 |
    ///
    /// 第三档在 Windows / macOS 上几乎不发生（未知类型也有「白纸」图标），
    /// 前端因此把 404 当「平台不提供」的探测信号用。
    pub async fn icon_png(&self, ext: &str) -> ApiResult<Arc<Vec<u8>>> {
        let key = if RESERVED_KEYS.contains(&ext) {
            ext.to_owned()
        } else {
            validate_ext(ext)?;
            ext.to_lowercase()
        };
        if !available() {
            return Err(ApiError::not_found("本平台无法取得文件类型图标"));
        }
        self.icons
            .get(&key)
            .await
            .ok_or_else(|| ApiError::not_found(format!("系统没有给出 {key} 的类型图标")))
    }

    /// `GET /files/icon-path?path=`：一个**具体条目**的系统图标，按绝对路径。
    ///
    /// 只有 macOS 有这条路（`NSWorkspace iconForFile:`）：`.app` bundle 显示
    /// 应用自己的图标而不是文件夹，符号链接解析到目标。Windows 不开——那边
    /// 按真实路径取图标是一次以**服务进程身份**的磁盘访问（`desktop.ini`、
    /// 图标文件），与「类型图标不碰磁盘所以可以不经 worker」的前提相抵触；
    /// macOS 能开是因为 [`available`] 已把它限定在「有窗口服务器连接」的
    /// 形态（本机以登录用户跑），root 守护进程形态下天然关闭。
    ///
    /// 展示范围（`files.allowed_roots`）的把关在路由层，与 `/files` 其余
    /// 端点同一份配置。
    pub async fn path_icon_png(&self, path: &str) -> ApiResult<Arc<Vec<u8>>> {
        validate_path(path)?;
        if !path_available() {
            return Err(ApiError::not_found("本平台无法按路径取图标"));
        }
        self.paths
            .get(path)
            .await
            .ok_or_else(|| ApiError::not_found(format!("系统没有给出 {path} 的图标")))
    }

    /// 当前缓存条目数（两份合计）。给测试与日志用。
    pub fn len(&self) -> usize {
        self.icons.len() + self.paths.len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 校验按路径取图标的路径：绝对路径、长度受限、不含控制字符、不含 `..`。
///
/// 拒绝 `..` 是承重的：展示范围的检查（`allowed_roots`）在路由层做的是
/// **归一化前**的字符串前缀比较，`/home/x/../../etc` 能对上 `/home/x`
/// 这个 root——放行 `..` 就等于放行范围外的任意路径。
fn validate_path(path: &str) -> ApiResult<()> {
    if !path.starts_with('/') {
        return Err(ApiError::invalid_request("必须是绝对路径"));
    }
    if path.len() > MAX_PATH_LEN {
        return Err(ApiError::invalid_request(format!(
            "路径过长（{} 字节，上限 {MAX_PATH_LEN}）",
            path.len()
        )));
    }
    if path.split('/').any(|seg| seg == "..") {
        return Err(ApiError::invalid_request("路径不能含 `..`"));
    }
    if path.chars().any(char::is_control) {
        return Err(ApiError::invalid_request("路径含控制字符"));
    }
    Ok(())
}

// ===========================================================================
// 平台分派
// ===========================================================================

#[cfg(windows)]
fn sys_available() -> bool {
    // 与进程图标同一结论：SHGetFileInfoW + 内存里的图标转换不需要窗口站与
    // 桌面，会话 0 的服务进程里同样可用。
    true
}

/// 按路径取图标只有 macOS 有，理由见 [`FileTypeIcons::path_icon_png`]。
fn path_available() -> bool {
    #[cfg(target_os = "macos")]
    return crate::platform::appkit::available();
    #[cfg(not(target_os = "macos"))]
    false
}

/// 保留 key → Windows 的 `KNOWNFOLDERID`。不是已知文件夹的保留 key 返回
/// `None`（它们各有各的取法）。
#[cfg(windows)]
fn known_folder_of(key: &str) -> Option<&'static windows_sys::core::GUID> {
    use windows_sys::Win32::UI::Shell as sh;
    Some(match key {
        HOME_KEY => &sh::FOLDERID_Profile,
        DESKTOP_KEY => &sh::FOLDERID_Desktop,
        DOCUMENTS_KEY => &sh::FOLDERID_Documents,
        DOWNLOADS_KEY => &sh::FOLDERID_Downloads,
        PICTURES_KEY => &sh::FOLDERID_Pictures,
        MUSIC_KEY => &sh::FOLDERID_Music,
        VIDEOS_KEY => &sh::FOLDERID_Videos,
        PUBLIC_KEY => &sh::FOLDERID_Public,
        COMPUTER_KEY => &sh::FOLDERID_ComputerFolder,
        _ => return None,
    })
}

#[cfg(windows)]
fn extract(key: &str) -> Option<Vec<u8>> {
    use crate::platform::windows::icon;
    let got = match key {
        DIR_KEY => icon::folder_icon_png(),
        GENERIC_FILE_KEY => icon::generic_file_icon_png(),
        // 磁盘没有路径可言，用外壳的库存图标。
        DRIVE_KEY => icon::stock_icon_png(windows_sys::Win32::UI::Shell::SIID_DRIVEFIXED),
        k if known_folder_of(k).is_some() => {
            icon::known_folder_icon_png(known_folder_of(k).expect("刚判过"))
        }
        ext => icon::file_type_icon_png(ext),
    };
    match got {
        Ok(png) => Some(png),
        Err(e) => {
            tracing::debug!(key, error = %e, "取文件类型图标失败");
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn sys_available() -> bool {
    crate::platform::appkit::available()
}

#[cfg(target_os = "macos")]
fn extract(key: &str) -> Option<Vec<u8>> {
    use crate::platform::appkit;
    match key {
        DIR_KEY => appkit::folder_icon_png(FILE_ICON_SIZE),
        GENERIC_FILE_KEY => appkit::generic_file_icon_png(FILE_ICON_SIZE),
        // 左栏那几个保留 key 在 macOS 上不走这里（`iconKeysOf` 在 unix 平台
        // 给 pathKey，按路径取的才是真身）。不拦的话它们会落到下一条、被当成
        // 扩展名交给 UTType——UTType 对未知扩展名照样给一张通用图，于是成了
        // 「200 + 一张错图」而不是 404。
        k if RESERVED_KEYS.contains(&k) => None,
        ext => appkit::file_type_icon_png(ext, FILE_ICON_SIZE),
    }
}

#[cfg(target_os = "macos")]
fn extract_by_path(path: &str) -> Option<Vec<u8>> {
    crate::platform::appkit::file_icon_png(path, FILE_ICON_SIZE)
}

/// Linux（及其余平台）：服务器常态既没有图标主题也没有桌面数据库，
/// 见模块文档的表格与 roadmap/12 §8 未决 3。
#[cfg(not(any(windows, target_os = "macos")))]
fn sys_available() -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
fn extract(_key: &str) -> Option<Vec<u8>> {
    None
}

#[cfg(not(target_os = "macos"))]
fn extract_by_path(_path: &str) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use strixmaid_types::ErrorCode;

    #[test]
    fn 扩展名消毒() {
        for ok in ["pdf", "tar", "c", "h", "rs", "7z", "c++", "中文后缀", "a-b_c~d#e"] {
            assert!(validate_ext(ok).is_ok(), "{ok:?} 应当放行");
        }
        for bad in [
            "",
            ".",
            "..",
            "tar.gz",
            ".pdf",
            "a/b",
            "a\\b",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
            "a b",
            "a\tb",
            "a\0b",
            "$dir", // 保留名字空间：`$` 对普通扩展名一律拒绝
            "a$b",
        ] {
            assert!(validate_ext(bad).is_err(), "{bad:?} 应当被拒绝");
        }
        assert!(validate_ext(&"a".repeat(MAX_EXT_LEN)).is_ok(), "刚好到上限应当放行");
        assert!(validate_ext(&"a".repeat(MAX_EXT_LEN + 1)).is_err(), "超长应当被拒绝");
    }

    #[tokio::test]
    async fn 不合法的扩展名报_400_不碰缓存() {
        let icons = FileTypeIcons::new();
        let err = icons.icon_png("tar.gz").await.expect_err("应当被拒绝");
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(icons.is_empty(), "被拒绝的请求不该在缓存里留条目");
    }

    #[tokio::test]
    async fn 大小写合并成同一个_key() {
        if !available() {
            eprintln!("本平台 / 本环境不提供文件类型图标，跳过");
            return;
        }
        let icons = FileTypeIcons::new();
        let a = icons.icon_png("txt").await.expect("txt 应当取得到");
        let b = icons.icon_png("TXT").await.expect("TXT 应当取得到");
        assert_eq!(a, b, "大小写不同的同一类应当给同一张图");
        assert_eq!(icons.len(), 1, "两次请求应当只占一格缓存");
    }

    #[test]
    fn 路径消毒() {
        for ok in ["/", "/Applications/Zed.app", "/home/kylin/文档"] {
            assert!(validate_path(ok).is_ok(), "{ok:?} 应当放行");
        }
        for bad in [
            "",
            "relative/path",
            "C:\\Windows",
            "/home/x/../../etc", // `..` 能骗过路由层的前缀检查，必须在这里挡
            "/..",
            "/tmp/a\0b",
            "/tmp/a\tb",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} 应当被拒绝");
        }
        assert!(validate_path(&format!("/{}", "a".repeat(MAX_PATH_LEN))).is_err());
    }

    #[tokio::test]
    async fn 保留_key_取得到文件夹与通用文件图标() {
        if !available() {
            eprintln!("本平台 / 本环境不提供文件类型图标，跳过");
            return;
        }
        let icons = FileTypeIcons::new();
        let dir = icons.icon_png(DIR_KEY).await.expect("$dir 应当取得到");
        let file = icons
            .icon_png(GENERIC_FILE_KEY)
            .await
            .expect("$file 应当取得到");
        assert_ne!(dir, file, "文件夹与通用文件不该是同一张图");
    }

    /// 快速访问栏那几个保留 key 各有各的图标。
    ///
    /// 只在 Windows 上跑：macOS 的左栏走的是**按路径**那条路（`iconKeysOf`
    /// 在 unix 平台给 pathKey），那边这些 key 本来就不该被请求。
    #[cfg(windows)]
    #[tokio::test]
    async fn 左栏保留_key_各有各的图标() {
        let icons = FileTypeIcons::new();
        let get = async |k: &str| icons.icon_png(k).await.unwrap_or_else(|e| panic!("{k}: {e:?}"));

        let downloads = get(DOWNLOADS_KEY).await;
        let desktop = get(DESKTOP_KEY).await;
        let drive = get(DRIVE_KEY).await;
        let dir = get(DIR_KEY).await;

        assert_ne!(downloads, desktop, "下载与桌面不该是同一张图");
        assert_ne!(drive, dir, "磁盘不该画成文件夹");
        assert_ne!(downloads, dir, "下载不该退回成通用文件夹");
    }

    /// 左栏的保留 key 只在 Windows 上有货，别的平台一律 404。
    ///
    /// 在 macOS（有窗口服务器时）这条是真的回归护栏：漏掉 `extract` 里那条
    /// 拦截的话，`$downloads` 会被当成扩展名拿去查 UTType 并拿回一张通用图。
    /// Linux 上 `available()` 为假，恒过。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn 左栏保留_key_在非_windows_上一律_404() {
        let icons = FileTypeIcons::new();
        for k in [HOME_KEY, DOWNLOADS_KEY, DRIVE_KEY, COMPUTER_KEY] {
            let err = icons.icon_png(k).await.expect_err(k);
            assert_eq!(err.code, ErrorCode::NotFound, "{k}");
        }
    }

    #[tokio::test]
    async fn 按路径取图标只在_macos_有() {
        let icons = FileTypeIcons::new();
        if path_available() {
            let png = icons
                .path_icon_png("/System/Library/CoreServices/Finder.app")
                .await;
            assert!(png.is_ok(), "macOS 上 Finder.app 的图标应当取得到");
        } else {
            let err = icons.path_icon_png("/tmp").await.expect_err("非 macOS 应当 404");
            assert_eq!(err.code, ErrorCode::NotFound);
        }
        // 消毒先于能力探测：坏路径在任何平台都是 400。
        let err = icons
            .path_icon_png("../etc")
            .await
            .expect_err("相对路径应当被拒绝");
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn 平台不可用时一律_404() {
        if available() {
            return;
        }
        let icons = FileTypeIcons::new();
        let err = icons.icon_png("txt").await.expect_err("应当 404");
        assert_eq!(err.code, ErrorCode::NotFound);
        assert!(icons.is_empty(), "不可用的平台一次缓存都不该碰");
    }
}
