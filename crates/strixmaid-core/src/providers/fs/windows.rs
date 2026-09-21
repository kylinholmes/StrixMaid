//! 文件浏览 provider 的 Windows 取数：路径语义、目录项元数据、驱动器枚举。
//!
//! 与 [`super`] 的分工：**流程共用，取数分叉**。读目录、跳过失败条目、排序、
//! 大小上限、二进制判定、UTF-8 有损转换全部留在 [`super`] 里两个平台共跑；
//! 这里只回答两个问题——「一条路径长什么样」与「一条目录项的属主 / 权限
//! 从哪来」。
//!
//! # 平台差异一览
//!
//! | 字段 | Unix | Windows |
//! |---|---|---|
//! | 根 | 唯一的 `/` | 每个驱动器一个根（`C:\`）；裸 `\` 是「全部驱动器」这个**虚拟根** |
//! | `..` 上弹到头 | 停在 `/` | 停在盘根 `C:\`（UNC 则停在 `\\server\share\`） |
//! | `mode` | `st_mode` 低 12 位，**真实权限** | **合成值**，见 §mode |
//! | `uid` / `gid` | 真实 uid / gid | 属主 / 属组 SID 的 **RID**；取不到为 0 |
//! | `user` / `group` | NSS（`getpwuid`） | `GetNamedSecurityInfoW` + `LookupAccountSidW`；取不到为 `None` |
//! | `size_bytes` | `st_size` | `Metadata::file_size()`；符号链接自身恒为 0 |
//! | `mtime_ts` | `st_mtime` | 最后写入的 `FILETIME` 换算成 unix 秒 |
//! | `kind` | 七种齐全 | 只可能是 `Dir` / `File` / `Symlink` / `Unknown` |
//!
//! # 「整个文件系统命名空间」这个根
//!
//! 跨平台的默认配置是 `files.allowed_roots = ["/"]`。Windows 上没有唯一的 `/`，
//! 因此**裸的 `/` 或 `\` 被定义为「全部驱动器」**——与资源管理器的「此电脑」
//! 同一层。规范化后它写作 `\`（[`NAMESPACE_ROOT`]），并且：
//!
//! - `is_allowed` 时它匹配**任何绝对路径**；
//! - `fs.list` 到它时不走 `read_dir`（那会落到当前盘的根上），
//!   改为枚举驱动器，每个驱动器一条 `DirEntryInfo`。
//!
//! 配置侧的对应判断是 [`crate::config::is_absolute_root`]，两边必须一致。
//!
//! **驱动器条目的 `name` 就是完整的绝对路径**（`C:\`），不是一段相对名。
//! 前端拿到根列表后应当**直接**用 `name` 作为下一次请求的 `path`，
//! 不要与父路径 `\` 拼接——拼出来的 `\C:\` 不是合法路径。
//!
//! # roots 比较为什么不区分大小写
//!
//! Windows 的文件系统在默认配置下是大小写不敏感的：`c:\users` 与 `C:\Users`
//! 指同一个目录，`CreateFileW` 两种写法都能打开。若照搬 Unix 的逐字节比较，
//! 用户在配置里写 `C:\Users`、在地址栏敲 `c:\users`，就会莫名其妙地吃一个 403。
//! 因此 [`is_allowed`] **按路径段做大小写不敏感比较**（段的边界仍然严格，
//! `C:\Users2` 不会被 `C:\Users` 放行）。
//!
//! NTFS 支持按目录打开大小写敏感标志（WSL 会用），此时这条比较偏宽松。
//! 可以接受：`allowed_roots` 按模块文档本来就**不是安全边界**，
//! 真正的边界是文件权限。
//!
//! # mode 是合成的
//!
//! Windows 的访问控制是 ACL，没有 `rwxrwxrwx` 这三组九位。而
//! [`strixmaid_types::file::DirEntryInfo::mode`] 是**非 Option** 的 `u32`，
//! 必须给一个值。这里按 WSL（DrvFs）与 Git for Windows 的既有约定，
//! 只从「是不是目录」与「有没有只读属性」两个位合成：
//!
//! | 目录 | 只读属性 | mode |
//! |---|---|---|
//! | 是 | 否 | `0o755` |
//! | 是 | 是 | `0o555` |
//! | 否 | 否 | `0o644` |
//! | 否 | 是 | `0o444` |
//!
//! **这是为展示而做的近似，不是权限事实**，近似在三处：
//!
//! 1. 它与 ACL 无关。一个 `0o644` 的文件可能因为 ACL 而连属主自己都读不了，
//!    也可能对 Everyone 可写。**要不要能读，唯一的答案是真去读一次**
//!    ——`fs.read` 正是这么做的，无权限就报 `PermissionDenied`。
//! 2. 组位与其他位是**编出来给格式化用的**，Windows 上没有对应概念。
//! 3. 目录的只读属性在 Windows 上并**不**表示不可写（它被资源管理器用来
//!    标记「这个文件夹有自定义图标/视图」）。照 WSL 的约定映射成 `0o555`，
//!    是为了与另外两个工具显示一致，而不是因为它真的只读。
//!
//! 需要真实权限的场景（P0 不做权限编辑）应当另开一个报 ACL 的字段，
//! 而不是去解读这个 mode。
//!
//! # 属主查询的代价
//!
//! 属主 / 属组只能靠 `GetNamedSecurityInfoW` 逐条目取——目录枚举
//! （`FindFirstFileW`）的返回结构里根本没有属主。**一条目录项一次系统调用**，
//! 在 `C:\Windows\System32`（约 5000 条）这种目录上是可观的开销。
//!
//! 试过、但**行不通**的快路径：「先取目录自身的属主，条目与目录同属主就复用」。
//! 要判断「是否同属主」，本来就得先把条目的属主查出来——省不掉的正是那次查询。
//! 而唯一能省的那一步（SID → 账户名的 `LookupAccountSidW`，域环境下可能走网络）
//! 已经被 [`crate::platform::windows::token::account_by_sid`] 的 SID 级缓存挡掉了。
//!
//! 因此这里只保留一道**预算上限** [`MAX_OWNER_QUERIES`]：超出的条目
//! `uid` / `gid` 报 0、`user` / `group` 报 `None`（即「不知道」，
//! 与查询失败同一种表示），而不是让一次列目录拖上几秒。
//!
//! 实测下来这道闸几乎用不上——单次查询只要 8–20 µs，比目录枚举本身还便宜，
//! `C:\Windows\System32`（4817 条）全查一遍也只多 77 ms。数据见
//! [`MAX_OWNER_QUERIES`] 的文档与测试 `本机_大目录列举耗时` 的输出。

