//! Agent 配置（roadmap/05 §3.1）。独立于 Server 的 `Config`。
//!
//! 加载顺序：内置默认 → TOML → 环境变量 `STRIXMAID_AGENT_*`（嵌套用 `__`）。
//! TOML 路径来自 `--config`；显式给出的文件必须存在，缺省路径
//! （[`DEFAULT_CONFIG_PATH`]）不存在则静默跳过——与 Server 的行为一致。
//!
//! # 两个平台的路径与节点标识
//!
//! 路径默认值按平台分叉，取向与 `strixmaid_core::config` 里那几个常量一致：
//! Unix 走 FHS 的 `/etc`、`/var/lib`，Windows 没有 FHS，等价物是
//! `%ProgramData%\StrixMaid`——「机器范围、非用户、可写」的系统目录。
//!
//! 节点标识（`node_id`）要求**同一台机器重装 Agent 后仍然不变**，否则 Server
//! 上会多出一个孤儿节点、历史指标断成两截。两个平台各有各的来源：
//!
//! | 平台 | 来源 | 何时产生 |
//! |---|---|---|
//! | Linux / 类 Unix | `/etc/machine-id`（回退 `/var/lib/dbus/machine-id`） | 系统首次启动时由 systemd 生成 |
//! | Windows | `HKLM\SOFTWARE\Microsoft\Cryptography` 的 `MachineGuid` | 系统安装时由 CryptoAPI 生成 |
//!
//! `MachineGuid` 是 Windows 上 `/etc/machine-id` 最接近的对应物：全局唯一、
//! 随系统安装确定、重启与重装应用都不变，且**不需要任何特权**就能读到
//! （该键对 Users 组可读）。它与网卡 MAC、主机名之类的候选不同——后两者会随
//! 换网卡、加域、改名而变，用它们当节点标识迟早出事。
//!
//! # 偏离记录（相对 roadmap/05 §3.1 的表）
//!
//! `tls.insecure` 未实现，`wss://` 暂不支持：TLS 栈（rustls 还是 native-tls、
//! 与 musl 静态链接的关系）该随 `06-packaging.md` 一并决策，先为一个开发用
//! 开关引入整套 TLS 依赖不划算。当前 `server_url` 只接受 `ws://`，配了
//! `wss://` 在校验时报错并说明原因。

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use figment::Figment;
use figment::providers::{Env, Format, Serialized, Toml};
use serde::{Deserialize, Serialize};
use strixmaid_core::config::MetricsConfig;

/// 缺省配置文件路径。
#[cfg(not(windows))]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/strixmaid/agent.toml";
/// 见上。
#[cfg(windows)]
pub const DEFAULT_CONFIG_PATH: &str = r"C:\ProgramData\StrixMaid\agent.toml";

/// 缺省数据目录（本地 SQLite 落在这里）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub const DEFAULT_DATA_DIR: &str = "/var/lib/strixmaid-agent";
/// 见上。
///
/// macOS 上**没有 `/var/lib`**，对应物是 `/var/db`。理由与
/// [`strixmaid_core::config::DEFAULT_DATA_DIR`] 那条完全相同（hier(7)：
/// `/var/db` 是「misc. automatically generated system-specific database files」），
/// 这里只是把同一个决定套在 agent 自己的目录名上。
///
/// 目录不必由安装脚本预先建好：[`strixmaid_core::store::Store::open`] 会
/// `create_dir_all` 出它的父目录。Linux 上 systemd 的 `StateDirectory=` 也会建，
/// 而 launchd 没有对应物，所以在 macOS 上这条自建路径是唯一的保障。
#[cfg(target_os = "macos")]
pub const DEFAULT_DATA_DIR: &str = "/var/db/strixmaid-agent";
/// 见上。
#[cfg(windows)]
pub const DEFAULT_DATA_DIR: &str = r"C:\ProgramData\StrixMaid\agent-data";

