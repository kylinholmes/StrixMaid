use super::{
    FileAccessState,
    range::{self, Selection},
    registry::{self, Entry, Lease},
};
use crate::error::ApiResult;
use axum::{
    Json,
    body::{Body, Bytes},
    extract::{ConnectInfo, Extension, Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::{io, net::SocketAddr, time::Duration};
use strixmaid_core::session::{Session, channel::IpcChannel};
use strixmaid_types::{
    ApiError,
    file::{FileAccessPurpose, FileAccessRequest, FileAccessResponse},
    rpc::{self, FsOpenStreamParams, FsOpenStreamResult},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const STALL: Duration = Duration::from_secs(30);

fn cookie(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|v| v.trim().split_once('='))
        .filter(|(k, _)| *k == registry::COOKIE)
        .map(|(_, v)| v);
    let first = values.next()?;
    if values.next().is_some() || first.len() != 64 || !first.bytes().all(|b| b.is_ascii_hexdigit())
    {
        None
    } else {
        Some(first)
    }
}
fn secure(
    st: &FileAccessState,
    uri: &Uri,
    headers: &HeaderMap,
    peer: Option<ConnectInfo<SocketAddr>>,
) -> bool {
    uri.scheme_str() == Some("https")
        || (peer.is_some_and(|ConnectInfo(p)| {
            st.auth
                .trusted_proxies
                .iter()
                .any(|t| t == &p.ip().to_string())
        }) && headers
            .get("x-forwarded-proto")
            .is_some_and(|v| v == "https"))
}
fn credential(secret: &str, secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{}={}; Path=/api/v1/file-access; HttpOnly; SameSite=Strict; Max-Age=600{}",
        registry::COOKIE,
        secret,
        if secure { "; Secure" } else { "" }
    ))
    .expect("hex cookie")
}
fn dto(entry: Entry) -> FileAccessResponse {
    FileAccessResponse {
        url: format!("/api/v1/file-access/{}/content", entry.id),
        id: entry.id,
        expires_in_secs: registry::TTL.as_secs(),
    }
}
fn no_store(response: &mut Response) {
    let h = response.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store, no-transform"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}
#[utoipa::path(post, path="/file-access", tag="files", security(("bearer"=[])), request_body=FileAccessRequest,
    responses((status=200, body=FileAccessResponse), (status=401, body=ApiError), (status=409, body=ApiError)))]
pub async fn create(
    State(st): State<FileAccessState>,
    Extension(session): Extension<Session>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    uri: Uri,
    headers: HeaderMap,
    Json(req): Json<FileAccessRequest>,
) -> ApiResult<Response> {
    // 这里只规范化字面路径；文件 open/stat 必须由 worker 完成。
    let path = strixmaid_core::providers::fs::normalize(&req.path)?;
    if !strixmaid_core::providers::fs::is_allowed(&path, &st.roots) {
        return Err(ApiError::permission_denied("路径不在 files.allowed_roots 范围内").into());
    }
    let revoked = st
        .auth
        .sessions
        .revocation(&session.token_hash)
        .await
        .ok_or_else(|| ApiError::unauthenticated("会话已过期"))?;
    let (entry, secret) = st.registry.create(
        &session.token_hash,
        req.path,
        req.purpose,
        cookie(&headers),
        revoked,
    )?;
    let mut response = Json(dto(entry)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        credential(
            &secret,
            secure(&st, &uri, &headers, peer.map(|Extension(p)| p)),
        ),
    );
    no_store(&mut response);
    Ok(response)
}
#[utoipa::path(post, path="/file-access/{id}/renew", tag="files", security(("bearer"=[])), params(("id"=String,Path)),
    responses((status=200, body=FileAccessResponse), (status=401, body=ApiError)))]
pub async fn renew(
    State(st): State<FileAccessState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    uri: Uri,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (entry, secret) = st
        .registry
        .renew(&id, &session.token_hash, cookie(&headers))?;
    let mut response = Json(dto(entry)).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        credential(
            &secret,
            secure(&st, &uri, &headers, peer.map(|Extension(p)| p)),
        ),
    );
    no_store(&mut response);
    Ok(response)
}
#[utoipa::path(delete, path="/file-access/{id}", tag="files", security(("bearer"=[])), params(("id"=String,Path)),
    responses((status=204), (status=401, body=ApiError)))]
pub async fn remove(
    State(st): State<FileAccessState>,
    Extension(session): Extension<Session>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    st.registry.remove(&id, &session.token_hash)?;
    Ok(StatusCode::NO_CONTENT)
}