use std::ffi::OsStr;
use std::fs::{FileType, Metadata};
use std::os::windows::ffi::OsStrExt as _;
use std::os::windows::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf, Prefix, PrefixComponent};

use strixmaid_types::file::{DirEntryInfo, DirListing, FileKind};
use strixmaid_types::{ApiError, ApiResult};
use windows_sys::Win32::Foundation::{ERROR_SUCCESS, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    GROUP_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY,
    FILE_ATTRIBUTE_SYSTEM,
};

use crate::platform::windows::{account_by_sid, filetime_to_unix, logical_volumes, rid_of_sid};

/// 「全部驱动器」这个虚拟根的规范写法。请求里写 `/` 或 `\` 都归一到它。
pub(super) const NAMESPACE_ROOT: &str = "\\";

/// 一次 `fs.list` 最多做多少次安全描述符查询，见模块文档「属主查询的代价」。
///
/// 取 8192，依据是本机（Windows 10、NVMe、缓存已热）的实测：
///
/// | 目录 | 条目 | 枚举 + lstat | 完整 list | 属主的增量 |
/// |---|---|---|---|---|
/// | `C:\Windows` | 103 | 2 ms | 5 ms | 3 ms |
/// | `C:\Windows\System32` | 4817 | 94 ms | 171 ms | 77 ms（≈16 µs/条） |
/// | `C:\Windows\WinSxS` | 14476 | 333 ms | 395 ms | 62 ms（只查了 8192 条） |
///
/// 一次 `GetNamedSecurityInfoW` 的边际成本是 **8–20 µs**，比目录枚举本身
/// （`read_dir` + `lstat`，同一批目录上 20–70 µs/条）还低一截——因此
/// **常规目录根本不需要设上限**，5000 条的 `System32` 全查也只多 77 ms。
/// 这个数字只是给病态目录（几万到几十万条）留的一道闸，
/// 免得属主查询把一次列目录的耗时再翻一倍。
const MAX_OWNER_QUERIES: usize = 8192;

// ============================ 路径 ============================

