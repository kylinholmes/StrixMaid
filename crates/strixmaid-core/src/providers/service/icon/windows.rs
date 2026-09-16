//! Windows 的服务图标：服务名 → `ImagePath` → 可执行文件 → PE 资源里的图标。
//!
//! 本文件只做「把服务名解析成一个二进制路径」这一件事，两端各自在别处：
//!
//! | 环节 | 在哪 |
//! |---|---|
//! | 服务名 → `ImagePath` 原始串 | [`crate::platform::windows::registry::reg_string`] |
//! | 二进制路径 → 32×32 PNG | [`crate::platform::windows::icon`] |
//! | 缓存、并发合并 | [`crate::providers::process::icon::IconCache`] |
//!
//! # 为什么读注册表而不是 `QueryServiceConfigW`
//!
//! 两条路拿到的是同一份数据——`QueryServiceConfigW` 的 `lpBinaryPathName` 读的
//! 就是 `HKLM\SYSTEM\CurrentControlSet\Services\<名字>\ImagePath`。取注册表这条
//! 是因为它不需要为每个服务各开一次 SCM 句柄（`OpenSCManagerW` +
//! `OpenServiceW(SERVICE_QUERY_CONFIG)`），而 [`super::super::scm::regkey`]
//! 已经在读同一个键，两处对「服务定义住在哪」的认定因此保持一致。
//!
//! # `ImagePath` 的形态很杂
//!
//! 本机实测（791 个服务键，719 条有 `ImagePath`）：
//!
//! | 形态 | 条数 | 例 |
//! |---|---|---|
//! | `REG_EXPAND_SZ` | 717 | `%SystemRoot%\system32\svchost.exe -k netsvcs -p` |
//! | `REG_SZ` | 2 | `"C:\Program Files (x86)\Microsoft\EdgeUpdate\MicrosoftEdgeUpdate.exe" /svc` |
//! | 含 `%变量%` | 268 | 同上第一行 |
//! | 以 `\SystemRoot\` 开头 | 198 | `\SystemRoot\System32\drivers\xxx.sys` |
//! | 以 `\??\` 开头 | 4 | `\??\C:\Program Files (x86)\...\SangforDnsDrv.sys` |
//! | 相对路径 | 195 | `System32\drivers\ACPI.sys` |
//! | 引号包裹（Win32 服务里） | 33 | `"C:\Program Files\Everything\Everything.exe" -svc` |
//! | 不带引号却跟着参数（Win32 服务里） | 244 | `C:\Windows\system32\svchost.exe -k netsvcs -p` |
//!
//! [`parse_image_path`] 是**纯函数**，上面每一种形态在本文件末尾都有对应的用例。
//! 真实世界里这个值是人和安装程序一起写出来的，只按「第一个空格前是路径」处理，
//! 装在 `C:\Program Files\...` 下的那一批会全部解析错。
//!
//! ## 环境变量展开发生两次，两次都必要
//!
//! `RegGetValueW` 在不带 `RRF_NOEXPAND` 时会**自己**把 `REG_EXPAND_SZ` 展开
//! （见 [`crate::platform::windows::registry::reg_string`]），717 条因此进不到
//! [`expand_env`] 就已经是实路径。剩下的 2 条是 `REG_SZ`：类型不是可展开串，
//! 注册表不会替它展开，值里真写了 `%变量%` 就得自己来。为这 2 条留一道展开
//! 不贵——[`expand_env`] 在没有 `%` 时直接原样返回，绝大多数调用连一次系统调用
//! 都不会发。
//!
//! # 驱动直接跳过
//!
//! 服务表里过半是内核驱动（本机 791 个键里 414 个），它们的 `ImagePath` 指向
//! `.sys`。`.sys` 是驱动映像，里面没有图标资源，拿去调 `PrivateExtractIconsW`
//! 只会失败一次再记一条负缓存。提前认出来省掉这一趟。
//!
//! 列表端点本身只枚举 `SERVICE_WIN32`（见 [`super::super::scm::sys::enum_services`]），
//! 驱动不会出现在界面上；但本端点是按名字直接查注册表的，调用方仍然可以拿一个
//! 驱动名来问，所以这道判断不能省。
//!
//! # 通用图标取自服务管理单元自己的 DLL
//!
//! [`GENERIC_ICON_SOURCE`] 是 `%SystemRoot%\System32\filemgmt.dll` 的**第 0 个
//! 图标**，也就是 `services.msc` 用的那对齿轮。选它而不是自己画一个：这是
//! Windows 对「服务」这个类别的官方图形，与用户在 services.msc 里看到的完全一致。
//!
//! 该 DLL 一共只有 4 组图标，索引 0 是齿轮，其余是文件夹之类，因此这里用的正是
//! [`crate::platform::windows::icon`] 已有的「取第 0 个」语义，不需要加索引参数。
//!
//! **提取一律在运行时做**，PNG 不进仓库：从用户自己机器上的系统文件取一张图发给
//! 该用户的浏览器，与把微软的位图签进代码库再分发，是两回事。
//!
//! ## MUI / MUN 重定向是透明的
//!
//! Windows 10 起大量系统 DLL 的资源被搬进了 `C:\Windows\SystemResources\*.mun`，
//! 原文件只剩存根（本机 `imageres.dll` 只有 2560 字节，而 `ExtractIconExW` 照样
//! 报出 334 个图标）。跟随重定向的是加载器，调用方什么也不用做——照常传
//! `System32` 下那个路径即可，`filemgmt.dll` 同理。