/// 所有内联类型都在白名单内，HTML、SVG、文本及未知类型一律 attachment。
fn media(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        _ => "application/octet-stream",
    }
}
fn filename(path: &str) -> String {
    let name = path
        .rsplit(['/', '\\'])
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("download");
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
#[utoipa::path(method(get, head), path="/file-access/{id}/content", tag="files", security(("file_cookie"=[])), params(("id"=String,Path)),
    responses((status=200, body=String,description="原始字节或图片预览"),(status=206,body=String),(status=416,description="范围不可满足"),(status=401,body=ApiError),(status=403,body=ApiError)))]
pub async fn content(
    State(st): State<FileAccessState>,
    Path(id): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let secret = cookie(&headers).ok_or_else(|| ApiError::unauthenticated("缺少文件凭证"))?;
    let mut lease = st.registry.authorize(&id, secret)?;
    let session = st
        .auth
        .sessions
        .resolve_hash(&lease.entry.owner)
        .await
        .ok_or_else(|| ApiError::unauthenticated("会话已过期"))?;
    let worker = st
        .auth
        .sessions
        .user_worker(&session.token_hash)
        .await
        .filter(|w| w.is_alive())
        .ok_or_else(|| ApiError::unauthenticated("文件 worker 已退出"))?;
    let params = FsOpenStreamParams {
        path: lease.entry.path.clone(),
        allowed_roots: (*st.roots).clone(),
        preview: lease.entry.purpose == FileAccessPurpose::Preview,
    };
    let result = tokio::select! {
        biased;
        _ = lease.revoked.changed() => return Err(ApiError::unauthenticated("会话已撤销").into()),
        r = worker.call_with_fds(rpc::FS_OPEN_STREAM, serde_json::to_value(params).map_err(|e| ApiError::internal(e.to_string()))?) => r?,
    };
    let (value, mut attachments) = result;
    let meta: FsOpenStreamResult = serde_json::from_value(value)
        .map_err(|e| ApiError::internal("文件元数据无效").with_detail(e.to_string()))?;
    if attachments.len() != 1 {
        return Err(ApiError::internal("文件通道附件数量无效").into());
    }
    let attachment = attachments.pop().unwrap();
    #[cfg(unix)]
    let channel = IpcChannel::from_owned_fd(attachment);
    #[cfg(windows)]
    let channel = unsafe { IpcChannel::from_client_handle(attachment) };
    let mut channel =
        channel.map_err(|e| ApiError::internal("无法接收文件通道").with_detail(e.to_string()))?;
    // 当前仅有弱验证器，If-Range 无法可靠证明同版本时必须回完整 200。
    let selected = if method == Method::HEAD || headers.contains_key(header::IF_RANGE) {
        Selection::Full
    } else {
        range::select(
            headers.get(header::RANGE).and_then(|v| v.to_str().ok()),
            meta.size,
        )
    };
    let (status, offset, length) = match selected {
        Selection::Full => (StatusCode::OK, 0, meta.size),
        Selection::Partial { offset, length } => (StatusCode::PARTIAL_CONTENT, offset, length),
        Selection::Unsatisfiable => (StatusCode::RANGE_NOT_SATISFIABLE, 0, 0),
    };
    let mime = meta
        .mime
        .as_deref()
        .filter(|m| ["image/jpeg", "image/png"].contains(m))
        .unwrap_or_else(|| media(&lease.entry.path));
    let disposition = if lease.entry.purpose == FileAccessPurpose::Preview
        && mime != "application/octet-stream"
    {
        "inline"
    } else {
        "attachment"
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_ENCODING, "identity")
        .header(header::CONTENT_LENGTH, length)
        .header(
            header::CONTENT_DISPOSITION,
            format!(
                "{disposition}; filename=\"download\"; filename*=UTF-8''{}",
                filename(&lease.entry.path)
            ),
        )
        .header(
            header::CONTENT_SECURITY_POLICY,
            "sandbox; default-src 'none'",
        );
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", offset, offset + length - 1, meta.size),
        );
    }
    if status == StatusCode::RANGE_NOT_SATISFIABLE {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes */{}", meta.size));
    }
    if let Some(mtime) = meta.modified_ms {
        builder = builder.header(header::ETAG, format!("W/\"{}-{mtime}\"", meta.size));
    }
    let body = if method == Method::HEAD
        || status == StatusCode::RANGE_NOT_SATISFIABLE
        || length == 0
    {
        Body::empty()
    } else {
        let handshake = async {
            channel.write_u64(offset).await?;
            channel.write_u64(length).await
        };
        tokio::select! {
            biased;
            _=lease.revoked.changed()=>return Err(ApiError::unauthenticated("会话已撤销").into()),
            r=tokio::time::timeout(STALL,handshake)=>r.map_err(|_|ApiError::internal("文件通道握手超时"))?.map_err(|e|ApiError::internal(e.to_string()))?,
        }
        stream_body(channel, length, lease, worker.closed_signal())
    };
    let mut response = builder
        .body(body)
        .map_err(|e| ApiError::internal(e.to_string()))?;
    no_store(&mut response);
    Ok(response)
}

