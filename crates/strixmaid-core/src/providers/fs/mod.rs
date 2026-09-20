//! 文件浏览 provider（roadmap/04 §A）：只读的 `fs.list` / `fs.read`。
//!
//! # 权限模型
//!
//! 本 provider 在 **user worker**（uid = 登录用户）里运行，读目录、读文件的
//! 权限全部由文件系统裁决（design.md §1 原则 3）：无权限就是
//! `PermissionDenied`，这里不做任何自己的判断。`allowed_roots` **不是安全
//! 边界**——它挡不住有权限的用户用别的工具看别的路径——只是「文件面板该看
//! 哪里」的部署策略，随调用由主进程下发（`strixmaid_types::rpc::FsParams`）。
//!
//! # 路径规范化不跟随符号链接
//!
//! `.` 与 `..` 逐段解析，但**不做 realpath**：`/data/link` 里的 `link` 若是
//! 指向别处的符号链接，realpath 会把路径解析到 `allowed_roots` 之外，而用户
//! 看到、请求的字面路径明明在里面。校验对象是用户所见的字面路径；代价是
//! 符号链接可以指向 roots 之外——见上：真正的边界是文件权限。
//!
//! # 平台分界
//!
//! 流程（读目录、跳过失败条目、排序、大小上限、二进制判定、UTF-8 有损转换）
//! 两个平台共用，**只有「路径长什么样」与「一条目录项的元数据从哪来」分叉**：
//!
//! | 分叉点 | Unix | Windows |
//! |---|---|---|
//! | [`normalize`] | 以 `/` 开头，前缀一律拒绝 | 盘符 / UNC / 裸 `\`（= 全部驱动器），见 [`windows`] |
//! | [`is_allowed`] | `Path::starts_with` | 裸根匹配一切绝对路径；段比较不区分大小写 |
//! | `EntryMapper` | `st_mode` / `st_uid` / NSS | 文件属性合成 mode、SID 的 RID、`GetNamedSecurityInfoW` |
//! | [`kind_of`] | 七种文件类型 | 只有 `Dir` / `File` / `Symlink` / `Unknown` |
//! | `list_blocking` | —— | 列虚拟根 `\` 时改为枚举驱动器 |
//!
//! Windows 侧的取数与它对 `mode` / `uid` / `gid` 所做**近似**的全部说明，
//! 见 [`windows`] 的模块文档。

#[cfg(unix)]
use std::collections::HashMap;
use std::io::Read as _;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
#[cfg(unix)]
use std::time::{Duration, Instant};

use async_trait::async_trait;
use strixmaid_types::file::DirEntryInfo;
use strixmaid_types::file::{DirListing, FileContent, FileKind, FileSortKey};
use strixmaid_types::process::SortOrder;
use strixmaid_types::rpc::{FS_RAW_MAX_CHUNK, FsRawChunk};
use strixmaid_types::{ApiError, ApiResult};

use super::{Probe, Provider};

pub mod icon;

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
use windows::EntryMapper;

/// `fs.read` 的大小上限。超出直接报错，不截断。
///
/// 原为 5 MiB（roadmap/04 §A.3），但 worker RPC 的单帧上限是 1 MiB
/// （`ipc::MAX_FRAME_LEN`）：**大于约 1 MiB 的响应从来就过不了通道**，
/// 只会在帧层炸出一个 IPC 错误——那 5 MiB 是纸面数字，测试只测了
/// `read_blocking`、没走真通道，所以活到今天。收到帧安全的 640 KiB
/// （JSON 转义最坏 ~2 倍后仍 < 1 MiB），超限得到的是清晰的 400 而不是
/// 通道错误。大文件走 `fs.raw` 的分块（12 号方案 §4.6）。
pub const MAX_READ_BYTES: u64 = 640 * 1024;

/// 二进制判定的扫描窗口：前 8 KiB 含 NUL 即视为二进制。
const NUL_SCAN_BYTES: usize = 8 * 1024;

/// uid / gid → 名字的缓存有效期（roadmap/04 §A.3）。
#[cfg(unix)]
const NAME_CACHE_TTL: Duration = Duration::from_secs(60);

// ============================ 路径 ============================

/// 规范化路径：必须以 `/` 开头；`.` 丢弃、`..` 逐段上弹（到根则停留在根）。
/// 不访问文件系统，因此不解析符号链接（理由见模块文档）。
#[cfg(unix)]
pub fn normalize(path: &str) -> ApiResult<PathBuf> {
    if path.is_empty() {
        return Err(ApiError::invalid_request("路径不能为空"));
    }
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err(ApiError::invalid_request(format!(
            "路径必须是绝对路径：{path}"
        )));
    }
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(seg) => out.push(seg),
            Component::Prefix(_) => {
                return Err(ApiError::invalid_request("不支持带前缀的路径"));
            }
        }
    }
    Ok(out)
}

/// 规范化路径（Windows）。语义与实现见 [`windows::normalize`]：
/// 认盘符 / UNC / verbatim，外加裸的 `/` 或 `\`（= 全部驱动器这个虚拟根）；
/// `..` 弹到盘根就停，与 Unix 弹到 `/` 就停同理。
#[cfg(windows)]
pub fn normalize(path: &str) -> ApiResult<PathBuf> {
    windows::normalize(path)
}

