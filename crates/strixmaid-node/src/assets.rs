//! 以 gzip 形态入库、以 gzip 形态发出的静态资源。
//!
//! `/api/docs` 的 Scalar 运行时（3.8 MiB）与 `/debug` 的 uPlot（51 KiB）都走这条路：
//! 仓库里存 `.gz`，运行时**不解压**，直接带 `Content-Encoding: gzip` 发给浏览器。
//!
//! `tower_http::compression` 见到响应已有 `Content-Encoding` 会跳过，不会重压一遍
//! （它的 `should_compress` 第一个条件就是「响应没有 `Content-Encoding`」）。

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

/// 发一份预压缩的资源。
///
/// `cache` 为 `true` 时按「版本已钉死、可长期强缓存」发 `immutable`——
/// vendored 的第三方运行时都属于这一类，升级必然伴随路径或内容变化。
pub fn gzipped(
    headers: &HeaderMap,
    content_type: &'static str,
    body: &'static [u8],
    cache: bool,
) -> Response {
    if !accepts_gzip(headers) {
        // 这里只能发 gzip：解压需要额外依赖，而这些端点唯一的调用方是浏览器，
        // 浏览器一定接受 gzip。与其静默发一份对方声明不接受的编码，
        // 不如给一句能看懂的话。
        return (
            StatusCode::NOT_ACCEPTABLE,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "本端点只提供 gzip 编码的响应；请发送 `Accept-Encoding: gzip`（curl 用 --compressed）。\n",
        )
            .into_response();
    }

    let cache_control = if cache {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_ENCODING, "gzip"),
            // 自己补 Vary：`CompressionLayer` 只在它真的压缩了的时候才追加这一项，
            // 而这里它会跳过。少了它，共享缓存可能把 gzip 体交给声明 identity 的客户端。
            (header::VARY, "accept-encoding"),
            (header::CACHE_CONTROL, cache_control),
        ],
        body,
    )
        .into_response()
}

/// 客户端是否接受 gzip。
///
/// RFC 9110 §12.5.3：**没有** `Accept-Encoding` 时任何编码都算可接受；
/// 字段值为**空**则表示不想要任何编码。所以只有显式给出、且既不含 `gzip`
/// 也不含 `*` 的请求才判为不接受（空值自然落进这一档）。
/// 不解析 `q=0` 这种精细写法——真发 `gzip;q=0` 的客户端不存在。
pub fn accepts_gzip(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(header::ACCEPT_ENCODING) else {
        return true;
    };
    let Ok(value) = value.to_str() else {
        return true;
    };
    value
        .split(',')
        .map(|part| part.split(';').next().unwrap_or_default().trim())
        .any(|coding| coding.eq_ignore_ascii_case("gzip") || coding == "*")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(accept_encoding: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = accept_encoding {
            h.insert(header::ACCEPT_ENCODING, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn 接受_gzip_的判定() {
        assert!(accepts_gzip(&headers(None)), "没有该头时任何编码都可接受");
        assert!(accepts_gzip(&headers(Some("gzip"))));
        assert!(accepts_gzip(&headers(Some("gzip, deflate, br"))));
        assert!(accepts_gzip(&headers(Some("br;q=1.0, gzip;q=0.8"))));
        assert!(accepts_gzip(&headers(Some("*"))));
        assert!(accepts_gzip(&headers(Some("GZIP"))), "大小写不敏感");

        assert!(!accepts_gzip(&headers(Some(""))), "空值表示不想要任何编码");
        assert!(!accepts_gzip(&headers(Some("br"))));
        assert!(!accepts_gzip(&headers(Some("identity"))));
    }

    #[test]
    fn 不接受时给_406_而不是硬发_gzip() {
        let r = gzipped(&headers(Some("br")), "text/javascript", b"x", true);
        assert_eq!(r.status(), StatusCode::NOT_ACCEPTABLE);
    }

    #[test]
    fn 响应头齐全() {
        let r = gzipped(&headers(Some("gzip")), "text/javascript", b"x", true);
        assert_eq!(r.status(), StatusCode::OK);
        let h = r.headers();
        assert_eq!(h[header::CONTENT_ENCODING], "gzip");
        assert_eq!(h[header::VARY], "accept-encoding");
        assert!(h[header::CACHE_CONTROL].to_str().unwrap().contains("immutable"));

        // 不缓存的那一档
        let r = gzipped(&headers(Some("gzip")), "text/html", b"x", false);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    }
}
