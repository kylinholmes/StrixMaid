//! 链接运行时的 PAM 库，**不依赖任何 dev 包**（design.md §10）。
//!
//! PAM 应用侧 API 只有十来个函数、二十年未变，我们自行声明 `extern "C"`
//! （见 `src/auth/unix.rs`），因此不需要头文件；这里只解决「怎么找到 .so / .dylib」。
//!
//! # Linux
//!
//! 普通的 `-lpam` 需要 dev 包提供的 `libpam.so` 符号链接；`-l:libpam.so.0` 则直接按
//! 文件名找 SONAME 文件，任何装了 PAM 的机器都有它。
//!
//! # macOS
//!
//! macOS 自带 OpenPAM，SDK 里有 `libpam.tbd`，`-lpam` 直接可用，不需要装任何东西。
//! （`-l:` 是 GNU ld 的语法，Apple 的链接器不认。）
//!
//! macOS 是开发平台而非交付目标——`design.md` §2.1 的三个产物都是 Linux 二进制。
//! 这里能链上，只是为了让认证链路能在本机跑通并联调。注意 OpenPAM 与 Linux-PAM
//! 的**常量数值不同**，那部分的处理见 `src/auth/unix.rs` 的 `consts` 模块。
//!
//! # 交叉工具链的出口：`STRIXMAID_PAM_LINK_ARG`
//!
//! `-l:` 是 GNU ld 的语法，各家链接器支持不一。CI 用 `cargo-zigbuild` 把 helper
//! 压到 glibc 2.28 基线时，zig 那套 clang/lld 包装就还原不出它，报
//! `ld.lld: unable to find library -l:libpam.so.0`——即使 `-L` 指到了确有
//! `libpam.so.0` 的目录。
//!
//! 设了这个环境变量就用它的值代替上面的默认链接参数，通常是一个 `.so` 的绝对路径：
//!
//! ```sh
//! STRIXMAID_PAM_LINK_ARG=/tmp/pamlib/libpam.so.0 cargo zigbuild --target x86_64-unknown-linux-gnu.2.28 ...
//! ```
//!
//! 按绝对路径链不会把该路径写进二进制：`DT_NEEDED` 记的是那个 `.so` 的 `SONAME`
//! （即 `libpam.so.0`），运行期解析到的仍是目标机自己的 PAM。CI 里紧跟着的
//! `ldd` 断言会把这一点验掉。
//!
//! # Windows
//!
//! 那里没有 PAM（认证走 `LogonUserW`），本文件改做另外两件事：嵌入版本资源，
//! 以及嵌入一份**明确写着 `asInvoker`** 的应用程序清单。
//!
//! ## 清单为什么必须是 `asInvoker`
//!
//! helper 不是用户从桌面双击起来的程序，而是由主进程（服务身份，通常是
//! LocalSystem）`CreateProcessAsUserW` 拉起的子进程。它需要的权限完全来自
//! 父进程的令牌，与 UAC 无关。
//!
//! 清单里若写 `requireAdministrator`，就等于声明「本程序必须经由提权才能运行」：
//! 从一个**未提权**的上下文创建它时 `CreateProcess` 直接失败（`ERROR_ELEVATION_REQUIRED`，
//! 740），而不会自动弹 UAC——弹窗是 `ShellExecute` 才有的行为。开发期从普通
//! 终端跑起主进程联调，或将来把主进程换成非 LocalSystem 的服务账户，都会撞上
//! 这一条，且症状是「worker 起不来」，离真正的原因很远。
//!
//! 反过来说，不给清单也不行：rustc 在 MSVC 目标上会嵌一份默认清单（`longPathAware`
//! 与 `activeCodePage=UTF-8`），而**自带清单会整个取代它**，不是合并。所以
//! `strixmaid-helper.exe.manifest` 里把那两项原样带上，再加 `asInvoker` 与
//! `supportedOS`，缺一不可。
//!
//! ## 为什么值得引入 `embed-resource`
//!
//! 资源编译要找到 `rc.exe`（Windows SDK 里，路径随 SDK 版本变）或交叉环境下的
//! `windres`。这件事本身就是 `embed-resource` 存在的理由，自己写等于把它抄一遍。
//! 它只是 `[build-dependencies]`，不进最终二进制。
//!
//! ## 版本资源里为什么全是 ASCII
//!
//! `rc.exe` 默认按系统 ANSI 代码页读 `.rc` 文件，中文在非中文机器上会变成乱码，
//! 而资源管理器的「属性」页正是拿它显示。中文说明留在文档里，资源里只放
//! 英文与版本号。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=STRIXMAID_PAM_LINK_ARG");

    // Windows 上没有 PAM：认证走 `LogonUserW`（advapi32，由 windows-sys 链接），
    // 链接参数无事可做。提前返回而不是往下走，免得给 MSVC 链接器
    // 递一个它看不懂的 `-l:libpam.so.0`。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resource();
        return;
    }

    // 构建环境显式指定时优先，供交叉工具链绕开 `-l:`（见上）。
    if let Ok(arg) = std::env::var("STRIXMAID_PAM_LINK_ARG") {
        let arg = arg.trim();
        if !arg.is_empty() {
            println!("cargo:rustc-link-arg={arg}");
            return;
        }
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" => println!("cargo:rustc-link-lib=pam"),
        // Linux 与其余 ELF 平台
        _ => println!("cargo:rustc-link-arg=-l:libpam.so.0"),
    }
}

