import { useEffect, useState } from "react";
import { authHeaders } from "@/api/client";
import { BlobCache } from "./blobcache";
import { isVirtualRoot, type Platform } from "./path";

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

/** 保留 key：目录（与后端 `providers/fs/icon.rs` 的常量一致）。 */
export const DIR_KEY = "$dir";
/** 保留 key：无扩展名 / 认不出类型的文件。 */
export const GENERIC_FILE_KEY = "$file";

// 左栏（快速访问）的保留 key。与后端 `providers/fs/icon.rs` 的 `RESERVED_KEYS`
// 一一对应，两边必须一致——对不上只会表现为「那一项没有图标」，不会报错。
/** 保留 key：主目录。 */
export const HOME_KEY = "$home";
/** 保留 key：「此电脑」/ 全部驱动器这个虚拟根。 */
export const COMPUTER_KEY = "$computer";
/** 保留 key：一块固定磁盘。 */
export const DRIVE_KEY = "$drive";
/** 保留 key：桌面。 */
export const DESKTOP_KEY = "$desktop";
/** 保留 key：文稿 / 文档。 */
export const DOCUMENTS_KEY = "$documents";
/** 保留 key：下载。 */
export const DOWNLOADS_KEY = "$downloads";
/** 保留 key：图片。 */
export const PICTURES_KEY = "$pictures";
/** 保留 key：音乐。 */
export const MUSIC_KEY = "$music";
/** 保留 key：视频 / 影片。 */
export const VIDEOS_KEY = "$videos";
/** 保留 key：公共。 */
export const PUBLIC_KEY = "$public";

/**
 * 已知文件夹的**物理目录名** → 保留 key。
 *
 * 键是磁盘上的真实目录名而不是展示名：简体中文 Windows 上「下载」就叫
 * `下载`，两种都要认。认不出的名字返回 `undefined`，由调用方回落内置图标
 * ——硬凑一个 key 只会画错。
 *
 * 这张表与 `QuickAccess.tsx` 的 `KNOWN_FOLDERS` 覆盖同一批名字。
 */
const KNOWN_FOLDER_KEY: Record<string, string> = {
  Desktop: DESKTOP_KEY,
  桌面: DESKTOP_KEY,
  Documents: DOCUMENTS_KEY,
  文档: DOCUMENTS_KEY,
  文稿: DOCUMENTS_KEY,
  Downloads: DOWNLOADS_KEY,
  下载: DOWNLOADS_KEY,
  Pictures: PICTURES_KEY,
  图片: PICTURES_KEY,
  Music: MUSIC_KEY,
  音乐: MUSIC_KEY,
  Movies: VIDEOS_KEY,
  Videos: VIDEOS_KEY,
  视频: VIDEOS_KEY,
  影片: VIDEOS_KEY,
  Public: PUBLIC_KEY,
  公共: PUBLIC_KEY,
};

/** 会话级缓存：类型 key → object URL；`null` 是负缓存（这一类真的取不到）。 */
// 上限与回收见 `blobcache.ts`。类型图标一个扩展名一张，256 绰绰有余。
const cache = new BlobCache<string | null>(256, (v) => v);
const inflight = new Map<string, Promise<string | null>>();

/** 按路径的会话级缓存（macOS 的 bundle / 符号链接，见 `usePathIcon`）。 */
// 按路径的 bundle 图标一个应用一张；512 覆盖整个 /Applications 有余。
const pathCache = new BlobCache<string | null>(512, (v) => v);
const pathInflight = new Map<string, Promise<string | null>>();

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
 * 一个具体条目的系统图标（按路径，只有 macOS 后端有货）：`.app` 显示应用
 * 自己的图标，符号链接解析到目标。同一探测把门：Linux 会话零请求；
 * **Windows 后端也会 404**（那边按路径要碰磁盘，见后端文档），所以调用方
 * 必须只在 unix 平台上传路径进来——探测通过 + unix 平台 ⇒ macOS。
 */
