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
        matches!(
            c,
            '.' | '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
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
#[derive(Clone)]
pub struct FileTypeIcons {
    icons: Arc<IconCache>,
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
        }
    }

    /// `GET /files/icon/{ext}`：这一类文件的系统图标。
    ///
    /// | 情况 | 返回 | HTTP |
    /// |---|---|---|
    /// | 扩展名不合法（含点号 / 分隔符 / 超长） | `invalid_request` | 400 |
    /// | 本平台不提供（[`available`] 为假，Linux 恒如此） | `not_found` | 404 |
    /// | 系统给不出这一类的图标 | `not_found` | 404 |
    ///
    /// 第三档在 Windows / macOS 上几乎不发生（未知类型也有「白纸」图标），
    /// 前端因此把 404 当「平台不提供」的探测信号用。
    pub async fn icon_png(&self, ext: &str) -> ApiResult<Arc<Vec<u8>>> {
        validate_ext(ext)?;
        if !available() {
            return Err(ApiError::not_found("本平台无法取得文件类型图标"));
        }
        let key = ext.to_lowercase();
        self.icons
            .get(&key)
            .await
            .ok_or_else(|| ApiError::not_found(format!("系统没有给出 .{key} 的类型图标")))
    }

    /// 当前缓存条目数。给测试与日志用。
    pub fn len(&self) -> usize {
        self.icons.len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.icons.is_empty()
    }
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

#[cfg(windows)]
fn extract(ext: &str) -> Option<Vec<u8>> {
    match crate::platform::windows::icon::file_type_icon_png(ext) {
        Ok(png) => Some(png),
        Err(e) => {
            tracing::debug!(ext, error = %e, "取文件类型图标失败");
            None
        }
    }
}

#[cfg(target_os = "macos")]
fn sys_available() -> bool {
    crate::platform::appkit::available()
}

#[cfg(target_os = "macos")]
fn extract(ext: &str) -> Option<Vec<u8>> {
    crate::platform::appkit::file_type_icon_png(ext, FILE_ICON_SIZE)
}

/// Linux（及其余平台）：服务器常态既没有图标主题也没有桌面数据库，
/// 见模块文档的表格与 roadmap/12 §8 未决 3。
#[cfg(not(any(windows, target_os = "macos")))]
fn sys_available() -> bool {
    false
}

#[cfg(not(any(windows, target_os = "macos")))]
fn extract(_ext: &str) -> Option<Vec<u8>> {
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