use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;

use crate::platform::windows::icon;
use crate::platform::windows::registry::{HKLM, reg_string};
use crate::platform::windows::wide::{from_wide_nul, to_wide};
use crate::providers::service::scm::map;

/// 通用图标的来源：服务管理单元自己的 DLL。见模块文档。
pub const GENERIC_ICON_SOURCE: &str = r"%SystemRoot%\System32\filemgmt.dll";

/// 可执行映像的扩展名。`ImagePath` 不带引号时靠它们判断路径在哪里结束。
///
/// `.bat` / `.cmd` 实际上当不了服务的主映像（SCM 要的是可执行映像），
/// 列在这里只是为了让「路径到哪结束」这一判断对畸形值也给得出答案。
const IMAGE_EXTS: [&str; 4] = [".exe", ".com", ".bat", ".cmd"];

/// 内核驱动的扩展名。见模块文档「驱动直接跳过」。
const DRIVER_EXT: &str = ".sys";

/// Windows 上恒为 true。
///
/// 与进程图标同理（见 [`crate::providers::process::icon::windows::available`]）：
/// 取图标只用到 `PrivateExtractIconsW` 与内存 DC，服务进程（会话 0）里同样可用。
/// 真正取不到图标的是**单个服务**，由 [`icon_png`] 逐个返回 `None`。
pub fn available() -> bool {
    true
}

/// 服务名 → 它的可执行文件完整路径。
///
/// 三种情况返回 `None`，都是常态：这个服务不存在或它的键读不出来、
/// 它没有 `ImagePath`、它是驱动。
pub fn binary_path(name: &str) -> Option<String> {
    let service = map::to_service_name(name);
    let raw = reg_string(HKLM, &map::service_subkey(&service), "ImagePath")?;
    Some(fold_case(&expand_env(&parse_image_path(&raw)?)?))
}

/// 通用齿轮图标的来源路径（已展开环境变量）。
pub fn generic_icon_path() -> Option<String> {
    Some(fold_case(&expand_env(GENERIC_ICON_SOURCE)?))
}