/// 规范化路径（Windows）。见 [`super::normalize`]。
///
/// 认这几种写法：
///
/// - `C:\Users` / `C:/Users`（盘符大小写随意，规范化后统一成大写）；
/// - `\\server\share\dir`（UNC）与 `\\?\C:\dir`（verbatim）；
/// - **裸的 `/` 或 `\`** —— 归一成 [`NAMESPACE_ROOT`]，含义见模块文档。
///
/// 拒绝相对路径，也拒绝设备命名空间（`\\.\PhysicalDrive0`、`\\?\GLOBALROOT\…`）：
/// 那些名字背后是设备而不是文件，打开它们会读到原始扇区流，
/// 文件面板不提供这种入口。
///
/// 不访问文件系统，因此不解析符号链接与 junction（理由见 [`super`] 的模块文档）。
pub(super) fn normalize(path: &str) -> ApiResult<PathBuf> {
    if path.is_empty() {
        return Err(ApiError::invalid_request("路径不能为空"));
    }
    let p = Path::new(path);
    if is_namespace_root(p) {
        return Ok(PathBuf::from(NAMESPACE_ROOT));
    }
    if !p.is_absolute() {
        return Err(ApiError::invalid_request(format!(
            "路径必须是绝对路径：{path}"
        )));
    }
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Prefix(pre) => {
                let Some(s) = prefix_string(&pre) else {
                    return Err(ApiError::invalid_request(format!(
                        "不支持设备命名空间路径：{path}"
                    )));
                };
                out.push(s);
            }
            // push 一个「有根无前缀」的段，效果是保留前缀、把其余清空，
            // 于是 `C:` + `\` 正好得到 `C:\`。
            Component::RootDir => out.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            // `PathBuf::pop` 走的是 `Path::parent`，而盘根 `C:\` 的 parent
            // 是 `None`——弹到盘根就自然停住，与 Unix 弹到 `/` 就停同理。
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(seg) => out.push(seg),
        }
    }
    Ok(out)
}

/// 前缀的规范串；设备命名空间返回 `None`（调用方据此拒绝）。
fn prefix_string(pre: &PrefixComponent<'_>) -> Option<String> {
    match pre.kind() {
        // 盘符统一成大写。这个串会进 `DirListing.path` 给人看，
        // 不该随请求写成 `c:` 还是 `C:` 而飘。
        Prefix::Disk(d) => Some(format!("{}:", d.to_ascii_uppercase() as char)),
        Prefix::VerbatimDisk(d) => Some(format!(r"\\?\{}:", d.to_ascii_uppercase() as char)),
        // UNC 的主机名与共享名原样保留（大小写由服务器决定，我们不猜）。
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => {
            Some(pre.as_os_str().to_string_lossy().into_owned())
        }
        Prefix::DeviceNS(_) | Prefix::Verbatim(_) => None,
    }
}

/// 这条路径是不是「全部驱动器」这个虚拟根：有根、且只有根这一段。
///
/// 判据与 [`crate::config::is_absolute_root`] 的第二个分支同源：
/// `Path::is_absolute` 在 Windows 上要求带盘符前缀，裸 `\` 只有 `has_root()`。
/// 既用来识别请求路径，也用来识别配置里写的 root。
pub(super) fn is_namespace_root(p: &Path) -> bool {
    p.has_root() && p.components().count() == 1
}

/// 规范化后的路径是否位于任一 root 之内（含 root 本身）。见 [`super::is_allowed`]。
///
/// 两点与 Unix 不同，理由见模块文档：裸根匹配一切绝对路径；段比较不区分大小写。
pub(super) fn is_allowed(path: &Path, roots: &[String]) -> bool {
    roots.iter().any(|r| {
        let root = Path::new(r);
        if is_namespace_root(root) {
            // 「全部驱动器」：任何绝对路径都在里面，虚拟根自己也算。
            return path.is_absolute() || is_namespace_root(path);
        }
        starts_with_ci(path, root)
    })
}

/// `Path::starts_with` 的大小写不敏感版本。仍然**按路径段**比较——
/// 字符串前缀会把 `C:\Users2` 误判进 `C:\Users`。
fn starts_with_ci(path: &Path, root: &Path) -> bool {
    let mut pc = path.components();
    for r in root.components() {
        match pc.next() {
            Some(p) if eq_ci(r.as_os_str(), p.as_os_str()) => {}
            _ => return false,
        }
    }
    true
}

