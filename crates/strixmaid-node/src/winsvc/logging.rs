//! 服务模式下的日志落地。
//!
//! # 为什么不能沿用 stderr
//!
//! `docs/design.md` §12 定的规矩是「日志一律写 stderr，不自写日志文件」，
//! 因为 Unix 上 systemd 会把 stderr 接到 journald，轮转、检索、保留期全由系统
//! 负责，程序自己写文件只会多出一份没人轮转的垃圾。
//!
//! Windows 的 SCM **没有**这一层。服务进程由 `services.exe` 拉起，没有控制台，
//! 也没有任何进程在读它的标准流：`GetStdHandle(STD_ERROR_HANDLE)` 返回的句柄
//! 无效，写进去的字节直接消失。也就是说，服务模式下沿用 stderr 等于关掉日志。
//! 这是本项目唯一自写日志文件的场景，前台运行（`strixmaid serve`）仍旧只写
//! stderr。
//!
//! # 为什么不是 Windows 事件日志
//!
//! 事件日志才是 journald 的严格对等物，但往里写**可读**的条目需要先注册一个
//! 消息资源 DLL（`ReportEventW` 的事件 ID 要能在 DLL 的消息表里查到文本），
//! 否则事件查看器里显示的是「找不到事件 ID 为 1 的描述」加一串原始参数。
//! 那份 DLL 属于打包阶段的产物，且与「不引入新的日志后端依赖」相冲突。
//! 本版先落纯文本文件，内容与 Unix 上 `journalctl -u strixmaid` 拿到的逐行一致。
//!
//! # 轮转只做最简单的一档
//!
//! 进程启动时，若当前文件已超过 [`MAX_BYTES`]，把它改名成 `.1`（覆盖上一代）
//! 再新建。**不做**按时间切分、不做多代保留：那需要一个后台线程与一套命名策略，
//! 而一个常驻服务的日志增长速度与重启频率决定了这一档已经够用。真要长期留存，
//! 应当在打包阶段接事件日志或外部采集。

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context as _;
use strixmaid_core::config::Config;

/// 缺省日志目录。
///
/// 与 `strixmaid_core::config::DEFAULT_DATA_DIR`（`C:\ProgramData\StrixMaid\data`）
/// 同一个父目录：`%ProgramData%\StrixMaid` 就是 Windows 上「机器范围、非用户、
/// 可写」的系统目录，服务身份（LocalSystem）对它有写权限，而普通用户默认只读
/// ——正是日志该待的地方。
///
/// 常量写死而不是运行时展开 `%ProgramData%`，与 core 里那几个路径常量同理：
/// 安装、排错、文档都要引用一个确定的字面量。
pub const DEFAULT_LOG_DIR: &str = r"C:\ProgramData\StrixMaid\logs";

/// 日志文件名。
pub const LOG_FILE: &str = "strixmaid-service.log";

/// 轮转阈值：超过这么大就在下次启动时改名让位。
const MAX_BYTES: u64 = 32 * 1024 * 1024;

/// 按配置定下日志目录。
///
/// 跟着 `data_dir` 走而不是死用 [`DEFAULT_LOG_DIR`]：把数据目录搬到别的盘
/// （`--data-dir D:\strixmaid\data`）通常是因为系统盘小或有独立的数据卷，
/// 日志跟着一起搬才合乎预期。默认配置下 `C:\ProgramData\StrixMaid\data`
/// 的父目录正是 `C:\ProgramData\StrixMaid`，结果与 [`DEFAULT_LOG_DIR`] 相同。
///
/// `data_dir` 没有父目录（例如被配成了 `C:\`）时回落到 [`DEFAULT_LOG_DIR`]
/// ——往盘根写日志是个更糟的选择。
pub fn log_dir_for(data_dir: &Path) -> PathBuf {
    match data_dir.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join("logs"),
        _ => PathBuf::from(DEFAULT_LOG_DIR),
    }
}

/// 初始化服务模式的 tracing 订阅者，返回实际写入的文件路径。
///
/// 过滤器与前台模式完全一致（`crate::log_filter`），只换输出端。ANSI 一律关闭：
/// 文件里存一堆转义序列既不便 `type`，也不便被日志采集读走。
pub fn init(config: &Config, log_level_from_cli: bool) -> anyhow::Result<PathBuf> {
    let dir = log_dir_for(&config.data_dir);
    let path = open_target(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("打开日志文件失败: {}", path.display()))?;

    let filter = crate::log_filter(config, log_level_from_cli)?;
    // `Mutex<File>` 而不是裸 `File`：tracing 的 fmt 层写一条事件可能分成多次
    // `write`，多个线程同时写会把两条日志绞在一起。`Mutex` 把一条事件的多次写
    // 串起来，代价是一次无竞争的加锁。
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .init();

    Ok(path)
}

/// 建目录、必要时轮转，返回该写的文件路径。
///
/// 目录建不出来（权限、盘符不存在）时回落到临时目录下的 `StrixMaid`：
/// 有日志可看远比「因为日志目录建不出来所以服务起不来」有用。回落发生时
/// 返回的路径与预期不同，调用方会把它打进第一条日志里，不至于让人找不到。
fn open_target(dir: &Path) -> anyhow::Result<PathBuf> {
    let dir = match std::fs::create_dir_all(dir) {
        Ok(()) => dir.to_path_buf(),
        Err(_) => {
            let fallback = std::env::temp_dir().join("StrixMaid");
            std::fs::create_dir_all(&fallback).with_context(|| {
                format!(
                    "日志目录 {} 与回落目录 {} 都建不出来",
                    dir.display(),
                    fallback.display()
                )
            })?;
            fallback
        }
    };

    let path = dir.join(LOG_FILE);
    rotate_if_large(&path);
    Ok(path)
}

