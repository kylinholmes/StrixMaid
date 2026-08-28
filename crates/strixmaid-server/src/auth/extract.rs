//! 从请求里取 Bearer token 与当前会话。

use std::sync::Arc;

use axum::extract::{FromRef, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use strixmaid_core::session::Session;
use strixmaid_types::ApiError;

use super::AuthState;
use crate::error::ApiErr;

/// `Authorization: Bearer <token>` 里的明文 token。
///
/// 只在请求处理期间存在，用于回传给浏览器（提权成功时 `AuthOutcome::Complete.token`
/// 回原 token）；**不要**记日志、不要存。
#[derive(Clone)]
pub struct BearerToken(pub String);

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

/// 解析 `Authorization` 头。scheme 大小写不敏感；token 为空视为没有。
pub fn bearer_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(raw) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let (scheme, token) = raw.trim().split_once(char::is_whitespace)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let token = token.trim();
        return if token.is_empty() { None } else { Some(token.to_string()) };
    }
    bearer_from_ws_protocol(headers)
}

/// WebSocket 握手拿不到 `Authorization` 头（浏览器不允许），约定用子协议携带：
/// `new WebSocket(url, ["bearer", token])` → `Sec-WebSocket-Protocol: bearer, <token>`。
/// token 不进 URL，因而不会落进访问日志与反代日志。服务端升级时要回 `bearer` 子协议，
/// 否则浏览器会中止握手（见 `ws::handler`）。
pub const WS_BEARER_PROTOCOL: &str = "bearer";

pub(crate) fn bearer_from_ws_protocol(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("sec-websocket-protocol")?.to_str().ok()?;
    let mut parts = raw.split(',').map(str::trim).filter(|s| !s.is_empty());
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case(WS_BEARER_PROTOCOL) {
        return None;
    }
    parts.next().map(str::to_string)
}

/// 当前已认证会话。
///
/// 优先取中间件放进 extensions 的 [`Session`]；没套中间件的路由（auth 路由自己）
/// 则直接解析 Bearer 并向 `SessionManager` 查询。两条路的失败都是 401。
#[derive(Debug, Clone)]
pub struct CurrentSession {
    /// 会话快照。
    pub session: Session,
    /// 明文 token（见 [`BearerToken`]）。
    pub token: BearerToken,
}

impl<S> FromRequestParts<S> for CurrentSession
where
    S: Send + Sync,
    Arc<AuthState>: FromRef<S>,
{
    type Rejection = ApiErr;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let token = bearer_from_headers(&parts.headers).ok_or_else(|| {
            ApiErr(ApiError::unauthenticated(
                "缺少 Authorization: Bearer token",
            ))
        })?;

        if let Some(session) = parts.extensions.get::<Session>() {
            return Ok(CurrentSession {
                session: session.clone(),
                token: BearerToken(token),
            });
        }

        let auth = Arc::<AuthState>::from_ref(state);
        let session = auth
            .sessions
            .resolve(&token)
            .await
            .ok_or_else(|| ApiErr(ApiError::unauthenticated("会话不存在或已过期")))?;
        Ok(CurrentSession {
            session,
            token: BearerToken(token),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn bearer_解析() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer_from_headers(&h), None);
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
        assert_eq!(bearer_from_headers(&h).as_deref(), Some("abc"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("bearer   xyz  "));
        assert_eq!(bearer_from_headers(&h).as_deref(), Some("xyz"));
        h.insert(AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer_from_headers(&h), None);
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer "));
        assert_eq!(bearer_from_headers(&h), None);
        h.insert(AUTHORIZATION, HeaderValue::from_static("Bearer"));
        assert_eq!(bearer_from_headers(&h), None);
    }

    #[test]
    fn bearer_token_debug_脱敏() {
        let s = format!("{:?}", BearerToken("secret-token".into()));
        assert!(!s.contains("secret-token"));
    }
}
