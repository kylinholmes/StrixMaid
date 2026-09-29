//! 配置约束校验，集中报告全部字段错误。

use super::defaults::*;
use super::{Config, ConfigError, FieldError, Result};
use std::{net::SocketAddr, path::Path};

impl Config {
    // ---------------------------------------------------------------- 校验

    /// 校验全部取值，一次性返回所有问题。
    pub fn validate(&self) -> Result<()> {
        let mut errors = Vec::new();

        // --- 监听地址 ---
        let listen = self.listen.trim();
        if listen.is_empty() {
            errors.push(FieldError::new(
                "listen",
                "<空>",
                format!("不能为空；应形如 `{DEFAULT_LISTEN}`"),
            ));
        } else if listen.parse::<SocketAddr>().is_err() {
            errors.push(FieldError::new(
                "listen",
                &self.listen,
                "必须是可解析的 `IP:端口`，如 `127.0.0.1:9700`、`0.0.0.0:9700`、`[::1]:9700`；\
                 不支持主机名，也不能省略端口",
            ));
        }

        // --- 目录与二进制路径 ---
        check_non_empty_path(
            "data_dir",
            &self.data_dir,
            "SQLite 数据库所在目录",
            &mut errors,
        );
        check_non_empty_path(
            "run_dir",
            &self.run_dir,
            "helper socket 所在目录",
            &mut errors,
        );
        check_non_empty_path(
            "helper_path",
            &self.helper_path,
            "strixmaid-helper 二进制路径；不含 `/` 时按 PATH 查找",
            &mut errors,
        );

        // --- PAM 服务名 ---
        if self.pam_service.is_empty() {
            errors.push(FieldError::new(
                "pam_service",
                "<空>",
                format!("不能为空；对应 /etc/pam.d/<名字>，默认 `{DEFAULT_PAM_SERVICE}`"),
            ));
        } else if self.pam_service.contains('/')
            || self.pam_service.contains('\0')
            || self.pam_service == "."
            || self.pam_service == ".."
        {
            errors.push(FieldError::new(
                "pam_service",
                &self.pam_service,
                "必须是一个合法文件名（对应 /etc/pam.d/<名字>），不能包含 `/` 或 NUL，也不能是 `.` / `..`",
            ));
        }

        // --- 文件浏览 ---
        if self.files.allowed_roots.is_empty() {
            errors.push(FieldError::new(
                "files.allowed_roots",
                "<空>",
                "至少要有一个根路径（默认 [\"/\"]）；想隐藏文件面板不该用空列表表达",
            ));
        }
        for root in &self.files.allowed_roots {
            if !is_absolute_root(root) {
                errors.push(FieldError::new(
                    "files.allowed_roots",
                    root.display().to_string(),
                    if cfg!(windows) {
                        "每项都必须是绝对路径（如 C:\\Users），或用 \\ 表示「全部驱动器」"
                    } else {
                        "每项都必须是绝对路径"
                    },
                ));
            }
        }

        // --- 指标 ---
        check_range(
            "metrics.interval_secs",
            self.metrics.interval_secs,
            METRICS_INTERVAL_MIN_SECS,
            METRICS_INTERVAL_MAX_SECS,
            "采集间隔",
            &mut errors,
        );
        check_range(
            "metrics.ring_secs",
            self.metrics.ring_secs,
            METRICS_RING_MIN_SECS,
            METRICS_RING_MAX_SECS,
            "内存环形缓冲时长",
            &mut errors,
        );
        if self.metrics.interval_secs > 0 && self.metrics.ring_secs < self.metrics.interval_secs {
            errors.push(FieldError::new(
                "metrics.ring_secs",
                self.metrics.ring_secs,
                format!(
                    "必须不小于 metrics.interval_secs（当前 {} 秒），否则环形缓冲连一个采样点都放不下",
                    self.metrics.interval_secs
                ),
            ));
        }

        // --- 会话 ---
        check_range(
            "session.idle_timeout_secs",
            self.session.idle_timeout_secs,
            SESSION_IDLE_MIN_SECS,
            SESSION_IDLE_MAX_SECS,
            "会话空闲超时",
            &mut errors,
        );
        check_range(
            "session.elevated_idle_timeout_secs",
            self.session.elevated_idle_timeout_secs,
            SESSION_ELEVATED_MIN_SECS,
            SESSION_ELEVATED_MAX_SECS,
            "提权状态空闲超时",
            &mut errors,
        );
        // 组名不能是空串或首尾带空白。这类值最常见的来源是把 elevate_groups 当成
        // 逗号分隔的字符串写（`"sudo, wheel"`），那样只会得到一个永远匹配不上的
        // 「组名」，而提权会静默地对所有人关闭——必须在启动时就报出来。
        check_range(
            "audit.retention_days",
            u64::from(self.audit.retention_days),
            u64::from(AUDIT_RETENTION_MIN_DAYS),
            u64::from(AUDIT_RETENTION_MAX_DAYS),
            "审计保留期（天）",
            &mut errors,
        );

        // 只支持精确 IP。写成 CIDR 要当场报错而不是静默不匹配——后者会让
        // 部署者以为 X-Forwarded-For 已被采信，而审计里记的其实一直是反代的地址。
        for (i, proxy) in self.trusted_proxies.iter().enumerate() {
            if proxy.parse::<std::net::IpAddr>().is_err() {
                let hint = if proxy.contains('/') {
                    "看起来是 CIDR 网段，暂不支持；请逐个写出反代的 IP"
                } else {
                    "必须是精确的 IP 字面量，如 127.0.0.1 或 ::1"
                };
                errors.push(FieldError::new(
                    format!("trusted_proxies[{i}]"),
                    format!("{proxy:?}"),
                    hint,
                ));
            }
        }

        for (i, g) in self.session.elevate_groups.iter().enumerate() {
            if g.trim().is_empty() || g.trim() != g {
                errors.push(FieldError::new(
                    format!("session.elevate_groups[{i}]"),
                    format!("{g:?}"),
                    "组名不能为空或首尾带空白；本项是 TOML 数组，\
                     形如 [\"sudo\", \"wheel\"]，不是逗号分隔的字符串",
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigError::Invalid(errors))
        }
    }
}

// ===========================================================================
// 校验小工具
// ===========================================================================

/// 一个 `files.allowed_roots` 项是不是合法的「根」。
///
/// Unix 上就是 `Path::is_absolute`。Windows 上多认一种写法：**裸的 `\` 或 `/`**。
///
/// 那是默认值 `["/"]` 在 Windows 上的含义——「整个文件系统命名空间」，
/// 即全部驱动器。`Path::is_absolute` 在 Windows 上要求带盘符前缀，
/// 单独一个 `\` 只有 `has_root()` 为真；若照搬 Unix 的判断，跨平台的默认配置
/// 在 Windows 上会直接启动失败。语义落地见
/// [`crate::providers::fs::is_allowed`]。
pub fn is_absolute_root(path: &Path) -> bool {
    if path.is_absolute() {
        return true;
    }
    cfg!(windows) && path.has_root() && path.components().count() == 1
}

fn check_non_empty_path(field: &str, path: &Path, what: &str, errors: &mut Vec<FieldError>) {
    if path.as_os_str().is_empty() {
        errors.push(FieldError::new(field, "<空>", format!("不能为空；{what}")));
    }
}

fn check_range(
    field: &str,
    value: u64,
    min: u64,
    max: u64,
    what: &str,
    errors: &mut Vec<FieldError>,
) {
    if value < min || value > max {
        errors.push(FieldError::new(
            field,
            value,
            format!("{what}必须在 {min} – {max} 秒之间（含两端）"),
        ));
    }
}