/// 文件超限时改名成 `.1`。任何一步失败都只是「没轮转成」，不该挡住启动。
fn rotate_if_large(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX_BYTES {
        return;
    }
    let previous = path.with_extension(format!(
        "{}.1",
        path.extension().and_then(|e| e.to_str()).unwrap_or("log")
    ));
    let _ = std::fs::remove_file(&previous);
    let _ = std::fs::rename(path, &previous);
}

/// 供 `service install` 打印「日志去哪了」。
///
/// 配置读不出来时回落到 [`DEFAULT_LOG_DIR`] 并如实按默认值显示——这里宁可
/// 报一个「默认位置」，也不猜一个可能不存在的路径。
/// `config` 为 `None` 表示配置读不出来——由调用方判断，因为「配置从哪读」
/// 是宿主的知识（server 与 agent 的配置文件不是同一个）。
pub fn describe_target(config: Option<&Config>) -> String {
    let dir = match config {
        Some(c) => log_dir_for(&c.data_dir),
        None => PathBuf::from(DEFAULT_LOG_DIR),
    };
    dir.join(LOG_FILE).display().to_string()
}

/// 兜底写一行：日志系统本身还没起来（或起不来）时用。
///
/// 服务模式下若配置解析失败、或日志目录建不出来，`tracing::error!` 写进的是一个
/// 没有订阅者的黑洞，而 SCM 那边只会在事件日志里留下一句「服务已终止，
/// 服务特定错误码 1」——对排错毫无帮助。这个函数尽最大努力把原因落到磁盘上；
/// 两个候选位置都写不进去时就此作罢，已经没有别的地方可以声张了。
///
/// 时间戳用的是 Unix 秒数而不是本地时间：格式化日期需要一个日期库，而这条路径
/// 的全部价值就是「别引入新依赖也要留下线索」。系统时钟早于 1970 时如实写
/// 「时间未知」，不编一个 0。
pub fn last_resort(message: &str) {
    let stamp = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("unix {}", d.as_secs()),
        Err(_) => "时间未知".to_owned(),
    };
    let candidates = [
        PathBuf::from(DEFAULT_LOG_DIR),
        std::env::temp_dir().join("StrixMaid"),
    ];
    for dir in candidates {
        if std::fs::create_dir_all(&dir).is_err() {
            continue;
        }
        let path = dir.join(LOG_FILE);
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path)
            && writeln!(file, "[{stamp}] 服务无法启动: {message}").is_ok()
        {
            return;
        }
    }
}

/// 供测试确认文件真的能建起来。
#[cfg(test)]
fn probe_open(dir: &Path) -> anyhow::Result<std::fs::File> {
    let path = open_target(dir)?;
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 日志目录跟着数据目录走() {
        assert_eq!(
            log_dir_for(Path::new(r"C:\ProgramData\StrixMaid\data")),
            PathBuf::from(DEFAULT_LOG_DIR),
            "默认配置下应当正好落在约定的日志目录"
        );
        assert_eq!(
            log_dir_for(Path::new(r"D:\strixmaid\data")),
            PathBuf::from(r"D:\strixmaid\logs")
        );
    }

    #[test]
    fn 数据目录在盘根时回落到默认日志目录() {
        // `C:\` 的 parent 是 None，往盘根写日志不可接受。
        assert_eq!(
            log_dir_for(Path::new(r"C:\")),
            PathBuf::from(DEFAULT_LOG_DIR)
        );
    }

    #[test]
    fn 超过阈值才轮转() {
        let dir = std::env::temp_dir().join(format!("strixmaid-log-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("临时目录应当可建");
        let path = dir.join(LOG_FILE);

        std::fs::write(&path, b"short").expect("应当可写");
        rotate_if_large(&path);
        assert!(path.exists(), "没到阈值不该动它");
        assert!(
            !dir.join(format!("{LOG_FILE}.1")).exists(),
            "没到阈值不该产生上一代"
        );

        // 稀疏地撑到阈值以上，不真写 32 MiB。
        let mut f = OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        f.flush().unwrap();
        drop(f);
        rotate_if_large(&path);
        assert!(!path.exists(), "超限后当前文件应当已让位");
        assert!(
            dir.join(format!("{LOG_FILE}.1")).exists(),
            "超限后应当留下上一代"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 目录建不出来时回落到临时目录() {
        // 用一个几乎不可能存在、且普通用户建不出来的盘符触发回落。
        // 本机若恰好有这个盘符（罕见），跳过而不是误判。
        let bogus = Path::new(r"\\?\GLOBALROOT\strixmaid-does-not-exist\logs");
        if bogus.exists() {
            eprintln!("本机存在 {}，跳过回落用例", bogus.display());
            return;
        }
        let file = probe_open(bogus).expect("回落到临时目录后应当仍能开出日志文件");
        drop(file);
        let fallback = std::env::temp_dir().join("StrixMaid").join(LOG_FILE);
        assert!(fallback.exists(), "回落文件应当落在 {}", fallback.display());
    }
}