export function fetchPathIcon(path: string): Promise<string | null> {
  const hit = pathCache.get(path);
  if (hit !== undefined) return Promise.resolve(hit);
  const going = pathInflight.get(path);
  if (going) return going;

  const p = (async () => {
    try {
      if (!(await probe())) return null;
      const headers = authHeaders();
      if (!headers) return null;
      const resp = await fetch(`/api/v1/files/icon-path?path=${encodeURIComponent(path)}`, {
        headers,
      });
      // 404 是确定性的「这条路没有」（非 macOS），负缓存；网络错误留给下次。
      if (!resp.ok) {
        if (resp.status === 404 || resp.status === 403) pathCache.set(path, null);
        return null;
      }
      const url = URL.createObjectURL(await resp.blob());
      pathCache.set(path, url);
      return url;
    } catch {
      return null;
    } finally {
      pathInflight.delete(path);
    }
  })();
  pathInflight.set(path, p);
  return p;
}

/** 通用的「异步取 → 会话缓存 → 状态」钩子骨架。 */
function useIconUrl(
  key: string | null,
  store: BlobCache<string | null>,
  fetcher: (key: string) => Promise<string | null>,
): string | null {
  const [url, setUrl] = useState<string | null>(() =>
    key === null ? null : (store.get(key) ?? null),
  );

  // biome-ignore lint/correctness/useExhaustiveDependencies: store 与 fetcher 是模块级常量，不进依赖
  useEffect(() => {
    if (key === null) {
      setUrl(null);
      return;
    }
    let live = true;
    void fetcher(key).then((u) => {
      if (live) setUrl(u);
    });
    return () => {
      live = false;
    };
  }, [key]);

  return key === null ? null : url;
}

/**
 * React 侧入口：`key` 是类型 key——扩展名（`extOf` 的产物）、[`DIR_KEY`] 或
 * [`GENERIC_FILE_KEY`]；`null` = 不取。命中会话缓存时首帧即有值，不闪内置图标。
 */
export function useSysIcon(key: string | null): string | null {
  return useIconUrl(key, cache, fetchTypeIcon);
}

/** React 侧入口：按路径取（见 [`fetchPathIcon`] 的适用面）。 */
export function usePathIcon(path: string | null): string | null {
  return useIconUrl(path, pathCache, fetchPathIcon);
}

// ===========================================================================
// 图标源判定：一处定义、编译期穷尽（负责人 2026-09-21 要求）
// ===========================================================================

/**
 * 会出现在界面上、需要一枚图标的**主体**。可辨识联合：每种主体是一个
 * 显式的 `kind`，[`iconKeysOf`] 对它做**穷尽** switch——新增一种主体而
 * 忘了写它该用哪个图标源，是 tsc 编译错误，不是运行时读错。
 *
 * 列表（ListPane）、平铺（TileGrid）与左侧快速访问栏（QuickAccess）都
 * 只经这一个判定拿钥匙，任何一处不许自己拼 URL 或散写条件。
 */
export type IconSubject =
  /** 目录项：普通文件夹，或 macOS 的 bundle（带扩展名的目录，如 `.app`）。 */
  | { kind: "dir"; name: string; fullPath: string | null }
  /** 文件：按扩展名归类，无扩展名走通用文件。 */
  | { kind: "file"; name: string }
  /** 符号链接：mac 上按路径解析到目标，其余回落链条形状。 */
  | { kind: "symlink"; name: string; fullPath: string | null }
  /**
   * 左栏的已知文件夹（桌面/下载……）：mac 有带徽标的专属图标（按路径），
   * Windows 按 `name` 换一个保留 key 去取（见 [`KNOWN_FOLDER_KEY`]）。
   */
  | { kind: "known-folder"; name: string; fullPath: string }
  /** 左栏的主目录。 */
  | { kind: "home"; fullPath: string }
  /** 左栏的挂载点/根：mac 按路径给磁盘/卷的真图标。 */
  | { kind: "mount"; fullPath: string };

/** 穷尽性哨兵：switch 漏了一种 `kind`，这里的参数类型就对不上，tsc 报错。 */
function assertNever(x: never): never {
  throw new Error(`未覆盖的图标主体：${JSON.stringify(x)}`);
}

