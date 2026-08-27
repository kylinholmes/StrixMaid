//! 链接运行时的 `libpam.so.0`，**不依赖 `libpam0g-dev`**（design.md §10）。
//!
//! 普通的 `-lpam` 需要 dev 包提供的 `libpam.so` 符号链接；`-l:libpam.so.0` 则直接按
//! 文件名找 SONAME 文件，任何装了 PAM 的机器都有它。PAM 应用侧 API 二十年未变，
//! 我们自行声明 `extern "C"`（见 `src/pam.rs`），不需要头文件。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-arg=-l:libpam.so.0");
}
