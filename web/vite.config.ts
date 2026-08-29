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
    // 字体子集只有 10 KB，内联成 data URI 反而省一次请求
    assetsInlineLimit: 16 * 1024,
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
