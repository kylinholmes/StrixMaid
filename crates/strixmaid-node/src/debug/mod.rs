//! `/debug` 开发调试页（`design.md` §12.1）。
//!
//! 单文件页面：HTML 里内联 CSS 与 JS，唯一的外部资源是 `vendor/` 下 vendored 的
//! uPlot（画图）与 xterm.js（终端），与 `/api/docs` 的 Scalar 同样以 gzip 形态入库与发出。
//! 按模块分面板直接调 API 并展示结果，图表类数据用 uPlot 绘制。
//! **不做刻意的 UI 设计**——它的用途是验证接口，不是产品界面。
//!
//! # 每个面板独立容错
//!
//! 这是本页最重要的性质：某个端点未实现、返回错误、或该能力在本机不存在时，
//! **只有那一个面板显示错误，其余面板照常工作**。页面里没有任何一处
//! 「先取 A 成功了才敢取 B」的串联——因为调试页恰恰要在系统半残时还能用。
//!
//! # 与 release 的关系
//!
//! 本模块与 `/api/docs`、`/api/v1/openapi.json` 同受
//! `cfg(any(debug_assertions, feature = "apidoc"))` 门控，release 构建里不存在，
//! `vendor/` 那几个 `include_bytes!` 自然也一个字节都不进二进制——xterm.js 有 283 KiB，
//! 这正是它必须留在门控之内的原因。
//!
//! # 历史
//!
//! 这个目录曾经因 `.gitignore` 里一条裸的 `debug` 规则被整个吞掉、从未进入版本库
//! （见 `docs/gap-analysis.md` §7）。规则已改为锚定的 `/debug/`，页面按 §12.1 重写。

use axum::Router;
use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;

use crate::assets::gzipped;

/// 页面本体。内联 CSS/JS，因此只有这一个 HTML 文件。
const PAGE: &str = include_str!("page.html");

/// 性能面板（roadmap/08 §6–§8），由样稿实时化而来。内联 CSS/JS，uPlot 复用
/// `/debug/vendor/uplot.js` 无关——它自带内联 uPlot。与 `/debug` 同 cfg 门控。
const PERF: &str = include_str!("perf.html");

/// uPlot 运行时（v1.6.32，gzip）。来源与升级步骤见 `README.md`。
const UPLOT_JS_GZ: &[u8] = include_bytes!("vendor/uplot.iife.min.js.gz");
/// uPlot 样式（v1.6.32，gzip）。
const UPLOT_CSS_GZ: &[u8] = include_bytes!("vendor/uplot.min.css.gz");

/// xterm.js 运行时（`@xterm/xterm` v5.5.0，gzip）。来源与升级步骤见 `README.md`。
const XTERM_JS_GZ: &[u8] = include_bytes!("vendor/xterm.js.gz");
/// xterm.js 样式（同版本，gzip）。
///
/// 它不是「美化」：字符网格、光标、选区的定位全靠这份样式，缺了终端会整个错位，
/// 连 `FitAddon` 反算出来的 cols/rows 都是错的。
const XTERM_CSS_GZ: &[u8] = include_bytes!("vendor/xterm.css.gz");
/// `@xterm/addon-fit` v0.10.0（gzip）。按容器像素尺寸反算 cols/rows，
/// 结果经 `resize` 帧同步给 PTY（`roadmap/03-terminal.md` §4.4）。
const ADDON_FIT_JS_GZ: &[u8] = include_bytes!("vendor/addon-fit.js.gz");

/// 挂上 `/debug` 与它的 vendored 静态资源。
///
/// 签名与 [`crate::apidoc::attach`] 保持一致。
pub fn attach<S: Clone + Send + Sync + 'static>(router: Router<S>) -> Router<S> {
    tracing::info!("调试页已启用：/debug");
    router
        .route("/debug", get(page))
        .route("/debug/vendor/{file}", get(vendor))
        .route("/perf", get(perf))
}

/// `/` 的重定向目标。debug 构建下根路径 302 到这里（`design.md` §12.1）。
pub async fn index_redirect() -> Redirect {
    Redirect::temporary("/debug")
}

/// 性能面板。同 [`page`] 的 no-store 语义。
async fn perf() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PERF,
    )
        .into_response()
}

/// 页面。`no-store`——开发期改一行就要看到效果，缓存只会碍事。
async fn page() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        PAGE,
    )
        .into_response()
}

