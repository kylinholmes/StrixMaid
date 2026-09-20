import { useEffect, useRef, useState } from "react";
import { authHeaders } from "@/api/client";
import type { DirEntry } from "./useDirListing";

/**
 * 缩略图（roadmap/12 §4.7，2026-09-21 改判）：**服务端缩好再下发**
 * （`GET /files/thumb`）。此前是下发原图让浏览器缩，并为带宽设了 8 MiB 的闸
 * ——相机直出的照片普遍 13～27 MiB，那条闸等于让真实照片目录一张缩略图都没有。
 *
 * 现在一张 20 MiB 的照片出去的只有几 KB，因此**这里不再有大小上限**：
 * 该不该缩、缩不缩得动，由服务端判断（见 `providers/fs/thumb.rs`）。
 *
 * `<img src>` 带不了 Authorization 头，走裸 `fetch` + object URL（与系统图标
 * 同一条路，见 `api/client.ts` 的 `authHeaders`）。
 */

/**
 * 服务端**可能**出得了缩略图的扩展名（与 `providers/fs/thumb.rs` 的
 * `supported` 对齐）。
 *
 * `heic` / `heif` 只有 macOS 后端解得了（借系统解码器），而前端分不出
 * mac 与 linux（`Platform` 只有 unix/windows）。仍然把它们列进来：代价是
 * Linux 后端上每个 HEIC 文件换回一个 400 并落一条负缓存，而懒加载只对
 * **进了视口的**条目发请求，一屏最多几十个，之后不再重试。反过来把它们
 * 排除掉的代价是 mac 上永远看不到 iPhone 照片的预览图——那才是真损失。
 */
const IMAGE_EXT = new Set(["png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "heif"]);

/**
 * 这个条目该不该出缩略图。
 *
 * 只看类型不看大小：大小的账已经由服务端接管（见模块文档）。
 * SVG / ICO / AVIF / HEIC 不在其中——服务端的纯 Rust 解码器不认它们，
 * 发请求只会换回一个 400。
 */
export function thumbEligible(e: DirEntry): boolean {
  if (e.kind !== "file") return false;
  const dot = e.name.lastIndexOf(".");
  if (dot < 0) return false;
  return IMAGE_EXT.has(e.name.slice(dot + 1).toLowerCase());
}

/** 一张取好的缩略图：object URL + EXIF 方向（1～8，1 = 正立）。 */
export interface Thumb {
  url: string;
  orientation: number;
}

/** path → 缩略图。会话级缓存：同一目录反复进出不重取。 */
const cache = new Map<string, Thumb>();
const inflight = new Map<string, Promise<Thumb | null>>();

/**
 * EXIF 方向 → CSS `transform`。
 *
 * 服务端不转像素（内嵌预览那条路一旦要转就得解码，正好违背它存在的理由），
 * 方向原样透传，由这里一行 CSS 解决。8 个取值按 EXIF 规范：
 * 2/4/5/7 含镜像，3 = 180°，6 = 顺时针 90°，8 = 逆时针 90°。
 */
export function orientationTransform(orientation: number): string | undefined {
  switch (orientation) {
    case 2:
      return "scaleX(-1)";
    case 3:
      return "rotate(180deg)";
    case 4:
      return "scaleY(-1)";
    case 5:
      return "rotate(90deg) scaleX(-1)";
    case 6:
      return "rotate(90deg)";
    case 7:
      return "rotate(270deg) scaleX(-1)";
    case 8:
      return "rotate(270deg)";
    default:
      return undefined;
  }
}

/**
 * 进入视口才取缩略图（列表与平铺共用）：一个几百张图的目录不该在打开瞬间
 * 全量拉取。把返回的 `ref` 挂到条目的图标容器上。
 */
export function useLazyThumb<T extends HTMLElement>(path: string, eligible: boolean) {
  const ref = useRef<T | null>(null);
  const [thumb, setThumb] = useState<Thumb | null>(() => cache.get(path) ?? null);

  useEffect(() => {
    if (!eligible) return;
    const hit = cache.get(path);
    if (hit) {
      // 命中会话缓存：首帧即有图，不必等进视口，也不闪一下类型图标。
      setThumb(hit);
      return;
    }
    const el = ref.current;
    if (!el) return;
    let live = true;
    const io = new IntersectionObserver((ioEntries) => {
      if (!ioEntries.some((e) => e.isIntersecting)) return;
      io.disconnect();
      void fetchThumb(path).then((t) => {
        if (live && t) setThumb(t);
      });
    });
    io.observe(el);
    return () => {
      live = false;
      io.disconnect();
    };
  }, [path, eligible]);

  return { ref, thumb: eligible ? thumb : null };
}

/** 取一个文件的缩略图；失败返回 `null`（显示类型图标，不重试轰炸）。 */
export function fetchThumb(path: string): Promise<Thumb | null> {
  const hit = cache.get(path);
  if (hit) return Promise.resolve(hit);
  const going = inflight.get(path);
  if (going) return going;

  const headers = authHeaders();
  if (!headers) return Promise.resolve(null);
  const p = (async () => {
    try {
      const resp = await fetch(`/api/v1/files/thumb?path=${encodeURIComponent(path)}`, { headers });
      // 400 = 这个文件服务端缩不了（格式不支持、图已损坏），不是故障。
      if (!resp.ok) return null;
      const orientation = Number(resp.headers.get("x-thumb-orientation") ?? "1") || 1;
      const thumb = { url: URL.createObjectURL(await resp.blob()), orientation };
      cache.set(path, thumb);
      return thumb;
    } catch {
      return null;
    } finally {
      inflight.delete(path);
    }
  })();
  inflight.set(path, p);
  return p;
}