/// 折叠 ASCII 大小写，让同一个文件的不同写法落到同一条缓存。
///
/// 注册表里同一个 `svchost.exe` 被写成 `System32` 与 `system32` 两种（本机
/// 317 条里就有两种写法），而 Windows 的文件名不区分大小写——不折叠的话
/// 同一个文件会各占一格缓存，各捶一遍 GDI。返回值只用作缓存 key 与提取时的
/// 路径参数，不对外展示，因此大小写变了不影响任何人。
///
/// 只折 ASCII：`to_lowercase` 对个别非 ASCII 字符会改变**字符数**
/// （如 `İ` → `i̇`），拼出来的路径就不再指向原文件。非 ASCII 分量只在大小写
/// 写法不同的两条之间多占一格，代价远小于拿错文件。
fn fold_case(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// 取一个二进制文件的图标，PNG 字节。这是 [`super::ServiceIcons`] 交给缓存的
/// extractor，因此**参数就是缓存的 key**：一条二进制路径。
pub fn icon_png(binary: &str) -> Option<Vec<u8>> {
    match icon::icon_png(binary) {
        Ok(png) => Some(png),
        Err(e) => {
            tracing::debug!(binary, error = %e, "取服务图标失败");
            None
        }
    }
}

/// `ImagePath` 原始串 → 可执行文件路径（**可能仍含 `%变量%`**，由调用方展开）。
///
/// 纯函数，处理的形态见模块文档的表。驱动（`.sys`）返回 `None`。
pub fn parse_image_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }

    // 引号包裹时整段就是路径，里面的空格不是分隔符——`C:\Program Files\` 下的
    // 服务全靠这一条。没有闭引号（值被写坏）时把剩下的全当路径。
    let image = match raw.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => split_image(raw),
    };

    // `\??\` 是 Win32 对象命名空间的前缀，后面跟的是普通的 DOS 路径。
    let image = image.trim().strip_prefix(r"\??\").unwrap_or(image.trim());
    if image.is_empty() {
        return None;
    }
    if ends_with_ignore_ascii_case(image, DRIVER_EXT) {
        return None;
    }

    // `\SystemRoot\x` 与相对路径 `System32\x` 都以 Windows 目录为基准。
    // 统一改写成 `%SystemRoot%\x` 交给展开，而不是在这里读一次环境变量——
    // 这样本函数保持纯函数，能用固定输入测。
    if let Some(rest) = strip_prefix_ignore_ascii_case(image, r"\SystemRoot\") {
        return Some(format!(r"%SystemRoot%\{rest}"));
    }
    if is_relative(image) {
        return Some(format!(r"%SystemRoot%\{image}"));
    }
    Some(image.to_owned())
}

/// 不带引号的 `ImagePath` 里，可执行文件路径到哪里结束。
///
/// 判据是「第一个后面跟着空白（或到头、或跟着引号）的可执行扩展名」。
/// 认得出来就切在那里，认不出来就把整段当路径——后者会在提取图标时失败，
/// 那正是如实的结论，好过猜一个「第一个空格前」出来。
fn split_image(s: &str) -> &str {
    for (i, c) in s.char_indices() {
        if c != '.' {
            continue;
        }
        for ext in IMAGE_EXTS.iter().chain(std::iter::once(&DRIVER_EXT)) {
            let end = i + ext.len();
            // `get` 在不是字符边界时给 None，因此对非 ASCII 路径同样安全。
            if !s.get(i..end).is_some_and(|got| got.eq_ignore_ascii_case(ext)) {
                continue;
            }
            match s[end..].chars().next() {
                None => return &s[..end],
                Some(next) if next.is_whitespace() || next == '"' => return &s[..end],
                Some(_) => {}
            }
        }
    }
    s
}