/// vendored 资源。版本已钉死，可长期强缓存。
async fn vendor(Path(file): Path<String>, headers: HeaderMap) -> Response {
    match file.as_str() {
        "uplot.js" => gzipped(&headers, "text/javascript; charset=utf-8", UPLOT_JS_GZ, true),
        "uplot.css" => gzipped(&headers, "text/css; charset=utf-8", UPLOT_CSS_GZ, true),
        "xterm.js" => gzipped(&headers, "text/javascript; charset=utf-8", XTERM_JS_GZ, true),
        "xterm.css" => gzipped(&headers, "text/css; charset=utf-8", XTERM_CSS_GZ, true),
        "addon-fit.js" => gzipped(
            &headers,
            "text/javascript; charset=utf-8",
            ADDON_FIT_JS_GZ,
            true,
        ),
        _ => (StatusCode::NOT_FOUND, "no such vendored asset\n").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 页面是自包含的() {
        // 唯一允许的外部引用是本地 vendor 路径；不能有任何 CDN / 外部域名，
        // 目标机器可能没有外网（design.md §12.1 对 Scalar 的要求同理）。
        assert!(!PAGE.contains("http://"), "页面里不该有明文 http 外链");
        // 性能面板同样必须自包含（CSP 禁外部请求）。github 归属注释里的 URL 除外。
        for host in ["fonts.googleapis", "cdn.", "unpkg", "jsdelivr"] {
            assert!(!PERF.contains(host), "perf.html 引用了外部资源：{host}");
        }
        assert!(PERF.contains("id=\"rail\"") && PERF.contains("id=\"detail\""));
        // §6.7 修正必须在位：成员格不画 PSI 压力带。
        assert!(PERF.contains("function psiIoOf"));
        for host in ["cdn.", "unpkg", "googleapis", "jsdelivr"] {
            assert!(!PAGE.contains(host), "页面引用了外部资源：{host}");
        }
        assert!(PAGE.contains("/debug/vendor/uplot.js"));
        assert!(PAGE.contains("/debug/vendor/uplot.css"));
        assert!(PAGE.contains("/debug/vendor/xterm.js"));
        assert!(PAGE.contains("/debug/vendor/xterm.css"));
        assert!(PAGE.contains("/debug/vendor/addon-fit.js"));
    }

    /// 每个「刷新」按钮都要有对应的面板。
    ///
    /// `data-reload="xxx"` 与 `definePanel("xxx", …)` 是靠字符串对上的，
    /// 拼错不会有任何报错——按钮就是按了没反应。这类静默失效正是调试页最不该有的。
    #[test]
    fn 每个刷新按钮都有对应面板() {
        let panels: Vec<&str> = PAGE
            .match_indices("definePanel(\"")
            .map(|(i, m)| {
                let rest = &PAGE[i + m.len()..];
                &rest[..rest.find('"').expect("definePanel 的第一个参数没有闭合引号")]
            })
            .collect();
        assert!(panels.len() >= 8, "面板数量异常：{panels:?}");

        for (i, m) in PAGE.match_indices("data-reload=\"") {
            let rest = &PAGE[i + m.len()..];
            let name = &rest[..rest.find('"').expect("data-reload 没有闭合引号")];
            assert!(
                panels.contains(&name),
                "按钮 data-reload=\"{name}\" 没有对应的 definePanel，点了不会有反应；\
                 已定义的面板：{panels:?}"
            );
        }

        // 面板挂载点也必须存在，否则 definePanel 里的 querySelector 会拿到 null
        for (i, m) in PAGE.match_indices("definePanel(\"") {
            let rest = &PAGE[i + m.len()..];
            let after_name = &rest[rest.find('"').unwrap() + 1..];
            let sec = after_name
                .split('"')
                .nth(1)
                .expect("definePanel 的第二个参数应是 section id");
            assert!(
                PAGE.contains(&format!("id=\"{sec}\"")),
                "面板绑定的 section id `{sec}` 在 HTML 里不存在"
            );
        }
    }

    #[test]
    fn vendored_资源确实是_gzip() {
        // gzip 魔数 1f 8b
        assert_eq!(&UPLOT_JS_GZ[..2], &[0x1f, 0x8b], "uplot.js 不是 gzip");
        assert_eq!(&UPLOT_CSS_GZ[..2], &[0x1f, 0x8b], "uplot.css 不是 gzip");
        assert_eq!(&XTERM_JS_GZ[..2], &[0x1f, 0x8b], "xterm.js 不是 gzip");
        assert_eq!(&XTERM_CSS_GZ[..2], &[0x1f, 0x8b], "xterm.css 不是 gzip");
        assert_eq!(&ADDON_FIT_JS_GZ[..2], &[0x1f, 0x8b], "addon-fit.js 不是 gzip");
        assert!(UPLOT_JS_GZ.len() > 10_000, "体积不对，可能没下全");
        // xterm.js 压缩后 65 KiB 出头；显著更小意味着下到的是 404 页或半截文件
        assert!(XTERM_JS_GZ.len() > 50_000, "xterm.js 体积不对，可能没下全");
        assert!(XTERM_CSS_GZ.len() > 1_000, "xterm.css 体积不对，可能没下全");
    }

    /// 页面里写的每个 `/debug/vendor/x` 都要真的能取到。
    ///
    /// 路由分支的字符串和 `page.html` 里的 URL 是两处独立的字面量，拼错不会有任何
    /// 编译错误——表现只是浏览器里 404 一个脚本，页面照开，终端面板静静地不工作。
    /// 这种静默失效正是调试页最不该有的，所以在这里对上一遍。
    #[tokio::test]
    async fn 页面引用的_vendor_文件都能取到() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT_ENCODING, "gzip".parse().unwrap());
        let mut checked = 0;
        for (i, m) in PAGE.match_indices("/debug/vendor/") {
            let rest = &PAGE[i + m.len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
                .collect();
            let r = vendor(Path(name.clone()), headers.clone()).await;
            assert_eq!(r.status(), StatusCode::OK, "页面引用了 {name}，但没有对应的路由分支");
            assert_eq!(r.headers()[header::CONTENT_ENCODING], "gzip", "{name} 没按 gzip 发出");
            checked += 1;
        }
        // 一条都没匹配上也会让上面的循环空转着通过，那等于这个测试不存在
        assert!(checked >= 5, "页面里只找到 {checked} 处 vendor 引用，太少了");
    }

    #[tokio::test]
    async fn 未知的_vendor_文件返回_404() {
        let r = vendor(Path("nope.js".to_owned()), HeaderMap::new()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn 页面不缓存() {
        let r = page().await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CACHE_CONTROL], "no-store");
    }
}
