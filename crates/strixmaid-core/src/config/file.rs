//! Server 与 Agent 共用的精确路径 TOML 来源。

use super::Result;
use figment::{
    providers::{Format, Toml},
    value::Dict,
};
use std::path::{Path, PathBuf};

/// 精确路径的配置来源：不向父目录搜索，也不把不可读或非普通文件当作不存在。
#[derive(Debug, Clone)]
pub struct ConfigFile {
    path: PathBuf,
    required: bool,
}

impl ConfigFile {
    /// 显式指定的文件：缺失也应当报错，不允许用环境或命令行掩盖路径错误。
    pub fn required(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            required: true,
        }
    }

    /// 缺省路径：仅在文件不存在时跳过，目录、设备及读取错误仍报错。
    pub fn optional(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_owned(),
            required: false,
        }
    }
}

impl figment::Provider for ConfigFile {
    fn metadata(&self) -> figment::Metadata {
        Toml::file_exact(&self.path).metadata()
    }

    fn data(&self) -> Result<figment::value::Map<figment::Profile, Dict>, figment::Error> {
        match std::fs::metadata(&self.path) {
            Err(e) if !self.required && e.kind() == std::io::ErrorKind::NotFound => {
                // Windows 也会把中间组件为普通文件报告为 NotFound。
                // 向上找到首个存在的父路径，确认它是目录后才允许缺省。
                for parent in self.path.ancestors().skip(1) {
                    if parent.as_os_str().is_empty() {
                        break;
                    }
                    match std::fs::metadata(parent) {
                        Ok(meta) if meta.is_dir() => break,
                        Ok(_) => return Err("配置路径的中间组件必须是目录".into()),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                        Err(e) => return Err(e.to_string().into()),
                    }
                }
                tracing::debug!(path = %self.path.display(), "配置文件不存在，使用默认值 + 环境变量 + 命令行");
                Ok(Default::default())
            }
            Err(e) => Err(e.to_string().into()),
            Ok(meta) if !meta.is_file() => Err("配置路径必须指向普通文件".into()),
            Ok(_) => Toml::file_exact(&self.path).data(),
        }
    }
}
