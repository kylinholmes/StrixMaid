/**
 * 平台感知的路径拼接与取父目录（`docs/roadmap/12-workspace.md` §4.5）。
 *
 * 为什么单独抽出来、且不在组件里手写拼接：`/debug` 原型把 `joinPath` 硬编码成
 * 正斜杠，在 Windows 上拼出非法路径——而 `providers/fs/windows.rs` 明确要求
 * 「驱动器根的 `name` 就是完整路径 `C:\`，不要与父路径拼接」。这一条只要有一处
 * 手写就会再犯，所以所有路径运算都过这里。
 */

export type Platform = "unix" | "windows";

/** Windows 上「全部驱动器」这个虚拟根，规范化后写作单个反斜杠（`NAMESPACE_ROOT`）。 */
const WINDOWS_VIRTUAL_ROOT = "\\";

/** `os_id` 含 `windows` 即 Windows；其余（含未知）当 Unix。 */
export function platformOf(osId: string | undefined): Platform {
  return osId?.toLowerCase().includes("windows") ? "windows" : "unix";
}

/** 形如 `C:\`（盘符 + 冒号 + 单个反斜杠）。 */
export function isDriveRoot(name: string, p: Platform): boolean {
  return p === "windows" && /^[A-Za-z]:\\$/.test(name);
}

/** Windows 的「全部驱动器」虚拟根。 */
export function isVirtualRoot(path: string, p: Platform): boolean {
  return p === "windows" && path === WINDOWS_VIRTUAL_ROOT;
}

/**
 * 把一个目录项名接到父路径后面。
 *
 * Windows 虚拟根（`\`）下的条目名本身就是完整路径 `C:\`——直接返回它，不拼接。
 */
export function joinPath(parent: string, name: string, p: Platform): string {
  if (p === "windows") {
    if (isVirtualRoot(parent, p) || isDriveRoot(name, p)) return name;
    const base = parent.endsWith("\\") ? parent.slice(0, -1) : parent;
    return `${base}\\${name}`;
  }
  const base = parent.endsWith("/") ? parent.slice(0, -1) : parent;
  return `${base}/${name}`;
}

/** 取父目录；已在根返回 `null`。 */
export function parentPath(path: string, p: Platform): string | null {
  if (p === "windows") {
    if (isVirtualRoot(path, p)) return null;
    // 盘根 `C:\` 的上一级是虚拟根「全部驱动器」。
    if (isDriveRoot(path, p)) return WINDOWS_VIRTUAL_ROOT;
    const trimmed = path.endsWith("\\") ? path.slice(0, -1) : path;
    const cut = trimmed.lastIndexOf("\\");
    if (cut < 0) return WINDOWS_VIRTUAL_ROOT;
    const head = trimmed.slice(0, cut);
    // 落到盘符时补回反斜杠，得到规范的盘根 `C:\`。
    return /^[A-Za-z]:$/.test(head) ? `${head}\\` : head;
  }
  if (path === "/") return null;
  const trimmed = path.endsWith("/") ? path.slice(0, -1) : path;
  const cut = trimmed.lastIndexOf("/");
  return cut <= 0 ? "/" : trimmed.slice(0, cut);
}

/**
 * 按平台猜会话用户的主目录。API 不下发 home（`SessionInfo` 没有这个字段），
 * 而快速访问只需要一个**大概率对**的起点：猜错时文件区会明确报错，
 * 用户仍可从根导航——比为此新增一个端点便宜。
 */
export function guessHome(osId: string | undefined, username: string, uid: number): string {
  if (platformOf(osId) === "windows") return `C:\\Users\\${username}`;
  if (osId?.toLowerCase() === "macos") return uid === 0 ? "/var/root" : `/Users/${username}`;
  return uid === 0 ? "/root" : `/home/${username}`;
}

/**
 * 地址栏补全：把输入拆成「已确定的父目录 + 正在敲的最后一段前缀」。
 * 认不出的形状（相对路径、裸盘符）返回 `null`——不补全好过瞎补。
 */
export function splitForCompletion(
  input: string,
  p: Platform,
): { parent: string; prefix: string } | null {
  if (p === "windows") {
    if (input === "\\" || input === "/") return { parent: "\\", prefix: "" };
    if (!/^[A-Za-z]:\\/.test(input)) return null;
    const cut = input.lastIndexOf("\\");
    // `C:\pre` 的父是盘根 `C:\`；更深的层级按段切。
    const head = input.slice(0, cut);
    const parent = /^[A-Za-z]:$/.test(head) ? `${head}\\` : head;
    return { parent, prefix: input.slice(cut + 1) };
  }
  if (!input.startsWith("/")) return null;
  const cut = input.lastIndexOf("/");
  return { parent: cut === 0 ? "/" : input.slice(0, cut), prefix: input.slice(cut + 1) };
}

/**
 * `child` 是否严格位于 `parent` 之下（任意层级）。层叠导航用它判断
 * push（进子目录）还是 pop（回祖先）；Windows 按段大小写不敏感。
 */
export function isDescendant(child: string, parent: string, p: Platform): boolean {
  if (child === parent) return false;
  const sep = p === "windows" ? "\\" : "/";
  const norm = (x: string) => {
    const t = x.endsWith(sep) && x.length > 1 ? x.slice(0, -1) : x;
    return p === "windows" ? t.toLowerCase() : t;
  };
  const c = norm(child);
  const pa = norm(parent);
  if (p === "windows" && parent === "\\") return c !== "\\"; // 虚拟根之下是一切
  const prefix = pa === sep ? sep : pa + sep;
  return c.startsWith(prefix);
}