/// Agent 运行时配置。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// Server 地址，如 `ws://server:9700`。必填。路径部分不用写，
    /// Agent 自己拼 `/ws/agent`。
    pub server_url: String,
    /// 节点稳定标识；必须与 Server 上 `POST /nodes` 登记的 id 一致。
    /// 缺省由 [`AgentConfig::resolve_node_id`] 按平台取机器标识。
    pub node_id: Option<String>,
    /// 显示名；缺省取主机名。
    pub node_name: Option<String>,
    /// 预共享 token（`POST /nodes` 的响应）。与 `token_file` 二选一。
    pub token: Option<String>,
    /// 从文件读 token（首行，去空白）。适合不想把 token 写进配置文件的部署。
    pub token_file: Option<PathBuf>,
    /// 本地 SQLite 目录。
    pub data_dir: PathBuf,
    /// 采集配置，与 Server 的 `[metrics]` 完全同构。
    pub metrics: MetricsConfig,
    /// 常规推送节拍（秒）：每隔这么久把本地新落盘的 `m_1m` 行推给 Server。
    /// 落盘本身每分钟一次，节拍取它的零头即可；允许 5–300。
    ///
    /// roadmap/05 未给这个字段命名，属实现补充。
    pub sync_interval_secs: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            server_url: String::new(),
            node_id: None,
            node_name: None,
            token: None,
            token_file: None,
            data_dir: PathBuf::from(DEFAULT_DATA_DIR),
            metrics: MetricsConfig::default(),
            sync_interval_secs: 20,
        }
    }
}

impl AgentConfig {

    /// 带注释的示例配置。
    ///
    /// 与 `Config::example_toml()` 同理：**由二进制自己生成**，不在打包脚本里手抄。
    /// 路径取值按平台不同（`data_dir` 的缺省在 Linux / macOS / Windows 上是三个值），
    /// 手抄一份必然会漂。安装脚本一律 `strixmaid-agent config example > agent.toml`。
    ///
    /// 只列必填项与最常改的几项；其余走内置默认值，写全反而让人以为都得填。
    pub fn example_toml() -> String {
        let d = AgentConfig::default();
        format!(
            "# StrixMaid agent 配置。填好两个必填项即可启动。\n\
             # 环境变量同名覆盖：STRIXMAID_AGENT_SERVER_URL 等（嵌套键用 __）。\n\
             # 命令行优先级最高：--server-url / --data-dir。\n\
             \n\
             # 【必填】指标推给哪台 Server。跨公网请在服务端前用反向代理终结 TLS。\n\
             server_url = \"ws://<server>:9700\"\n\
             \n\
             # 【必填】预共享 token：在服务端注册节点（POST /nodes）时返回，只出现一次。\n\
             # 不想写进本文件可改用 token_file（读首行）：\n\
             #   token_file = \"/etc/strixmaid/agent.token\"\n\
             token = \"<node token>\"\n\
             \n\
             # 节点标识；必须与服务端登记的 id 一致。缺省按平台取机器标识。\n\
             # node_id = \"\"\n\
             \n\
             # 面板上的显示名。缺省取主机名。\n\
             # node_name = \"\"\n\
             \n\
             # 本地 SQLite 目录。本机缺省：{data_dir}\n\
             # data_dir = \"{data_dir}\"\n\
             \n\
             # 推送节拍（秒），允许 5–300。落盘每分钟一次，取它的零头即可。\n\
             # sync_interval_secs = {sync}\n",
            data_dir = d.data_dir.display(),
            sync = d.sync_interval_secs,
        )
    }

