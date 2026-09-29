use super::*;
use figment::providers::{Format, Serialized, Toml};
use figment::{Figment, value::Value};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

/// 一天的秒数。只有 §7.2 保留期表的用例需要它。
const DAY: u64 = 86_400;

/// 只用「默认值 + TOML」两层构造配置，完全不碰进程环境，
/// 因此可以和其它测试并行跑。
fn from_toml(toml: &str) -> Result<Config> {
    Config::from_figment(
        Figment::from(Serialized::defaults(Config::default())).merge(Toml::string(toml)),
    )
}

// ---------------------------------------------------------- 环境变量夹具

/// 进程环境是全局状态，改它的测试必须互斥。
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// 在设置好指定环境变量的前提下执行 `f`，结束后清理。
fn with_env<R>(vars: &[(&str, &str)], f: impl FnOnce() -> R) -> R {
    // 中毒说明上一个测试 panic 了，环境已被 unset，继续用即可。
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: 所有会修改进程环境的测试都持有 ENV_LOCK，不存在并发读写。
    unsafe {
        for (k, v) in vars {
            std::env::set_var(k, v);
        }
    }
    let result = f();
    // SAFETY: 同上。
    unsafe {
        for (k, _) in vars {
            std::env::remove_var(k);
        }
    }
    result
}

/// 临时 TOML 文件，Drop 时删除。
struct TempToml(PathBuf);

