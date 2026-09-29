//! 用真实可执行文件验证配置检查不会进入服务运行阶段。
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "strixmaid-check-config-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn check(mode: &str, path: &Path, data: &Path, extra: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_strixmaid"));
    // 不让开发者的真实配置环境影响夹具，也不读取真实 token。
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("STRIXMAID_") {
            command.env_remove(key);
        }
    }
    let mut process = Process(
        command
            .arg(mode)
            .arg("--check-config")
            .arg("--config")
            .arg(path)
            .arg("--data-dir")
            .arg(data)
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            use std::io::Read;
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            process
                .0
                .stdout
                .take()
                .unwrap()
                .read_to_end(&mut stdout)
                .unwrap();
            process
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_end(&mut stderr)
                .unwrap();
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        assert!(
            Instant::now() < deadline,
            "{mode} --check-config 没有及时退出"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn 两种模式检查有效配置后立即退出且不创建数据目录() {
    let dir = TempDir::new();
    for (mode, contents) in [
        ("serve", ""),
        (
            "agent",
            "server_url = 'ws://127.0.0.1:9'\ntoken = 'test-only'\nnode_id = 'check-only'\n",
        ),
    ] {
        let config = dir.0.join(format!("{mode}.toml"));
        std::fs::write(&config, contents).unwrap();
        let data = dir.0.join(format!("{mode}-data"));
        let result = check(mode, &config, &data, &[]);
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout).contains("配置有效"));
        assert!(!data.exists(), "检查配置不能打开数据库或创建数据目录");
    }
}

#[test]
fn 两种模式拒绝把目录作为配置文件() {
    let dir = TempDir::new();
    for mode in ["serve", "agent"] {
        let data = dir.0.join(format!("{mode}-data"));
        let result = check(mode, &dir.0, &data, &[]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("普通文件"));
        assert!(!data.exists());
    }
}

#[test]
fn agent检查模式同样拒绝仅适用于server的参数() {
    let dir = TempDir::new();
    let config = dir.0.join("agent.toml");
    std::fs::write(
        &config,
        "server_url = 'ws://127.0.0.1:9'\ntoken = 'test-only'\n",
    )
    .unwrap();
    let data = dir.0.join("data");
    let result = check("agent", &config, &data, &["--listen", "127.0.0.1:9700"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--listen"));
    assert!(!data.exists());
}
