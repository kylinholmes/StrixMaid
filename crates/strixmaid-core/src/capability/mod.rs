//! 两层能力探测（`docs/design.md` §6）。
//!
//! - **system 层**：进程启动时探测一次——「这台机器上有没有这个东西」。
//!   六项探测（systemd / journal / helper / polkit / user_units / podman）全部直接看文件系统，
//!   见 [`probe_system`]；已注册的 provider 的 [`Provider::probe`] 结果会**覆盖**同名项
//!   （service provider 真的连过 bus，比「`/run/systemd/system` 存在」更可信）。
//! - **user 层**：会话建立时由认证模块给出 [`UserIdentity`]，[`derive_user_caps`] 是纯函数，
//!   从 uid 与组推导「当前用户能不能用」。
//!
//! 未认证时 `GET /capabilities` 的 `user` 为 `None`——登录页必须先拿到 system 层
//! 才能显示「helper 不可用，无法登录」。

use std::path::{Path, PathBuf};

use strixmaid_types::capability::{SystemCapabilities, UserCapabilities};

use crate::config::Config;
use crate::providers::{Probe, Provider};

// ================================ user 层 ================================

/// 已认证会话的身份，由认证中间件放进请求的 `Extension`。
///
/// 字段与 helper 的 `AuthOk { uid, gid, username, groups }` 对应，再加上会话的提权状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserIdentity {
    pub uid: u32,
    pub username: String,
    /// 所属组名列表（含主组）。
    pub groups: Vec<String>,
    /// 当前会话是否已提权（admin worker 已就绪）。
    pub elevated: bool,
}

impl UserIdentity {
    /// 推导用户级能力。
    pub fn capabilities(&self) -> UserCapabilities {
        derive_user_caps(self.uid, &self.username, &self.groups, self.elevated)
    }
}

/// 能读全量 journal 的组：journald 的 tmpfiles 规则给 `systemd-journal` / `adm` / `wheel` 加了 ACL。
pub const JOURNAL_GROUPS: &[&str] = &["systemd-journal", "adm", "wheel"];
/// 能走 sudo 提权的组：Debian 系 `sudo`、RHEL / Arch 系 `wheel`、老 Ubuntu 的 `admin`。
pub const ELEVATE_GROUPS: &[&str] = &["sudo", "wheel", "admin"];

/// 纯函数：从 uid / 组 / 提权状态推导用户级能力。
///
/// * `can_read_journal`：uid 0、已提权，或在 [`JOURNAL_GROUPS`] 任一组；
/// * `can_manage_units`：uid 0 或已提权（polkit 的细粒度裁决留给真正的操作，这里只给前端一个粗略信号）；
/// * `can_elevate`：uid 0（本来就是 root）或在 [`ELEVATE_GROUPS`] 任一组；
/// * `elevated`：原样透传。
pub fn derive_user_caps(uid: u32, name: &str, groups: &[String], elevated: bool) -> UserCapabilities {
    let is_root = uid == 0;
    let in_any = |wanted: &[&str]| groups.iter().any(|g| wanted.contains(&g.as_str()));
    UserCapabilities {
        uid,
        name: name.to_owned(),
        groups: groups.to_vec(),
        can_read_journal: is_root || elevated || in_any(JOURNAL_GROUPS),
        can_manage_units: is_root || elevated,
        can_elevate: is_root || in_any(ELEVATE_GROUPS),
        elevated,
    }
}

// =============================== system 层 ===============================

/// 单个 provider 的探测结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProbe {
    pub id: &'static str,
    pub probe: Probe,
}

/// [`CapabilityRegistry::probe_all`] 的产出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// 汇总后的 system 层能力。
    pub system: SystemCapabilities,
    /// 每个已注册 provider 的原始探测结果，按注册顺序；供启动日志与 `Degraded` 提示使用。
    pub providers: Vec<ProviderProbe>,
}

/// provider 注册表：收集所有 provider，启动时统一 `probe()` 汇总成 [`SystemCapabilities`]。
///
/// 用法：
///
/// ```no_run
/// # use strixmaid_core::capability::CapabilityRegistry;
/// # use strixmaid_core::config::Config;
/// # use strixmaid_core::providers::{process::ProcProvider, system::HostProvider};
/// # async fn demo(config: &Config) {
/// let mut registry = CapabilityRegistry::from_config(config);
/// registry
///     .register(Box::new(HostProvider::new()))
///     .register(Box::new(ProcProvider::new()));
/// let report = registry.probe_all().await;
/// let caps = report.system; // 放进 capabilities 路由的 state
/// # }
/// ```
pub struct CapabilityRegistry {
    providers: Vec<Box<dyn Provider>>,
    helper_path: PathBuf,
}

