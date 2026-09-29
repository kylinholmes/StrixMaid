//! 平台默认值与配置范围。

// ===========================================================================
// 常量
// ===========================================================================

// ---------------------------------------------------------------------------
// 路径默认值
//
// Unix 上是 FHS 的 /etc、/var/lib、/run（§12）。Windows 上没有 FHS，
// 等价物是 `%ProgramData%\StrixMaid`：那正是「机器范围、非用户、可写」的
// 系统目录，所有以服务身份运行的软件都装在那儿。
//
// 为什么写成编译期常量而不是运行时展开 %ProgramData%：配置的默认值必须在
// `Config::default()` 里是确定的（示例配置、错误信息、测试都要引用它）。
// `%ProgramData%` 在实际部署里几乎总是 `C:\ProgramData`——它被改掉的机器
// 极少，而那种机器上用 `--config` 或 `STRIXMAID_DATA_DIR` 显式指定即可。
//
// **macOS 自成一组**（2026-09 起它从开发平台提升为交付目标）。它既不是 FHS
// 也不是 Windows，而是 BSD 的 hier(7) 布局：有 `/etc`、有 `/var`，但
// **没有 `/run`**，也**没有 `/var/lib`**。原先 macOS 跟着 Linux 走
// `#[cfg(not(windows))]`，`run_dir` 因此默认指向一条本机根本不存在、
// 且开机后也不会被任何人创建的路径——那是 bug，不是风格差异。
// 逐条的依据写在各常量的文档注释里。
//
// 分支写成 `all(not(windows), not(target_os = "macos"))` 而不是 `unix`：
// 保持「除 Windows 与 macOS 之外的一切」仍走原来那组取值，与改动前逐字等价。
// ---------------------------------------------------------------------------

/// 默认配置文件路径（§12）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/strixmaid/config.toml";
/// 见上。
///
/// macOS 取与 Linux **相同**的值，这是刻意的而不是漏改：
///
/// * hier(7) 给 `/etc` 的定义就是「system configuration files and scripts」。
///   macOS 上它是 `/private/etc` 的符号链接，位于数据卷、root 可写，
///   不在 SIP 的保护清单里；
/// * 更硬的约束来自 PAM。OpenPAM 只认 `/etc/pam.d/<服务名>`，装这个软件
///   本来就必须往 `/etc` 下放一份（`packaging/pam.d/strixmaid.macos`）。
///   把主配置挪到 `/Library/Application Support` 只会让同一套安装物
///   一半在 `/etc`、一半在 `/Library`，运维要记两个地方；
/// * 不用 `/usr/local/etc`：那是 Homebrew 的前缀（Apple Silicon 上还改成了
///   `/opt/homebrew`），属于包管理器的约定而非系统约定，且 `/usr/local`
///   在一台干净的 macOS 上并不存在。
#[cfg(target_os = "macos")]
pub const DEFAULT_CONFIG_PATH: &str = "/etc/strixmaid/config.toml";
/// 见上。
#[cfg(windows)]
pub const DEFAULT_CONFIG_PATH: &str = r"C:\ProgramData\StrixMaid\config.toml";

/// 环境变量前缀。
pub const ENV_PREFIX: &str = "STRIXMAID_";
/// 环境变量里表示「嵌套一层」的分隔符。
pub const ENV_NESTED_SEPARATOR: &str = "__";
/// 用于覆盖配置文件路径的环境变量。它本身不是配置项，会被 env provider 过滤掉。
pub const CONFIG_PATH_ENV: &str = "STRIXMAID_CONFIG";

/// 默认监听地址（§12）。
pub const DEFAULT_LISTEN: &str = "127.0.0.1:9700";

/// 默认数据目录（§12）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub const DEFAULT_DATA_DIR: &str = "/var/lib/strixmaid";
/// 见上。
///
/// macOS 上**没有 `/var/lib`**——那是 FHS 的目录，hier(7) 里没有它。
/// 对应物是 `/var/db`，hier(7) 的原话是「misc. automatically generated
/// system-specific database files」，而这里放的正是程序自己生成的 SQLite 库
/// （指标 / 会话 / 审计）。系统自带的 `/var/db/sudo`、`/var/db/dhcpclient`
/// 是同一类东西，可以照着找。
///
/// 不用 `/Library/Application Support/StrixMaid`：那是给**应用程序**的支持
/// 文件准备的，Finder 里可见、会被迁移助理一并搬走；而这里是一个只有 root
/// 能读的指标与审计库（目录权限 0700），不该出现在用户会去翻的地方。
///
/// SIP 保护的是 `rootless.conf` 里逐条列出的路径（`/var/db` 下确有几个
/// Apple 自己的子目录在列），`/var/db` 本身不在其中，root 可以在它下面
/// 新建子目录。
#[cfg(target_os = "macos")]
pub const DEFAULT_DATA_DIR: &str = "/var/db/strixmaid";
/// 见上。
#[cfg(windows)]
pub const DEFAULT_DATA_DIR: &str = r"C:\ProgramData\StrixMaid\data";

