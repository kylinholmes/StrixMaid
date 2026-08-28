//! `/api/v1/files/*` —— 只读文件浏览（`design.md` §9.1「文件」组、roadmap/04 §A）。
//!
//! # 为什么经 worker
//!
//! 文件的可见性由文件系统按 uid 裁决（`design.md` §1 原则 3）。两个端点经
//! [`crate::auth::exec`] 投递到会话的 user worker：普通用户看不到 `/etc/shadow`
//! 就是 403，不需要服务端写一行判断。主进程（可能是 root）里读文件会让任何
//! 登录用户看到全部文件，授权模型直接失效。
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
use axum::extract::{Extension, Query, State};
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;
use strixmaid_types::file::{DirListing, FileContent, FilePathQuery};
use strixmaid_types::rpc::{self, FsParams};
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
        .with_state(state)
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
    Query(q): Query<FilePathQuery>,
) -> ApiResult<Json<DirListing>> {
    Ok(Json(
        exec::call(
            &st.auth,
            &session,
            Privilege::User,
            rpc::FS_LIST,
            st.params(q.path),
        )
        .await?,
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