impl CapabilityRegistry {
    /// `helper_path` 即配置里的 `helper_path`：不含 `/` 时按 PATH 查找。
    pub fn new(helper_path: impl Into<PathBuf>) -> Self {
        Self {
            providers: Vec::new(),
            helper_path: helper_path.into(),
        }
    }

    /// 从配置构造。
    pub fn from_config(config: &Config) -> Self {
        Self::new(config.helper_path.clone())
    }

    /// 注册一个 provider。同一 id 重复注册时后者覆盖前者的探测结果。
    pub fn register(&mut self, provider: Box<dyn Provider>) -> &mut Self {
        self.providers.push(provider);
        self
    }

    /// 已注册的 provider。
    pub fn providers(&self) -> impl Iterator<Item = &dyn Provider> {
        self.providers.iter().map(Box::as_ref)
    }

    /// 按 id 查找 provider。
    pub fn get(&self, id: &str) -> Option<&dyn Provider> {
        self.providers().find(|p| p.id() == id)
    }

    /// 探测全部：先做六项文件系统探测，再让各 provider 的结果覆盖同名项。
    ///
    /// id → 字段的映射：`systemd` → `systemd`，`journald` → `journal`，`podman` → `podman`。
    /// 其它 id（`host` / `proc`）没有对应字段，只进 `providers` 列表。
    pub async fn probe_all(&self) -> ProbeReport {
        let mut system = probe_system(&self.helper_path);
        let mut providers = Vec::with_capacity(self.providers.len());
        for p in &self.providers {
            let probe = p.probe().await;
            let available = probe.is_available();
            match p.id() {
                "systemd" => system.systemd = available,
                "journald" => system.journal = available,
                "podman" => system.podman = available,
                _ => {}
            }
            if let Probe::Unavailable { reason } | Probe::Degraded { reason } = &probe {
                tracing::info!(provider = p.id(), probe = ?probe, %reason, "provider 探测结果");
            } else {
                tracing::debug!(provider = p.id(), "provider 可用");
            }
            providers.push(ProviderProbe {
                id: p.id(),
                probe,
            });
        }
        // user_units 以 systemd 为前提
        system.user_units = system.user_units && system.systemd;
        ProbeReport { system, providers }
    }
}

/// 六项 system 层探测，全部只看文件系统，不启动任何子进程。
pub fn probe_system(helper_path: &Path) -> SystemCapabilities {
    let systemd = has_systemd();
    SystemCapabilities {
        systemd,
        journal: has_journalctl(),
        helper: find_executable(helper_path).is_some(),
        polkit: has_polkit(),
        user_units: systemd && has_user_units(),
        podman: has_podman(),
    }
}

/// systemd 作为 init 在运行：`/run/systemd/system` 目录存在（这是 `sd_booted()` 的判据）。
pub fn has_systemd() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

/// `journalctl` 在 PATH 里且可执行。
pub fn has_journalctl() -> bool {
    find_executable(Path::new("journalctl")).is_some()
}

/// polkit 守护进程二进制存在（Debian 系在 `/usr/lib`，RHEL 系在 `/usr/libexec`）。
pub fn has_polkit() -> bool {
    ["/usr/lib/polkit-1/polkitd", "/usr/libexec/polkit-1/polkitd"]
        .iter()
        .any(|p| Path::new(p).is_file())
}

/// 支持用户级 unit：`/run/user` 存在（pam_systemd 会为登录用户在此建运行时目录）。
pub fn has_user_units() -> bool {
    Path::new("/run/user").is_dir()
}

/// `podman` 在 PATH 里。
pub fn has_podman() -> bool {
    find_executable(Path::new("podman")).is_some()
}

