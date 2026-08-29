/**
 * 发行版身份（2026-08-29 第二版）。
 *
 * 界面的灰不再从发行版派生色温——中性色统一纯灰。发行版色改为**主题色 accent**
 * 直接使用。官方色在两种底上大多不够对比度（openSUSE 对亮面板只有 2.2:1），
 * 所以每个发行版按亮 / 暗各存一档：沿 OKLCH 明度轴调到对面板 ≥4.5:1，
 * 色相与彩度一律不动。数值全部实算，不要凭感觉改。
 *
 * **不使用任何发行版的商标图形**——那些标识各有商标政策。身份 = 色 + 首字母方块。
 * 认不出的发行版：accent 回落到与界面灰明度错开的灰，仍然「像个主题色」，
 * 但不假装认识这台机器。
 */

export interface Distro {
  /** 与后端 `SystemInfo.distro_id`（os-release 的 ID 字段）对齐 */
  readonly id: string;
  readonly name: string;
  /** 首字母方块上的字 */
  readonly initial: string;
  /** 官方主色。只用于 16px 首字母方块的底，不作界面色 */
  readonly brand: string | null;
  /** 主题色，对各自面板 ≥4.5:1 */
  readonly accent: { readonly light: string; readonly dark: string };
}

export const DISTROS: readonly Distro[] = [
  {
    id: "ubuntu",
    name: "Ubuntu",
    initial: "U",
    brand: "#E95420",
    accent: { light: "#C53000", dark: "#F05B28" },
  },
  {
    id: "debian",
    name: "Debian",
    initial: "D",
    brand: "#D70A53",
    accent: { light: "#D0004D", dark: "#FD4371" },
  },
  {
    id: "rhel",
    name: "RHEL",
    initial: "R",
    brand: "#EE0000",
    accent: { light: "#D30000", dark: "#FF4737" },
  },
  {
    id: "fedora",
    name: "Fedora",
    initial: "F",
    brand: "#51A2DA",
    accent: { light: "#116DA2", dark: "#51A2DA" },
  },
  {
    id: "arch",
    name: "Arch",
    initial: "A",
    brand: "#1793D1",
    accent: { light: "#006DA8", dark: "#1A94D2" },
  },
  {
    id: "opensuse",
    name: "openSUSE",
    initial: "S",
    brand: "#73BA25",
    accent: { light: "#347700", dark: "#73BA25" },
  },
  {
    id: "opensuse-leap",
    name: "openSUSE Leap",
    initial: "S",
    brand: "#73BA25",
    accent: { light: "#347700", dark: "#73BA25" },
  },
  {
    id: "alpine",
    name: "Alpine",
    initial: "L",
    brand: "#0D597F",
    accent: { light: "#0D597F", dark: "#5193BC" },
  },
  {
    id: "rocky",
    name: "Rocky",
    initial: "R",
    brand: "#10B981",
    accent: { light: "#007845", dark: "#10B981" },
  },
  {
    id: "almalinux",
    name: "AlmaLinux",
    initial: "A",
    brand: "#0B7FAB",
    accent: { light: "#006F9B", dark: "#3195C2" },
  },
  {
    id: "centos",
    name: "CentOS",
    initial: "C",
    brand: "#932279",
    accent: { light: "#932279", dark: "#D461B4" },
  },
];

export const UNKNOWN_DISTRO: Distro = {
  id: "unknown",
  name: "通用",
  initial: "?",
  brand: null,
  accent: { light: "#5A5A5A", dark: "#9A9A9A" },
};

export function findDistro(id: string | null | undefined): Distro {
  if (!id) return UNKNOWN_DISTRO;
  const key = id.toLowerCase();
  return DISTROS.find((d) => d.id === key) ?? UNKNOWN_DISTRO;
}