/// 是不是相对路径：既没有盘符（`C:`）也不以 `\` 开头。
///
/// 以 `%` 开头的一律当作绝对路径：那是还没展开的环境变量（`%SystemRoot%\…`、
/// `%ProgramFiles%\…`），展开出来必然是绝对路径。不排除它的话会再补一层
/// `%SystemRoot%\`，拼出 `%SystemRoot%\%SystemRoot%\…` 这种必然失败的路径。
fn is_relative(p: &str) -> bool {
    if p.starts_with('\\') || p.starts_with('/') || p.starts_with('%') {
        return false;
    }
    let mut chars = p.chars();
    !matches!(
        (chars.next(), chars.next()),
        (Some(c), Some(':')) if c.is_ascii_alphabetic()
    )
}

fn ends_with_ignore_ascii_case(s: &str, suffix: &str) -> bool {
    s.len() >= suffix.len()
        && s.get(s.len() - suffix.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

fn strip_prefix_ignore_ascii_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &s[prefix.len()..])
}

/// 展开 `%变量%`。没有 `%` 时原样返回，连系统调用都不发。
///
/// 展开不出来（变量不存在）时 `ExpandEnvironmentStringsW` 把 `%变量%` 原样留在
/// 结果里，随后取图标会失败并记一条负缓存——这与「那台机器上确实没有这个文件」
/// 的结论一致，不需要在这里另行报错。
fn expand_env(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_owned());
    }
    let src = to_wide(s);
    // SAFETY: src 以 NUL 结尾；文档规定 lpdst 为空、nsize 为 0 时只回报所需长度。
    let needed = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), std::ptr::null_mut(), 0) };
    if needed == 0 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    // SAFETY: buf 有 needed 个 u16 可写，nsize 如实描述它的容量；src 在调用期间存活。
    let written = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), buf.as_mut_ptr(), needed) };
    if written == 0 || written > needed {
        return None;
    }
    let out = from_wide_nul(&buf);
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn 解析不带引号的路径与其后的参数() {
        assert_eq!(
            parse_image_path(r"C:\Windows\system32\svchost.exe -k netsvcs -p").as_deref(),
            Some(r"C:\Windows\system32\svchost.exe")
        );
        assert_eq!(
            parse_image_path(r"C:\Windows\System32\alg.exe").as_deref(),
            Some(r"C:\Windows\System32\alg.exe")
        );
        // 扩展名大小写不敏感
        assert_eq!(
            parse_image_path(r"C:\Windows\System32\SVCHOST.EXE -k x").as_deref(),
            Some(r"C:\Windows\System32\SVCHOST.EXE")
        );
    }

    #[test]
    fn 不带引号但路径里有空格时靠扩展名切() {
        // 这是「未加引号的服务路径」那一类，按第一个空格切会得到 `C:\Program`
        assert_eq!(
            parse_image_path(r"C:\Program Files\Foo\foo.exe").as_deref(),
            Some(r"C:\Program Files\Foo\foo.exe")
        );
        assert_eq!(
            parse_image_path(r"C:\Program Files\Foo\foo.exe --service -x").as_deref(),
            Some(r"C:\Program Files\Foo\foo.exe")
        );
    }

    #[test]
    fn 解析引号包裹的路径() {
        assert_eq!(
            parse_image_path(r#""C:\Program Files\Oray\AweSun\AweSun.exe" --mod=service"#)
                .as_deref(),
            Some(r"C:\Program Files\Oray\AweSun\AweSun.exe")
        );
        assert_eq!(
            parse_image_path(r#""D:\Program Files\BaiduNetdisk\YunUtilityService.exe""#).as_deref(),
            Some(r"D:\Program Files\BaiduNetdisk\YunUtilityService.exe")
        );
        // 参数里还带引号的也只取第一段
        assert_eq!(
            parse_image_path(r#""C:\a b\x.exe" -c "C:\y.exe""#).as_deref(),
            Some(r"C:\a b\x.exe")
        );
        // 少了闭引号（值被写坏）：把剩下的当路径，而不是整条放弃
        assert_eq!(
            parse_image_path(r#""C:\a b\x.exe"#).as_deref(),
            Some(r"C:\a b\x.exe")
        );
    }

    #[test]
    fn 剥掉_nt_命名空间前缀() {
        assert_eq!(
            parse_image_path(r"\??\C:\Program Files\Foo\foo.exe -x").as_deref(),
            Some(r"C:\Program Files\Foo\foo.exe")
        );
        assert_eq!(
            parse_image_path(r#""\??\C:\Program Files\Foo\foo.exe""#).as_deref(),
            Some(r"C:\Program Files\Foo\foo.exe")
        );
    }

    #[test]
    fn 系统根与相对路径都改写成变量形式() {
        assert_eq!(
            parse_image_path(r"\SystemRoot\System32\foo.exe").as_deref(),
            Some(r"%SystemRoot%\System32\foo.exe")
        );
        // 大小写不敏感
        assert_eq!(
            parse_image_path(r"\systemroot\System32\foo.exe -a").as_deref(),
            Some(r"%SystemRoot%\System32\foo.exe")
        );
        assert_eq!(
            parse_image_path(r"system32\foo.exe -a").as_deref(),
            Some(r"%SystemRoot%\system32\foo.exe")
        );
        // 已经是变量形式的保持原样，留给展开那一步
        assert_eq!(
            parse_image_path(r"%SystemRoot%\system32\svchost.exe -k AarSvcGroup -p").as_deref(),
            Some(r"%SystemRoot%\system32\svchost.exe")
        );
    }

    #[test]
    fn 驱动一律跳过() {
        for raw in [
            r"System32\drivers\ACPI.sys",
            r"\SystemRoot\System32\drivers\acpiex.sys",
            r"\??\C:\Program Files (x86)\Sangfor\SSL\DnsDriver2\SangforDnsDrv.sys",
            r#""\??\C:\Program Files (x86)\Qianxin\TrustAgent\trustproxy64.sys""#,
            r"C:\Windows\System32\drivers\foo.SYS",
            r"System32\drivers\foo.sys -x",
        ] {
            assert_eq!(parse_image_path(raw), None, "{raw} 是驱动，应当跳过");
        }
    }

    #[test]
    fn 空值与畸形值不会解析出垃圾() {
        assert_eq!(parse_image_path(""), None);
        assert_eq!(parse_image_path("   "), None);
        assert_eq!(parse_image_path("\"\""), None);
        assert_eq!(parse_image_path(r"\??\"), None);
        // 认不出扩展名时整段当路径：取图标会失败，那才是如实的结论
        assert_eq!(
            parse_image_path(r"C:\foo\bar -x").as_deref(),
            Some(r"C:\foo\bar -x")
        );
        // `.exe` 后面还跟着别的字符，不算路径的结尾
        assert_eq!(
            parse_image_path(r"C:\foo\bar.exe.config").as_deref(),
            Some(r"C:\foo\bar.exe.config")
        );
        // 非 ASCII 路径不会在切分时越过字符边界
        assert_eq!(
            parse_image_path(r"C:\程序\服务端.exe -k x").as_deref(),
            Some(r"C:\程序\服务端.exe")
        );
    }

    #[test]
    fn 展开环境变量() {
        // 没有 % 的原样返回
        assert_eq!(expand_env(r"C:\x.exe").as_deref(), Some(r"C:\x.exe"));
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        assert_eq!(
            expand_env(r"%SystemRoot%\System32\x.exe").as_deref(),
            Some(format!(r"{root}\System32\x.exe").as_str())
        );
        // 不存在的变量原样留下，后续取图标会失败——不在这里编一个路径出来
        assert_eq!(
            expand_env("%绝不会有这个变量_9f3a1c%").as_deref(),
            Some("%绝不会有这个变量_9f3a1c%")
        );
    }

    #[test]
    fn 本机的服务名能解析出真实存在的可执行文件() {
        use crate::platform::windows::registry::reg_subkeys;

        let names = reg_subkeys(HKLM, map::SERVICES_KEY);
        assert!(names.len() > 50, "只枚举到 {} 个服务键", names.len());

        let mut 有路径 = 0usize;
        let mut 存在 = 0usize;
        let mut 缺失 = Vec::new();
        for name in &names {
            let Some(path) = binary_path(name) else {
                continue;
            };
            有路径 += 1;
            if Path::new(&path).is_file() {
                存在 += 1;
            } else if 缺失.len() < 5 {
                缺失.push(format!("{name} => {path}"));
            }
        }
        eprintln!(
            "服务键 {} 个，解析出可执行文件 {有路径} 个，其中 {存在} 个确实在磁盘上",
            names.len()
        );
        assert!(有路径 > 10, "只解析出 {有路径} 个可执行文件，解析多半坏了");
        // 卸载残留的服务键会留下指向已删除文件的 ImagePath，少量缺失是正常的
        assert!(
            存在 * 10 >= 有路径 * 9,
            "解析出的路径里只有 {存在}/{有路径} 真的存在，样例：{缺失:?}"
        );
    }

    #[test]
    fn svchost_托管的服务共用同一条路径() {
        use crate::platform::windows::registry::reg_subkeys;

        let mut 按路径 = std::collections::HashMap::<String, usize>::new();
        for name in reg_subkeys(HKLM, map::SERVICES_KEY) {
            if let Some(path) = binary_path(&name) {
                *按路径.entry(path.to_lowercase()).or_default() += 1;
            }
        }
        let 总数: usize = 按路径.values().sum();
        eprintln!("解析出 {总数} 个服务、{} 个不同的二进制", 按路径.len());
        let Some((路径, 条数)) = 按路径.iter().max_by_key(|(_, n)| **n) else {
            eprintln!("本机一个服务都解析不出来，跳过");
            return;
        };
        eprintln!("最集中的一个：{路径} 被 {条数} 个服务共用");
        assert!(
            按路径.len() < 总数,
            "按路径做 key 却没有任何一条被共用，缓存去重就没有意义了"
        );
    }

    #[test]
    fn 通用齿轮取自服务管理单元的_dll() {
        let path = generic_icon_path().expect("%SystemRoot% 一定展开得出来");
        if !Path::new(&path).exists() {
            eprintln!("本机没有 {path}，跳过");
            return;
        }
        let Some(png) = icon_png(&path) else {
            eprintln!("本机取不到 {path} 的图标，跳过");
            return;
        };
        assert_eq!(
            &png[..8],
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            "通用图标不是合法 PNG"
        );
    }

    /// 前端手里的名字是 `UnitSummary.name`，Windows 上带 `.service` 后缀。
    /// 两种写法必须解析到同一个二进制，否则界面上点得到的行取不到图标。
    #[test]
    fn 带不带_service_后缀都解析得出来() {
        use crate::platform::windows::registry::reg_subkeys;

        for name in reg_subkeys(HKLM, map::SERVICES_KEY) {
            let Some(path) = binary_path(&name) else {
                continue;
            };
            assert_eq!(
                binary_path(&format!("{name}.service")).as_deref(),
                Some(path.as_str()),
                "{name} 带后缀与不带后缀解析结果不一致"
            );
            return;
        }
        eprintln!("本机一个服务都解析不出来，跳过");
    }

    #[test]
    fn 不存在的服务与驱动都取不到路径() {
        assert_eq!(binary_path("绝不会有这个服务_9f3a1c"), None);
        assert_eq!(binary_path(""), None);
        // 驱动在注册表里有键，但 ImagePath 指向 .sys
        for driver in ["ACPI", "disk", "Null"] {
            if reg_string(HKLM, &map::service_subkey(driver), "ImagePath").is_some() {
                assert_eq!(binary_path(driver), None, "{driver} 是驱动，不该给出路径");
                return;
            }
        }
        eprintln!("本机没有找到可用于比对的驱动键，跳过这一半");
    }
}