fn eq_ci(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

// ============================ 驱动器 ============================

/// 列「全部驱动器」这个虚拟根：一个驱动器一条目录项。
///
/// 只列**驱动器根**（`C:\`）。[`logical_volumes`] 还会报出挂到目录上的卷
/// （`C:\Data\`，Windows 的 mount point）——那些在 `C:\` 里浏览时本来就看得到，
/// 在「此电脑」这一层重复列出只会让同一个目录出现两次。
///
/// 字段取值与理由：`kind = Dir`（它就是个能进去的目录）；`size_bytes = 0`
/// （卷的容量是 `SystemInfo.filesystems` 的口径，不是一条目录项的 `size`，
/// 塞进来会被当成「这个目录占多少字节」而误导）；`mode = 0o555`
/// （驱动器根谁都能列、没人能删）；`uid`/`gid`/`user`/`group` 报 0 / `None`
/// ——卷本身没有属主这个概念，**不拿根目录的属主冒充**；
/// `mtime_ts = 0` 同理。
pub(super) fn list_drives(dir: &Path) -> DirListing {
    let entries = logical_volumes()
        .into_iter()
        .filter(|v| is_drive_root(&v.mount_point))
        .map(|v| DirEntryInfo {
            name: v.mount_point,
            kind: FileKind::Dir,
            size_bytes: 0,
            mode: 0o555,
            uid: 0,
            gid: 0,
            user: None,
            group: None,
            mtime_ts: 0,
            // 驱动器是虚拟根下合成出来的条目，没有属性位可言，永不隐藏。
            target: None,
            target_kind: None,
            hidden: false,
        })
        .collect();
    DirListing {
        path: dir.to_string_lossy().into_owned(),
        entries,
        skipped: 0,
        // 排序分页收尾（`finish_listing`）会重建这份 DirListing 并填上真值。
        total: None,
    }
}

/// 挂载点是不是一个驱动器根（`C:\` 这样的三个字符）。
fn is_drive_root(mount: &str) -> bool {
    let b = mount.as_bytes();
    b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
}

// ============================ 目录项 ============================

/// 把一条目录项映射成 DTO。与 Unix 侧的 `EntryMapper` 同签名。
///
/// 需要跨条目的状态（属主查询预算），所以是结构体而不是自由函数。
pub(super) struct EntryMapper {
    /// 还剩多少次安全描述符查询，见模块文档「属主查询的代价」。
    budget: usize,
}

impl EntryMapper {
    pub(super) fn new(_dir: &Path) -> Self {
        Self {
            budget: MAX_OWNER_QUERIES,
        }
    }

    pub(super) fn map(
        &mut self,
        _caches: &super::Caches,
        path: &Path,
        name: String,
        meta: &Metadata,
        kind: FileKind,
        target: Option<String>,
    ) -> DirEntryInfo {
        let own = if self.budget > 0 {
            self.budget -= 1;
            ownership_of(path).unwrap_or_default()
        } else {
            Ownership::default()
        };
        // 在 name 被搬进结构体之前判：dotfile 那一条要看名字。
        let hidden = hidden_of(&name, meta.file_attributes());
        DirEntryInfo {
            name,
            kind,
            size_bytes: meta.file_size(),
            mode: mode_of(meta.file_attributes()),
            uid: own.uid,
            gid: own.gid,
            user: own.user,
            group: own.group,
            mtime_ts: filetime_to_unix(meta.last_write_time()),
            target,
            target_kind: target_kind_of(kind, meta.file_attributes()),
            hidden,
        }
    }
}

/// 一条目录项的属主 / 属组。默认值就是「不知道」：0 + `None`。
#[derive(Default)]
struct Ownership {
    uid: u32,
    gid: u32,
    user: Option<String>,
    group: Option<String>,
}

/// 一次 `GetNamedSecurityInfoW` 取出属主与属组。
///
/// 失败（没有 `READ_CONTROL`、路径超过 `MAX_PATH` 且未开长路径、网络盘不可达）
/// 一律返回 `None` → 上报 0 / `None`，**不猜**。
fn ownership_of(path: &Path) -> Option<Ownership> {
    // 不经 `to_string_lossy`：那会把无法表示的 UTF-16 码位换成 U+FFFD，
    // 查的就不是同一个文件了。直接按 UTF-16 原样传。
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut owner: PSID = std::ptr::null_mut();
    let mut group: PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: wide 是以 NUL 结尾的宽串，生存期覆盖本次调用；owner / group / sd
    // 都是本函数栈上的输出变量；不需要 DACL / SACL 时按文档传空指针。
    // 成功时 sd 指向一块 LocalAlloc 的安全描述符，owner / group 指向它内部
    // （不是独立分配），因此必须在 LocalFree 之前读完。
    let rc = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION,
            &raw mut owner,
            &raw mut group,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return None;
    }
    // SAFETY: 调用成功时 owner / group 要么为空，要么指向 sd 内部的有效 SID；
    // sd 直到下一条语句的 LocalFree 之前都还在，四次读取都发生在归还之前。
    let out = unsafe {
        Ownership {
            uid: sid_rid(owner),
            gid: sid_rid(group),
            user: sid_account(owner),
            group: sid_account(group),
        }
    };
    // SAFETY: sd 由上面的调用用 LocalAlloc 分配，按文档用 LocalFree 归还；
    // owner / group 指向 sd 内部，已在上一条语句里读完，此后不再使用。
    unsafe {
        LocalFree(sd);
    }
    Some(out)
}

