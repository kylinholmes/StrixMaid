//! 前端静态资源。
//!
//! 用 `rust-embed` 嵌入 `web/dist`（§4：编译期嵌入，debug 模式改为磁盘读，热更新不受影响）：
//!
//! - **release**：文件内容以 `&'static [u8]` 编进二进制，运行时无任何磁盘 IO；
//! - **debug**：宏只记下编译期算出的**绝对**路径，每次请求现读磁盘 —— 改 `web/dist`
//!   下的文件后刷新页面即可生效，不用重新编译。因为记的是绝对路径，
//!   从任意工作目录启动都能读到（`cargo run` 与直接跑 `target/debug/strixmaid` 行为一致）。
//!
//! 这一行为差异由 rust-embed 依据 `debug_assertions` 自行切换，我们不需要写 cfg。
//!
//! # SPA 回退
//!
//! 未命中静态文件的请求返回 `index.html`，交给前端路由。但 `/api` 与 `/ws` 前缀例外：
//! 这两处的未命中是**真的 404**，回退成 HTML 只会让调用方拿到一个 200 的 HTML 而不自知。

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;
use strixmaid_types::ApiError;

use crate::error::ApiErr;

/// `web/dist` 的编译期视图。
///
/// 路径相对于本 crate 的 `CARGO_MANIFEST_DIR`。
#[derive(Embed)]
#[folder = "../../web/dist"]
struct WebAssets;

/// 全局 fallback：静态文件 → SPA `index.html` → 404。
pub async fn fallback(uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();

    // API 与 WS 命名空间不做 SPA 回退。
    if is_api_namespace(path) {
        return ApiErr(ApiError::not_found(format!("没有这个端点: {path}"))).into_response();
    }

    let rel = path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    if let Some(file) = WebAssets::get(rel) {
        return serve(rel, file, &headers);
    }

    // 最后一段带扩展名（`/foo.js`、`/img/x.png`）的路径是在要一个具体文件，
    // 找不到就该 404，不能回退成 index.html——浏览器请求 JS 却收到一段 HTML
    // 是最难排查的一类前端故障（报错只有一句 "Unexpected token '<'"）。
    // 只有无扩展名的路径（`/services`、`/logs/xyz`）才可能是前端路由。
    if looks_like_file(rel) {
        return (StatusCode::NOT_FOUND, "no such static file").into_response();
    }

    // 未命中 → 交给前端路由。
    match WebAssets::get("index.html") {
        Some(file) => serve("index.html", file, &headers),
        None => (
            StatusCode::NOT_FOUND,
            "前端资源未构建：web/dist 里没有 index.html",
        )
            .into_response(),
    }
}

/// 路径最后一段是否是在要一个**静态资源文件**（按扩展名白名单判断）。
///
/// 不能用「含点就是文件」这种启发式——本应用的前端路由天然带点：
/// `/services/nginx.service`、`/services/docker.socket`、`/logs/sshd.service`。
/// 所以只把已知的静态资源扩展名当文件，其余一律视为前端路由交给 SPA。
/// 新增一种前端产物类型时把扩展名加进 `STATIC_EXTS` 即可。
fn looks_like_file(rel: &str) -> bool {
    const STATIC_EXTS: &[&str] = &[
        "js",
        "mjs",
        "css",
        "map",
        "html",
        "json",
        "txt",
        "xml",
        "webmanifest",
        "png",
        "jpg",
        "jpeg",
        "gif",
        "svg",
        "ico",
        "webp",
        "avif",
        "woff",
        "woff2",
        "ttf",
        "otf",
        "eot",
        "wasm",
    ];
    let last = rel.rsplit('/').next().unwrap_or(rel);
    match last.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => {
            STATIC_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e))
        }
        _ => false,
    }
}

/// `/api`、`/ws` 及其子路径。
fn is_api_namespace(path: &str) -> bool {
    for ns in ["/api", "/ws"] {
        if path == ns || path.starts_with(&format!("{ns}/")) {
            return true;
        }
    }
    false
}

/// 把一个嵌入文件渲染成响应：Content-Type + ETag + Cache-Control，并处理条件请求。
fn serve(rel: &str, file: rust_embed::EmbeddedFile, req_headers: &HeaderMap) -> Response {
    let etag = format!("\"{}\"", hex32(&file.metadata.sha256_hash()));

    // 内容寻址的构建产物哈希不变即内容不变，直接 304。
    if let Some(inm) = req_headers.get(header::IF_NONE_MATCH)
        && inm.as_bytes() == etag.as_bytes()
    {
        return (StatusCode::NOT_MODIFIED, cache_headers(rel, &etag)).into_response();
    }

    let mut headers = cache_headers(rel, &etag);
    if let Ok(v) = HeaderValue::from_str(&content_type(rel)) {
        headers.insert(header::CONTENT_TYPE, v);
    }

    (headers, Body::from(file.data)).into_response()
}

/// 由扩展名猜 MIME，并给文本类型补上 `charset=utf-8`。
///
/// `mime_guess` 对 `.html` / `.js` / `.css` 返回的都是不带 charset 的类型；
/// 前端构建产物一律是 UTF-8，显式声明可以省掉浏览器的编码嗅探。
fn content_type(rel: &str) -> String {
    let mime = mime_guess::from_path(rel).first_or_octet_stream();
    let needs_charset = mime.type_() == mime_guess::mime::TEXT
        || matches!(
            (mime.type_().as_str(), mime.subtype().as_str()),
            ("application", "javascript") | ("application", "json")
        );
    if needs_charset && mime.get_param(mime_guess::mime::CHARSET).is_none() {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_string()
    }
}

/// 缓存策略：
/// - `assets/` 下是 Vite 产出的带内容哈希的文件名 → 一年 immutable；
/// - 其余（尤其 `index.html`）→ `no-cache`，即允许缓存但每次必须带 ETag 回源校验。
///
/// 两者都带 ETag，所以「no-cache」的实际代价是一次 304，不是一次全量传输。
fn cache_headers(rel: &str, etag: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let cc = if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cc));
    if let Ok(v) = HeaderValue::from_str(etag) {
        headers.insert(header::ETAG, v);
    }
    headers
}

/// 32 字节摘要转小写十六进制（不值得为此拉一个 `hex` 依赖）。
fn hex32(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_like_paths_do_not_fall_back_to_spa() {
        // 带扩展名 → 是在要具体文件
        assert!(looks_like_file("missing.js"));
        assert!(looks_like_file("img/x.png"));
        assert!(looks_like_file("assets/app.abc123.css"));
        assert!(looks_like_file("favicon.ICO"));
        assert!(looks_like_file("app.wasm"));
        // 无扩展名 → 前端路由
        assert!(!looks_like_file("services"));
        // 前端路由天然带点（unit 名），必须交给 SPA 而不是 404
        assert!(!looks_like_file("services/nginx.service"));
        assert!(!looks_like_file("services/docker.socket"));
        assert!(!looks_like_file("logs/sshd.service"));
        assert!(!looks_like_file("files/etc/hosts.backup"));
        assert!(!looks_like_file("v1.2/overview"));
        // 点开头的隐藏名不算扩展名
        assert!(!looks_like_file(".well-known"));
        assert!(!looks_like_file(""));
    }
}
