//! `/api/v1/files/*` —— 只读文件浏览（`design.md` §9.1「文件」组、roadmap/04 §A）。
//!
//! # 为什么经 worker
//!
//! 文件的可见性由文件系统按 uid 裁决（`design.md` §1 原则 3）。读文件的端点经
//! [`crate::auth::exec`] 投递到会话的 user worker：普通用户看不到 `/etc/shadow`
//! 就是 403，不需要服务端写一行判断。主进程（可能是 root）里读文件会让任何
//! 登录用户看到全部文件，授权模型直接失效。
//!
//! 唯一的例外是 [`type_icon`]：类型图标是操作系统的属性、按扩展名取、
//! **不产生任何文件访问**，因此留在主进程里让缓存跨会话共用——与
//! [`super::processes::icon`] 同一套理由，展开见
//! [`strixmaid_core::providers::fs::icon`] 的模块文档。
//!
//! # `allowed_roots` 不是安全边界
//!
//! `files.allowed_roots` 只是文件面板的展示范围（部署策略），随每次调用经
//! [`FsParams`] 下发、由 worker 里的 fs provider 校验。真正的安全边界是
//! 文件权限。
//!
//! 完整的文件管理（上传、编辑、复制移动）由项目负责人另行打磨，本模块只有
//! 两个只读端点，接口形态为后续扩展留位。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse as _, Response};
use futures::StreamExt as _;
use strixmaid_core::providers::fs;
use strixmaid_core::providers::fs::icon::FileTypeIcons;
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::file::{DirListing, FileContent, FileListQuery, FilePathQuery};
use strixmaid_types::rpc::{self, FS_RAW_MAX_CHUNK, FsListParams, FsParams, FsRawChunk, FsRawParams};
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::auth::AuthState;
use crate::auth::exec::{self, Privilege};
use crate::error::ApiResult;

/// 文件路由的状态：找 worker 的路径 + 展示范围配置。
#[derive(Clone)]
pub struct FilesState {
    auth: Arc<AuthState>,
    /// `files.allowed_roots`，启动时转成字符串，随每次 RPC 下发。
    allowed_roots: Arc<Vec<String>>,
    /// 文件类型图标的缓存。**只**给 [`type_icon`] 用——那是 `/files/*` 里唯一
    /// 不经 worker 的端点（类型图标是操作系统的属性、不产生文件访问，
    /// 理由见 `strixmaid_core::providers::fs::icon` 的模块文档）。
    type_icons: FileTypeIcons,
}

impl FilesState {
    pub fn new(auth: Arc<AuthState>, roots: &[std::path::PathBuf]) -> Self {
        FilesState {
            auth,
            allowed_roots: Arc::new(
                roots
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            ),
            type_icons: FileTypeIcons::new(),
        }
    }

    fn params(&self, path: String) -> FsParams {
        FsParams {
            path,
            allowed_roots: (*self.allowed_roots).clone(),
        }
    }
}

/// 构建文件路由。挂到 `/api/v1` 之下（路径已含 `/files` 前缀）。
pub fn router(state: FilesState) -> OpenApiRouter<()> {
    OpenApiRouter::new()
        .routes(routes!(list_dir))
        .routes(routes!(read_file))
        .routes(routes!(raw_file))
        .routes(routes!(type_icon))
        .routes(routes!(path_icon))
        .with_state(state)
}

/// 具体条目的系统图标（按路径，macOS）
///
/// `.app` 这类 bundle 显示应用自己的图标而不是文件夹，符号链接解析到目标。
/// 只有 macOS 提供（`NSWorkspace iconForFile:`），其余平台一律 404；
/// 为什么 Windows 不开这条路见 `providers/fs/icon` 的文档。
#[utoipa::path(
    get,
    path = "/files/icon-path",
    tag = "files",
    params(FilePathQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "这个条目的系统图标（响应体是原始 PNG 字节）", content_type = "image/png", body = String),
        (status = 400, description = "路径不合法（相对路径、含 `..` 或控制字符）", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 403, description = "路径在 files.allowed_roots 之外", body = ApiError),
        (status = 404, description = "本平台不提供按路径取图标（macOS 之外）", body = ApiError),
    ),
)]
pub async fn path_icon(
    State(st): State<FilesState>,
    Extension(_session): Extension<Session>,
    Query(q): Query<FilePathQuery>,
) -> ApiResult<Response> {
    // 展示范围与 /files 其余端点同一份配置、同一套函数。worker 里由 fs
    // provider 校验，这条不经 worker，就在这里用同一对 normalize/is_allowed
    // ——`..` 先被消解再比较，按路径段判断（字符串前缀会把 /home2 误进 /home）。
    let normalized = fs::normalize(&q.path)?;
    if !fs::is_allowed(&normalized, &st.allowed_roots) {
        return Err(ApiError::permission_denied(format!(
            "路径 {} 不在允许浏览的范围内（files.allowed_roots）",
            normalized.display()
        ))
        .into());
    }
    let normalized = normalized
        .to_str()
        .ok_or_else(|| ApiError::invalid_request("路径不是合法 UTF-8"))?;
    let png = st.type_icons.path_icon_png(normalized).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            // 具体应用的图标随应用更新换，比类型图标勤一档：一小时。
            (header::CACHE_CONTROL, "max-age=3600"),
        ],
        png.as_slice().to_vec(),
    )
        .into_response())
}

