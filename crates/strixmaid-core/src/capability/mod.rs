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

use strixmaid_types::auth::may_elevate;
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
    ///
    /// `elevate_groups` 必须来自 `session.elevate_groups` 那份配置，**不能是常量**：
    /// 这里报的 `can_elevate` 决定前端显不显示「启用管理访问」，而真正放不放行由
    /// helper 按同一份配置判断。两边取值不同就会出现「按钮点得下去、点了被拒」，
    /// 那是最难查的一类不一致。
    pub fn capabilities(&self, elevate_groups: &[String]) -> UserCapabilities {
        derive_user_caps(
            self.uid,
            &self.username,
            &self.groups,
            self.elevated,
            elevate_groups,
        )
    }
}

/// 能读全量 journal 的组：journald 的 tmpfiles 规则给 `systemd-journal` / `adm` / `wheel` 加了 ACL。
pub const JOURNAL_GROUPS: &[&str] = &["systemd-journal", "adm", "wheel"];
// 允许提权的**默认**组见 `strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS`；
// 实际生效的是配置项 `session.elevate_groups`，逐处传入而不是在这里写常量。

/// 纯函数：从 uid / 组 / 提权状态推导用户级能力。
///
/// * `can_read_journal`：uid 0、已提权，或在 [`JOURNAL_GROUPS`] 任一组；
/// * `can_manage_units`：uid 0 或已提权（polkit 的细粒度裁决留给真正的操作，这里只给前端一个粗略信号）；
/// * `can_elevate`：与 helper 完全同一个判断——[`may_elevate`]，参数是配置里的
///   `session.elevate_groups`；
/// * `elevated`：原样透传。
pub fn derive_user_caps(
    uid: u32,
    name: &str,
    groups: &[String],
    elevated: bool,
    elevate_groups: &[String],
) -> UserCapabilities {
    let is_root = uid == 0;
    let in_any = |wanted: &[&str]| groups.iter().any(|g| wanted.contains(&g.as_str()));
    UserCapabilities {
        uid,
        name: name.to_owned(),
        groups: groups.to_vec(),
        can_read_journal: is_root || elevated || in_any(JOURNAL_GROUPS),
        can_manage_units: is_root || elevated,
        can_elevate: may_elevate(uid, groups, elevate_groups),
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
    /// id → 字段的映射：
    ///
    /// | provider id | 字段 | 平台 |
    /// |---|---|---|
    /// | `systemd` / `launchd` | `systemd` | Linux / macOS |
    /// | `journald` / `oslog` | `journal` | Linux / macOS |
    /// | `podman` | `podman` | 两者 |
    ///
    /// **字段名沿用 Linux 实现的名字，语义是「这项能力可用」而不是「装了这个软件」**
    /// ——见 [`SystemCapabilities`] 各字段的文档。macOS 上的 launchd 与统一日志
    /// 必须点亮同样的位，否则前端会把明明可用的服务页与日志页隐藏掉。
    /// 与其为一个开发平台在 API 契约里加两个新字段（下游代码生成器全要跟着改），
    /// 不如让「后端是谁」留在 `providers` 列表里，那才是它该待的地方。
    ///
    /// 其它 id（`host` / `proc`）没有对应字段，只进 `providers` 列表。
    pub async fn probe_all(&self) -> ProbeReport {
        let mut system = probe_system(&self.helper_path);
        let mut providers = Vec::with_capacity(self.providers.len());
        for p in &self.providers {
            let probe = p.probe().await;
            let available = probe.is_available();
            match p.id() {
                "systemd" | "launchd" => system.systemd = available,
                "journald" | "oslog" => system.journal = available,
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
///
/// 这只是**启动期的粗判**，`systemd` / `journal` / `podman` 三项随后会被各
/// provider 的真实 `probe()` 覆盖（见 [`CapabilityRegistry::probe_all`]）。
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

/// 服务管理器在运行。
///
/// Linux：systemd 作为 init 在跑，判据是 `/run/systemd/system` 目录存在
/// （`sd_booted()` 用的就是这一条）。
/// macOS：launchd 就是 PID 1，永远在跑，恒为 true。
pub fn has_systemd() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        Path::new("/run/systemd/system").is_dir()
    }
}

/// 日志后端的命令行工具可执行。
///
/// Linux 是 `journalctl`，macOS 是 `/usr/bin/log`。
pub fn has_journalctl() -> bool {
    #[cfg(target_os = "macos")]
    {
        Path::new("/usr/bin/log").is_file()
    }
    #[cfg(not(target_os = "macos"))]
    {
        find_executable(Path::new("journalctl")).is_some()
    }
}

/// polkit 守护进程二进制存在（Debian 系在 `/usr/lib`，RHEL 系在 `/usr/libexec`）。
///
/// macOS 没有 polkit——它的授权走 Authorization Services / TCC，与 polkit
/// 的「按 action id 询问策略」模型对不上，因此恒为 false。这不影响提权：
/// 提权的权威判定在 helper 内部（`design.md` §5），polkit 只是让 systemd
/// 操作能有更细的裁决。
pub fn has_polkit() -> bool {
    #[cfg(target_os = "macos")]
    {
        false
    }
    #[cfg(not(target_os = "macos"))]
    {
        ["/usr/lib/polkit-1/polkitd", "/usr/libexec/polkit-1/polkitd"]
            .iter()
            .any(|p| Path::new(p).is_file())
    }
}

/// 支持用户级服务单元。
///
/// Linux：`/run/user` 存在（pam_systemd 会为登录用户在此建运行时目录）。
/// macOS：launchd 的 `gui/<uid>` 与 `user/<uid>` 域是内建的，恒为 true。
pub fn has_user_units() -> bool {
    #[cfg(target_os = "macos")]
    {
        true
    }
    #[cfg(not(target_os = "macos"))]
    {
        Path::new("/run/user").is_dir()
    }
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

    /// 默认允许提权的组（`session.elevate_groups` 的缺省值）。
    fn default_allow() -> Vec<String> {
        groups(strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS)
    }

    /// **`can_elevate` 必须与 helper 的放行判断完全一致。**
    ///
    /// 前端拿 `can_elevate` 决定显不显示「启用管理访问」，helper 拿
    /// `may_elevate` 决定放不放行。两者一旦分叉，用户就会看到一个点了必被拒的
    /// 按钮——所以这里穷举各种组合，逐一比对两个判断的结果。
    #[test]
    fn can_elevate_与_helper_的判断完全一致() {
        let cases: &[(u32, &[&str], &[&str])] = &[
            (1000, &["alice"], &["sudo", "wheel", "admin"]),
            (1000, &["alice", "sudo"], &["sudo", "wheel", "admin"]),
            (1000, &["alice", "wheel"], &["sudo"]),
            (1000, &["alice", "sudo"], &[]),
            (0, &[], &["sudo"]),
            (0, &["root"], &[]),
            (501, &["staff", "admin"], &["admin"]),
            (1000, &["sudoers"], &["sudo"]),
        ];
        for (uid, user_groups, allow) in cases {
            let ug = groups(user_groups);
            let al = groups(allow);
            let caps = derive_user_caps(*uid, "u", &ug, false, &al);
            assert_eq!(
                caps.can_elevate,
                strixmaid_types::auth::may_elevate(*uid, &ug, &al),
                "uid={uid} groups={user_groups:?} allow={allow:?}：\
                 前端看到的 can_elevate 与 helper 的放行判断不一致"
            );
        }
    }

    /// `can_elevate` 随配置变，不再是写死的常量。
    #[test]
    fn can_elevate_跟随配置而非常量() {
        let g_wheel = groups(&["carol", "wheel"]);
        assert!(derive_user_caps(1000, "carol", &g_wheel, false, &groups(&["wheel"])).can_elevate);
        assert!(
            !derive_user_caps(1000, "carol", &g_wheel, false, &groups(&["sudo"])).can_elevate,
            "允许列表里没有 wheel 时就不该报可提权"
        );
        assert!(
            !derive_user_caps(1000, "carol", &g_wheel, false, &[]).can_elevate,
            "空列表 = 禁止提权"
        );
    }

    #[test]
    fn 普通用户() {
        let c = derive_user_caps(1000, "alice", &groups(&["alice", "users"]), false, &default_allow());
        assert_eq!(c.uid, 1000);
        assert_eq!(c.name, "alice");
        assert!(!c.can_read_journal);
        assert!(!c.can_manage_units);
        assert!(!c.can_elevate);
        assert!(!c.elevated);
    }

    #[test]
    fn sudo_组可提权_adm_组可读日志() {
        let c = derive_user_caps(1000, "alice", &groups(&["alice", "adm", "sudo"]), false, &default_allow());
        assert!(c.can_read_journal);
        assert!(!c.can_manage_units);
        assert!(c.can_elevate);
        let c = derive_user_caps(1000, "bob", &groups(&["bob", "systemd-journal"]), false, &default_allow());
        assert!(c.can_read_journal);
        assert!(!c.can_elevate);
        let c = derive_user_caps(1000, "carol", &groups(&["carol", "wheel"]), false, &default_allow());
        assert!(c.can_read_journal && c.can_elevate);
    }

    #[test]
    fn 提权后可管理_unit() {
        let c = derive_user_caps(1000, "alice", &groups(&["alice", "sudo"]), true, &default_allow());
        assert!(c.elevated);
        assert!(c.can_manage_units);
        assert!(c.can_read_journal);
    }

    #[test]
    fn root_全开() {
        let c = derive_user_caps(0, "root", &groups(&["root"]), false, &default_allow());
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
        assert_eq!(id.capabilities(&default_allow()), derive_user_caps(1000, "alice", &id.groups, false, &default_allow()));
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
        // 只要求与本平台的判据一致，不假设机器上装了什么
        assert_eq!(caps.systemd, has_systemd());
        assert_eq!(caps.journal, has_journalctl());
        assert_eq!(caps.polkit, has_polkit());
        eprintln!("本机 SystemCapabilities = {caps:?}");
    }

    /// 各平台的判据本身。分开写是为了让「判据变了」与「组装错了」两类错误
    /// 指向不同的用例。
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_的判据看文件系统() {
        assert_eq!(has_systemd(), Path::new("/run/systemd/system").is_dir());
        assert_eq!(has_user_units(), Path::new("/run/user").is_dir());
    }

    /// macOS 上 launchd 是 PID 1、统一日志是系统组件，三项都是恒真/恒假。
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_的判据是平台事实() {
        assert!(has_systemd(), "launchd 就是 PID 1");
        assert!(has_journalctl(), "/usr/bin/log 是系统自带的");
        assert!(has_user_units(), "launchd 的 gui/<uid> 域是内建的");
        assert!(!has_polkit(), "macOS 没有 polkit");
        // user_units 不该被 systemd 那一项拖成 false
        let caps = probe_system(Path::new("strixmaid-helper"));
        assert!(caps.user_units);
    }
}
