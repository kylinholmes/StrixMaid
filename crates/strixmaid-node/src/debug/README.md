# `/debug` 的 vendored 资源

## `uplot.iife.min.js.gz` / `uplot.min.css.gz`

指标面板画图用的 [uPlot](https://github.com/leeoniya/uPlot)。

| 项 | 值 |
|---|---|
| 包 | `uplot` |
| **版本（已钉死）** | **1.6.32** |
| 来源 URL | `https://cdn.jsdelivr.net/npm/uplot@1.6.32/dist/uPlot.iife.min.js` |
| | `https://cdn.jsdelivr.net/npm/uplot@1.6.32/dist/uPlot.min.css` |
| 原文体积 | 51,081 B / 1,857 B |
| 本仓库存储体积 | 22,009 B / 772 B（gzip -9） |

### 为什么是 iife 构建

`dist/uPlot.esm.js` 要 `<script type="module">`，而模块脚本受 CORS 与 MIME 的严格限制，
调试页却要在各种奇怪的部署形态下都能打开。iife 构建把 `uPlot` 挂到全局，一个
普通 `<script>` 就能用。

### 为什么要 vendor

与 `src/vendor/README.md` 里 Scalar 的理由完全相同：StrixMaid 面向内网 / 离线服务器，
指向 CDN 的 `<script>` 只会转圈。`design.md` §12.1 对调试页的要求就是「vendored uPlot」。

### 为什么存成 `.gz`

同 Scalar：仓库里存压缩后的字节，`debug/mod.rs` 带 `Content-Encoding: gzip` 原样发出，
运行时不解压。这两个文件只在 `cfg(any(debug_assertions, feature = "apidoc"))` 下
被 `include_bytes!`，release 构建里一个字节都不进二进制。

### 升级步骤

```sh
V=1.6.33   # 新版本号
cd crates/strixmaid-server/src/debug/vendor
curl -fsSL "https://cdn.jsdelivr.net/npm/uplot@$V/dist/uPlot.iife.min.js" | gzip -9 > uplot.iife.min.js.gz
curl -fsSL "https://cdn.jsdelivr.net/npm/uplot@$V/dist/uPlot.min.css"     | gzip -9 > uplot.min.css.gz
```

然后更新上表的版本号与体积，并在浏览器里确认指标面板的 band 图仍然正常
（`debug/mod.rs` 的单测只校验 gzip 魔数与体积下限，挡不住 API 变更）。

## `xterm.js.gz` / `xterm.css.gz` / `addon-fit.js.gz`

终端面板用的 [xterm.js](https://github.com/xtermjs/xterm.js)（`roadmap/03-terminal.md` §4.7）。

| 项 | 值 |
|---|---|
| 包 | `@xterm/xterm` / `@xterm/addon-fit` |
| **版本（已钉死）** | **5.5.0** / **0.10.0** |
| 来源 URL | `https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.js` |
| | `https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.css` |
| | `https://cdn.jsdelivr.net/npm/@xterm/addon-fit@0.10.0/lib/addon-fit.js` |
| 原文体积 | 289,441 B / 5,559 B / 1,497 B |
| 本仓库存储体积 | 67,253 B / 2,079 B / 649 B（gzip -9） |

### 为什么是 `lib/*.js`（UMD）而不是 esm

理由同 uPlot 的 iife：包里 `lib/xterm.js` 与 `lib/addon-fit.js` 是 UMD 构建，普通
`<script>` 就能加载并把 `Terminal`、`FitAddon.FitAddon` 挂到全局；`.mjs` 要
`<script type="module">`，而模块脚本受 CORS 与 MIME 的严格限制，调试页却要在各种
奇怪的部署形态下都能打开。

### 为什么 `xterm.css` 是必需的而不是可选的

它不是「美化」。xterm.js 把每一行渲染成绝对定位的字符网格，行高、字符盒、光标与
选区的定位全部由这份样式决定；不加载它终端会整个错位，连 `FitAddon` 按容器像素
反算出来的 cols/rows 都是错的——而那个值会经 `resize` 一路传到 PTY 的
`ioctl(TIOCSWINSZ)`，错的尺寸会让远端 shell 的换行也跟着错。

### 为什么要连 `addon-fit` 一起 vendor

它只有 1.5 KB，逻辑也简单（读容器与 `.xterm` 的 computed style，除以单元格宽高）。
自己写一份等于把 xterm.js 内部的 `_renderService.dimensions` 这个私有字段抄进
页面里，版本一升就会静默失效；用官方 addon 至少版本是对齐的。

### 为什么存成 `.gz`

同 uPlot 与 Scalar：仓库里存压缩后的字节，`debug/mod.rs` 带 `Content-Encoding: gzip`
原样发出，运行时不解压。xterm.js 原文 283 KiB，它只在
`cfg(any(debug_assertions, feature = "apidoc"))` 下被 `include_bytes!`，
release 构建里一个字节都不进二进制。

### 升级步骤

```sh
XV=5.5.1     # @xterm/xterm 新版本号
FV=0.10.0    # @xterm/addon-fit 新版本号
cd crates/strixmaid-server/src/debug/vendor
curl -fsSL "https://cdn.jsdelivr.net/npm/@xterm/xterm@$XV/lib/xterm.js"          | gzip -9 > xterm.js.gz
curl -fsSL "https://cdn.jsdelivr.net/npm/@xterm/xterm@$XV/css/xterm.css"         | gzip -9 > xterm.css.gz
curl -fsSL "https://cdn.jsdelivr.net/npm/@xterm/addon-fit@$FV/lib/addon-fit.js"  | gzip -9 > addon-fit.js.gz
```

然后更新上表的版本号与体积，并在浏览器里开一个终端确认输入输出、`FitAddon` 的
尺寸同步、以及断开重连后的回放都正常（`debug/mod.rs` 的单测只校验 gzip 魔数与
体积下限，挡不住 API 变更）。addon 与主包的版本有兼容矩阵，两者最好一起升。
