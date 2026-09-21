/**
 * 「点一个条目会发生什么」的唯一判定处。
 *
 * 抽出来是因为它有多个调用方（列表、平铺，以及将来的键盘回车），而判断里
 * 藏着两条容易各写各的规则：**链接要跟到目标**，以及**目标是文件还是目录
 * 决定跳法不同**。散写就会出现「列表里点得动、平铺里点不动」这种差异。
 */

import { joinPath, type Platform, parentPath } from "./path";

/** 点一个条目之后该做什么。`null` = 这一下没有去处。 */
export type EntryAction =
  /** 进这个目录。 */
  | { kind: "enter"; path: string }
  /** 跳到 `dir` 并选中其中的 `name`（目标是文件时的跳法）。 */
  | { kind: "reveal"; dir: string; name: string };

/** [`activateEntry`] 需要的最小条目形状；`DirEntry` 结构上满足它。 */
export interface ActivatableEntry {
  kind: string;
  name: string;
  /** 链接目标，服务端原样给出、未解引用。 */
  target?: string | null;
  /** 链接目标的类型，见 `DirEntryInfo::target_kind`。 */
  target_kind?: string | null;
}

/**
 * 目标看起来是不是绝对路径。
 *
 * Windows 认三种形状：盘符开头（`C:\`）、UNC（`\\nas\share`），以及以单个
 * 反斜杠开头的「当前盘根相对」路径。第三种也按绝对处理——把它接到 cwd
 * 后面一定是错的，原样交给服务端至少能得到一条真实的错误。
 */
function isAbsolute(target: string, p: Platform): boolean {
  return p === "windows" ? /^[A-Za-z]:[\\/]|^[\\/]/.test(target) : target.startsWith("/");
}

/**
 * 链接目标 → 可以直接交给 `/files` 的路径。
 *
 * 相对目标按**链接自己所在的目录**解析，这是符号链接的语义。拼出来的
 * `…/a/../b` 不在这里消解——服务端的 `fs::normalize` 本来就要再规范化一次，
 * 前端再实现一份只会多一处会跑偏的实现。
 */
export function resolveLinkTarget(cwd: string, target: string, p: Platform): string {
  return isAbsolute(target, p) ? target : joinPath(cwd, target, p);
}

/** 取路径最后一段。 */
function baseName(path: string, p: Platform): string {
  const sep = p === "windows" ? "\\" : "/";
  const trimmed = path.endsWith(sep) && path.length > 1 ? path.slice(0, -1) : path;
  const cut = trimmed.lastIndexOf(sep);
  return cut < 0 ? trimmed : trimmed.slice(cut + 1);
}

/**
 * 点 `entry` 该做什么。
 *
 * - 目录 → 进去；
 * - 链接 → 跟到目标：目标是目录就进去，是文件就跳到它所在目录并选中它；
 *   **目标类型未知时按目录去试**（断链、或老服务端不给 `target_kind`），
 *   让服务端给出真实的错误，好过点了毫无反应；
 * - 其余（普通文件、设备、fifo……）→ `null`。预览与下载还没有界面
 *   （见 `docs/HANDOFF-2026-09-21.md` §6），有了之后这里再加一种动作。
 */
export function activateEntry(
  cwd: string | null,
  entry: ActivatableEntry,
  p: Platform,
): EntryAction | null {
  if (cwd === null) return null;

  if (entry.kind === "dir") return { kind: "enter", path: joinPath(cwd, entry.name, p) };

  if (entry.kind === "symlink") {
    if (!entry.target) return null;
    const path = resolveLinkTarget(cwd, entry.target, p);
    if (entry.target_kind === "file") {
      return { kind: "reveal", dir: parentPath(path, p) ?? path, name: baseName(path, p) };
    }
    return { kind: "enter", path };
  }

  return null;
}