/// 文件类型图标
///
/// 按**扩展名**（不含点，如 `pdf`）取这一类文件的系统图标。Linux 与无窗口
/// 服务器的 macOS 环境一律 404，前端据此回落到内置图标集（§4.7）。
#[utoipa::path(
    get,
    path = "/files/icon/{ext}",
    tag = "files",
    security(("bearer" = [])),
    params(("ext" = String, Path, description = "扩展名，不含前导点，如 `pdf`")),
    responses(
        // body 用 `String` 表达二进制响应的理由见 `processes::icon`。
        (status = 200, description = "这一类文件的系统图标（响应体是原始 PNG 字节）", content_type = "image/png", body = String),
        (status = 400, description = "扩展名不合法（含点号、分隔符或过长）", body = ApiError),
        (status = 401, description = "未认证", body = ApiError),
        (status = 404, description = "本平台不提供文件类型图标（Linux、无窗口服务器的 macOS），\
                                      或系统给不出这一类的图标", body = ApiError),
    ),
)]
pub async fn type_icon(
    State(st): State<FilesState>,
    // 不用这个值，但必须提取：与 `processes::icon` 同一理由——它是本端点
    // 需要会话这件事在代码里的凭据。
    Extension(_session): Extension<Session>,
    Path(ext): Path<String>,
) -> ApiResult<Response> {
    let png = st.type_icons.icon_png(&ext).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            // 类型图标只随「换默认打开程序 / 换系统主题」这种小时级事件变化，
            // 浏览器端存一天；服务端缓存仍是 5 分钟档（IconCache::TTL）。
            (header::CACHE_CONTROL, "max-age=86400"),
        ],
        png.as_slice().to_vec(),
    )
        .into_response())
}

/// 列目录
///
/// 在会话用户的 worker 内执行，可见性由文件权限裁决。目录在前、其余按名称排序；
/// `lstat` 失败的条目被跳过并计入 `skipped`。
#[utoipa::path(
    get,
    path = "/files",
    tag = "files",
    params(FilePathQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "目录列表", body = DirListing),
        (status = 400, description = "路径不合法（相对路径、不是目录）", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "无权限读取，或路径在 files.allowed_roots 之外", body = ApiError),
        (status = 404, description = "路径不存在", body = ApiError),
    ),
)]
pub async fn list_dir(
    State(st): State<FilesState>,
    Extension(session): Extension<Session>,
    Query(q): Query<FileListQuery>,
) -> ApiResult<Json<DirListing>> {
    let params = FsListParams {
        path: q.path,
        allowed_roots: (*st.allowed_roots).clone(),
        limit: q.limit,
        offset: q.offset.unwrap_or(0),
        sort: q.sort,
        order: q.order,
    };
    Ok(Json(
        exec::call(&st.auth, &session, Privilege::User, rpc::FS_LIST, params).await?,
    ))
}

/// 查看文本文件
///
/// 只支持文本：二进制（前 8 KiB 含 NUL）与超过 5 MiB 的文件返回 400。
/// 无效 UTF-8 序列被替换成 U+FFFD 并以 `lossy = true` 标出。
#[utoipa::path(
    get,
    path = "/files/content",
    tag = "files",
    params(FilePathQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "文件内容", body = FileContent),
        (status = 400, description = "路径不合法、二进制文件或超出大小上限", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "无权限读取，或路径在 files.allowed_roots 之外", body = ApiError),
        (status = 404, description = "文件不存在", body = ApiError),
    ),
)]
pub async fn read_file(
    State(st): State<FilesState>,
    Extension(session): Extension<Session>,
    Query(q): Query<FilePathQuery>,
) -> ApiResult<Json<FileContent>> {
    Ok(Json(
        exec::call(
            &st.auth,
            &session,
            Privilege::User,
            rpc::FS_READ,
            st.params(q.path),
        )
        .await?,
    ))
}

