//! 构建期注入 git sha 与目标三元组（roadmap/06 §3.5）：
//! `strixmaid --version` → `strixmaid 0.1.0 (<sha>, <target>)`。
//! 无 git（从 tar 构建）时 sha 为 `unknown`——如实，不编造。
//!
//! # Windows 资源
//!
//! 另外在 Windows 目标上嵌入两样东西：
//!
//! * **版本资源（`VERSIONINFO`）**。没有它，资源管理器的「属性 → 详细信息」页
//!   整页是空的，任务管理器的「描述」列只显示文件名。运维要确认机器上跑的是
//!   哪一版时，第一反应是去看文件属性而不是 `strixmaid --version`。
//! * **应用程序清单**，明确写 `asInvoker`。主进程由 SCM 以服务身份拉起，权限
//!   来自服务账户而非 UAC；写 `requireAdministrator` 只会让开发期从普通终端
//!   `strixmaid serve` 直接失败（`ERROR_ELEVATION_REQUIRED`，`CreateProcess`
//!   不会自动弹 UAC）。helper 那侧的同一问题更严重，详见
//!   `crates/strixmaid-helper/build.rs`。
//!
//! 需要管理员权限的实际只有 `strixmaid service install` / `uninstall`，
//! 那是**一条命令**而不是整个程序的运行条件；`install.ps1` 在开头就检查了
//! 提权状态并给出明确提示，不需要靠清单来兜。
//!
//! 自带清单会**整个取代** rustc 在 MSVC 目标上默认嵌入的那一份（`longPathAware`
//! 与 `activeCodePage=UTF-8`），不是合并，所以 `strixmaid.exe.manifest` 里把
//! 那两项原样带上。
//!
//! 资源编译靠 `embed-resource`：找到 `rc.exe`（在 Windows SDK 里，路径随 SDK
//! 版本变）本身就是它存在的理由。它只是 `[build-dependencies]`，不进二进制。

fn main() {
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=STRIXMAID_GIT_SHA={sha}");

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_string());
    println!("cargo:rustc-env=STRIXMAID_BUILD_TARGET={target}");

    // sha 随 HEAD 变，但 build.rs 默认只在自身或依赖变化时重跑；
    // 用 .git/HEAD 做重跑依据，切分支 / 提交后版本串才会更新。
    println!("cargo:rerun-if-changed=../../.git/HEAD");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        windows_resource();
    }

    check_ui_dist();
}

/// release + `ui` feature 下确认前端产物存在。
///
/// # 为什么这里只检查、不去跑 `bun run build`
///
/// 让 build.rs 调包管理器会把 `cargo build` 变成非 hermetic 的:要联网
/// (`bun install`)、要 JS 工具链、每次 `cargo check` 都可能触发。
/// 交叉编译、离线构建、`cargo install` 都会因此破掉。
/// 构建前端是**打包脚本**的职责(`scripts/package.sh`、
/// `scripts/package-windows.ps1`),那里本来就要装 bun。
///
/// 所以这里只做一件事:在缺产物时给出一句说得清的话,而不是让
/// `rust-embed` 抛一个指向 `../../web/dist` 的、看不出该干什么的错误。
///
/// # debug 与 release 要的不是同一件事
///
/// 两档的要求不同，这一点起初弄错过，代价是 CI 的 `check` 与 `windows` 两个
/// job 一起红：
///
/// | 构建 | `rust-embed` 的行为 | 硬性要求 |
/// |---|---|---|
/// | debug | 运行期从磁盘读（前端热更新不受影响） | **目录必须存在**，内容可以为空 |
/// | release | 编译期把文件嵌进二进制 | 目录存在**且**有 `index.html` |
///
/// 「debug 不需要产物」是错的：`#[derive(RustEmbed)]` 的 `folder` 在**两档**
/// 都会在编译期求值，目录不存在就直接报 `folder '…/web/dist' does not exist`。
/// `web/dist` 从 git 里摘掉之后，任何不构建前端的 job 都会撞上这条。
///
/// 用 `PROFILE` 判断而不是 `debug_assertions`：build.rs 自身是用宿主 profile
/// 编译的，读不到目标 crate 的 cfg。两者在默认配置下一致；若有人把 release
/// 的 `debug-assertions` 打开，这里会多要求一个 `index.html`，属于提示过度
/// 而非漏报，比反过来安全。
fn check_ui_dist() {
    // 产物路径变化时重跑，避免「补上产物后仍然报错」。
    println!("cargo:rerun-if-changed=../../web/dist/index.html");

    if std::env::var("CARGO_FEATURE_UI").is_err() {
        return;
    }

    let dir = std::path::Path::new("../../web/dist");
    let release = std::env::var("PROFILE").as_deref() == Ok("release");
    // debug 只要目录在就够；release 还要真有东西可嵌。
    let ok = dir.is_dir() && (!release || dir.join("index.html").is_file());
    if ok {
        return;
    }

    // 直接 panic 而不是 `cargo:warning`：缺了产物，`rust-embed` 随后一定会失败，
    // 只是那条错误指向宏内部、看不出该做什么。既然结果都是构建失败，
    // 不如在这里以一句可操作的话结束。
    panic!(
        "缺少前端产物 web/dist{}。\n\
         web/dist 不在 git 里（它是 web/src 的派生物），需要先构建前端：\n\
         \n    cd web && bun install --frozen-lockfile && bun run build\n\
         \n或直接用打包脚本（scripts/package.sh、scripts/package-windows.ps1），\
         它们会自动构建前端。\n\
         若这个二进制本来就不需要内置 UI，用 --no-default-features 关掉 ui feature。",
        if release { "/index.html" } else { " 目录" },
    );
}

/// 嵌入版本资源与应用程序清单。
///
/// 只在**宿主**是 Windows 时有实体：`[target.'cfg(windows)'.build-dependencies]`
/// 的 cfg 按宿主而非目标匹配（实测：Windows 宿主上
/// `--target x86_64-unknown-linux-musl` 仍会编译该依赖）。因此「从 Linux 交叉
/// 编译出 Windows 产物」这条路拿不到资源——本项目不走它，Windows 产物在
/// windows runner 上原生构建（`.github/workflows/ci.yml` 的 `windows` job）。
#[cfg(windows)]
fn windows_resource() {
    println!("cargo:rerun-if-changed=strixmaid.exe.manifest");

    let rc = write_version_rc(
        "strixmaid",
        "StrixMaid server (UI, AgentCore, Server, worker)",
        "strixmaid.exe.manifest",
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
/// git sha 也不放进来：它随每次提交变，会让资源段每次都重编；要看 sha 用
/// `strixmaid --version`。
///
/// 与 `crates/strixmaid-helper/build.rs` 里的同名函数是同一份逻辑。构建脚本
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
