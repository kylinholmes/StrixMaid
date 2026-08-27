//! OpenAPI 文档端点。
//!
//! # 只在 debug 构建（或显式开启 `apidoc` feature）时暴露
//!
//! ```text
//! #[cfg(any(debug_assertions, feature = "apidoc"))]
//! ```
//!
//! `debug_assertions` 精确对应「debug 版本才有」，不需要任何构建参数；再 OR 一个默认关闭的
//! `apidoc` feature，保留「构建一个带文档的 release 版给前端开发者用」这条路。
//!
//! 被 gate 掉的只有**把 API 表面暴露出去**的这几个路由：
//! `GET /api/v1/openapi.json`、`GET /api/docs`、`GET /api/docs/scalar.js`。
//! 业务端点在 release 里照常工作，`#[utoipa::path]` 标注与 `OpenApiRouter` 的
//! 路由收集也照常进行（[`attach`] 两个版本签名相同，关掉时只是把 `OpenApi` 丢弃，
//! 不产生 dead_code 警告）。
//!
//! # Scalar 的离线化
//!
//! `utoipa-scalar` 自带的 HTML 模板（`res/scalar.html`）最后一行是
//! `<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>`
//! —— 从 jsdelivr CDN 拉 JS。StrixMaid 的目标是内网/离线服务器，那条 `<script>` 只会转圈。
//! 因此这里改用 [`Scalar::custom_html`] 传入自己的模板，指向随二进制提供的
//! `src/vendor/scalar.standalone.js.gz`（`@scalar/api-reference@1.66.1` 的
//! `dist/browser/standalone.js`，版本已钉死）。
//!
//! 那份 JS **以 gzip 形态入库、以 gzip 形态发出**：仓库与二进制里都只有 1.04 MiB
//! 而非 3.63 MiB，运行时不解压，`CompressionLayer` 见到已有 `Content-Encoding`
//! 也会跳过（`tower_http::compression` 的 `should_compress` 第一个条件就是
//! 「响应没有 `Content-Encoding`」），不会重压一遍。
//! 详见 `src/vendor/README.md`。

use axum::Router;
use utoipa::openapi::OpenApi;


#[cfg(any(debug_assertions, feature = "apidoc"))]
mod enabled {
    use axum::Router;
    use axum::http::{HeaderMap, header};
    use axum::response::{Html, Response};
    use axum::routing::get;
    use utoipa::openapi::OpenApi;
    use utoipa_scalar::Scalar;

    use crate::assets::gzipped;


    /// Scalar 的 standalone 打包产物，**gzip 压缩后的字节**。
    ///
    /// 来源与升级步骤见 `src/vendor/README.md`。
    /// 原文 3,803,403 B，压缩后 1,092,519 B；原样带 `Content-Encoding: gzip` 发出，
    /// 由浏览器解压。只在本模块编译进来时才占空间。
    const SCALAR_JS_GZ: &[u8] = include_bytes!("vendor/scalar.standalone.js.gz");

    /// 替换 utoipa-scalar 默认模板：把 CDN 的 `<script src>` 换成本地路径。
    ///
    /// `$spec` 会被 [`Scalar::to_html`] 替换成序列化后的 OpenAPI 文档，因此页面自包含。
    ///
    /// `data-configuration` 关掉 Scalar 剩下的两处外部依赖（其值里的 `&quot;`
    /// 是 Scalar 自己约定的转义，它会先 `split("&quot;").join("\"")` 再 `JSON.parse`）：
    /// - `withDefaultFonts:false` —— 默认字体来自 `fonts.scalar.com`，离线时只会白等一轮超时；
    /// - `proxyUrl:""` —— 默认的 `proxy.scalar.com` 是给跨域 "Try it" 用的 CORS 代理。
    ///   我们的 spec 与页面同源，直连即可；留着它反而可能把请求发出机器。
    const SCALAR_HTML: &str = r#"<!doctype html>
<html>
<head>
  <title>StrixMaid API</title>
  <meta charset="utf-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1"/>
</head>
<body>
<script id="api-reference" type="application/json"
        data-configuration="{&quot;withDefaultFonts&quot;:false,&quot;proxyUrl&quot;:&quot;&quot;}">$spec</script>
<script src="/api/docs/scalar.js"></script>
</body>
</html>
"#;

    /// 挂上文档路由。
    ///
    /// 规范 JSON 与 HTML 都在启动时渲染一次，之后每次请求只是拷贝一个 `String`。
    pub fn attach<S: Clone + Send + Sync + 'static>(router: Router<S>, openapi: OpenApi) -> Router<S> {
        let spec_json = match serde_json::to_string(&openapi) {
            Ok(json) => json,
            Err(e) => {
                // 只影响文档端点，不该拖垮服务。
                tracing::error!(error = %e, "序列化 OpenAPI 文档失败，/api/v1/openapi.json 将返回空对象");
                "{}".to_owned()
            }
        };
        let docs_html = Scalar::new(openapi).custom_html(SCALAR_HTML).to_html();

        tracing::info!("API 文档已启用：/api/docs 与 /api/v1/openapi.json");

        router
            .route(
                "/api/v1/openapi.json",
                get(move || {
                    let json = spec_json.clone();
                    async move {
                        (
                            [(header::CONTENT_TYPE, "application/json")],
                            [(header::CACHE_CONTROL, "no-store")],
                            json,
                        )
                    }
                }),
            )
            .route(
                "/api/docs",
                get(move || {
                    let html = docs_html.clone();
                    async move { Html(html) }
                }),
            )
            .route("/api/docs/scalar.js", get(scalar_js))
    }

    /// 供 `/api/docs` 使用的 Scalar 运行时。版本钉死，故可长期强缓存。
    ///
    /// 响应体是仓库里那份 `.gz` 的原始字节，不在服务端解压——细节与
    /// `Accept-Encoding` 的处理见 [`crate::assets`]。
    async fn scalar_js(headers: HeaderMap) -> Response {
        gzipped(&headers, "text/javascript; charset=utf-8", SCALAR_JS_GZ, true)
    }
}

#[cfg(not(any(debug_assertions, feature = "apidoc")))]
mod disabled {
    use axum::Router;
    use utoipa::openapi::OpenApi;


    /// 文档端点被编译掉：原样返回路由，并丢弃已收集好的 OpenAPI 文档。
    pub fn attach<S: Clone + Send + Sync + 'static>(router: Router<S>, _openapi: OpenApi) -> Router<S> {
        router
    }
}

#[cfg(not(any(debug_assertions, feature = "apidoc")))]
use disabled::attach as attach_impl;
#[cfg(any(debug_assertions, feature = "apidoc"))]
use enabled::attach as attach_impl;

/// 按构建配置决定是否挂上 OpenAPI 文档端点。
pub fn attach<S: Clone + Send + Sync + 'static>(router: Router<S>, openapi: OpenApi) -> Router<S> {
    attach_impl(router, openapi)
}