/// 取原始字节
///
/// 缩略图与将来的下载都走它（roadmap/12 §4.6）。不放宽 `/files/content`：
/// 「绝不把二进制当文本吐回去」是那个端点的安全属性。
///
/// worker RPC 单帧上限 1 MiB，文件按 ≤256 KiB 的块从 worker 逐块取回并
/// 流式转发——控制面在块与块之间有喘息，不会被一个大文件顶住。
#[utoipa::path(
    get,
    path = "/files/raw",
    tag = "files",
    params(FilePathQuery),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "文件的原始字节", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 400, description = "路径不合法或是目录", body = ApiError),
        (status = 401, description = "未认证，或会话的 worker 已退出", body = ApiError),
        (status = 403, description = "无权限读取，或路径在 files.allowed_roots 之外", body = ApiError),
        (status = 404, description = "文件不存在", body = ApiError),
    ),
)]
pub async fn raw_file(
    State(st): State<FilesState>,
    Extension(session): Extension<Session>,
    Query(q): Query<FilePathQuery>,
) -> ApiResult<Response> {
    let raw_params = |path: &str, offset: u64| FsRawParams {
        path: path.to_owned(),
        allowed_roots: (*st.allowed_roots).clone(),
        offset,
        len: FS_RAW_MAX_CHUNK,
    };

    // 第一块在返回响应头之前取：总大小与 MIME 都从它来，路径类错误
    // （404/403/是目录）也在这里以正常错误体报出，而不是断在流中间。
    let first: FsRawChunk = exec::call(
        &st.auth,
        &session,
        Privilege::User,
        rpc::FS_RAW,
        raw_params(&q.path, 0),
    )
    .await?;
    let total = first.total_bytes;
    let mime = first.mime.clone();
    let first_bytes = decode_chunk(&first)?;

    let auth = st.auth.clone();
    let roots = st.allowed_roots.clone();
    let path = q.path.clone();
    let rest = futures::stream::try_unfold(
        first_bytes.len() as u64,
        move |offset| {
            let auth = auth.clone();
            let session = session.clone();
            let roots = roots.clone();
            let path = path.clone();
            async move {
                if offset >= total {
                    return Ok::<_, std::io::Error>(None);
                }
                let chunk: FsRawChunk = exec::call(
                    &auth,
                    &session,
                    Privilege::User,
                    rpc::FS_RAW,
                    FsRawParams {
                        path,
                        allowed_roots: (*roots).clone(),
                        offset,
                        len: FS_RAW_MAX_CHUNK,
                    },
                )
                .await
                .map_err(|e| std::io::Error::other(e.message))?;
                let bytes = decode_chunk(&chunk).map_err(|e| std::io::Error::other(e.message))?;
                if bytes.is_empty() {
                    // 文件在读取中途被截短：按已取到的部分结束，不无限空转。
                    return Ok(None);
                }
                let next = offset + bytes.len() as u64;
                Ok(Some((axum::body::Bytes::from(bytes), next)))
            }
        },
    );
    let stream = futures::stream::once(async move {
        Ok::<_, std::io::Error>(axum::body::Bytes::from(first_bytes))
    })
    .chain(rest);

    let resp = Response::builder()
        .header(axum::http::header::CONTENT_TYPE, mime)
        .header(axum::http::header::CONTENT_LENGTH, total)
        .body(axum::body::Body::from_stream(stream))
        .map_err(|e| ApiError::internal("组装响应失败").with_detail(e.to_string()))?;
    Ok(resp)
}

/// hex → 字节。worker 是我们自己的进程，坏 hex 意味着协议损坏，报 internal。
fn decode_chunk(chunk: &FsRawChunk) -> Result<Vec<u8>, ApiError> {
    hex::decode(&chunk.data_hex)
        .map_err(|e| ApiError::internal("fs.raw 数据块不是合法 hex").with_detail(e.to_string()))
}
