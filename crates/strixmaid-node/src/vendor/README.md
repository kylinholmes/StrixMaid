# vendored 第三方资源

## `scalar.standalone.js.gz`

`/api/docs` 用的 [Scalar](https://scalar.com/) API 文档渲染器运行时。

| 项 | 值 |
|---|---|
| 包 | `@scalar/api-reference` |
| **版本（已钉死）** | **1.66.1** |
| 来源 URL | `https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.66.1/dist/browser/standalone.js` |
| 原文体积 | 3,803,403 B |
| 本仓库存储体积 | 1,092,519 B（gzip -9） |

### 为什么要 vendor

`utoipa-scalar` 自带的 HTML 模板（`res/scalar.html`）最后一行是

```html
<script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
```

即从 jsdelivr CDN 拉取。StrixMaid 面向内网 / 离线服务器，那条 `<script>` 只会转圈，
所以改用 `Scalar::custom_html` 传入自定义模板，指向由本二进制自己提供的这份 JS。

### 为什么存成 `.gz`

3.8 MB 的文本进 git 是长期成本。压缩后约 1/3.5，且 `src/apidoc.rs` 直接带
`Content-Encoding: gzip` 原样发出——运行时不解压，`CompressionLayer` 见到已有
`Content-Encoding` 也会跳过，不会重压一遍。

### 升级步骤

1. 查最新版本号：

   ```sh
   curl -s 'https://data.jsdelivr.com/v1/packages/npm/@scalar/api-reference/resolved?specifier=latest'
   ```

2. 下载**钉死版本号**的 standalone 包并重新压缩（`-n` 不写入文件名与时间戳，
   保证同样输入产出同样字节，git diff 才有意义）：

   ```sh
   VER=1.66.1   # 换成新版本号
   curl -sSL "https://cdn.jsdelivr.net/npm/@scalar/api-reference@${VER}/dist/browser/standalone.js" \
     | gzip -9 -n > crates/strixmaid-server/src/vendor/scalar.standalone.js.gz
   ```

3. 更新本文件里的版本号与两个体积数字。

4. 复核 `src/apidoc.rs` 里 `SCALAR_HTML` 的 `data-configuration`。当前关掉了两处
   **运行时外部依赖**，升级后需确认这两个配置项仍然存在且语义未变：

   - `withDefaultFonts: false` —— 默认字体来自 `fonts.scalar.com`，离线时白等一轮超时；
   - `proxyUrl: ""` —— 默认值是 `https://proxy.scalar.com`，那是给跨域 "Try it" 用的
     CORS 代理。我们的 spec 与页面同源，直连即可；留着它会把管理请求发出本机，
     这在服务器管理工具里不可接受。

   排查办法：`gzip -dc scalar.standalone.js.gz | grep -oE 'https?://[a-zA-Z0-9.-]+' | sort -u`

5. 重新构建并打开 `/api/docs` 目视确认（需 debug 构建或 `--features apidoc`），
   同时用浏览器开发者工具的 Network 面板确认**没有任何跨域请求**。
