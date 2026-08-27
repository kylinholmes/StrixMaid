//! [`strixmaid_types::ApiError`] 的 HTTP 适配层。
//!
//! 错误 DTO 本身住在 `strixmaid-types`（§3），连同 `ErrorCode::http_status()` 这张
//! 状态码映射表——它属于协议，Server 与 worker 共用。本文件只补上 axum 侧的两件事：
//! 转成 `Response`，以及 5xx 时写一条 journald 日志。
//!
//! # 为什么要 newtype
//!
//! `ApiError` 与 `IntoResponse` 对本 crate 都是外部条目，孤儿规则不允许直接
//! `impl IntoResponse for ApiError`。而 `strixmaid-types` 按 §3 不能依赖 axum，
//! 所以适配只能落在这里，包一层 [`ApiErr`]。
//! 处理器里用 `?` 时 [`From`] 会自动套上，写法上感知不到。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use strixmaid_types::ApiError;

/// [`ApiError`] 的 axum 响应包装。
#[derive(Debug)]
pub struct ApiErr(pub ApiError);

impl From<ApiError> for ApiErr {
    fn from(inner: ApiError) -> Self {
        Self(inner)
    }
}

impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        // http_status() 返回的是协议里写死的合法状态码；真出了范围也只能算内部错误。
        let status = StatusCode::from_u16(self.0.http_status()).unwrap_or_else(|_| {
            tracing::error!(
                status = self.0.http_status(),
                "ErrorCode 映射出了非法状态码"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        });

        // 5xx 记一条日志：客户端只拿到 message，排障细节留在 journald 里。
        if status.is_server_error() {
            tracing::error!(
                code = %self.0.code,
                message = %self.0.message,
                detail = ?self.0.detail,
                "请求失败"
            );
        }

        (status, Json(self.0)).into_response()
    }
}

/// 处理器返回值的惯用别名。
pub type ApiResult<T> = Result<T, ApiErr>;
