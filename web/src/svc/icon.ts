/**
 * 服务图标：两个端点，一张真图标加一张通用齿轮。
 *
 * | 端点 | 给什么 |
 * |---|---|
 * | `GET /api/v1/services/icon/{name}` | **这个服务自己**的可执行文件里的图标，取不到 404 |
 * | `GET /api/v1/services/icon-generic` | 所有服务共用的齿轮，与 `services.msc` 里那张同源 |
 *
 * 取回与缓存的规则全在 `@/lib/icons`，与进程页共用同一份实现。
 *
 * # 为什么齿轮要单独走一个端点
 *
 * 后端刻意**不**在按名字那个端点里回落成齿轮：那样一来 200 响应里既可能是这个服务
 * 真实的图标、也可能是一张通用图，调用方无从区分。回落是展示层的决定，因此由本文件
 * 在收到 404 之后去取通用那张——**整张表只发一次**（同 key 并发去重 + 5 分钟缓存），
 * 不是每行一次。
 *
 * # 为什么这是必须的
 *
 * 本机实测：317 个能解析出可执行文件的服务里，只有 17 个取得到自己的图标。四分之三
 * 的服务由 `svchost.exe` 托管，而它没有图标资源。也就是说**大部分行显示的都会是齿轮**
 * ——这正是 Windows 自己在 `services.msc` 里的做法。
 *
 * # 三态渲染，没有中间的闪烁
 *
 * {@link peekSvcIcon} 按「真图标 → 齿轮 → 空白」取值，而在**还没有结论**之前一律给
 * 空白：先画齿轮再换成真图标会让那 17 行各闪一次。非 Windows 平台上两个端点都 404，
 * 于是恒为空白——不画齿轮，因为那边根本没有「服务图标」这回事。
 */

import { createIconSource, isPlainName } from "@/lib/icons";

/**
 * 服务名长度上限，与服务端消毒规则同界（Windows 服务名 256 字符 + `.service` 后缀）。
 */
const MAX_NAME = 264;

/**
 * 通用图标只有一张，端点不带参数，这里用一个固定 key 占住它在缓存里的位置。
 * 调用方看不到这个 key。
 */
const GENERIC_KEY = "generic";

const byName = createIconSource({
  url: (name) => `/api/v1/services/icon/${encodeURIComponent(name)}`,
  accepts: (name) => isPlainName(name, MAX_NAME),
});

const generic = createIconSource({
  url: () => "/api/v1/services/icon-generic",
});

/** 某个服务自己的图标 URL。`name` 里有空格、括号、逗号，要转义。 */
export function svcIconUrl(name: string): string {
  return byName.url(name);
}

/** 通用齿轮的 URL。 */
export function genericIconUrl(): string {
  return generic.url(GENERIC_KEY);
}

/** 名字是否值得发请求。与服务端的消毒规则同界，省掉注定 400 的一次往返。 */
export function isSvcIconName(name: string): boolean {
  return byName.accepts(name);
}

/**
 * 渲染时同步读出这一行该显示哪张图：真图标 → 齿轮 → 空白。
 *
 * 还没有结论时给空白而不是齿轮，见模块文档「三态渲染」。
 */
export function peekSvcIcon(name: string): string | null {
  const own = byName.peek(name);
  if (own !== null) return own;
  if (!byName.settled(name)) return null;
  return generic.peek(GENERIC_KEY);
}

/**
 * 确保这一行的图标已经有结论：先取它自己的，取不到再确保通用那张取过一次。
 *
 * 第二步对整张表只会真的发一次请求——同 key 的并发被合并，之后 5 分钟内命中缓存。
 * 失败不抛：取不到图标是常态。
 */
export async function ensureSvcIcon(name: string): Promise<void> {
  await byName.ensure(name);
  if (byName.peek(name) === null) await generic.ensure(GENERIC_KEY);
}

/** 清空两个源的缓存。生产里靠 TTL 自然过期，这个入口是给测试用的。 */
export function clearSvcIcons(): void {
  byName.clear();
  generic.clear();
}
