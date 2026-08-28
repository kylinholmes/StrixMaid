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

use std::collections::HashMap;
use std::io::Read as _;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use strixmaid_types::file::{DirEntryInfo, DirListing, FileContent, FileKind};
use strixmaid_types::{ApiError, ApiResult};

use super::{Probe, Provider};

/// `fs.read` 的大小上限（roadmap/04 §A.3）。超出直接报错，不截断。
pub const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;

/// 二进制判定的扫描窗口：前 8 KiB 含 NUL 即视为二进制。
const NUL_SCAN_BYTES: usize = 8 * 1024;

/// uid / gid → 名字的缓存有效期（roadmap/04 §A.3）。
const NAME_CACHE_TTL: Duration = Duration::from_secs(60);

// ============================ 路径 ============================

/// 规范化路径：必须以 `/` 开头；`.` 丢弃、`..` 逐段上弹（到根则停留在根）。
/// 不访问文件系统，因此不解析符号链接（理由见模块文档）。
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

/// 规范化后的路径是否位于任一 root 之内（含 root 本身）。
///
/// 用 `Path::starts_with` 做**按路径段**的前缀判断——字符串前缀会把
/// `/home2` 误判进 `/home`。
pub fn is_allowed(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|r| path.starts_with(r))
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

type NameCache = Mutex<HashMap<u32, (Instant, Option<String>)>>;

/// 查缓存，过期或缺失再做一次 NSS 查询。查不到（NSS 不可用、uid 无对应用户）
/// 时缓存 `None`，避免对着一个不存在的 uid 每个条目查一次。
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

fn user_name(uid: u32) -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
}

fn group_name(gid: u32) -> Option<String> {
    nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
        .ok()
        .flatten()
        .map(|g| g.name)
}

// ============================ provider ============================

/// 文件浏览 provider。`Clone` 共享同一份名字缓存。
#[derive(Clone, Default)]
pub struct FsProvider {
    caches: Arc<Caches>,
}

#[derive(Default)]
struct Caches {
    users: NameCache,
    groups: NameCache,
}

impl FsProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// `fs.list`：列目录。
    pub async fn list(&self, path: &str, roots: &[String]) -> ApiResult<DirListing> {
        let dir = resolve(path, roots)?;
        let caches = Arc::clone(&self.caches);
        // 卡死的 NFS 挂载点上一次 lstat 就能挂住，整段放进阻塞线程池，
        // 不让它占住 worker 的 runtime 线程。
        tokio::task::spawn_blocking(move || list_blocking(&caches, &dir))
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

fn list_blocking(caches: &Caches, dir: &Path) -> ApiResult<DirListing> {
    let rd = std::fs::read_dir(dir).map_err(|e| io_err(dir, &e))?;
    let mut entries = Vec::new();
    let mut skipped = 0u32;
    for item in rd {
        let Ok(item) = item else {
            skipped += 1;
            continue;
        };
        // read_dir 之后条目随时可能消失（/proc 尤甚），lstat 失败跳过并计数，
        // 不让一个条目毁掉整个列表。
        let Ok(meta) = std::fs::symlink_metadata(item.path()) else {
            skipped += 1;
            continue;
        };
        let kind = kind_of(&meta.file_type());
        let target = (kind == FileKind::Symlink)
            .then(|| std::fs::read_link(item.path()).ok())
            .flatten()
            .map(|t| t.to_string_lossy().into_owned());
        entries.push(DirEntryInfo {
            name: item.file_name().to_string_lossy().into_owned(),
            kind,
            size_bytes: meta.size(),
            mode: meta.permissions().mode() & 0o7777,
            uid: meta.uid(),
            gid: meta.gid(),
            user: cached_name(&caches.users, meta.uid(), user_name),
            group: cached_name(&caches.groups, meta.gid(), group_name),
            mtime_ts: meta.mtime(),
            target,
        });
    }
    // 目录在前，其余按名称（roadmap/04 §A.3）。
    entries.sort_by(|a, b| {
        let (da, db) = (a.kind == FileKind::Dir, b.kind == FileKind::Dir);
        db.cmp(&da).then_with(|| a.name.cmp(&b.name))
    });
    Ok(DirListing {
        path: dir.to_string_lossy().into_owned(),
        entries,
        skipped,
    })
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

    #[tokio::test(flavor = "multi_thread")]
    async fn 本机_list_proc_self_不炸() {
        // procfs 是 Linux 专有的。按 roadmap/README §7，依赖真实系统的用例用运行期
        // 探测跳过而非 #[ignore]，这样在有 procfs 的机器上它永远是跑着的。
        if !Path::new("/proc/self").exists() {
            eprintln!("本机无 procfs，跳过 /proc/self 用例");
            return;
        }
        let fs = FsProvider::new();
        let listing = fs.list("/proc/self", &roots(&["/"])).await.unwrap();
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
            .list(&dir.to_string_lossy(), &roots(&["/"]))
            .await
            .unwrap();
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a_dir", "z_dir", "a_file", "b_file"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

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

        let err = fs.list("/etc/passwd", &all).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidRequest, "对文件 list 应报不是目录");
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
            .list(&dir.to_string_lossy(), &roots(&["/"]))
            .await
            .unwrap();
        let ln = listing.entries.iter().find(|e| e.name == "ln").unwrap();
        assert_eq!(ln.kind, FileKind::Symlink);
        assert_eq!(ln.target.as_deref(), Some("/etc/hostname"));

        let _ = std::fs::remove_dir_all(&dir);
    }

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