/// SID → `uid` / `gid`。
///
/// 口径与 [`crate::platform::windows::token`] 的 uid 映射**只差一处**：
/// 那里把 LocalSystem（`S-1-5-18`）特判成 0，因为 `uid == 0` 是全项目的
/// 「这是 root」判据；这里是**展示用的属主**，保留原始 RID（SYSTEM 报 18），
/// 好让 SYSTEM、TrustedInstaller、本地用户在列表里彼此可分。
///
/// # Safety
///
/// `sid` 为空指针时返回 0；非空时必须指向一个有效的 SID 结构，
/// 并在调用期间保持有效。
unsafe fn sid_rid(sid: PSID) -> u32 {
    if sid.is_null() {
        0
    } else {
        // SAFETY: 刚判过非空，其余由调用方保证。
        unsafe { rid_of_sid(sid) }
    }
}

/// SID → 账户名。用 `DOMAIN\name` 的限定形式：
/// 系统文件的属主大量是 `BUILTIN\Administrators`、`NT SERVICE\TrustedInstaller`
/// 这类内建账户，只给 `Administrators` 会分不清是内建组还是同名的域组。
///
/// # Safety
///
/// `sid` 为空指针时返回 `None`；非空时必须指向一个有效的 SID 结构，
/// 并在调用期间保持有效。
unsafe fn sid_account(sid: PSID) -> Option<String> {
    if sid.is_null() {
        return None;
    }
    // SAFETY: 刚判过非空，其余由调用方保证。
    unsafe { account_by_sid(sid) }.map(|a| a.qualified())
}

/// 链接**目标**的类型，从链接自身的属性位读出。非链接返回 `None`。
///
/// **不碰目标，零额外系统调用**：重解析点（符号链接、junction）自身就带
/// `FILE_ATTRIBUTE_DIRECTORY`——指向目录的链接置位，指向文件的不置位，
/// 与 [`mode_of`] 用同一个位出于同一个理由。
///
/// 代价是**断链仍按链接声明的形态报**（Unix 侧要 `stat` 才知道目标类型，
/// 因此那边断链报 `None`）。两个平台在界面上的结果一致：跳过去之后由
/// `/files` 正常报「找不到」，而不是点了没反应。
pub(super) fn target_kind_of(kind: FileKind, attrs: u32) -> Option<FileKind> {
    if kind != FileKind::Symlink {
        return None;
    }
    Some(if attrs & FILE_ATTRIBUTE_DIRECTORY != 0 {
        FileKind::Dir
    } else {
        FileKind::File
    })
}

/// 这一项在 Windows 的约定下算不算隐藏。
///
/// **两套约定都算**（项目负责人 2026-09-21 定）：
///
/// 1. `FILE_ATTRIBUTE_HIDDEN`（用户可见的「隐藏」勾选框）与
///    `FILE_ATTRIBUTE_SYSTEM`（系统簿记文件：`NTUSER.DAT{…}.regtrans-ms`、
///    `pagefile.sys`）——资源管理器默认这两类都不显示；
/// 2. 名字以 `.` 开头——Unix 的 dotfile 约定。
///
/// 第 2 条**与资源管理器不一致**（它会显示 `.gitignore`），是有意的：全仓
/// 一贯的取向是「三平台一致」，而 `.git` / `.venv` / `.vscode` 在服务器管理
/// 界面上就是噪音，不该因为宿主换成 Windows 就摊开一屏。这条推翻了本函数
/// 最初「Windows 只看属性位」的写法。
pub(super) fn hidden_of(name: &str, attrs: u32) -> bool {
    attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 || name.starts_with('.')
}