/// 嵌入 Windows 的版本资源与应用程序清单。
///
/// 只在**宿主**是 Windows 时有实体：`[target.'cfg(windows)'.build-dependencies]`
/// 的 cfg 按宿主而非目标匹配（实测：Windows 宿主上
/// `--target x86_64-unknown-linux-musl` 仍会编译该依赖）。因此「从 Linux 交叉
/// 编译出 Windows 产物」这条路拿不到资源——本项目不走它，Windows 产物在
/// windows runner 上原生构建（`.github/workflows/ci.yml` 的 `windows` job）。
#[cfg(windows)]
fn windows_resource() {
    println!("cargo:rerun-if-changed=strixmaid-helper.exe.manifest");

    let rc = write_version_rc(
        "strixmaid-helper",
        "StrixMaid privileged helper (authentication, worker spawn)",
        "strixmaid-helper.exe.manifest",
    );

    // `NotAttempted`（机器上没有 Windows SDK，找不到 rc.exe）放行：资源是
    // 锦上添花，不该让一台缺 SDK 的开发机连编译都过不去。真的编译失败
    // （`Failed`）才中止——那说明 .rc 或清单本身写错了，必须立刻知道。
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_optional()
        .expect("嵌入 Windows 资源失败");
}

#[cfg(not(windows))]
fn windows_resource() {}

/// 生成一份 `VERSIONINFO` + `RT_MANIFEST` 的 `.rc`，返回其路径。
///
/// 写进 `OUT_DIR` 而不是签入仓库：版本号取自 `CARGO_PKG_VERSION_*`，
/// 签入的话每次改版本都要改两个地方，迟早对不上。清单则相反——它是需要
/// 人读、需要写注释的东西，签在 crate 根目录下。
///
/// 全部取值都是 ASCII。`rc.exe` 默认按系统 ANSI 代码页读文件，中文在
/// 非中文 Windows 上会变成乱码，而这些字符串正是「属性」页显示的内容。
///
/// 与 `crates/strixmaid-server/build.rs` 里的同名函数是同一份逻辑。构建脚本
/// 之间无法共享代码（除非为此单开一个 crate），两份各二十行的重复比多一个
/// 只被 build.rs 用到的 crate 更划算。
#[cfg(windows)]
fn write_version_rc(internal_name: &str, description: &str, manifest: &str) -> std::path::PathBuf {
    let major = std::env::var("CARGO_PKG_VERSION_MAJOR").unwrap_or_else(|_| "0".into());
    let minor = std::env::var("CARGO_PKG_VERSION_MINOR").unwrap_or_else(|_| "0".into());
    let patch = std::env::var("CARGO_PKG_VERSION_PATCH").unwrap_or_else(|_| "0".into());
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");

    // `.rc` 里的路径要么用正斜杠，要么把反斜杠写成两个——rc.exe 把字符串里的
    // 反斜杠当转义符。正斜杠更省事，Windows 的文件 API 一样认。
    let manifest_path = format!("{manifest_dir}/{manifest}").replace('\\', "/");

    // `1` 是 EXE 的清单资源号（CREATEPROCESS_MANIFEST_RESOURCE_ID），`24` 是
    // RT_MANIFEST。直接写数字，省掉一行 `#define`。
    let rc = format!(
        r#"1 24 "{manifest_path}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName",      "kylinholmes\0"
            VALUE "FileDescription",  "{description}\0"
            VALUE "FileVersion",      "{version}\0"
            VALUE "InternalName",     "{internal_name}\0"
            VALUE "LegalCopyright",   "Copyright (C) kylinholmes. GNU GPL v3 only.\0"
            VALUE "OriginalFilename", "{internal_name}.exe\0"
            VALUE "ProductName",      "StrixMaid\0"
            VALUE "ProductVersion",   "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    );

    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"))
        .join("strixmaid-version.rc");
    std::fs::write(&out, rc).expect("写入 .rc 失败");
    out
}