/// 守护进程的 PATH 往往极简（systemd 默认只有 `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin`），
/// 环境变量之外再补这份标准目录，避免「装了但没找到」。
const FALLBACK_PATH: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// 找可执行文件：含 `/` 的路径直接检查；否则依次在 `PATH` 与 [`FALLBACK_PATH`] 里找。
pub fn find_executable(name_or_path: &Path) -> Option<PathBuf> {
    if name_or_path.as_os_str().is_empty() {
        return None;
    }
    if name_or_path.components().count() > 1 || name_or_path.is_absolute() {
        return is_executable_file(name_or_path).then(|| name_or_path.to_path_buf());
    }
    let env_path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&env_path)
        .chain(FALLBACK_PATH.iter().map(PathBuf::from))
        .map(|dir| dir.join(name_or_path))
        .find(|candidate| is_executable_file(candidate))
}

/// 是常规文件且任一执行位置位。
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    fn groups(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn 普通用户() {
        let c = derive_user_caps(1000, "alice", &groups(&["alice", "users"]), false);
        assert_eq!(c.uid, 1000);
        assert_eq!(c.name, "alice");
        assert!(!c.can_read_journal);
        assert!(!c.can_manage_units);
        assert!(!c.can_elevate);
        assert!(!c.elevated);
    }

    #[test]
    fn sudo_组可提权_adm_组可读日志() {
        let c = derive_user_caps(1000, "alice", &groups(&["alice", "adm", "sudo"]), false);
        assert!(c.can_read_journal);
        assert!(!c.can_manage_units);
        assert!(c.can_elevate);
        let c = derive_user_caps(1000, "bob", &groups(&["bob", "systemd-journal"]), false);
        assert!(c.can_read_journal);
        assert!(!c.can_elevate);
        let c = derive_user_caps(1000, "carol", &groups(&["carol", "wheel"]), false);
        assert!(c.can_read_journal && c.can_elevate);
    }

    #[test]
    fn 提权后可管理_unit() {
        let c = derive_user_caps(1000, "alice", &groups(&["alice", "sudo"]), true);
        assert!(c.elevated);
        assert!(c.can_manage_units);
        assert!(c.can_read_journal);
    }

    #[test]
    fn root_全开() {
        let c = derive_user_caps(0, "root", &groups(&["root"]), false);
        assert!(c.can_read_journal && c.can_manage_units && c.can_elevate);
        assert!(!c.elevated);
    }

    #[test]
    fn identity_便捷方法() {
        let id = UserIdentity {
            uid: 1000,
            username: "alice".into(),
            groups: groups(&["alice", "sudo"]),
            elevated: false,
        };
        assert_eq!(id.capabilities(), derive_user_caps(1000, "alice", &id.groups, false));
    }

    #[test]
    fn 查找可执行文件() {
        assert!(find_executable(Path::new("sh")).is_some());
        assert!(find_executable(Path::new("/bin/sh")).is_some());
        assert!(find_executable(Path::new("")).is_none());
        assert!(find_executable(Path::new("definitely-not-a-real-binary-xyz")).is_none());
        assert!(find_executable(Path::new("/etc/passwd")).is_none(), "不可执行");
        assert!(find_executable(Path::new("/etc")).is_none(), "目录不算");
    }

    struct Fake(&'static str, Probe);

    #[async_trait]
    impl Provider for Fake {
        fn id(&self) -> &'static str {
            self.0
        }
        async fn probe(&self) -> Probe {
            self.1.clone()
        }
    }

    #[tokio::test]
    async fn provider_结果覆盖文件系统探测() {
        let mut reg = CapabilityRegistry::new("strixmaid-helper-not-installed");
        reg.register(Box::new(Fake("systemd", Probe::unavailable("bus 连不上且没有 systemctl"))))
            .register(Box::new(Fake("journald", Probe::degraded("journalctl 版本过旧"))))
            .register(Box::new(Fake("host", Probe::Available)));
        let report = reg.probe_all().await;
        assert!(!report.system.systemd, "provider 说不可用就是不可用");
        assert!(report.system.journal, "Degraded 仍算可用");
        assert!(!report.system.user_units, "没有 systemd 就没有 user units");
        assert!(!report.system.helper);
        assert_eq!(report.providers.len(), 3);
        assert_eq!(report.providers[2].id, "host");
        assert!(reg.get("host").is_some());
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn 本机_system_层探测() {
        let caps = probe_system(Path::new("strixmaid-helper"));
        // 只要求与文件系统一致，不假设机器上装了什么
        assert_eq!(caps.systemd, Path::new("/run/systemd/system").is_dir());
        assert_eq!(caps.polkit, has_polkit());
        eprintln!("本机 SystemCapabilities = {caps:?}");
    }
}
