//! Windows 的进程图标：映像名 → 正在运行的同名进程的 exe → PE 资源里的图标。
//!
//! 本文件只做「接起来」这一件事，两端各自在别处：
//!
//! | 环节 | 在哪 |
//! |---|---|
//! | 映像名 → exe 完整路径 | [`crate::providers::process::windows::image_path_by_name`] |
//! | exe → 32×32 PNG | [`crate::platform::windows::icon`] |
//! | 缓存、并发合并、预热 | [`super`] |
//!
//! # 名字为什么一定要去进程表反查
//!
//! 安全要害，完整论证在 [`crate::providers::process::windows::image_path_by_name`]
//! 的文档里。一句话：不去 `PATH` 里搜同名文件（机器上装了三个 `python.exe` 时
//! 会拿错），也不让调用方传路径（那等于开一个读任意文件的口子）。
//!
//! # `exe` 参数在这里是可选的快捷方式
//!
//! 给了就直接用，省掉一次进程表遍历；没给就自己反查。Windows 只需要路径，
//! 名字对它而言只是查路径的钥匙——与 Linux 恰好相反，见 [`super`] 的模块文档。
//!
//! # 取不到图标是常态，不是错误
//!
//! 三种情况都会返回 `None`，并且都是正常的：
//!
//! 1. 没有同名进程在跑（乱传的名字走到这里）；
//! 2. 有同名进程，但非管理员身份下 `OpenProcess` 被拒，取不到它的 exe 路径
//!    （`services.exe`、`csrss.exe` 这类；任务管理器不提权时那些行同样没有图标）；
//! 3. 取到了路径，但那个文件里没有图标资源（不少控制台程序就没有）。
//!
//! 一律如实返回 `None` 由上层记负缓存，不拿占位图冒充。

use std::path::Path;

use crate::platform::windows::icon;
use crate::providers::process::windows::image_path_by_name;

/// Windows 上恒为 true。
///
/// 取图标只用到 `PrivateExtractIconsW` 与内存 DC，两者都不需要窗口站与桌面，
/// 服务进程（会话 0、非交互窗口站）里同样可用。也就是说没有「headless 的
/// Windows」这种情形需要在这里挡掉——真正取不到图标的是**单个程序**，
/// 由 [`icon_png`] 逐个返回 `None`，而不是整个平台不可用。
pub fn available() -> bool {
    true
}

/// 取一个正在运行的程序的图标，PNG 字节。
pub fn icon_png(name: &str, exe: Option<&Path>) -> Option<Vec<u8>> {
    let path = match exe {
        Some(p) => p.to_string_lossy().into_owned(),
        None => image_path_by_name(name)?,
    };
    match icon::icon_png(&path) {
        Ok(png) => Some(png),
        Err(e) => {
            tracing::debug!(name, path, error = %e, "取程序图标失败");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 当前测试进程自己一定在进程表里，名字也一定能反查出路径
    /// （自己的进程总是打得开）。
    fn 本进程映像名() -> Option<String> {
        std::env::current_exe()
            .ok()?
            .file_name()?
            .to_str()
            .map(str::to_owned)
    }

    #[test]
    fn 本进程的名字能反查出自己的路径() {
        let Some(name) = 本进程映像名() else {
            eprintln!("取不到当前可执行文件名，跳过");
            return;
        };
        let path = image_path_by_name(&name).expect("自己的进程一定打得开");
        assert!(
            path.to_lowercase().ends_with(&name.to_lowercase()),
            "反查出的路径 {path} 与名字 {name} 对不上"
        );
    }

    #[test]
    fn 不存在的名字取不到() {
        assert!(image_path_by_name("绝不会有这个进程_9f3a1c.exe").is_none());
        assert!(icon_png("绝不会有这个进程_9f3a1c.exe", None).is_none());
        assert!(image_path_by_name("").is_none());
    }

    #[test]
    fn 取一个必然在跑的程序的图标() {
        // explorer.exe 在有交互会话的机器上必然在跑；Server Core 上没有，
        // 那时改用本进程自己（Rust 测试二进制未必带图标资源，取不到就跳过）。
        for name in ["explorer.exe"].into_iter().map(str::to_owned).chain(本进程映像名()) {
            if let Some(png) = icon_png(&name, None) {
                assert_eq!(
                    &png[..4],
                    &[0x89, b'P', b'N', b'G'],
                    "{name} 的图标不是合法 PNG"
                );
                return;
            }
        }
        eprintln!("本机没有任何一个候选程序能取到图标（可能是 Server Core 或无图标资源），跳过");
    }

    #[test]
    fn 直接给路径时不再反查进程表() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_owned());
        let exe = std::path::PathBuf::from(format!("{root}\\explorer.exe"));
        if !exe.is_file() {
            eprintln!("本机没有 {}，跳过", exe.display());
            return;
        }
        // 名字故意给一个不存在的进程：走 exe 这条路就不该受影响。
        match icon_png("绝不会有这个进程_9f3a1c.exe", Some(&exe)) {
            Some(png) => assert_eq!(&png[..4], &[0x89, b'P', b'N', b'G']),
            None => eprintln!("本机取不到 explorer.exe 的图标，跳过"),
        }
    }
}
