/**
 * 发行版身份色（spec §2.2）。
 *
 * 只取色相用于派生中性色，**不使用任何发行版的商标图形**——那些标识各有商标政策，
 * 在 GPL 工具里分发是给自己找麻烦。身份由「色 + 首字母方块」承担。
 *
 * 认不出的发行版回落纯灰（chroma = 0）。自编译内核、容器里跑、小众发行版都会认不出，
 * **猜错色比不猜更糟**。
 */

export interface Distro {
  /** 与后端 `SystemInfo.distro_id`（os-release 的 ID 字段）对齐 */
  readonly id: string;
  readonly name: string;
  /** 首字母方块上的字 */
  readonly initial: string;
  /** 官方主色。仅用于取色相与画那个 16px 方块，不直接用作界面色 */
  readonly brand: string | null;
}

export const DISTROS: readonly Distro[] = [
  { id: "ubuntu", name: "Ubuntu", initial: "U", brand: "#E95420" },
  { id: "debian", name: "Debian", initial: "D", brand: "#D70A53" },
  { id: "rhel", name: "RHEL", initial: "R", brand: "#EE0000" },
  { id: "fedora", name: "Fedora", initial: "F", brand: "#51A2DA" },
  { id: "arch", name: "Arch", initial: "A", brand: "#1793D1" },
  { id: "opensuse", name: "openSUSE", initial: "S", brand: "#73BA25" },
  { id: "opensuse-leap", name: "openSUSE Leap", initial: "S", brand: "#73BA25" },
  { id: "alpine", name: "Alpine", initial: "L", brand: "#0D597F" },
  { id: "rocky", name: "Rocky", initial: "R", brand: "#10B981" },
  { id: "almalinux", name: "AlmaLinux", initial: "A", brand: "#0B7FAB" },
  { id: "centos", name: "CentOS", initial: "C", brand: "#932279" },
];

export const UNKNOWN_DISTRO: Distro = {
  id: "unknown",
  name: "通用",
  initial: "?",
  brand: null,
};

export function findDistro(id: string | null | undefined): Distro {
  if (!id) return UNKNOWN_DISTRO;
  const key = id.toLowerCase();
  return DISTROS.find((d) => d.id === key) ?? UNKNOWN_DISTRO;
}