/// 规范化后的路径是否位于任一 root 之内（含 root 本身）。
///
/// 用 `Path::starts_with` 做**按路径段**的前缀判断——字符串前缀会把
/// `/home2` 误判进 `/home`。
#[cfg(unix)]
pub fn is_allowed(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
}

/// 规范化后的路径是否位于任一 root 之内（含 root 本身）。见 [`windows::is_allowed`]：
/// 仍然按路径段比较，但裸根匹配任何绝对路径，且段比较不区分大小写
/// （`c:\users` 与 `C:\Users` 在 Windows 上是同一个目录）。
#[cfg(windows)]
pub fn is_allowed(path: &Path, roots: &[String]) -> bool {
    windows::is_allowed(path, roots)
}

/// 规范化 + roots 校验，两个方法共用的入口。
fn resolve(path: &str, roots: &[String]) -> ApiResult<PathBuf> {
    let p = normalize(path)?;
    if !is_allowed(&p, roots) {
        return Err(ApiError::permission_denied(format!(
            "路径 {} 不在允许浏览的范围内（files.allowed_roots）",
            p.display()
        )));
    }
    Ok(p)
}

// ============================ 名字缓存 ============================
//
// Windows 上没有这一层：名字来自 SID 而不是 uid，而 SID → 账户名的缓存已经在
// `platform::windows::token::account_by_sid` 里（键是完整 SID 串——RID 在域环境
// 下不唯一，拿它当缓存键会串台）。

#[cfg(unix)]
type NameCache = Mutex<HashMap<u32, (Instant, Option<String>)>>;

/// 查缓存，过期或缺失再做一次 NSS 查询。查不到（NSS 不可用、uid 无对应用户）
/// 时缓存 `None`，避免对着一个不存在的 uid 每个条目查一次。
#[cfg(unix)]
fn cached_name(
    cache: &NameCache,
    id: u32,
    lookup: impl FnOnce(u32) -> Option<String>,
) -> Option<String> {
    let now = Instant::now();
    if let Some((at, name)) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(&id)
        && now.duration_since(*at) < NAME_CACHE_TTL
    {
        return name.clone();
    }
    let name = lookup(id);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, (now, name.clone()));
    name
}

#[cfg(unix)]
fn user_name(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
}

#[cfg(unix)]
fn group_name(gid: u32) -> Option<String> {
    nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
        .ok()
        .flatten()
        .map(|g| g.name)
}

// ============================ 目录项映射 ============================

/// 把一条目录项映射成 DTO。**每个平台的取数差异都收在这里**。
///
/// 做成结构体而不是自由函数，是因为 Windows 侧需要跨条目的状态
/// （属主查询的预算，见 [`windows::EntryMapper`]）。Unix 侧不需要，
/// 它是个零大小类型，`map` 的内容与引入这个结构体之前逐字相同。
#[cfg(unix)]
struct EntryMapper;

#[cfg(unix)]
impl EntryMapper {
    fn new(_dir: &Path) -> Self {
        Self
    }

    fn map(
        &mut self,
        caches: &Caches,
        _path: &Path,
        name: String,
        meta: &std::fs::Metadata,
        kind: FileKind,
        target: Option<String>,
    ) -> DirEntryInfo {
        DirEntryInfo {
            name,
            kind,
            size_bytes: meta.size(),
            mode: meta.permissions().mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            user: cached_name(&caches.users, meta.uid(), user_name),
            group: cached_name(&caches.groups, meta.gid(), group_name),
            mtime_ts: meta.mtime(),
            target,
        }
    }
}

// ============================ provider ============================

/// 文件浏览 provider。`Clone` 共享同一份名字缓存。
#[derive(Clone, Default)]
pub struct FsProvider {
    caches: Arc<Caches>,
}

/// Windows 上是个空结构：名字缓存在 `account_by_sid` 里（见上）。
/// 保留这个类型是为了让 [`FsProvider`] 与 `list_blocking` 的签名跨平台一致。
#[derive(Default)]
struct Caches {
    #[cfg(unix)]
    users: NameCache,
    #[cfg(unix)]
    groups: NameCache,
}

impl FsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// `fs.list`：列目录（含排序与分页，`opts` 见 [`ListOptions`]）。
    pub async fn list(
        &self,
        path: &str,
        roots: &[String],
        opts: ListOptions,
    ) -> ApiResult<DirListing> {
        let dir = resolve(path, roots)?;
        let caches = Arc::clone(&self.caches);
        // 卡死的 NFS 挂载点上一次 lstat 就能挂住，整段放进阻塞线程池，
        // 不让它占住 worker 的 runtime 线程。
        tokio::task::spawn_blocking(move || list_blocking(&caches, &dir, &opts))
            .await
            .map_err(|e| ApiError::internal("fs.list 任务异常").with_detail(e.to_string()))?
    }

    /// `fs.read`：读文本文件。
    pub async fn read(&self, path: &str, roots: &[String]) -> ApiResult<FileContent> {
        let file = resolve(path, roots)?;
        tokio::task::spawn_blocking(move || read_blocking(&file))
            .await
            .map_err(|e| ApiError::internal("fs.read 任务异常").with_detail(e.to_string()))?
    }

    /// `fs.raw`：按块读原始字节（roadmap/12 §4.6）。
    pub async fn raw(
        &self,
        path: &str,
        roots: &[String],
        offset: u64,
        len: u32,
    ) -> ApiResult<FsRawChunk> {
        let file = resolve(path, roots)?;
        tokio::task::spawn_blocking(move || raw_blocking(&file, offset, len))
            .await
            .map_err(|e| ApiError::internal("fs.raw 任务异常").with_detail(e.to_string()))?
    }
}

