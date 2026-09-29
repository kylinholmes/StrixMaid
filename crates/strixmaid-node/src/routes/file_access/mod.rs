//! 浏览器原生读流：Bearer 创建记录，文件专用 Cookie 读取，字节从 user worker 转发。
mod http;
mod range;
mod registry;
#[cfg(test)]
mod tests;

use crate::auth::AuthState;
pub use registry::Registry;
use std::{path::PathBuf, sync::Arc};
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(Clone)]
pub struct FileAccessState {
    pub(crate) auth: Arc<AuthState>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) roots: Arc<Vec<String>>,
}
impl FileAccessState {
    pub fn new(auth: Arc<AuthState>, roots: &[PathBuf], registry: Arc<Registry>) -> Self {
        Self {
            auth,
            registry,
            roots: Arc::new(
                roots
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect(),
            ),
        }
    }
}
pub fn router(state: FileAccessState) -> OpenApiRouter<()> {
    let control = OpenApiRouter::new()
        .routes(routes!(http::create))
        .routes(routes!(http::renew))
        .routes(routes!(http::remove))
        .with_state(state.clone());
    let control = crate::auth::middleware::protect_openapi(control, state.auth.clone());
    // content 自行验证文件 Cookie，不接受长会话 Bearer 作为文件凭据。
    control.merge(
        OpenApiRouter::new()
            .routes(routes!(http::content))
            .with_state(state),
    )
}
