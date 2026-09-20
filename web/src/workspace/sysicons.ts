import { useEffect, useState } from "react";
import { authHeaders } from "@/api/client";

/**
 * 系统文件类型图标（roadmap/12 §8 未决 8）：按扩展名向 `GET /files/icon/{ext}`
 * 取一张系统真图标，优先于内置图标集；后端 404（Linux、无窗口服务器的 macOS）
 * 就回落内置集（`icons.ts`）。
 *
 * `<img src>` 带不了 Authorization 头，走裸 `fetch` + object URL——与缩略图
 * （`thumbs.ts`）、进程图标同一条路。
 *
 * # 先探测一次，再按扩展名取
 *
 * 前端只知道服务端是 unix 还是 windows（`path.ts` 的 `Platform`），分不出
 * mac 与 linux；而 Linux 后端对任何扩展名都是 404。不探测的话，每次会话都会
 * 对着不支持的后端按扩展名种类发一串注定 404 的请求。做法：首次要图标时先用
 * `txt` 问一次——支持的平台上 `txt` 必然有图（未知类型都有「白纸」回落，
 * 见 `providers/fs/icon.rs`），404 就整个会话不再来问。
 */

/** 会话级缓存：扩展名 → object URL；`null` 是负缓存（这一类真的取不到）。 */
const cache = new Map<string, string | null>();
const inflight = new Map<string, Promise<string | null>>();

/** 平台探测的结论。`null` = 还没问过或上次因网络/未登录没问成，可以再试。 */
let availability: Promise<boolean> | null = null;

/** 探测用的扩展名，顺手把它自己的图标也存进缓存。 */
const PROBE_EXT = "txt";

/**
 * 取文件名的扩展名（小写、不含点），取不出（无点、dotfile、尾点、
 * 含后端会拒绝的字符）返回 `null`。规则与 `providers/fs/icon.rs` 的
 * `validate_ext` 对齐——取不出的名字根本不该发请求。
 */
export function extOf(name: string): string | null {
  const dot = name.lastIndexOf(".");
  if (dot <= 0 || dot === name.length - 1) return null;
  const ext = name.slice(dot + 1).toLowerCase();
  if (ext.length > 32 || /[./\\:*?"<>|\s]/.test(ext)) return null;
  return ext;
}

async function fetchIconBlob(ext: string): Promise<string | null> {
  const headers = authHeaders();
  if (!headers) return null;
  const resp = await fetch(`/api/v1/files/icon/${encodeURIComponent(ext)}`, { headers });
  if (!resp.ok) return null;
  return URL.createObjectURL(await resp.blob());
}

/**
 * 后端到底提供不提供系统图标。结论对整个会话生效；没问成（未登录、断网、5xx）
 * 不定论——`availability` 保持 / 重置为 `null`，下次再问。
 *
 * 未登录的分支在**创建 promise 之前**判：async 体里对 `availability` 的重置
 * 会被 `availability = promise` 这句赋值覆盖（体的同步段先跑），「不知道」
 * 就被永久当成了「不支持」。
 */
function probe(): Promise<boolean> {
  if (availability !== null) return availability;
  const headers = authHeaders();
  if (!headers) return Promise.resolve(false); // 还没登录：不定论
  const p = (async () => {
    try {
      const resp = await fetch(`/api/v1/files/icon/${PROBE_EXT}`, { headers });
      if (resp.ok) {
        cache.set(PROBE_EXT, URL.createObjectURL(await resp.blob()));
        return true;
      }
      if (resp.status === 404) return false; // 平台不提供：txt 在支持的平台上必有图
      availability = null; // 5xx 之类：不定论
      return false;
    } catch {
      availability = null;
      return false;
    }
  })();
  availability = p;
  return p;
}

/** 取一个扩展名的系统图标 object URL；取不到返回 `null`（回落内置集）。 */
export function fetchTypeIcon(ext: string): Promise<string | null> {
  const hit = cache.get(ext);
  if (hit !== undefined) return Promise.resolve(hit);
  const going = inflight.get(ext);
  if (going) return going;

  const p = (async () => {
    try {
      if (!(await probe())) return null;
      const url = await fetchIconBlob(ext);
      // 只有确定性的「没有」才负缓存；网络错误留给下次重试。
      if (url !== null || availability !== null) cache.set(ext, url);
      return url;
    } catch {
      return null;
    } finally {
      inflight.delete(ext);
    }
  })();
  inflight.set(ext, p);
  return p;
}

/**
 * React 侧入口：`name` 是文件名（目录与符号链接传 `null`），返回系统图标的
 * object URL 或 `null`。命中会话缓存时首帧即有值，不闪内置图标。
 */
export function useSysIcon(name: string | null): string | null {
  const ext = name === null ? null : extOf(name);
  const [url, setUrl] = useState<string | null>(() =>
    ext === null ? null : (cache.get(ext) ?? null),
  );

  useEffect(() => {
    if (ext === null) {
      setUrl(null);
      return;
    }
    let live = true;
    void fetchTypeIcon(ext).then((u) => {
      if (live) setUrl(u);
    });
    return () => {
      live = false;
    };
  }, [ext]);

  return ext === null ? null : url;
}

/** 测试用：清掉模块级状态（缓存与探测结论都是会话级单例）。 */
export function resetForTest(): void {
  cache.clear();
  inflight.clear();
  availability = null;
}
