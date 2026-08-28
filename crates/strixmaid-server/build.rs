//! 构建期注入 git sha 与目标三元组（roadmap/06 §3.5）：
//! `strixmaid --version` → `strixmaid 0.1.0 (<sha>, <target>)`。
//! 无 git（从 tar 构建）时 sha 为 `unknown`——如实，不编造。

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
}