pub(super) fn stream_body(
    mut channel: IpcChannel,
    mut remaining: u64,
    mut lease: Lease,
    mut worker_closed: tokio::sync::watch::Receiver<bool>,
) -> Body {
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let failure = std::sync::Arc::new(std::sync::Mutex::new(None));
    let task_failure = failure.clone();
    tokio::spawn(async move {
        let result: io::Result<()> = async {
            while remaining > 0 {
                if *worker_closed.borrow() { return Err(io::Error::new(io::ErrorKind::BrokenPipe,"worker 已退出")); }
                let mut buf=vec![0;remaining.min(64*1024) as usize];
                let n=tokio::select! {
                    biased;
                    _=lease.revoked.changed()=>return Err(io::Error::new(io::ErrorKind::PermissionDenied,"会话已撤销")),
                    _=worker_closed.changed()=>return Err(io::Error::new(io::ErrorKind::BrokenPipe,"worker 已退出")),
                    _=tx.closed()=>return Ok(()),
                    r=tokio::time::timeout(STALL,channel.read(&mut buf))=>r.map_err(|_|io::Error::new(io::ErrorKind::TimedOut,"文件读取超时"))??,
                };
                if n==0 { return Err(io::Error::new(io::ErrorKind::UnexpectedEof,"文件提前结束")); }
                remaining-=n as u64;
                buf.truncate(n);
                tokio::select! {
                    biased;
                    _=lease.revoked.changed()=>return Err(io::Error::new(io::ErrorKind::PermissionDenied,"会话已撤销")),
                    _=worker_closed.changed()=>return Err(io::Error::new(io::ErrorKind::BrokenPipe,"worker 已退出")),
                    r=tokio::time::timeout(STALL,tx.send(Bytes::from(buf)))=> {
                        match r {
                            Ok(Ok(()))=>{},
                            Ok(Err(_))=>return Ok(()),
                            Err(_)=>return Err(io::Error::new(io::ErrorKind::TimedOut,"文件发送超时")),
                        }
                    },
                }
            }
            Ok(())
        }.await;
        *task_failure.lock().unwrap_or_else(|e| e.into_inner()) = result.err();
        // 先关闭文件通道和释放名额，再结束消息流；取消不依赖 HTTP body 的 poll。
        drop(channel);
        drop(lease);
        drop(tx);
    });
    Body::from_stream(futures::stream::unfold(
        (rx, failure),
        |(mut rx, failure)| async move {
            let item = match rx.recv().await {
                Some(bytes) => Some(Ok::<_, io::Error>(bytes)),
                None => failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .map(Err),
            };
            item.map(|item| (item, (rx, failure)))
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cookie_headers_and_safe_content_types() {
        let secret = "a".repeat(64);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!("other=yes; {}={secret}", registry::COOKIE)).unwrap(),
        );
        assert_eq!(cookie(&headers), Some(secret.as_str()));
        headers.append(
            header::COOKIE,
            HeaderValue::from_str(&format!("{}={secret}", registry::COOKIE)).unwrap(),
        );
        assert!(cookie(&headers).is_none(), "重复Cookie必须拒绝歧义");
        let secure_cookie = credential(&secret, true).to_str().unwrap().to_owned();
        for flag in [
            "HttpOnly",
            "SameSite=Strict",
            "Secure",
            "Path=/api/v1/file-access",
            "Max-Age=600",
        ] {
            assert!(secure_cookie.contains(flag));
        }
        assert!(
            !credential(&secret, false)
                .to_str()
                .unwrap()
                .contains("Secure")
        );
        for path in ["/a.html", "/a.svg", "/a.js", "/unknown", "/a.txt"] {
            assert_eq!(media(path), "application/octet-stream");
        }
        assert_eq!(
            filename("/tmp/报告\r\n\".pdf"),
            "%E6%8A%A5%E5%91%8A%0D%0A%22.pdf"
        );
    }
}