/// 文件属性 → 合成的 `mode`，口径与近似说明见模块文档「mode 是合成的」。
///
/// 用 `FILE_ATTRIBUTE_DIRECTORY` 而不是 `Metadata::is_dir()`：后者对重解析点
/// （符号链接、junction）一律为 false，于是一个指向目录的链接会被算成文件。
/// 属性位反映的是目标的形态，更接近 Unix 上 `lstat` 一个目录链接的观感。
pub(super) fn mode_of(attrs: u32) -> u32 {
    let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
    let read_only = attrs & FILE_ATTRIBUTE_READONLY != 0;
    match (is_dir, read_only) {
        (true, false) => 0o755,
        (true, true) => 0o555,
        (false, false) => 0o644,
        (false, true) => 0o444,
    }
}

/// 文件类型映射。见 [`super::kind_of`]。
///
/// `BlockDevice` / `CharDevice` / `Fifo` / `Socket` 在 Windows 的普通文件系统里
/// **不会出现**——命名管道在 `\\.\pipe\` 这个独立命名空间里，设备在
/// `\\.\` 下，两者都被 [`normalize`] 挡在门外。因此只可能是三种，
/// 剩下的 `Unknown` 是兜底而非常态。
///
/// **junction（目录联接）也报 `Symlink`**：它是重解析点，`is_symlink()` 为 true，
/// `std::fs::read_link` 取得到目标，行为与符号链接一致——没有必要为它
/// 另立一种 `FileKind`（DTO 是跨平台契约，加一种 Windows 专有类型
/// 会让所有客户端都得处理）。
pub(super) fn kind_of(ft: &FileType) -> FileKind {
    // 先判 symlink：重解析点的 is_dir() 恒为 false，顺序反了会把目录链接
    // 当成普通文件。
    if ft.is_symlink() {
        FileKind::Symlink
    } else if ft.is_dir() {
        FileKind::Dir
    } else if ft.is_file() {
        FileKind::File
    } else {
        FileKind::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 驱动器根判断() {
        assert!(is_drive_root("C:\\"));
        assert!(is_drive_root("z:\\"));
        assert!(!is_drive_root("C:"));
        assert!(!is_drive_root("C:\\Data\\"), "挂到目录上的卷不算驱动器根");
        assert!(!is_drive_root(""));
    }

    #[test]
    fn mode_合成() {
        assert_eq!(mode_of(FILE_ATTRIBUTE_DIRECTORY), 0o755);
        assert_eq!(
            mode_of(FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_READONLY),
            0o555
        );
        assert_eq!(mode_of(0), 0o644);
        assert_eq!(mode_of(FILE_ATTRIBUTE_READONLY), 0o444);
    }

    #[test]
    fn 虚拟根判断() {
        assert!(is_namespace_root(Path::new("/")));
        assert!(is_namespace_root(Path::new("\\")));
        assert!(!is_namespace_root(Path::new("C:\\")));
        assert!(!is_namespace_root(Path::new("")));
        assert!(!is_namespace_root(Path::new("\\\\server\\share")));
    }

    /// 属主查询是整个列目录里最贵的一步，单独验一次它在真实路径上确实有结果。
    #[test]
    fn 本机_属主可查() {
        let dir = std::env::temp_dir();
        let own = ownership_of(&dir).expect("自己的临时目录总该读得到安全描述符");
        eprintln!(
            "{} 属主 uid={} user={:?} gid={} group={:?}",
            dir.display(),
            own.uid,
            own.user,
            own.gid,
            own.group
        );
        assert!(
            own.user.is_some(),
            "临时目录的属主应当查得到名字（当前用户或 Administrators）"
        );
    }

    /// 不存在的路径：报 `None`，不 panic、不编一个属主出来。
    #[test]
    fn 不存在的路径属主为空() {
        assert!(ownership_of(Path::new("C:\\no\\such\\strixmaid-path")).is_none());
    }
}