/**
 * 主体 → 两把钥匙：
 *
 * - `pathKey`：按路径取（优先）。只在 unix 平台上给——探测通过 + unix ⇒
 *   后端是 macOS（Windows 后端按路径恒 404，白发请求；Linux 探测就拦了），
 *   `iconForFile:` 给的是这个条目的真身（`.app` 的应用图标、下载文件夹的
 *   徽标、磁盘的样子）。
 * - `typeKey`：按类型取（回落 / 常规）。
 *
 * 左栏主体（known-folder / home / mount）**没有** typeKey：Windows 上
 * `$dir` 会把 桌面/下载/磁盘 全画成同一只黄色文件夹，反而丢信息——
 * 那里的回落是各自的内置图标（QuickAccess 自己传），mac 上则全是系统真身。
 */
export function iconKeysOf(
  subject: IconSubject,
  platform: Platform,
): { pathKey: string | null; typeKey: string | null } {
  const unix = platform === "unix";
  switch (subject.kind) {
    case "dir":
      return {
        // 带扩展名的目录 = macOS bundle，按路径取应用/框架自己的图标。
        pathKey: unix && extOf(subject.name) !== null ? subject.fullPath : null,
        typeKey: DIR_KEY,
      };
    case "file":
      return { pathKey: null, typeKey: extOf(subject.name) ?? GENERIC_FILE_KEY };
    case "symlink":
      return { pathKey: unix ? subject.fullPath : null, typeKey: null };
    // 左栏三种主体：unix 上按路径取（mac 给的是真身：下载文件夹的徽标、
    // 磁盘的样子）；Windows 上按各自的保留 key 取。
    //
    // **Windows 这条原先是 `typeKey: null`**，即一律回落内置 Papirus 图标集，
    // 理由是拿 `$dir` 去取会把 桌面/下载/磁盘 全画成同一只黄文件夹。现在不
    // 再借用 `$dir`，而是每种主体一个保留 key，那条理由随之作废（项目负责人
    // 2026-09-21 定）。后端仍然不接受路径，只认这张固定的 key 表，所以
    // 「图标端点不碰用户文件」的前提没有松动。
    case "known-folder":
      return {
        pathKey: unix ? subject.fullPath : null,
        typeKey: unix ? null : (KNOWN_FOLDER_KEY[subject.name] ?? null),
      };
    case "home":
      return { pathKey: unix ? subject.fullPath : null, typeKey: unix ? null : HOME_KEY };
    case "mount":
      return {
        pathKey: unix ? subject.fullPath : null,
        typeKey: unix ? null : isVirtualRoot(subject.fullPath, platform) ? COMPUTER_KEY : DRIVE_KEY,
      };
    default:
      return assertNever(subject);
  }
}

/**
 * 目录项 → 主体。`FileKind` 有八种，图标只分三路：目录、符号链接，
 * 其余（普通文件、设备、fifo、socket……）都按文件归类。
 */
export function entrySubject(
  e: { kind: string; name: string },
  fullPath: string | null,
): IconSubject {
  switch (e.kind) {
    case "dir":
      return { kind: "dir", name: e.name, fullPath };
    case "symlink":
      return { kind: "symlink", name: e.name, fullPath };
    default:
      return { kind: "file", name: e.name };
  }
}

/**
 * React 侧的唯一入口：主体 → 已解析的系统图标 URL（`null` = 系统给不出，
 * 调用方按主体自己的口味回落——内置集、lucide 形状）。
 */
export function useEntryIcon(subject: IconSubject, platform: Platform): string | null {
  const { pathKey, typeKey } = iconKeysOf(subject, platform);
  const pathIcon = usePathIcon(pathKey);
  const typeIcon = useSysIcon(typeKey);
  return pathIcon ?? typeIcon;
}

/** 测试用：清掉模块级状态（缓存与探测结论都是会话级单例）。 */
export function resetForTest(): void {
  cache.clear();
  inflight.clear();
  pathCache.clear();
  pathInflight.clear();
  availability = null;
}