/// 默认运行目录（§12）。
#[cfg(all(not(windows), not(target_os = "macos")))]
pub const DEFAULT_RUN_DIR: &str = "/run/strixmaid";
/// 见上。
///
/// **macOS 上没有 `/run`**。BSD 传统里这个位置叫 `/var/run`，hier(7) 的
/// 措辞是「system information files describing various info about system
/// since it was booted」——"since it was booted" 也点明了它的生命周期：
/// 内容随每次启动重置，其中的子目录**不能假定跨重启存在**。
///
/// 两处与 Linux 的实际差别，安装脚本与 `docs/macos-platform.md` 里都记了：
///
/// * launchd 没有 systemd `RuntimeDirectory=` 的对应物，不会替进程建这个目录；
/// * 因此需要用到它的那天，得由进程自己 `mkdir` 或由安装脚本在开机后补建。
///
/// 当前代码路径其实还用不到它：helper 走 socketpair，不落文件系统 socket
/// （见 `session::channel`）。保留该项只为配置形状在三个平台上一致。
#[cfg(target_os = "macos")]
pub const DEFAULT_RUN_DIR: &str = "/var/run/strixmaid";
/// 见上。
///
/// Windows 上没有 tmpfs 那样的「重启即空」目录，用 `ProgramData` 下的子目录。
/// 本项目在 Windows 上其实用不到它——helper 走**命名管道**而不是文件系统
/// socket（见 `session::channel`），这个目录留着只为配置形状在三个平台上一致。
#[cfg(windows)]
pub const DEFAULT_RUN_DIR: &str = r"C:\ProgramData\StrixMaid\run";

/// helper 二进制默认值：不含路径分隔符，交由 `PATH` 查找。
///
/// Windows 上磁盘文件是 `strixmaid-helper.exe`，后缀由
/// [`crate::capability::find_executable`] 补，配置里不必写。
pub const DEFAULT_HELPER_PATH: &str = "strixmaid-helper";

/// 默认 PAM 服务名（§5.4）。
///
/// **Windows 上这一项没有意义**：那里的认证走 `LogonUserW`，不读
/// `/etc/pam.d/<名字>`。字段保留是为了让配置形状在三个平台上一致
/// （下游的配置管理不必按平台分叉），helper 在 Windows 上收到后直接忽略。
pub const DEFAULT_PAM_SERVICE: &str = "strixmaid";

/// SQLite 数据库文件名，位于 `data_dir` 下。
pub const DB_FILE_NAME: &str = "strixmaid.db";
/// helper 的 Unix socket 文件名，位于 `run_dir` 下（§10）。
pub const HELPER_SOCKET_NAME: &str = "helper.sock";

/// 采集间隔下限（秒），§7.2 规定可配 1–60s。
pub const METRICS_INTERVAL_MIN_SECS: u64 = 1;
/// 采集间隔上限（秒）。
pub const METRICS_INTERVAL_MAX_SECS: u64 = 60;
/// 内存环形缓冲时长下限（秒）：低于 1 分钟连一个 `m_1m` 桶都凑不满。
pub const METRICS_RING_MIN_SECS: u64 = 60;
/// 内存环形缓冲时长上限（秒）：1 天。再长应该查落盘数据，而不是撑大常驻内存。
pub const METRICS_RING_MAX_SECS: u64 = 24 * 3600;
/// 会话空闲超时下限（秒）。
/// 审计保留天数的默认值。
pub const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 90;

/// 未附着的终端闲置多久即回收，默认 30 分钟。
pub const DEFAULT_TERMINAL_IDLE_TIMEOUT_SECS: u64 = 1800;

/// 单个会话最多同时开几个终端，默认 8。
pub const DEFAULT_TERMINAL_MAX_PER_SESSION: usize = 8;
/// 审计保留天数下限。低于一周的保留期基本等于没有审计。
pub const AUDIT_RETENTION_MIN_DAYS: u32 = 7;
/// 审计保留天数上限（约 10 年）。
pub const AUDIT_RETENTION_MAX_DAYS: u32 = 3650;

pub const SESSION_IDLE_MIN_SECS: u64 = 60;
/// 会话空闲超时上限（秒）：7 天。
pub const SESSION_IDLE_MAX_SECS: u64 = 7 * 24 * 3600;
/// 提权空闲超时下限（秒）。
pub const SESSION_ELEVATED_MIN_SECS: u64 = 30;
/// 提权空闲超时上限（秒）：1 天。
pub const SESSION_ELEVATED_MAX_SECS: u64 = 24 * 3600;

pub(super) const HOUR: u64 = 3_600;