/// [`FsProvider::list`] 的排序与分页选项。缺省即旧行为：全量、目录在前按名称。
#[derive(Debug, Clone, Copy)]
pub struct ListOptions {
    /// 最多返回多少条；`None` 不分页。
    pub limit: Option<u32>,
    /// 排序后跳过前多少条。
    pub offset: u32,
    /// 组内排序键（目录永远在前）。
    pub sort: FileSortKey,
    /// 排序方向。
    pub order: SortOrder,
}

/// **不 derive**：`SortOrder::default()` 是 `Desc`（进程页「先看最占资源的」），
/// 文件列表的缺省必须是名称**升序**——derive 会把这两个语境的默认值悄悄绑在一起。
impl Default for ListOptions {
    fn default() -> Self {
        ListOptions {
            limit: None,
            offset: 0,
            sort: FileSortKey::Name,
            order: SortOrder::Asc,
        }
    }
}

#[async_trait]
impl Provider for FsProvider {
    fn id(&self) -> &'static str {
        "fs"
    }

    /// 文件系统总是存在，探测恒为可用（roadmap/04 §A.3）。
    async fn probe(&self) -> Probe {
        Probe::Available
    }
}

fn list_blocking(caches: &Caches, dir: &Path, opts: &ListOptions) -> ApiResult<DirListing> {
    // Windows：裸 `\` 是「全部驱动器」这个虚拟根，它不是一个真目录
    // （read_dir 会落到**当前盘**的根上，那是另一个目录）。在这里截住，
    // 改为枚举驱动器，见 `windows::list_drives`。
    #[cfg(windows)]
    if windows::is_namespace_root(dir) {
        let d = windows::list_drives(dir);
        return Ok(finish_listing(d.path, d.entries, d.skipped, opts));
    }

    let rd = std::fs::read_dir(dir).map_err(|e| io_err(dir, &e))?;
    let mut mapper = EntryMapper::new(dir);
    let mut entries = Vec::new();
    let mut skipped = 0u32;
    for item in rd {
        let Ok(item) = item else {
            skipped += 1;
            continue;
        };
        // read_dir 之后条目随时可能消失（/proc 尤甚），lstat 失败跳过并计数，
        // 不让一个条目毁掉整个列表。
        let path = item.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            skipped += 1;
            continue;
        };
        let kind = kind_of(&meta.file_type());
        let target = (kind == FileKind::Symlink)
            .then(|| std::fs::read_link(&path).ok())
            .flatten()
            .map(|t| t.to_string_lossy().into_owned());
        entries.push(mapper.map(
            caches,
            &path,
            item.file_name().to_string_lossy().into_owned(),
            &meta,
            kind,
            target,
        ));
    }
    Ok(finish_listing(
        dir.to_string_lossy().into_owned(),
        entries,
        skipped,
        opts,
    ))
}

/// 排序 + 分页收尾。**目录永远在前**（roadmap/04 §A.3），排序键与方向只
/// 决定目录组内与文件组内的顺序；`total` 是分页前的条目数。
fn finish_listing(
    path: String,
    mut entries: Vec<DirEntryInfo>,
    skipped: u32,
    opts: &ListOptions,
) -> DirListing {
    let key_cmp = |a: &DirEntryInfo, b: &DirEntryInfo| match opts.sort {
        FileSortKey::Name => a.name.cmp(&b.name),
        // 大小/时间相同的条目退回名称序：排序要稳定可预期，刷新不跳动。
        FileSortKey::Size => a.size_bytes.cmp(&b.size_bytes).then_with(|| a.name.cmp(&b.name)),
        FileSortKey::Mtime => a.mtime_ts.cmp(&b.mtime_ts).then_with(|| a.name.cmp(&b.name)),
    };
    entries.sort_by(|a, b| {
        let (da, db) = (a.kind == FileKind::Dir, b.kind == FileKind::Dir);
        db.cmp(&da).then_with(|| match opts.order {
            SortOrder::Asc => key_cmp(a, b),
            SortOrder::Desc => key_cmp(b, a),
        })
    });

    let total = entries.len() as u32;
    let paged = match opts.limit {
        None => entries,
        Some(limit) => entries
            .into_iter()
            .skip(opts.offset as usize)
            .take(limit as usize)
            .collect(),
    };
    DirListing {
        path,
        entries: paged,
        skipped,
        total: Some(total),
    }
}

