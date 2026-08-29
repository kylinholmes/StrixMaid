/**
 * 中性色的明度阶（2026-08-29 第二版：统一纯灰）。
 *
 * 第一版曾把发行版色相派生进整套中性色。现已退役：界面灰一律 C=0，
 * 发行版身份改由 `--accent` 承担（见 distro.ts）。
 *
 * 明度数值全部经过对比度实算，**不要凭感觉改**：
 * - 暗色 ground 22.4% 是双层阴影的落脚点，再暗阴影就消失了，两者绑定；
 * - 亮色 surface 93%（不是纯白）：面板是屏幕上面积最大的东西，L99.5 刺眼；
 * - `ink` 钳在 L92 / L24，纯白正文在暗底上会产生光晕。
 */

export type Mode = "dark" | "light";

/** token 名 → OKLCH 明度百分比 */
export type Ramp = Readonly<Record<string, number>>;

export const RAMP: Readonly<Record<Mode, Ramp>> = {
  dark: {
    ground: 22.4,
    surface: 26.4,
    "surface-2": 30.4,
    "surface-3": 35.4,
    line: 37.4,
    "line-strong": 52.4,
    sel: 44.0,
    "ink-3": 63.0,
    "ink-2": 79.0,
    ink: 92.0,
  },
  light: {
    ground: 88.0,
    surface: 93.0,
    "surface-2": 89.5,
    "surface-3": 85.0,
    line: 82.0,
    "line-strong": 64.0,
    sel: 78.0,
    "ink-3": 48.0,
    "ink-2": 34.0,
    ink: 24.0,
  },
};
