import { fileURLToPath, URL } from "node:url";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// 后端在 debug 构建下由 rust-embed 现读磁盘上的 web/dist（见
// crates/strixmaid-server/src/embed.rs），所以产物直接写进 dist 即可，
// 改前端不需要重编 Rust。开发时走 Vite dev server + 代理。
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // **字体永不内联成 data URI。**
    //
    // 这一项原来是一个数字 16 KB，为的是让唯一那对 IBM Plex Mono 子集
    //（各 10 KB）内联进 CSS，省两次请求。那时全站只有一套字体，内联确实划算。
    //
    // 第六版起 `styles/base.css` 里躺着三套设计语言的十份 woff2，前提变了：
    // 同一时刻只有一套语言的 `--ui` / `--mono` 生效，浏览器也只去取那一套真正
    // 匹配上的那几份（`@font-face` 的加载是惰性的）。这个性质的前提是字体以
    // URL 形式引用。一旦内联，字体就成了样式表字节流的一部分，跟着 CSS 无条件
    // 下载：选 Fluent 的用户要把 Ubuntu 与 Adwaita 的字形一起背走，何况 base64
    // 还要再胖三分之一。十份内联进来就是三百多 KB 的首屏 CSS。
    //
    // 于是按扩展名排除：`false` 是「永不内联」，`undefined` 是「交回 Vite 的
    // 默认判断」。改成回调之后那个 16 KB 的数字就没有位置了，也不再需要——
    // 它当初只为字体而设，而字体现在正是被排除的那一类；其余资源按 Vite 默认的
    // 4 KB 走（今天 `src/assets` 下除了字体没有别的东西）。
    assetsInlineLimit: (filePath) => (filePath.endsWith(".woff2") ? false : undefined),
  },
  server: {
    port: 5173,
    // localtest.me / lvh.me 都解析到 127.0.0.1。允许它们只是为了让
    // 那些拒绝访问裸 localhost 的工具（浏览器扩展的安全分类、部分代理）
    // 能打开开发服务器。仅 dev 生效，与产物无关。
    allowedHosts: ["localtest.me", "lvh.me"],
    proxy: {
      "/api": { target: "http://127.0.0.1:9700", changeOrigin: true },
      "/ws": { target: "ws://127.0.0.1:9700", ws: true },
    },
  },
});
