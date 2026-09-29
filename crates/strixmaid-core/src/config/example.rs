//! 与本平台默认值一致的示例配置。

use super::{Config, DEFAULT_CONFIG_PATH};
use std::path::{Path, PathBuf};

impl Config {
    // ---------------------------------------------------------------- 示例

    /// 一份带中文注释的完整示例配置，用于生成 [`DEFAULT_CONFIG_PATH`] 那个文件。
    ///
    /// 其中所有取值均等于**本平台的**内置默认值（有单元测试保证），
    /// 因此原样安装也不会改变行为。路径与提权组在三个平台上不同，由本函数
    /// 按平台填进模板——示例配置是给人照抄的，印一份在本机根本不存在的路径
    /// （比如在 Windows 上写 `/var/lib/strixmaid`）比不给示例更糟。
    pub fn example_toml() -> String {
        let d = Config::default();
        EXAMPLE_TOML
            .replace("@CONFIG_PATH@", DEFAULT_CONFIG_PATH)
            .replace("@DATA_DIR@", &toml_path(&d.data_dir))
            .replace("@RUN_DIR@", &toml_path(&d.run_dir))
            .replace(
                "@ELEVATE_GROUPS@",
                &toml_string_list(&d.session.elevate_groups),
            )
            .replace("@ALLOWED_ROOTS@", &toml_path_list(&d.files.allowed_roots))
            .replace("@PLATFORM_NOTE@", PLATFORM_NOTE)
    }
}

/// 路径 → TOML 字符串字面量。
///
/// Windows 的路径里全是反斜杠，而 TOML 的**基本字符串**会把 `\U` 当成转义
/// （`"C:\Users"` 直接是语法错误）。因此一律用**字面量字符串**（单引号），
/// 它不做任何转义。Unix 路径里没有反斜杠，用同一套写法也完全合法。
fn toml_path(p: &Path) -> String {
    format!("'{}'", p.display())
}

/// 路径列表 → TOML 数组。
fn toml_path_list(items: &[PathBuf]) -> String {
    let inner: Vec<String> = items.iter().map(|p| toml_path(p)).collect();
    format!("[{}]", inner.join(", "))
}

/// 字符串列表 → TOML 数组（组名里不会有反斜杠，用普通双引号）。
fn toml_string_list(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", inner.join(", "))
}

// ===========================================================================
// 示例配置
// ===========================================================================

const EXAMPLE_TOML: &str = include_str!("example.toml");

/// 示例配置里的平台提示行，由 [`Config::example_toml`] 填进 `@PLATFORM_NOTE@`。
///
/// 只讲「本平台与别的平台不一样的地方」，避免读者照着另一个平台的文档找不存在的路径。
#[cfg(windows)]
const PLATFORM_NOTE: &str = r"#
# 本文件是 Windows 版：路径默认在 %ProgramData%\StrixMaid 下；
# pam_service 与 run_dir 在本平台无效（认证走 LogonUserW，IPC 走命名管道）。
";

/// 见上。
#[cfg(target_os = "macos")]
const PLATFORM_NOTE: &str = r"#
# 本文件是 macOS 版：数据目录在 /var/db、运行目录在 /var/run，
# 不是 Linux 的 /var/lib 与 /run（那两条路径在 macOS 上不存在）。
# 服务宿主是 launchd 而非 systemd，日志由 plist 的 StandardOutPath 收走。
";

/// 见上。
#[cfg(all(not(windows), not(target_os = "macos")))]
const PLATFORM_NOTE: &str = "";
