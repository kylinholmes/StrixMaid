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
  // 2026-08-29 修订：原来的面板底 L99.5 ≈ 纯白，而面板是屏幕上面积最大的东西，
  // 正文对它 17.8:1 也过高（长时间阅读的舒适区约 10–14:1）。整条压深。
  // 注意这次连**亮色档的数据色**一起压深了 10（见 styles/tokens.css）——
  // 顶住中性色的其实是 CPU 的青，不动它的话面板底最低只能到 L96.5。
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

/** 界面彩度。浅底对低彩度更敏感，故亮色取值更低。 */
export const CHROMA: Readonly<Record<Mode, number>> = { dark: 0.005, light: 0.004 };

/** 文字端彩度系数——高彩度的文字会显脏。 */
const INK_CHROMA_SCALE = 0.55;
/**
 * 选中底的彩度按主题分档。同一个 0.017 在暗色 L44 上几乎察觉不到，
 * 在亮色 L78 上却直接读成「粉」——截图实测里它叠在失败行上像一块错误高亮。
 * 亮色压到 0.010：仍带得出发行版色温，但不再显色。
 */
const SEL_CHROMA: Readonly<Record<Mode, number>> = { dark: 0.017, light: 0.01 };

export function chromaFor(token: string, base: number, mode: Mode): number {
  // 认不出发行版时 base 为 0，此时**每一个** token 都必须是纯灰。
  // sel 的固定档也不例外，不能让它在纯灰模式下把颜色带回来。
  if (base === 0) return 0;
  if (token.startsWith("ink")) return base * INK_CHROMA_SCALE;
  if (token === "sel") return SEL_CHROMA[mode];
  return base;
}