/// [`FsProvider::raw`] 的阻塞部分：定位 + 读一块。
fn raw_blocking(file: &Path, offset: u64, len: u32) -> ApiResult<FsRawChunk> {
    use std::io::{Read as _, Seek as _};

    // 跟随符号链接：读的就是链接指向的内容（与 `fs.read` 同一规则）。
    let meta = std::fs::metadata(file).map_err(|e| io_err(file, &e))?;
    if meta.is_dir() {
        return Err(ApiError::invalid_request(format!(
            "{} 是目录，不是文件",
            file.display()
        )));
    }
    let len = len.min(FS_RAW_MAX_CHUNK) as usize;
    let mut fh = std::fs::File::open(file).map_err(|e| io_err(file, &e))?;
    fh.seek(std::io::SeekFrom::Start(offset))
        .map_err(|e| io_err(file, &e))?;
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        match fh.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_err(file, &e)),
        }
    }
    buf.truncate(got);
    Ok(FsRawChunk {
        total_bytes: meta.len(),
        mime: mime_of(file).to_owned(),
        data_hex: hex::encode(buf),
    })
}

/// 按扩展名猜 MIME。只覆盖浏览器要「按图渲染」的那几类；其余一律
/// octet-stream——错报成 text/* 会让浏览器把二进制当页面打开。
fn mime_of(file: &Path) -> &'static str {
    let ext = file
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

fn read_blocking(file: &Path) -> ApiResult<FileContent> {
    // 这里的 metadata **跟随**符号链接：读的就是链接指向的内容。
    let meta = std::fs::metadata(file).map_err(|e| io_err(file, &e))?;
    if meta.is_dir() {
        return Err(ApiError::invalid_request(format!(
            "{} 是目录，不是文件",
            file.display()
        )));
    }
    // FIFO / 设备文件打开或读取会阻塞、或读出无意义的字节流。
    // Windows 上没有 FIFO / 设备节点混在普通文件系统里（命名管道在 `\\.\pipe\`、
    // 设备在 `\\.\` 下，两者都被 `normalize` 挡在门外），这一条在那边等价于
    // 「不是普通文件就拒」——正是同一行代码的字面含义。
    if !meta.file_type().is_file() {
        return Err(ApiError::invalid_request(format!(
            "{} 不是普通文件",
            file.display()
        )));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(oversize(file, meta.len()));
    }

    let f = std::fs::File::open(file).map_err(|e| io_err(file, &e))?;
    let mut buf = Vec::new();
    // 再兜一道：procfs 文件 stat 报 0，普通文件也可能在 stat 之后变大。
    f.take(MAX_READ_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| io_err(file, &e))?;
    if buf.len() as u64 > MAX_READ_BYTES {
        return Err(oversize(file, buf.len() as u64));
    }
    if buf.iter().take(NUL_SCAN_BYTES).any(|b| *b == 0) {
        return Err(ApiError::invalid_request(format!(
            "{} 是二进制文件，不支持在线查看",
            file.display()
        )));
    }

    let size_bytes = buf.len() as u64;
    let (content, lossy) = match String::from_utf8(buf) {
        Ok(s) => (s, false),
        Err(e) => (String::from_utf8_lossy(e.as_bytes()).into_owned(), true),
    };
    Ok(FileContent {
        path: file.to_string_lossy().into_owned(),
        size_bytes,
        content,
        truncated: false,
        lossy,
    })
}

/// 超限错误。detail 里给出上限与实际大小（`ErrorCode` 没有 413 对应项，不新增）。
fn oversize(file: &Path, actual: u64) -> ApiError {
    ApiError::invalid_request(format!("{} 超出在线查看的大小上限", file.display()))
        .with_detail(format!("上限 {MAX_READ_BYTES} 字节，实际 {actual} 字节"))
}

/// 文件类型映射（Windows）。见 [`windows::kind_of`]：只可能是
/// `Dir` / `File` / `Symlink` / `Unknown`，junction 按 `Symlink` 报。
#[cfg(windows)]
fn kind_of(ft: &std::fs::FileType) -> FileKind {
    windows::kind_of(ft)
}

#[cfg(unix)]
fn kind_of(ft: &std::fs::FileType) -> FileKind {
    if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_file() {
        FileKind::File
    } else if ft.is_block_device() {
        FileKind::BlockDevice
    } else if ft.is_char_device() {
        FileKind::CharDevice
    } else if ft.is_fifo() {
        FileKind::Fifo
    } else if ft.is_socket() {
        FileKind::Socket
    } else {
        FileKind::Unknown
    }
}

