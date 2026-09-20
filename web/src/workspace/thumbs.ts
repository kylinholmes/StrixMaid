import { authHeaders } from "@/api/client";
import type { DirEntry } from "./useDirListing";

/**
 * 缩略图（roadmap/12 §4.7）：后端只送字节（`/files/raw`），浏览器自己解码
 * ——worker 里不进任何图片解码库，图片解析器历来是 CVE 重灾区。
 *
 * `<img src>` 带不了 Authorization 头，走裸 `fetch` + object URL（与进程
 * 图标同一条路，见 `api/client.ts` 的 `authHeaders`）。
 */

/** 超过这个大小不出缩略图（§4.7：小图走全尺寸带宽可以，大图不行），显示通用图标。 */
export const THUMB_MAX_BYTES = 8 * 1024 * 1024;

/** 浏览器能 `<img>` 解码的扩展名。 */
const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "svg", "avif"]);

/** 这个条目该不该出缩略图。 */
export function thumbEligible(e: DirEntry): boolean {
  if (e.kind !== "file" || e.size_bytes > THUMB_MAX_BYTES) return false;
  const dot = e.name.lastIndexOf(".");
  if (dot < 0) return false;
  return IMAGE_EXT.has(e.name.slice(dot + 1).toLowerCase());
}

/** path → object URL。会话级缓存：同一目录反复进出不重取。 */
const cache = new Map<string, string>();
const inflight = new Map<string, Promise<string | null>>();

/** 取一个文件的缩略图 object URL；失败返回 `null`（显示通用图标，不重试轰炸）。 */
export function fetchThumb(path: string): Promise<string | null> {
  const hit = cache.get(path);
  if (hit) return Promise.resolve(hit);
  const going = inflight.get(path);
  if (going) return going;

  const headers = authHeaders();
  if (!headers) return Promise.resolve(null);
  const p = (async () => {
    try {
      const resp = await fetch(`/api/v1/files/raw?path=${encodeURIComponent(path)}`, { headers });
      if (!resp.ok) return null;
      const blob = await resp.blob();
      const url = URL.createObjectURL(blob);
      cache.set(path, url);
      return url;
    } catch {
      return null;
    } finally {
      inflight.delete(path);
    }
  })();
  inflight.set(path, p);
  return p;
}