impl TempToml {
    fn new(name: &str, content: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "strixmaid-config-test-{}-{name}.toml",
            std::process::id()
        ));
        std::fs::write(&path, content).expect("写入临时配置文件");
        TempToml(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempToml {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// ---------------------------------------------------------------- 默认值

#[test]
fn 默认值符合设计文档() {
    let c = Config::default();
    assert_eq!(c.listen, "127.0.0.1:9700");
    assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
    assert_eq!(c.run_dir, PathBuf::from(DEFAULT_RUN_DIR));
    assert_eq!(c.helper_path, PathBuf::from("strixmaid-helper"));
    assert_eq!(c.pam_service, "strixmaid");
    assert_eq!(c.log.level, LogLevel::Info);
    assert_eq!(c.metrics.interval_secs, 2);
    assert_eq!(c.metrics.ring_secs, 3600);
    assert_eq!(c.metrics.retention, RetentionPreset::Normal);
    assert_eq!(c.session.idle_timeout_secs, 900);
    assert_eq!(c.session.elevated_idle_timeout_secs, 300);
    c.validate().expect("默认值必须自洽");
}

#[test]
fn 默认值的派生路径() {
    let c = Config::default();
    assert_eq!(
        c.db_path(),
        PathBuf::from(DEFAULT_DATA_DIR).join(DB_FILE_NAME)
    );
    assert_eq!(
        c.helper_socket_path(),
        PathBuf::from(DEFAULT_RUN_DIR).join(HELPER_SOCKET_NAME)
    );
    assert_eq!(
        c.listen_addr().unwrap(),
        "127.0.0.1:9700".parse::<SocketAddr>().unwrap()
    );
    // 1 小时 / 2 秒 = 1800 个采样点
    assert_eq!(c.metrics.ring_capacity(), 1800);
    assert_eq!(c.metrics.interval(), Duration::from_secs(2));
}

#[test]
fn 顶层键清单与结构体保持同步() {
    let dict = Value::serialize(Config::default())
        .unwrap()
        .into_dict()
        .unwrap();
    let mut actual: Vec<&str> = dict.keys().map(String::as_str).collect();
    actual.sort_unstable();
    let mut known: Vec<&str> = Config::TOP_LEVEL_KEYS.to_vec();
    known.sort_unstable();
    assert_eq!(actual, known);
}

// ------------------------------------------------------------- 示例配置

#[test]
fn 示例配置等于默认值且能通过校验() {
    let from_example = from_toml(&Config::example_toml()).expect("示例配置必须合法");
    assert_eq!(from_example, Config::default());

    // 就算不叠默认值层，示例也应当能独立解析出完整配置（serde(default) 兜底）。
    let standalone =
        Config::from_figment(Figment::from(Toml::string(&Config::example_toml()))).unwrap();
    assert_eq!(standalone, Config::default());
}

// ----------------------------------------------------- 第 2 层：TOML

#[test]
fn toml_覆盖默认值且未提及的字段保持默认() {
    let c = from_toml(
        r#"
        listen = "0.0.0.0:8080"
        data_dir = "/srv/strixmaid"

        [log]
        level = "debug"

        [metrics]
        interval_secs = 10
        retention = "less"
        "#,
    )
    .unwrap();

    // 被覆盖的
    assert_eq!(c.listen, "0.0.0.0:8080");
    assert_eq!(c.data_dir, PathBuf::from("/srv/strixmaid"));
    assert_eq!(c.log.level, LogLevel::Debug);
    assert_eq!(c.metrics.interval_secs, 10);
    assert_eq!(c.metrics.retention, RetentionPreset::Less);
    // 同一张表里没提到的字段必须保持默认（部分覆盖，不是整表替换）
    assert_eq!(c.metrics.ring_secs, 3600);
    // 完全没提到的表
    assert_eq!(c.run_dir, PathBuf::from(DEFAULT_RUN_DIR));
    assert_eq!(c.session, SessionConfig::default());
}

// --------------------------------------------------- 第 3 层：环境变量

#[test]
fn 环境变量覆盖_toml() {
    let file = TempToml::new(
        "env-over-toml",
        r#"
        listen = "0.0.0.0:8080"

        [metrics]
        interval_secs = 7
        ring_secs = 7200

        [session]
        idle_timeout_secs = 1200
        "#,
    );

    with_env(
        &[
            ("STRIXMAID_LISTEN", "127.0.0.1:19700"),
            ("STRIXMAID_METRICS__INTERVAL_SECS", "5"),
            ("STRIXMAID_LOG__LEVEL", "trace"),
            ("STRIXMAID_METRICS__RETENTION", "less"),
            // 与配置无关的 STRIXMAID_* 变量不应导致「未知字段」错误
            ("STRIXMAID_CONFIG", "/dev/null"),
        ],
        || {
            let c = Config::load_from(file.path(), None).expect("加载应成功");
            // env 覆盖 toml
            assert_eq!(c.listen, "127.0.0.1:19700");
            assert_eq!(c.metrics.interval_secs, 5);
            // env 覆盖默认值
            assert_eq!(c.log.level, LogLevel::Trace);
            assert_eq!(c.metrics.retention, RetentionPreset::Less);
            // env 没提到的字段，toml 值仍然生效
            assert_eq!(c.metrics.ring_secs, 7200);
            assert_eq!(c.session.idle_timeout_secs, 1200);
            // 三层都没提到的字段仍是默认值
            assert_eq!(c.data_dir, PathBuf::from(DEFAULT_DATA_DIR));
        },
    );
}

#[test]
fn 配置文件路径可用环境变量覆盖() {
    let file = TempToml::new("config-path", "pam_service = \"strixmaid-test\"\n");
    let path_str = file.path().to_str().unwrap().to_string();
    with_env(&[("STRIXMAID_CONFIG", &path_str)], || {
        assert_eq!(Config::config_path(), file.path());
        let c = Config::load(None).unwrap();
        assert_eq!(c.pam_service, "strixmaid-test");
    });
}

// ------------------------------------------------- 第 4 层：命令行参数

#[derive(Serialize)]
struct FakeCli {
    listen: Option<String>,
    data_dir: Option<PathBuf>,
    metrics: FakeCliMetrics,
}

#[derive(Serialize)]
struct FakeCliMetrics {
    interval_secs: Option<u64>,
}

#[test]
fn 命令行覆盖环境变量且_none_不清空低优先级来源() {
    let file = TempToml::new(
        "cli",
        r#"
        data_dir = "/srv/from-toml"

        [metrics]
        ring_secs = 1800
        "#,
    );

    let cli = cli_layer(FakeCli {
        listen: Some("127.0.0.1:29700".into()),
        data_dir: None,
        metrics: FakeCliMetrics {
            interval_secs: Some(30),
        },
    })
    .unwrap();

    with_env(
        &[
            ("STRIXMAID_LISTEN", "127.0.0.1:19700"),
            ("STRIXMAID_METRICS__INTERVAL_SECS", "5"),
        ],
        || {
            let c = Config::load_from(file.path(), Some(cli.clone())).unwrap();
            // 命令行 > 环境变量
            assert_eq!(c.listen, "127.0.0.1:29700");
            assert_eq!(c.metrics.interval_secs, 30);
            // 命令行里为 None 的项被剔除，TOML 的值不受影响
            assert_eq!(c.data_dir, PathBuf::from("/srv/from-toml"));
            assert_eq!(c.metrics.ring_secs, 1800);
        },
    );
}

#[test]
fn cli_layer_剔除全部空值() {
    let layer = cli_layer(FakeCli {
        listen: None,
        data_dir: None,
        metrics: FakeCliMetrics {
            interval_secs: None,
        },
    })
    .unwrap();
    // 全空 -> 该层不产生任何键，配置等于默认值
    let c =
        Config::from_figment(Figment::from(Serialized::defaults(Config::default())).merge(layer))
            .unwrap();
    assert_eq!(c, Config::default());
}

// ---------------------------------------------------------------- 校验

#[test]
fn 采集间隔越界被拒并给出可操作的错误() {
    for bad in [0_u64, 61, 3600] {
        let err = from_toml(&format!("[metrics]\ninterval_secs = {bad}\n")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("metrics.interval_secs"), "缺字段名：{msg}");
        assert!(
            msg.contains("STRIXMAID_METRICS__INTERVAL_SECS"),
            "缺环境变量名：{msg}"
        );
        assert!(msg.contains(&format!("当前值 `{bad}`")), "缺当前值：{msg}");
        assert!(msg.contains("1 – 60 秒"), "缺合法范围：{msg}");
    }
    // 边界值合法
    for ok in [1_u64, 60] {
        from_toml(&format!("[metrics]\ninterval_secs = {ok}\n")).unwrap();
    }
}

#[test]
fn 监听地址不可解析时被拒() {
    for bad in ["localhost:9700", "9700", "127.0.0.1", "not an addr", ""] {
        let err = from_toml(&format!("listen = \"{bad}\"\n")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("listen"), "缺字段名：{msg}");
        assert!(msg.contains("STRIXMAID_LISTEN"), "缺环境变量名：{msg}");
    }
    for ok in ["127.0.0.1:9700", "0.0.0.0:80", "[::1]:9700", "[::]:9700"] {
        let c = from_toml(&format!("listen = \"{ok}\"\n")).unwrap();
        assert!(c.listen_addr().is_ok());
    }
}

#[test]
fn 目录为空时被拒() {
    let err = from_toml("data_dir = \"\"\nrun_dir = \"\"\n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("data_dir"), "{msg}");
    assert!(msg.contains("run_dir"), "{msg}");
    assert!(msg.contains("STRIXMAID_DATA_DIR"), "{msg}");
    assert!(msg.contains("共 2 项"), "{msg}");
}

#[test]
fn pam_服务名必须是合法文件名() {
    for bad in ["", "../etc/shadow", "a/b"] {
        let err = from_toml(&format!("pam_service = \"{bad}\"\n")).unwrap_err();
        assert!(err.to_string().contains("pam_service"), "{err}");
    }
    from_toml("pam_service = \"strixmaid-agent\"\n").unwrap();
}

#[test]
fn 会话超时越界被拒() {
    let err = from_toml("[session]\nidle_timeout_secs = 10\n").unwrap_err();
    assert!(
        err.to_string().contains("session.idle_timeout_secs"),
        "{err}"
    );

    let err = from_toml("[session]\nelevated_idle_timeout_secs = 0\n").unwrap_err();
    assert!(
        err.to_string()
            .contains("session.elevated_idle_timeout_secs"),
        "{err}"
    );
}

#[test]
fn 环形缓冲必须能容纳至少一个采样点() {
    // 60s 缓冲 + 61s 间隔：两个字段各自都越界/临界，交叉约束必须报出来
    let err = from_toml("[metrics]\ninterval_secs = 60\nring_secs = 30\n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("metrics.ring_secs"), "{msg}");
}

#[test]
fn 一次性报出全部问题() {
    let err = from_toml(
        r#"
        listen = "nope"
        data_dir = ""

        [metrics]
        interval_secs = 0
        "#,
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("共 3 项"), "{msg}");
    assert!(msg.contains("listen"), "{msg}");
    assert!(msg.contains("data_dir"), "{msg}");
    assert!(msg.contains("metrics.interval_secs"), "{msg}");
}

#[test]
fn 拼错的配置项会被拒绝而不是被忽略() {
    let err = from_toml("lisen = \"127.0.0.1:9700\"\n").unwrap_err();
    assert!(err.to_string().contains("lisen"), "{err}");

    let err = from_toml("[metrics]\ninterval_sec = 5\n").unwrap_err();
    assert!(err.to_string().contains("interval_sec"), "{err}");
}

#[test]
fn 非法枚举值给出候选列表() {
    let err = from_toml("[metrics]\nretention = \"lots\"\n").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("less") && msg.contains("normal"), "{msg}");

    let err = from_toml("[log]\nlevel = \"verbose\"\n").unwrap_err();
    assert!(err.to_string().contains("trace"), "{err}");
}

// ------------------------------------------------------- §7.2 保留期表

#[test]
fn 保留期预设与设计文档_7_2_一致() {
    use MetricLayer::{M1d, M1m, M5m, M12h, M15m};

    let less = MetricsConfig {
        retention: RetentionPreset::Less,
        ..MetricsConfig::default()
    };
    let normal = MetricsConfig::default();
    assert_eq!(normal.retention, RetentionPreset::Normal, "默认预设");

    let expect = [
        (M1m, 60, 6 * HOUR, DAY),
        (M5m, 300, 3 * DAY, 7 * DAY),
        (M15m, 900, 14 * DAY, 30 * DAY),
        (M12h, 43_200, 90 * DAY, 90 * DAY),
        (M1d, 86_400, 365 * DAY, 365 * DAY),
    ];
    for (layer, bucket, l, n) in expect {
        assert_eq!(layer.bucket_secs(), Some(bucket as u32), "{layer} 桶宽");
        assert_eq!(less.retention_secs(layer), Some(l), "{layer} less 保留期");
        assert_eq!(
            normal.retention_secs(layer),
            Some(n),
            "{layer} normal 保留期"
        );
        // 保留期必须是桶宽的整数倍，否则清理边界会切到半个桶
        assert_eq!(l % bucket, 0);
        assert_eq!(n % bucket, 0);
    }
    assert_eq!(M15m.table_name(), Some("m_15m"));
    assert_eq!(MetricLayer::PERSISTED.len(), expect.len());

    // live 层不落盘，没有保留期可言。聚合链（谁从谁来）归 store 的
    // `retention_table_matches_design` 守，本用例只管 config 这一侧看到的数值。
    assert_eq!(normal.retention_secs(MetricLayer::Live), None);
}

#[test]
fn 枚举的字符串形式可直接用于_serde_与日志() {
    for level in LogLevel::ALL {
        let toml = format!("[log]\nlevel = \"{}\"\n", level.as_str());
        assert_eq!(from_toml(&toml).unwrap().log.level, level);
    }
    for preset in [RetentionPreset::Less, RetentionPreset::Normal] {
        let toml = format!("[metrics]\nretention = \"{}\"\n", preset.as_str());
        assert_eq!(from_toml(&toml).unwrap().metrics.retention, preset);
    }
    // 大小写变体（运维手写配置时常见）
    assert_eq!(
        from_toml("[log]\nlevel = \"WARN\"\n").unwrap().log.level,
        LogLevel::Warn
    );
    assert_eq!(
        from_toml("[metrics]\nretention = \"Normal\"\n")
            .unwrap()
            .metrics
            .retention,
        RetentionPreset::Normal
    );
}

// -------------------------------------------------- 配置文件缺失与损坏

#[test]
fn 配置文件不存在时回落到默认值() {
    let missing = std::env::temp_dir().join(format!(
        "strixmaid-config-test-{}-不存在的配置.toml",
        std::process::id()
    ));
    assert!(!missing.exists(), "夹具前提：该路径确实不存在");

    // 全新安装的机器上没有 /etc/strixmaid/config.toml，此时必须照常起服务
    // （design.md §12），而不是因为读不到文件就退出。
    // 借 with_env 拿锁，避免并发测试改动的 STRIXMAID_* 干扰 env 层。
    let c = with_env(&[], || Config::load_from(&missing, None)).expect("配置文件不存在不应报错");
    assert_eq!(c.listen, DEFAULT_LISTEN);
    assert_eq!(c.metrics.retention, RetentionPreset::Normal);
}

#[test]
fn 配置路径的多层父目录不存在时仍允许缺省() {
    let missing = PathBuf::from(format!(
        "strixmaid-config-test-{}-不存在的父目录",
        std::process::id()
    ));
    // 同时覆盖绝对路径和相对路径；后者的 ancestors 最终包含空路径。
    for parent in [std::env::temp_dir().join(&missing), missing] {
        assert!(!parent.exists(), "夹具前提：父目录确实不存在");
        let path = parent.join("nested").join("config.toml");
        let config = Config::from_figment(
            Figment::from(Serialized::defaults(Config::default()))
                .merge(ConfigFile::optional(&path)),
        )
        .expect("真正缺失的父目录不应阻止首次启动");
        assert_eq!(config, Config::default());
    }
}

#[test]
fn 显式配置文件缺失不能回落默认值() {
    let missing = std::env::temp_dir().join(format!(
        "strixmaid-config-test-{}-显式缺失.toml",
        std::process::id()
    ));
    assert!(!missing.exists(), "夹具前提：该路径确实不存在");
    for path in [missing.clone(), missing.join("nested").join("config.toml")] {
        let err = Config::from_figment(
            Figment::from(Serialized::defaults(Config::default()))
                .merge(ConfigFile::required(&path)),
        )
        .expect_err("显式配置缺失必须报错");
        assert!(matches!(err, ConfigError::Source(_)));
        assert!(err.to_string().contains(&path.display().to_string()));
    }
}

#[test]
fn 配置文件存在但解析失败必须报错且带上路径() {
    // 语法错误：`listen` 没有值。静默忽略这种文件比直接失败危险得多。
    let file = TempToml::new("语法错误", "listen = \n");
    let err = with_env(&[], || Config::load_from(file.path(), None))
        .expect_err("语法错误的配置文件必须报错");

    let msg = err.to_string();
    assert!(
        msg.contains(&file.path().display().to_string()),
        "报错信息必须指出是哪个文件：{msg}"
    );
}

#[test]
fn 字段路径到环境变量名的映射() {
    let e = FieldError::new("metrics.interval_secs", 0, "x");
    assert_eq!(e.env_var(), "STRIXMAID_METRICS__INTERVAL_SECS");
    let e = FieldError::new("listen", "x", "y");
    assert_eq!(e.env_var(), "STRIXMAID_LISTEN");
}

mod elevate_groups_tests {
    use super::*;

    /// 默认放行组必须就是 [`strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS`]，
    /// 而那份常量是按平台给的：Unix 上覆盖三大发行版惯例（Debian 的 `sudo`、
    /// RHEL / Arch 的 `wheel`、老 Ubuntu 与 macOS 的 `admin`），
    /// Windows 上只有内建的 `Administrators`——那边根本不存在前三个组，
    /// 列上去只会让人以为「配置里写了就该生效」。
    #[test]
    fn 默认放行组是本平台的惯例() {
        let c = Config::default();
        let expect: Vec<String> = strixmaid_types::auth::DEFAULT_ELEVATE_GROUPS
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(c.session.elevate_groups, expect);
        #[cfg(unix)]
        assert_eq!(c.session.elevate_groups, ["sudo", "wheel", "admin"]);
        #[cfg(windows)]
        assert_eq!(c.session.elevate_groups, ["Administrators"]);
        c.validate().expect("默认配置必须合法");
    }

    #[test]
    fn 示例配置里的值与默认值一致() {
        // 示例文件是给人抄的；它与默认值不符时，照抄的人会得到意料之外的行为。
        let from_example: Config = toml::from_str(&Config::example_toml()).expect("示例可解析");
        assert_eq!(
            from_example.session.elevate_groups,
            Config::default().session.elevate_groups
        );
    }

    #[test]
    fn 空列表合法_表示禁止任何人提权() {
        let c: Config = toml::from_str("[session]\nelevate_groups = []\n").unwrap();
        assert!(c.session.elevate_groups.is_empty());
        c.validate().expect("空列表是合法配置，不该报错");
    }

    #[test]
    fn 把数组写成逗号分隔的字符串会被拦下() {
        // `elevate_groups = ["sudo, wheel"]` 这种写法只会得到一个永远匹配不上的
        // 「组名」，提权就对所有人静默关闭了。必须在启动时报错。
        let c: Config = toml::from_str("[session]\nelevate_groups = [\" sudo\"]\n").unwrap();
        let err = c.validate().unwrap_err();
        assert!(
            err.to_string().contains("session.elevate_groups[0]"),
            "{err}"
        );
    }

    #[test]
    fn 空组名被拦下() {
        let c: Config = toml::from_str("[session]\nelevate_groups = [\"sudo\", \"\"]\n").unwrap();
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("elevate_groups[1]"), "{err}");
    }
}

#[test]
fn 配置路径指向目录必须报来源错误() {
    let path = std::env::temp_dir();
    let err = with_env(&[], || Config::load_from(&path, None))
        .expect_err("目录不是缺失文件，不能静默使用默认配置");
    assert!(matches!(err, ConfigError::Source(_)));
    assert!(err.to_string().contains(&path.display().to_string()));
}

#[test]
fn 配置路径的中间组件不是目录必须报错() {
    let parent = TempToml::new("非目录父级", "");
    for path in [
        parent.path().join("config.toml"),
        parent.path().join("nested").join("config.toml"),
    ] {
        let err =
            with_env(&[], || Config::load_from(&path, None)).expect_err("路径无效不应当作首次启动");
        assert!(matches!(err, ConfigError::Source(_)));
        assert!(err.to_string().contains(&path.display().to_string()));

        let err = Config::from_figment(
            Figment::from(Serialized::defaults(Config::default()))
                .merge(ConfigFile::required(&path)),
        )
        .expect_err("显式配置同样必须拒绝无效路径");
        assert!(matches!(err, ConfigError::Source(_)));
        assert!(err.to_string().contains(&path.display().to_string()));
    }
}

#[cfg(unix)]
#[test]
fn 配置路径指向设备必须报错而不是读取设备() {
    let err = with_env(&[], || Config::load_from("/dev/null", None))
        .expect_err("设备不是配置文件，不应被当成空 TOML");
    assert!(matches!(err, ConfigError::Source(_)));
    let message = err.to_string();
    assert!(message.contains("普通文件"));
    assert!(message.contains("/dev/null"));
}
