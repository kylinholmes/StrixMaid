/**
 * 进程图标：端点是 `GET /api/v1/processes/icon/{name}`。
 *
 * 取回与缓存的规则全在 `@/lib/icons`，与服务页共用同一份实现；本文件只定下
 * 这个端点的两件事：URL 怎么拼，以及什么样的名字值得发请求。
 *
 * **key 是映像名而不是 pid 或 exe 路径**：pid 每次重启都变，exe 路径在程序升级后
 * 也会变，两者都会让 URL 与浏览器缓存频繁失效，名字是三者里最稳的。代价是同名
 * 不同程序共用一张图标（机器上三个 `python.exe` 只会有一张），这是已经接受的取舍。
 *
 * **取不到就留空，不回落到任何通用图形**。服务页那边取不到时会换成一个齿轮，
 * 那是因为「服务」是一个有公认图形的类别；进程没有这种类别——取不到的原因是权限
 * 不够或那个程序压根没有图标资源，给它贴一个齿轮会暗示「这是个服务」，语义不对。
 */

import { createIconSource, isPlainName } from "@/lib/icons";

/** name 长度上限，与服务端消毒规则同界。 */
const MAX_NAME = 255;

const source = createIconSource({
  url: (name) => `/api/v1/processes/icon/${encodeURIComponent(name)}`,
  accepts: (name) => isPlainName(name, MAX_NAME),
});

/**
 * 名字是否值得发请求。
 *
 * 服务端对含路径分隔符、`..` 或超长的 name 直接回 400，客户端照同一套规则先挡一道，
 * 省掉注定失败的一次往返。这里**不做**路径消毒的活——名字最终是拿去反查当前进程表的，
 * 不落到文件系统上。
 */
export function isIconName(name: string): boolean {
  return source.accepts(name);
}

/** 图标 URL。`name` 里的空格等字符要转义，否则拼出来的路径不合法。 */
export function iconUrl(name: string): string {
  return source.url(name);
}

/**
 * 同步读缓存。`null` 同时表示「还没取」与「取不到」——两种情况的呈现是一样的（空位），
 * 调用方不需要区分。
 */
export function peekIcon(name: string): string | null {
  return source.peek(name);
}

/**
 * 确保某个名字的图标已取过一次。已有未过期结论时立即返回，同名并发只发一次请求。
 * 失败不抛——取不到图标不是异常路径。
 */
export function ensureIcon(name: string): Promise<void> {
  return source.ensure(name);
}
