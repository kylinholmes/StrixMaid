//! 文件浏览（`docs/design.md` §9.1「文件」组）。
//!
//! **P0 仅留壳**，后续专门打磨（§13 步骤 26）。这里只定义两个只读端点所需的最小类型，
//! 不做写入、上传、权限编辑。
//!
//! 所有文件操作都在 **worker 进程**（uid = 登录用户）里执行，
//! 由文件系统权限裁决可见性（`docs/design.md` §1 原则 3）；
//! 无权限返回 [`crate::ErrorCode::PermissionDenied`]。

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

/// 文件类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// 普通文件。
    File,
    /// 目录。
    Dir,
    /// 符号链接（`target` 给出链接目标）。
    Symlink,
    /// 块设备。
    BlockDevice,
    /// 字符设备。
    CharDevice,
    /// FIFO。
    Fifo,
    /// Unix socket。
    Socket,
    /// 无法归类。
    Unknown,
}

/// `GET /api/v1/files` 与 `GET /api/v1/files/content` 的查询参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct FilePathQuery {
    /// 绝对路径。必须以 `/` 开头且**不含 `..`**——服务端做规范化后校验，
    /// 不合法返回 [`crate::ErrorCode::InvalidRequest`]。
    #[param(example = "/etc")]
    pub path: String,
}

/// 目录项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DirEntryInfo {
    /// 条目名，不含路径。
    #[schema(example = "hosts")]
    pub name: String,
    /// 类型。
    pub kind: FileKind,
    /// 大小，字节。目录为其自身的 inode 大小，无实际意义。
    #[schema(example = 221_u64)]
    pub size_bytes: u64,
    /// 权限位，八进制低 12 位（如 `0o644` = 420）。
    #[schema(example = 420)]
    pub mode: u32,
    /// 属主 uid。
    pub uid: u32,
    /// 属组 gid。
    pub gid: u32,
    /// 属主用户名。NSS 不可用时为 `None`（见 `docs/design.md` §10）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// 属组名。同上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// 最后修改时刻。
    pub mtime_ts: i64,
    /// 符号链接的目标；`kind != symlink` 时为 `None`。不解引用，原样返回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// `GET /api/v1/files` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DirListing {
    /// 被列出的目录的规范化绝对路径。
    #[schema(example = "/etc")]
    pub path: String,
    /// 目录项。排序由前端决定，服务端不保证顺序。
    #[serde(default)]
    pub entries: Vec<DirEntryInfo>,
}

/// `GET /api/v1/files/content` 的响应体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FileContent {
    /// 规范化绝对路径。
    #[schema(example = "/etc/hosts")]
    pub path: String,
    /// 文件大小，字节。
    pub size_bytes: u64,
    /// 文件内容。
    ///
    /// 只支持 UTF-8 文本；二进制文件返回 [`crate::ErrorCode::InvalidRequest`]
    /// （P0 不做下载与十六进制视图）。
    pub content: String,
    /// 内容是否被截断。超过服务端上限（建议 1 MiB）时为 `true`，
    /// 此时 `content` 只是文件开头的一部分。
    pub truncated: bool,
}
