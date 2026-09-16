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
  {
    // 平台无关不等于只认 Linux：macOS 是一等开发平台（docs/macos-dev-platform.md）。
    // 取的是铝壳的颜色，不是商标：亮色主题用星空灰（#504D4B，偏暖，6.97:1），
    // 暗色主题用银色（#C8CACD，偏冷，8.5:1）。微量色温把它和「认不出」的
    // 纯灰（#5A5A5A / #9A9A9A）区分开——那个是没有身份，这个是铝。
    id: "macos",
    name: "macOS",
    initial: "M",
    brand: "#504D4B",
    accent: { light: "#504D4B", dark: "#C8CACD" },
  },
  {
    // Windows 是正式支持的运行平台（docs/windows-platform.md），后端的
    // `SystemInfo.os.id` 固定给 "windows"。缺这一条时 findDistro 会回落到
    // UNKNOWN_DISTRO——界面变成中性灰、方块显示「?」、名字写「通用」，
    // 看起来像「认不出这台机器」，而实际上认得出。
    //
    // **这一条不走本文件顶部那套 OKLCH 实算，直接取 Fluent 官方色阶的原值。**
    // 理由是「按各平台自己的惯例来」比「三个平台套同一条公式」更像那台机器：
    // Communication Blue 是 Windows 用户每天看见的那个蓝，算出来的近似色不是。
    //
    // accent 不跟着设计语言走（它是身份，不是样式），而设计语言是用户选的，
    // 所以这两个值会落在**任意一套**已注册语言的面板上，两套都得算：
    //
    // | 主题 | Fluent 档位 | 色值 | 对 StrixMaid 面板 | 对 Fluent 面板 |
    // |---|---|---|---|---|
    // | light | Primary | #0078D4 | 3.68:1 | 4.15:1 |
    // | dark | Tint10 | #2B88D8 | 4.11:1 | 3.89:1 |
    //
    // 暗色取浅一档也是 Fluent 自己的惯例（暗主题上用品牌色的 tint）。
    //
    // 四个数都达不到 4.5:1，但那个门槛对 accent 是**定错了**：
    // `--accent` 在本项目里一次都没用在文字上（4px 上边框、3px 内阴影边条、
    // 边框色、焦点轮廓，以及进度条填充——上面都没有字），适用的是
    // WCAG 1.4.11 非文字对比度 **3:1**，不是正文的 4.5:1。两档都过 3:1。
    //
    // 这条依赖写进了 tokens.test.ts 的断言注释：**一旦 accent 被用到文字上，
    // 门槛就要退回 4.5:1**，那时这两个值需要重新选（Fluent 的 Shade20 #005A9E
    // 对亮面板是 5.78:1，可作替换）。
    id: "windows",
    name: "Windows",
    initial: "W",
    brand: "#0078D4",
    accent: { light: "#0078D4", dark: "#2B88D8" },
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