    /// 同上，外加一层命令行覆盖（优先级最高，见 `design.md` §12）。
    ///
    /// `cli` 经 [`strixmaid_core::config::cli_layer`] 转换，它会**递归剔除所有
    /// `None` / 空值**。这一步不能省：clap 的可选参数未指定时序列化成 `null`，
    /// 直接交给 figment 会把配置文件与环境变量里的值覆盖成空，而且报的错是
    /// 「invalid type: found option, expected path string」这种看不出因果的话。
    /// core 那边的文档管它叫「figment 分层配置里最常见的坑」，确实。
    pub fn load_with<T: serde::Serialize>(
        path: Option<&Path>,
        cli: Option<&T>,
    ) -> anyhow::Result<AgentConfig> {
        let mut figment = Figment::from(Serialized::defaults(AgentConfig::default()));
        match path {
            Some(p) => {
                if !p.exists() {
                    bail!("配置文件 {} 不存在", p.display());
                }
                figment = figment.merge(Toml::file(p));
            }
            None => {
                let p = Path::new(DEFAULT_CONFIG_PATH);
                if p.exists() {
                    figment = figment.merge(Toml::file(p));
                }
            }
        }
        let mut figment = figment.merge(Env::prefixed("STRIXMAID_AGENT_").split("__"));
        if let Some(cli) = cli {
            figment = figment.merge(strixmaid_core::config::cli_layer(cli)?);
        }
        let cfg: AgentConfig = figment
            .extract()
            .context("解析 Agent 配置失败")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验。错误信息面向改配置的人。
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.server_url.trim().is_empty() {
            bail!("server_url 必填，如 ws://server:9700");
        }
        if self.server_url.starts_with("wss://") {
            bail!(
                "暂不支持 wss://（TLS 栈随 06-packaging 一并决策）；\
                 开发与内网部署请用 ws://，公网请在 Server 前挂反代终结 TLS"
            );
        }
        if !self.server_url.starts_with("ws://") {
            bail!("server_url 必须以 ws:// 开头，收到 {}", self.server_url);
        }
        if self.token.is_none() && self.token_file.is_none() {
            bail!("token 与 token_file 必须配置其一（来自 Server 的 POST /nodes）");
        }
        if !(1..=60).contains(&self.metrics.interval_secs) {
            bail!(
                "metrics.interval_secs 允许 1–60，收到 {}",
                self.metrics.interval_secs
            );
        }
        if !(5..=300).contains(&self.sync_interval_secs) {
            bail!(
                "sync_interval_secs 允许 5–300，收到 {}",
                self.sync_interval_secs
            );
        }
        Ok(())
    }

    /// 本地数据库路径。
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("strixmaid-agent.db")
    }

    /// 定下节点 id：配置优先，其次取本机的机器标识（来源按平台不同，见模块文档）。
    ///
    /// 取不到时**报错而不是生成一个**：随机生成的 id 每次重启都不一样，
    /// Server 那边会把同一台机器当成源源不断的新节点，比直接失败更难排查。
    pub fn resolve_node_id(&self) -> anyhow::Result<String> {
        if let Some(id) = &self.node_id {
            return Ok(id.clone());
        }
        machine_id().ok_or_else(|| anyhow::anyhow!("{}", MACHINE_ID_MISSING))
    }

    /// 定下 token：`token` 优先，其次读 `token_file` 首行。
    pub fn resolve_token(&self) -> anyhow::Result<String> {
        if let Some(t) = &self.token {
            return Ok(t.clone());
        }
        let path = self.token_file.as_ref().expect("validate 保证二者有一");
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读取 token_file {} 失败", path.display()))?;
        let token = text.lines().next().unwrap_or("").trim().to_string();
        if token.is_empty() {
            bail!("token_file {} 是空的", path.display());
        }
        Ok(token)
    }
}

/// 取不到机器标识时的提示，按平台指出该去看哪里。
#[cfg(not(windows))]
const MACHINE_ID_MISSING: &str =
    "读不到 /etc/machine-id（也读不到 /var/lib/dbus/machine-id），请显式配置 node_id";
/// 见上。
#[cfg(windows)]
const MACHINE_ID_MISSING: &str = concat!(
    "读不到 HKLM\\SOFTWARE\\Microsoft\\Cryptography 的 MachineGuid，",
    "请显式配置 node_id"
);