/// IO 错误 → API 错误。「找不到」与「无权限」是用户可理解的结果，其余按内部错误报。
fn io_err(path: &Path, e: &std::io::Error) -> ApiError {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => ApiError::not_found(format!("{} 不存在", path.display())),
        ErrorKind::PermissionDenied => {
            ApiError::permission_denied(format!("没有权限访问 {}", path.display()))
        }
        ErrorKind::NotADirectory => {
            ApiError::invalid_request(format!("{} 不是目录", path.display()))
        }
        _ => ApiError::internal(format!("访问 {} 失败", path.display()))
            .with_detail(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use strixmaid_types::ErrorCode;

    use super::*;

    fn roots(rs: &[&str]) -> Vec<String> {
        rs.iter().map(|s| s.to_string()).collect()
    }

    /// 临时目录守卫（仓库惯例：不为测试引 tempfile，`temp_dir` + pid + Drop 清理）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "strixmaid-fs-test-{}-{name}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 造一个临时目录：两个子目录 + 三个大小可区分的文件。
    fn sample_dir(name: &str) -> TempDir {
        let td = TempDir::new(name);
        std::fs::create_dir(td.path().join("zdir")).unwrap();
        std::fs::create_dir(td.path().join("adir")).unwrap();
        std::fs::write(td.path().join("big.bin"), vec![0u8; 300]).unwrap();
        std::fs::write(td.path().join("mid.bin"), vec![0u8; 200]).unwrap();
        std::fs::write(td.path().join("small.bin"), vec![0u8; 100]).unwrap();
        td
    }

    fn names(l: &DirListing) -> Vec<&str> {
        l.entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[tokio::test]
    async fn 排序_目录永远在前_键只管组内() {
        let td = sample_dir("sort");
        let p = td.path().to_string_lossy().into_owned();
        let f = FsProvider::new();

        // 按大小降序：目录仍在最前（组内按名称），文件从大到小。
        let l = f
            .list(
                &p,
                &roots(&[&p]),
                ListOptions {
                    sort: FileSortKey::Size,
                    order: SortOrder::Desc,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            names(&l),
            vec!["zdir", "adir", "big.bin", "mid.bin", "small.bin"],
            "目录组在前；Desc 下目录组内名称也反序"
        );
        assert_eq!(l.total, Some(5));
    }

    #[tokio::test]
    async fn 分页_在排序之后生效且_total_是全量() {
        let td = sample_dir("page");
        let p = td.path().to_string_lossy().into_owned();
        let f = FsProvider::new();
        let l = f
            .list(
                &p,
                &roots(&[&p]),
                ListOptions {
                    limit: Some(2),
                    offset: 1,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        // 默认名称升序全量为 adir zdir big mid small → 跳 1 取 2。
        assert_eq!(names(&l), vec!["zdir", "big.bin"]);
        assert_eq!(l.total, Some(5), "total 必须是分页前的全量");
    }

    #[tokio::test]
    async fn raw_分块取回且越界返回空块() {
        let td = TempDir::new("raw");
        let file = td.path().join("blob.png");
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &data).unwrap();
        let p = file.to_string_lossy().into_owned();
        let root = td.path().to_string_lossy().into_owned();
        let f = FsProvider::new();

        // 两块拼回原文件。
        let c1 = f.raw(&p, &roots(&[&root]), 0, 600).await.unwrap();
        assert_eq!(c1.total_bytes, 1000);
        assert_eq!(c1.mime, "image/png", "按扩展名猜 MIME");
        let b1 = hex::decode(&c1.data_hex).unwrap();
        assert_eq!(b1.len(), 600);
        let c2 = f.raw(&p, &roots(&[&root]), 600, 600).await.unwrap();
        let b2 = hex::decode(&c2.data_hex).unwrap();
        assert_eq!(b2.len(), 400, "尾块按剩余长度截断");
        assert_eq!([b1, b2].concat(), data);

        // 越界偏移：空块而不是错误——调用方以 offset >= total 判终。
        let c3 = f.raw(&p, &roots(&[&root]), 2000, 600).await.unwrap();
        assert!(c3.data_hex.is_empty());

        // 目录不是文件。
        let err = f.raw(&root, &roots(&[&root]), 0, 100).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn raw_单块长度被夹到上限() {
        let td = TempDir::new("clamp");
        let file = td.path().join("big.bin");
        std::fs::write(&file, vec![7u8; (FS_RAW_MAX_CHUNK + 1024) as usize]).unwrap();
        let p = file.to_string_lossy().into_owned();
        let root = td.path().to_string_lossy().into_owned();
        let f = FsProvider::new();
        // 请求超过上限的块长：夹到 FS_RAW_MAX_CHUNK——单帧 1 MiB 的约束在
        // worker 侧守住，不信任主进程恰好传对。
        let c = f
            .raw(&p, &roots(&[&root]), 0, FS_RAW_MAX_CHUNK * 4)
            .await
            .unwrap();
        assert_eq!(hex::decode(&c.data_hex).unwrap().len(), FS_RAW_MAX_CHUNK as usize);
    }

    #[cfg(unix)]
    #[test]
    fn 规范化() {
        let n = |p: &str| normalize(p).unwrap().to_string_lossy().into_owned();
        assert_eq!(n("/a/../b"), "/b");
        assert_eq!(n("/a/./b"), "/a/b");
        assert_eq!(n("/"), "/");
        assert_eq!(n("/data/../.."), "/");
        assert_eq!(n("/a//b/"), "/a/b");
        assert_eq!(normalize("a/b").unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(normalize("").unwrap_err().code, ErrorCode::InvalidRequest);
    }

    /// Unix 版 `规范化` 的等价物：盘符、正反斜杠、裸根、`..` 弹到盘根就停。
    #[cfg(windows)]
    #[test]
    fn 规范化_windows() {
        let n = |p: &str| normalize(p).unwrap().to_string_lossy().into_owned();
        assert_eq!(n(r"C:\a\..\b"), r"C:\b");
        assert_eq!(n(r"C:\a\.\b"), r"C:\a\b");
        assert_eq!(n("C:/a/b"), r"C:\a\b");
        assert_eq!(n(r"C:\a\\b\"), r"C:\a\b");
        assert_eq!(n(r"C:\"), r"C:\");
        // 盘符归一成大写：同一个目录不该随请求的写法出现两种字面。
        assert_eq!(n(r"c:\Users"), r"C:\Users");
        // `..` 弹到盘根就停，弹不出 C:\——与 Unix 弹到 / 就停同理。
        assert_eq!(n(r"C:\data\..\.."), r"C:\");
        assert_eq!(n(r"C:\..\..\Windows"), r"C:\Windows");
        // 裸 `/` 与 `\` 都是「全部驱动器」这个虚拟根，归一成 `\`。
        assert_eq!(n("/"), "\\");
        assert_eq!(n("\\"), "\\");
        // UNC 保留主机与共享名。
        assert_eq!(n(r"\\srv\share\a\..\b"), r"\\srv\share\b");
        assert_eq!(n(r"\\srv\share\..\.."), r"\\srv\share\");

        // 相对路径、空串、设备命名空间一律拒绝。
        assert_eq!(normalize(r"a\b").unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(normalize("C:a").unwrap_err().code, ErrorCode::InvalidRequest);
        assert_eq!(normalize("").unwrap_err().code, ErrorCode::InvalidRequest);
        let err = normalize(r"\\.\PhysicalDrive0").unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("设备"), "{}", err.message);
    }

    #[cfg(unix)]
    #[test]
    fn roots_校验按路径段() {
        let rs = roots(&["/home", "/var/log"]);
        assert!(!is_allowed(Path::new("/etc"), &rs));
        assert!(is_allowed(Path::new("/home/x"), &rs));
        assert!(is_allowed(Path::new("/home"), &rs), "root 本身要通过");
        assert!(is_allowed(Path::new("/var/log/syslog"), &rs));
        assert!(!is_allowed(Path::new("/home2/x"), &rs), "字符串前缀不算");
        assert!(!is_allowed(Path::new("/var"), &rs));

        // `..` 先规范化再校验：/home/../etc 实际是 /etc，必须拒绝。
        let err = resolve("/home/../etc", &rs).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        // 空列表一律拒绝。
        assert_eq!(
            resolve("/", &[]).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }

    /// Unix 版 `roots_校验按路径段` 的等价物：段边界、盘符与段的大小写、
    /// 裸根、以及「`..` 必须先规范化再校验」这条安全性质。
    #[cfg(windows)]
    #[test]
    fn roots_校验按路径段_windows() {
        let rs = roots(&[r"C:\Users", r"D:\var\log"]);
        assert!(!is_allowed(Path::new(r"C:\Windows"), &rs));
        assert!(is_allowed(Path::new(r"C:\Users\x"), &rs));
        assert!(is_allowed(Path::new(r"C:\Users"), &rs), "root 本身要通过");
        assert!(is_allowed(Path::new(r"D:\var\log\sys.log"), &rs));
        assert!(!is_allowed(Path::new(r"C:\Users2\x"), &rs), "字符串前缀不算");
        assert!(!is_allowed(Path::new(r"D:\var"), &rs));
        // 盘符不同就是不同的树。
        assert!(!is_allowed(Path::new(r"E:\Users\x"), &rs));

        // 大小写：Windows 的文件系统默认不区分，配置与请求的写法不该打架。
        assert!(is_allowed(Path::new(r"c:\users\x"), &rs), "盘符大小写不敏感");
        assert!(is_allowed(Path::new(r"C:\USERS\x"), &rs), "段大小写不敏感");
        assert!(is_allowed(
            Path::new(r"C:\Users"),
            &roots(&[r"c:\USERS"])
        ));

        // 裸根 = 全部驱动器，匹配任何绝对路径，也匹配虚拟根自己。
        let all = roots(&["/"]);
        assert!(is_allowed(Path::new(r"C:\Windows"), &all));
        assert!(is_allowed(Path::new(r"\\srv\share\x"), &all));
        assert!(is_allowed(Path::new("\\"), &all));
        assert!(is_allowed(Path::new(r"Z:\"), &roots(&["\\"])));

        // `..` 先规范化再校验：C:\Users\..\Windows 实际是 C:\Windows，必须拒绝。
        let err = resolve(r"C:\Users\..\Windows", &rs).unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied);
        // 弹到盘根也一样：C:\Users\..\.. 是 C:\，不在 C:\Users 之内。
        assert_eq!(
            resolve(r"C:\Users\..\..", &rs).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        // 空列表一律拒绝。
        assert_eq!(
            resolve("/", &[]).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn 本机_list_proc_self_不炸() {
        // procfs 是 Linux 专有的。按 roadmap/README §7，依赖真实系统的用例用运行期
        // 探测跳过而非 #[ignore]，这样在有 procfs 的机器上它永远是跑着的。
        //
        // Windows 上这条探测恒为假（`/proc/self` 会被当成当前盘的
        // `C:\proc\self`，不存在），用例干净地跳过；就算真有那个目录，
        // `normalize` 也会先以「不是绝对路径」拒绝它。
        if !Path::new("/proc/self").exists() {
            eprintln!("本机无 procfs，跳过 /proc/self 用例");
            return;
        }
        let fs = FsProvider::new();
        let listing = fs.list("/proc/self", &roots(&["/"]), ListOptions::default()).await.unwrap();
        assert!(!listing.entries.is_empty());
        // /proc/self 下必有 status 这个普通文件与 fd 这个目录。
        assert!(listing.entries.iter().any(|e| e.name == "status"));
    }

    /// 排序规则（目录在前、组内按名）不依赖任何系统路径，单独用临时目录测，
    /// 这样它在无 procfs 的平台上也照跑。
    #[tokio::test(flavor = "multi_thread")]
    async fn list_目录在前组内按名排序() {
        let dir = std::env::temp_dir().join(format!("strixmaid-fs-sort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("z_dir")).unwrap();
        std::fs::create_dir_all(dir.join("a_dir")).unwrap();
        std::fs::write(dir.join("b_file"), b"x").unwrap();
        std::fs::write(dir.join("a_file"), b"x").unwrap();

        let fs = FsProvider::new();
        let listing = fs
            .list(&dir.to_string_lossy(), &roots(&["/"]), ListOptions::default())
            .await
            .unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a_dir", "z_dir", "a_file", "b_file"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 本机_读文本_二进制_目录_不存在() {
        let fs = FsProvider::new();
        let all = roots(&["/"]);

        // 用 /etc/passwd 而不是 /etc/hostname：后者是 Linux 惯例，macOS 上不存在。
        let c = fs.read("/etc/passwd", &all).await.unwrap();
        assert!(!c.content.is_empty());
        assert!(!c.lossy);
        assert!(!c.truncated);
        assert_eq!(c.size_bytes as usize, c.content.len());

        // stat 大小为 0 的 procfs 文件也要能读（procfs 只有 Linux 有）。
        if Path::new("/proc/self/status").exists() {
            let c = fs.read("/proc/self/status", &all).await.unwrap();
            assert!(c.content.contains("Pid"));
        }

        let err = fs.read("/bin/ls", &all).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest, "{err:?}");
        assert!(err.message.contains("二进制"), "{}", err.message);

        let err = fs.read("/etc", &all).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);

        let err = fs.read("/no/such/strixmaid-file", &all).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);

        let err = fs.list("/etc/passwd", &all, ListOptions::default()).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest, "对文件 list 应报不是目录");
    }

    /// Unix 版的等价物。取的三条路径是 Windows 上必然存在的：
    /// `drivers\etc\hosts`（文本）、`cmd.exe`（二进制）、`C:\Windows`（目录）。
    /// 即便如此也照旧做运行期探测——精简安装 / 非 `C:` 系统盘都可能让假设落空。
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 本机_读文本_二进制_目录_不存在_windows() {
        let fs = FsProvider::new();
        let all = roots(&["/"]);
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let hosts = format!(r"{sysroot}\System32\drivers\etc\hosts");
        let cmd = format!(r"{sysroot}\System32\cmd.exe");

        if Path::new(&hosts).is_file() {
            let c = fs.read(&hosts, &all).await.unwrap();
            assert!(!c.content.is_empty());
            assert!(!c.truncated);
            assert_eq!(c.size_bytes as usize, c.content.len());
        } else {
            eprintln!("本机没有 {hosts}，跳过文本用例");
        }

        if Path::new(&cmd).is_file() {
            let err = fs.read(&cmd, &all).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRequest, "{err:?}");
            assert!(err.message.contains("二进制"), "{}", err.message);
        } else {
            eprintln!("本机没有 {cmd}，跳过二进制用例");
        }

        let err = fs.read(&sysroot, &all).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("目录"), "{}", err.message);

        let err = fs
            .read(r"C:\no\such\strixmaid-file", &all)
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);

        if Path::new(&hosts).is_file() {
            let err = fs.list(&hosts, &all, ListOptions::default()).await.unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRequest, "对文件 list 应报不是目录：{err:?}");
        }
    }

    /// Windows 上 `fs.list` 到虚拟根 `\` 要列出驱动器（资源管理器的「此电脑」）。
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 本机_列根得到驱动器() {
        let fs = FsProvider::new();
        for req in ["/", "\\"] {
            let listing = fs.list(req, &roots(&["/"]), ListOptions::default()).await.unwrap();
            assert_eq!(listing.path, "\\", "虚拟根的规范形式是 `\\`");
            assert_eq!(listing.skipped, 0);
            assert!(
                !listing.entries.is_empty(),
                "总该有至少一个驱动器：{listing:?}"
            );
            for e in &listing.entries {
                assert_eq!(e.kind, FileKind::Dir);
                assert_eq!(e.mode, 0o555);
                assert!(e.user.is_none() && e.group.is_none(), "卷没有属主概念");
                // name 本身就是可直接请求的绝对路径，前端不必与父路径拼接。
                assert_eq!(normalize(&e.name).unwrap().to_string_lossy(), e.name);
            }
            eprintln!(
                "本机驱动器（请求 {req:?}）：{:?}",
                listing.entries.iter().map(|e| &e.name).collect::<Vec<_>>()
            );
        }
    }

    /// 属主查询是列目录里最贵的一步，拿本机最大的系统目录量一次，
    /// 数据用来校准 `windows::MAX_OWNER_QUERIES`。
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 本机_大目录列举耗时() {
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let dir = format!(r"{sysroot}\System32");
        if !Path::new(&dir).is_dir() {
            eprintln!("本机没有 {dir}，跳过大目录用例");
            return;
        }
        let fs = FsProvider::new();
        let t0 = std::time::Instant::now();
        let listing = fs.list(&dir, &roots(&["/"]), ListOptions::default()).await.unwrap();
        let cost = t0.elapsed();
        let named = listing.entries.iter().filter(|e| e.user.is_some()).count();
        eprintln!(
            "列 {dir}：{} 条（跳过 {}），{} 条查到属主，耗时 {cost:?}",
            listing.entries.len(),
            listing.skipped,
            named
        );
        assert!(!listing.entries.is_empty());
        // 属主至少要有一部分查得到，否则说明安全描述符这条路整个断了。
        assert!(named > 0, "一条属主都没查到，安全描述符查询可能失效了");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn 超过大小上限被拒绝() {
        let dir = std::env::temp_dir().join(format!("strixmaid-fs-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.txt");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(6 * 1024 * 1024).unwrap();

        let fs = FsProvider::new();
        let err = fs
            .read(&big.to_string_lossy(), &roots(&["/"]))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        let detail = err.detail.as_deref().unwrap_or("");
        assert!(
            detail.contains(&MAX_READ_BYTES.to_string()),
            "detail 要说明上限：{detail}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 符号链接原样报告不解引用() {
        let dir = std::env::temp_dir().join(format!(
            "strixmaid-fs-link-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join("ln")).unwrap();

        let fs = FsProvider::new();
        let listing = fs
            .list(&dir.to_string_lossy(), &roots(&["/"]), ListOptions::default())
            .await
            .unwrap();
        let ln = listing.entries.iter().find(|e| e.name == "ln").unwrap();
        assert_eq!(ln.kind, FileKind::Symlink);
        assert_eq!(ln.target.as_deref(), Some("/etc/hostname"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix 版的等价物。Windows 上建符号链接需要 `SeCreateSymbolicLinkPrivilege`
    /// （管理员）或开了开发者模式，普通用户跑测试时建不出来——
    /// 因此用**运行期探测**跳过，而不是让用例红着。
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 符号链接原样报告不解引用_windows() {
        let dir = std::env::temp_dir().join(format!(
            "strixmaid-fs-link-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let target = r"C:\Windows\System32\drivers\etc\hosts";
        if let Err(e) = std::os::windows::fs::symlink_file(target, dir.join("ln")) {
            eprintln!(
                "本机建不了符号链接（需要管理员或开发者模式）：{e}；跳过该用例"
            );
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        let fs = FsProvider::new();
        let listing = fs
            .list(&dir.to_string_lossy(), &roots(&["/"]), ListOptions::default())
            .await
            .unwrap();
        let ln = listing.entries.iter().find(|e| e.name == "ln").unwrap();
        assert_eq!(ln.kind, FileKind::Symlink);
        // 原样报告，不解引用。
        assert_eq!(ln.target.as_deref(), Some(target));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 无权限的文件报_permission_denied() {
        if nix::unistd::getuid().is_root() {
            eprintln!("以 root 运行，跳过无权限用例");
            return;
        }
        // 不用 /etc/shadow：那是 Linux 专有路径，macOS 上不存在，会得到 NotFound
        // 而不是 PermissionDenied——断言看着是过了权限判断，其实测的是另一条分支。
        // 自己造一个 0o000 的文件，两个平台语义一致。
        let dir = std::env::temp_dir().join(format!("strixmaid-fs-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("secret");
        std::fs::write(&secret, b"secret").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();

        let fs = FsProvider::new();
        let err = fs
            .read(&secret.to_string_lossy(), &roots(&["/"]))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied, "{err:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Unix 版的等价物。Windows 上没有 `0o000` 这种表达方式（权限是 ACL），
    /// 自己造一个不可读文件要调 `icacls` 或一串 ACL API——对一条断言而言太重。
    /// 改用系统上必然存在、且**普通用户必然读不到**的 `System32\config\SAM`；
    /// 以管理员身份跑测试时它是读得到的，所以照旧做运行期探测再跳过。
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread")]
    async fn 无权限的文件报_permission_denied_windows() {
        let sysroot = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
        let sam = format!(r"{sysroot}\System32\config\SAM");
        // 探测要直接去开，不能先 `Path::exists()`：`System32\config` 这个目录
        // 本身就不让普通用户 stat，`exists()` 会把「没权限」也报成 false，
        // 于是用例永远跳过、永远测不到想测的那条分支。
        //
        // 以管理员跑测试时 SAM 是打得开的（或报「文件被占用」），那时同样跳过。
        match std::fs::File::open(&sam) {
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
            other => {
                eprintln!("本机对 {sam} 的访问结果是 {other:?}，不是无权限，跳过该用例");
                return;
            }
        }

        let fs = FsProvider::new();
        let err = fs.read(&sam, &roots(&["/"])).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::PermissionDenied, "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn 属主名带缓存() {
        let caches = Caches::default();
        let mut calls = 0;
        let name = cached_name(&caches.users, 0, |_| {
            calls += 1;
            Some("root".into())
        });
        assert_eq!(name.as_deref(), Some("root"));
        // 命中缓存时不再查。
        let name = cached_name(&caches.users, 0, |_| {
            calls += 1;
            None
        });
        assert_eq!(name.as_deref(), Some("root"));
        assert_eq!(calls, 1);
    }
}
