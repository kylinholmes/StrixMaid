//! 链接运行时的 PAM 库，**不依赖任何 dev 包**（design.md §10）。
//!
//! PAM 应用侧 API 只有十来个函数、二十年未变，我们自行声明 `extern "C"`
//! （见 `src/pam.rs`），因此不需要头文件；这里只解决「怎么找到 .so / .dylib」。
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
//! 的**常量数值不同**，那部分的处理见 `src/pam.rs` 的 `consts` 模块。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "macos" => println!("cargo:rustc-link-lib=pam"),
        // Linux 与其余 ELF 平台
        _ => println!("cargo:rustc-link-arg=-l:libpam.so.0"),
    }
}