/// 本机的机器标识，取不到返回 `None`。
///
/// systemd 生成的 `machine-id` 是 32 位十六进制；`/var/lib/dbus/machine-id`
/// 是它在没有 systemd 的系统上的老位置，很多发行版把前者软链到后者。
#[cfg(not(windows))]
fn machine_id() -> Option<String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            let id = text.trim();
            if !id.is_empty() {
                return Some(id.to_owned());
            }
        }
    }
    None
}

/// 本机的机器标识，取不到返回 `None`。
///
/// 值形如 `4f1a2b3c-...`（带连字符的 GUID，无花括号）。这里**原样返回**，
/// 不做大小写或格式归一：`node_id` 是与 Server 之间的约定标识，
/// 换一台 Server、换一个版本都必须得到同一个字符串，任何「顺手规范化」
/// 都会在将来变成一次静默的节点身份漂移。只去掉首尾空白——注册表值理论上
/// 不该带空白，但真带了的话，那是脏数据而不是标识的一部分。
#[cfg(windows)]
fn machine_id() -> Option<String> {
    use strixmaid_core::platform::windows::{HKLM, reg_string};

    let raw = reg_string(HKLM, r"SOFTWARE\Microsoft\Cryptography", "MachineGuid")?;
    let id = raw.trim();
    if id.is_empty() {
        None
    } else {
        Some(id.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只为 `load_with` 的两条用例造一个临时配置文件。
    ///
    /// 不引 `tempfile`：整个仓库没有这个依赖，为两条测试加一个不划算。
    /// 名字带进程 id 与用例名，并发跑也不会撞。
    struct TempToml(PathBuf);

    impl TempToml {
        fn new(tag: &str, body: &str) -> TempToml {
            let p = std::env::temp_dir().join(format!(
                "strixmaid-agent-{}-{}.toml",
                std::process::id(),
                tag
            ));
            std::fs::write(&p, body).expect("写临时配置");
            TempToml(p)
        }
    }

    impl Drop for TempToml {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    const FILE_BODY: &str =
        "server_url = \"ws://from-file:9700\"\ntoken = \"t\"\ndata_dir = \"/from/file\"\n";

    #[derive(serde::Serialize)]
    struct Overrides {
        server_url: Option<String>,
        data_dir: Option<PathBuf>,
    }

    /// 命令行层必须经 `cli_layer` 剔空，否则未指定的可选参数会把下层覆盖成空。
    ///
    /// 2026-09-17 踩过：`load_with` 里直接 `Serialized::defaults(cli)`，于是
    /// `--data-dir` 没给时 `Option::None` 被当成一个值序列化进去，figment 报
    /// 「invalid type: found option, expected path string」——一句看不出因果的话。
    /// 本机从没真正起过 agent（只测了 `config example` 与参数拒绝），
    /// 直到 CI 的 07 验收里 agent 起不来才暴露。
    ///
    /// 这条钉的就是「全 None 的覆盖层等于没有覆盖层」。
    #[test]
    fn 全空的命令行层不覆盖配置文件里的值() {
        let f = TempToml::new("allnone", FILE_BODY);
        let cfg = AgentConfig::load_with(
            Some(&f.0),
            Some(&Overrides {
                server_url: None,
                data_dir: None,
            }),
        )
        .expect("全 None 的命令行层不该让解析失败");

        assert_eq!(cfg.server_url, "ws://from-file:9700");
        assert_eq!(cfg.data_dir, PathBuf::from("/from/file"));
    }

    /// 给了值就要盖住文件里的；没给的那项不受影响。
    #[test]
    fn 命令行给的值优先于配置文件() {
        let f = TempToml::new("given", FILE_BODY);
        let cfg = AgentConfig::load_with(
            Some(&f.0),
            Some(&Overrides {
                server_url: Some("ws://from-cli:9700".into()),
                data_dir: None,
            }),
        )
        .expect("解析");

        assert_eq!(cfg.server_url, "ws://from-cli:9700", "命令行该盖住文件");
        assert_eq!(
            cfg.data_dir,
            PathBuf::from("/from/file"),
            "没给的那项仍取文件里的"
        );
    }

    fn from_toml(toml: &str) -> anyhow::Result<AgentConfig> {
        let cfg: AgentConfig = Figment::from(Serialized::defaults(AgentConfig::default()))
            .merge(Toml::string(toml))
            .extract()?;
        cfg.validate()?;
        Ok(cfg)
    }

    #[test]
    fn 最小配置与缺省值() {
        let cfg = from_toml(
            r#"
            server_url = "ws://server:9700"
            token = "abc"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        assert_eq!(
            cfg.db_path(),
            Path::new(DEFAULT_DATA_DIR).join("strixmaid-agent.db")
        );
        assert_eq!(cfg.sync_interval_secs, 20);
        assert_eq!(cfg.metrics.interval_secs, 2, "metrics 与 Server 同构同默认");
    }

    #[test]
    fn 必填与边界() {
        assert!(from_toml("").is_err(), "缺 server_url");
        assert!(
            from_toml(r#"server_url = "ws://s""#).is_err(),
            "缺 token / token_file"
        );
        assert!(
            from_toml(r#"server_url = "http://s"
token = "t""#)
            .is_err(),
            "只认 ws://"
        );
        let err = from_toml(r#"server_url = "wss://s"
token = "t""#)
        .unwrap_err();
        assert!(err.to_string().contains("wss"), "{err}");
        assert!(
            from_toml(r#"server_url = "ws://s"
token = "t"
sync_interval_secs = 1"#)
            .is_err()
        );
    }

    #[test]
    fn token_file_读首行() {
        let dir = std::env::temp_dir().join(format!("strixmaid-agent-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("token");
        std::fs::write(&f, "  s3cret \n下一行不算\n").unwrap();
        // TOML 的双引号串把反斜杠当转义符，Windows 路径（`C:\Users\...`）原样塞
        // 进去会解析失败；单引号的字面量串不做任何转义，两个平台通用。
        let cfg = from_toml(&format!(
            "server_url = \"ws://s\"\ntoken_file = '{}'",
            f.display()
        ))
        .unwrap();
        assert_eq!(cfg.resolve_token().unwrap(), "s3cret");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 显式配置的_node_id_压过机器标识() {
        let cfg = from_toml(
            r#"
            server_url = "ws://s"
            token = "t"
            node_id = "手工指定"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.resolve_node_id().unwrap(), "手工指定");
    }

    #[test]
    fn 本机机器标识可读且稳定() {
        let Some(first) = machine_id() else {
            // 容器里 /etc/machine-id 可能是空的，Windows 上该注册表键理论上
            // 也可能被裁剪掉。取不到时说明情况并跳过，而不是判本机不合格。
            eprintln!("本机取不到机器标识，跳过：{MACHINE_ID_MISSING}");
            return;
        };
        assert!(!first.trim().is_empty(), "取到的标识不该是空白");
        assert_eq!(
            machine_id().as_deref(),
            Some(first.as_str()),
            "同一次运行里连取两次必须一致——它是节点身份的唯一来源"
        );
        eprintln!("本机机器标识：{first}");

        let cfg = from_toml(
            r#"
            server_url = "ws://s"
            token = "t"
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.resolve_node_id().unwrap(),
            first,
            "未配置 node_id 时应当回落到机器标识"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_上的标识来自_machine_guid() {
        use strixmaid_core::platform::windows::{HKLM, reg_string};

        let Some(guid) = reg_string(HKLM, r"SOFTWARE\Microsoft\Cryptography", "MachineGuid") else {
            eprintln!("本机 HKLM\\SOFTWARE\\Microsoft\\Cryptography 下没有 MachineGuid，跳过");
            return;
        };
        assert_eq!(
            machine_id().as_deref(),
            Some(guid.trim()),
            "原样取用，不做大小写或格式归一"
        );
        // MachineGuid 是不带花括号的 GUID：8-4-4-4-12。
        assert_eq!(guid.trim().len(), 36, "取到的不像 GUID：{guid}");
        assert_eq!(
            guid.trim().matches('-').count(),
            4,
            "取到的不像 GUID：{guid}"
        );
    }
}
