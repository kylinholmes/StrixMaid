# A3 前端预览交付记录（2026-09-29）

本次只修改 `web/src/workspace` 的激活/预览组件与测试，以及本目录的预览测试。
没有新增依赖、提交、推送或切换分支。`schema.d.ts` 由协调者生成；访问 wrapper
使用其中的 `FileAccessResponse`、`FileAccessRequest` 和 `FileAccessPurpose`。

## 文件

- 修改：`src/workspace/activate.ts`、`activate.test.ts`、`FileList.tsx`、`ListPane.tsx`、`TileGrid.tsx`。
- 新增 `src/workspace/preview/`：`FilePreview.tsx`、`PreviewBody.tsx`、`TextPreview.tsx`、
  `Preview.module.css`、`access.ts`、`kind.ts`、`highlight.ts`、`syntaxC.ts`、`syntaxHash.ts`、
  `access.test.ts`、`highlight.test.ts`。
- 新增 `e2e/`：`preview.html`、`preview.tsx`、`preview.e2e.ts`、`preview-server.ts`、本记录；
  `fixtures/preview-image.png`、`fixtures/preview-seek.webm`。

## 行为

列表和平铺的普通文件均为单击选择、双击/Enter 预览。目录与链接保留导航/定位行为；
设备等特殊文件仅显示原因，禁用下载，不发读取请求。原生 modal dialog 约束焦点，
Esc/关闭恢复原条目焦点；主题变量沿用现有设计语言，文本可选择复制。

文本复用 `GET /api/v1/files/content`，640 KiB 以上直接降级；目录大小过时导致服务端
拒绝时显示真实原因。无效 UTF-8、截断均有提示。HTML/SVG 只进入 React 文本节点。

图片/PDF/音视频通过 Bearer POST 创建 preview 记录，原生元素接收 Cookie + 非秘密 id URL。
图片由后端 preview 用途返回最长边 1600px 且已转正的图片；前端不请求原图或二次转正。
PDF 使用空 sandbox 和 no-referrer，始终提供下载降级说明。媒体使用原生 controls/URL。
不支持、读取失败或续期失败会卸载读取元素并释放记录。

每 5 分钟续期；关闭/换文件/会话重置取消旧请求并释放记录。回收捕获旧会话 Bearer，
不能误用新账号。StrictMode 首轮同步回收不会发起多余的创建请求。
下载单独建立 download 记录，用原生 a 下载；不会 fetch 整文件或构造 Blob，关闭预览
不释放下载记录。下载记录在自然过期或会话重置时 DELETE；已获准流的寿命由后端处理。

## 实测

在 `web/` 运行：

- `bun run check`：通过，174 文件。
- `bun run typecheck`：通过。
- `bun run test`：24 测试文件，276/276 通过。
- `bun run build`：通过。
- `PLAYWRIGHT_CHANNEL=msedge bun run test:browser`：17/17 通过，其中预览专项 13/13。
- `git diff --check -- src/workspace e2e`：通过。

Edge 覆盖列表/平铺、单击/双击/Enter/Esc、焦点恢复及可见 outline、HTML/SVG 安全文本、
UTF-8 提示、640 KiB 超限及文件增长、lazy 加载失败、原生下载、Cookie 自动携带且 HttpOnly、
图片原生尺寸、PDF sandbox/下载、5min 续期及失败卸载、切换取消、会话清理。
音频与视频均在原生元素 seek 后观察到非零 bytes Range。测试 HTTP 服务对初始响应限速，
确保本地极速全量缓冲不会掩盖 seek 发起的新请求，结束后关闭服务与定时器。

测试字节来源是本地 HTTP 夹具：音频为 60s PCM WAV（5,292,044 bytes），
视频为 20s VP8 WebM（453,489 bytes）。下载验证的是 5 GiB **列表元数据**和实际
22 bytes `native-download-fixture` 落盘一致，不能当作真实 5 GiB 传输证据。
图片夹具为 1600×1000 PNG（169,438 bytes），不代替后端 EXIF/解码限额验收。
PDF 检查沙箱属性、生命周期和下载路径，没有将内置 PDF 插件成功渲染记为已验收；
浏览器限制安全嵌入时界面保留下载入口。真实 worker 权限、大文件/RSS/fd 与 Linux
发布物门槛仍由 A4 集成验收提供。

## 近似词法高亮与产物

不承诺完整语法或语义分析。只按 C 风格/井号注释两类词法着色（字符串、数字、注释、
关键字），不支持的语言保留纯文本。动态加载失败或超过 **20,000 tokens** 时回退完整纯文本。
测试覆盖 20,000/20,001 边界、Unicode/emoji/组合字符/双向控制字符、CRLF、恶意 HTML
逐字重组一致；着色始终使用 React span 文本节点。

`bun run build` 后实测独立 lazy chunks：

| chunk | 原始 bytes | gzip bytes |
|---|---:|---:|
| `highlight-DTvovXbD.js` | 1015 | 581 |
| `syntaxC-Deo1w38f.js` | 513 | 370 |
| `syntaxHash-B2qWYnCs.js` | 388 | 295 |
| 合计 | 1916 | 1246 |

本轮整个 `web/dist` 为 1,381,224 bytes；这不是 musl 发布物体积，也不替代 18 MiB 门槛。
测试图片、视频位于 e2e，不进入 Vite 发布入口。

## 截图与夹具复现

已查看实际 Edge 截图，主题和关闭按钮键盘焦点均可见：

- `/tmp/strixmaid-files-preview-text.png`
- `/tmp/strixmaid-files-preview-image.png`

图片/视频是 FFmpeg 合成测试图形，无外部图片来源。复现：

```sh
ffmpeg -f lavfi -i testsrc2=size=1600x1000:rate=1 -frames:v 1 e2e/fixtures/preview-image.png
ffmpeg -f lavfi -i testsrc2=size=320x180:rate=10 -t 20 -c:v libvpx -b:v 180k -g 10 -an e2e/fixtures/preview-seek.webm
```
