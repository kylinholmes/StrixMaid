/**
 * 图标的取回与缓存（与 React 无关，便于单测）。
 *
 * 进程页与服务页各有一套图标端点，取回的规则却一模一样：带鉴权头 fetch、
 * 只认 PNG、成功与失败都缓存 5 分钟、同名并发只发一次。这里是那套规则的**唯一**
 * 一份实现，两个页面各用 {@link createIconSource} 建一个自己的源，区别只有
 * 「key 怎么变成 URL」和「什么样的 key 值得发请求」。
 *
 * **为什么不是裸 `<img src="/api/v1/...">`**：本项目的鉴权是
 * `Authorization: Bearer <token>` 请求头，token 刻意不进 URL（理由见 `lib/ws.ts`），
 * 而浏览器发 `<img>` 请求时不会带这个头；图标端点与同组其余端点一样受保护，
 * 于是裸 `<img>` 必然 401。改成带头的 `fetch` 取回 PNG、转成 data URL 再交给 `<img>`，
 * 与后端的契约（URL 形状、`image/png`、失败 404）一字不动。
 *
 * 顺带的好处：`fetch` 眼里 404/401 只是普通返回值，不像 `<img>` 的加载失败那样必定在
 * 控制台留下一行网络错误。取不到图标是常态——系统进程非管理员下取不到 exe 路径，
 * 服务里四分之三由 `svchost.exe` 托管而它没有图标资源，非 Windows 平台整组端点都不存在
 * ——常态不该刷控制台。
 */

import { authHeaders } from "@/api/client";

/** 客户端缓存 TTL，与服务端缓存及响应里的 `Cache-Control: max-age=300` 对齐。 */
const TTL_MS = 5 * 60 * 1000;
/** 每个源的缓存条数上限。名字数量有限，但不能让它无界增长；超出后淘汰最久未写入的。 */
const MAX_ENTRIES = 512;
/** 响应体上限。32×32 的 PNG 只有几 KB，超过这个数说明拿回来的不是图标。 */
const MAX_BYTES = 256 * 1024;
/** PNG 文件头。只认真正的 PNG，见 {@link createIconSource}。 */
const PNG_MAGIC = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
/** 分块转 base64 的块大小：一次展开太多实参会撑爆调用栈。 */
const B64_CHUNK = 0x8000;

interface Entry {
  /** 写入时刻。 */
  at: number;
  /** data URL；`null` 表示这个 key 取不到图标（负缓存）。 */
  src: string | null;
}

/** 一个图标来源：一个端点，加上它自己的一份缓存。 */
export interface IconSource {
  /** 这个 key 值不值得发请求（服务端会 400 的形态先挡掉）。 */
  accepts(key: string): boolean;
  /** key 对应的端点 URL。 */
  url(key: string): string;
  /**
   * 同步读缓存。`null` 同时表示「还没取」与「取不到」，两者的区别用
   * {@link IconSource.settled} 区分。
   */
  peek(key: string): string | null;
  /** 缓存里已经有未过期的结论（无论成功还是取不到）。 */
  settled(key: string): boolean;
  /** 确保这个 key 取过一次。失败不抛——取不到图标不是异常路径。 */
  ensure(key: string): Promise<void>;
  /** 清空本源的缓存。生产里靠 TTL 自然过期，这个入口是给测试用的。 */
  clear(): void;
}

/** 名字里不该出现路径分隔符、`..`，也不该超长——服务端对这三类一律回 400。 */
export function isPlainName(name: string, maxLen: number): boolean {
  if (name === "" || name.length > maxLen) return false;
  if (name.includes("/") || name.includes("\\")) return false;
  return !name.includes("..");
}

function isPng(bytes: Uint8Array): boolean {
  return bytes.length >= PNG_MAGIC.length && PNG_MAGIC.every((b, i) => bytes[i] === b);
}

function toDataUrl(bytes: Uint8Array): string {
  let bin = "";
  for (let i = 0; i < bytes.length; i += B64_CHUNK) {
    bin += String.fromCharCode(...bytes.subarray(i, i + B64_CHUNK));
  }
  return `data:image/png;base64,${btoa(bin)}`;
}

/**
 * 建一个图标来源。每个源有**自己的**缓存，互不干扰：两个端点的 key 语义不同
 * （进程侧是映像名，服务侧是服务名），混在一张表里同名的两者会互相顶掉。
 *
 * @param url 把 key 变成端点 URL，负责转义。
 * @param accepts 哪些 key 值得发请求，缺省全放行（例如只有一张图的通用图标端点）。
 */
export function createIconSource({
  url,
  accepts = () => true,
}: {
  url: (key: string) => string;
  accepts?: (key: string) => boolean;
}): IconSource {
  /** Map 保持插入序，淘汰时取最前面那个。 */
  const cache = new Map<string, Entry>();
  /** 同 key 并发去重：一屏里同名的行可以有几十个，只该发一次请求。 */
  const inflight = new Map<string, Promise<void>>();

  const fresh = (key: string): boolean => {
    const e = cache.get(key);
    return e !== undefined && Date.now() - e.at < TTL_MS;
  };

  const put = (key: string, src: string | null): void => {
    // 先删后插：刷新过的条目回到队尾，淘汰的才是真正最久没动过的
    cache.delete(key);
    cache.set(key, { at: Date.now(), src });
    while (cache.size > MAX_ENTRIES) {
      const oldest = cache.keys().next();
      if (oldest.done === true) break;
      cache.delete(oldest.value);
    }
  };

  const load = async (key: string): Promise<void> => {
    if (!accepts(key)) {
      put(key, null);
      return;
    }
    const headers = authHeaders();
    // 没有会话就别发：必然 401，而把 401 记进负缓存会让登录后的五分钟内都没有图标
    if (headers === null) return;
    try {
      const res = await fetch(url(key), { headers });
      // 401 说明的是会话问题，不是「这个东西没有图标」，因此不写负缓存
      if (res.status === 401) return;
      if (!res.ok) {
        // 404 才是常态：名字匹配不到、取不到可执行文件、或平台上根本没这个端点
        put(key, null);
        return;
      }
      const buf = await res.arrayBuffer();
      if (buf.byteLength === 0 || buf.byteLength > MAX_BYTES) {
        put(key, null);
        return;
      }
      const bytes = new Uint8Array(buf);
      // 只认真正的 PNG：万一中间层塞回一张错误页，宁可当作没有图标，也不要把它交给
      // <img> 去解码失败——那才会在控制台留下报错
      put(key, isPng(bytes) ? toDataUrl(bytes) : null);
    } catch {
      // 网络断开或请求被中止：当作这次没取到，不写负缓存，下次渲染再试
    }
  };

  return {
    accepts,
    url,
    peek: (key) => cache.get(key)?.src ?? null,
    settled: fresh,
    ensure(key) {
      if (fresh(key)) return Promise.resolve();
      const running = inflight.get(key);
      if (running !== undefined) return running;
      const task = load(key).finally(() => {
        inflight.delete(key);
      });
      inflight.set(key, task);
      return task;
    },
    clear() {
      cache.clear();
      inflight.clear();
    },
  };
}
