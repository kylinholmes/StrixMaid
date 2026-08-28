/**
 * 中性色的明度阶与彩度（spec §2.3）。
 *
 * 数值全部经过对比度实算，**不要凭感觉改**：
 * - 暗色 ground 之所以是 22.4% 而不是更暗，是因为再暗下去 §5.5 的双层阴影就没有落脚点了，
 *   两者是绑定的；
 * - `ink` 三档钳在 L92 / L20，纯白正文在暗底上会产生光晕；
 * - 彩度的可用区间只有三个千分点宽（C ≤ 0.008 安全，0.018 撞色），故写死不外露。
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
    ground: 96.0,
    surface: 99.5,
    "surface-2": 97.0,
    "surface-3": 93.5,
    line: 89.0,
    "line-strong": 72.0,
    sel: 86.0,
    "ink-3": 55.0,
    "ink-2": 40.0,
    ink: 20.0,
  },
};

/** 界面彩度。浅底对低彩度更敏感，故亮色取值更低。 */
export const CHROMA: Readonly<Record<Mode, number>> = { dark: 0.005, light: 0.004 };

/** 文字端彩度系数——高彩度的文字会显脏。 */
const INK_CHROMA_SCALE = 0.55;
/** 选中底需要看得出来，彩度给足。 */
const SEL_CHROMA_SCALE = 1.7;
const SEL_CHROMA_FLOOR = 0.01;

export function chromaFor(token: string, base: number): number {
  // 认不出发行版时 base 为 0，此时**每一个** token 都必须是纯灰。
  // 下面 sel 的彩度下限是给「有色相」准备的，不能让它在纯灰模式下把颜色带回来。
  if (base === 0) return 0;
  if (token.startsWith("ink")) return base * INK_CHROMA_SCALE;
  if (token === "sel") return Math.max(base, SEL_CHROMA_FLOOR) * SEL_CHROMA_SCALE;
  return base;
}
