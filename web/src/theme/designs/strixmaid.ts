import type { Design, Mode, ModeToken, NeutralToken } from "../design";

/**
 * StrixMaid：本项目自己的设计语言，同时是「认不出平台」的回落档。
 *
 * ## 这个文件里的每一个数字都来自今天的界面
 *
 * 第四版把圆角、字阶、控件尺寸、间距、描边、阴影、动效从 CSS 里搬进 token，
 * **搬而不改**：`--fs-500` 是 13px，因为 `base.css` 的 `body` 今天就是 13px；
 * `--radius-*` 全是 0，因为 `base.css` 有一条无例外的全局 `border-radius: 0`；
 * `--t-shape` 是 440ms，因为侧栏的宽度动画今天就是 440ms。
 * 这一轮不调设计——改任何一个值都会让「零视觉变化」这条无从验证。
 *
 * 逐值比对由 `designs.test.ts` 守着：它把 `applyTheme` 落到 `:root` 上的每一个
 * 变量与一张写死的「今天的取值」表逐 key 对齐，少一条、多一条、值不同都失败。
 *
 * ## 有几处值得改，但不在这一轮改
 *
 * 字阶有 15 档，其中 11.5 / 12.5 / 13.5 三个半像素档各自只服务一两处；
 * 8.5px 只出现在设计合集页的色卡标签上。这是十几轮界面迭代自然长出来的清单，
 * 收敛它是一件独立的事，与「把值搬进 token」混在一起做，出了问题分不清是谁的。
 *
 * ## 中性灰阶（2026-08-29 第二版原值，一个数字都没动）
 *
 * 第一版曾把发行版色相派生进整套中性色。已退役：界面灰一律 C=0，
 * 发行版身份改由 `--accent` 承担（见 distro.ts）。
 *
 * 明度数值全部经过对比度实算，**不要凭感觉改**：
 * - 暗色 ground 22.4% 是双层阴影的落脚点，再暗阴影就消失了，两者绑定；
 * - 亮色 surface 93%（不是纯白）：面板是屏幕上面积最大的东西，L99.5 刺眼；
 * - `ink` 钳在 L92 / L24，纯白正文在暗底上会产生光晕。
 *
 * 这三条是**本设计语言**的结论，不是全局公理。Fluent 那套另有出处，见 fluent.ts。
 */

/** token 名 → OKLCH 明度百分比 */
export type Ramp = Readonly<Record<NeutralToken, number>>;

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

/**
 * 明度阶 → 这一档模式下的全部随模式变化的 token。
 *
 * `toFixed(1)` 是第二版 `applyTheme` 写 `:root` 时用的格式，逐字保留：
 * 换的是取值的地方，不是取出来之后长什么样。
 *
 * 阴影两档在暗色下整体加深（0.1/0.16 → 0.5/0.62）：暗底上浅阴影看不见。
 * 蒙层两档同值——今天的 `tokens.css` 就没有给它写暗色覆盖。
 *
 * **本语言只有一档浮层阴影**（spec §5.5：双层阴影 + 1px line-strong 边框），
 * 所以 raised / pop / dialog 三档取同值。这不是偷懒，是这套语言的主张：
 * 界面是平的，浮层只分「浮起来了」与「没浮」两种状态，不分几层楼高。
 * 轴仍然留着——Fluent 的三档就落在这三个名字上。
 */
function modeSet(
  ramp: Ramp,
  accent: string,
  shadow: string,
  scrim: string,
): Record<ModeToken, string> {
  const grey = (l: number) => `oklch(${l.toFixed(1)}% 0 0)`;
  return {
    ground: grey(ramp.ground),
    surface: grey(ramp.surface),
    "surface-2": grey(ramp["surface-2"]),
    "surface-3": grey(ramp["surface-3"]),
    line: grey(ramp.line),
    "line-strong": grey(ramp["line-strong"]),
    sel: grey(ramp.sel),
    "ink-3": grey(ramp["ink-3"]),
    "ink-2": grey(ramp["ink-2"]),
    ink: grey(ramp.ink),
    accent,
    "shadow-raised": shadow,
    "shadow-pop": shadow,
    "shadow-dialog": shadow,
    scrim,
    // 焦点是「你正在操作的地方」，正是身份层的领地（spec §5.4）。
    // 写成 `var(--accent)` 而不是把 accent 抄一份：发行版身份会在运行时把
    // `--accent` 盖掉，抄一份就跟不上了。
    focus: "var(--accent)",
    // 本语言的焦点环是单层 2px 实线，没有内环。轴要填满，所以填透明。
    "focus-inner": "transparent",
  };
}

const SHADOW_LIGHT = "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)";
const SHADOW_DARK = "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)";
const SCRIM = "rgb(0 0 0 / 0.45)";

/** 中文回退链。`ui` 与 `mono` 都引它，改一处两处都跟着变。 */
const CJK =
  '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"';

export const STRIXMAID = {
  id: "generic",
  // 界面上这一套就叫 StrixMaid——设置里的选项、合集页的标题取的都是这个字段。
  // id 仍然是 `generic`，因为它同时是「认不出平台」的回落档，存储里存的是 id。
  // 文档与注释里沿用「通用主题」的叫法，说的是同一套。
  name: "StrixMaid",
  color: {
    // accent 取的是「认不出」那档灰，与 `UNKNOWN_DISTRO.accent` 同值——
    // 认得出发行版时 `applyTheme` 会用发行版自己的那档盖掉它。
    // 两处数值的一致由 tokens.test.ts 守着，不要单独改其中一边。
    light: modeSet(RAMP.light, "#5A5A5A", SHADOW_LIGHT, SCRIM),
    dark: modeSet(RAMP.dark, "#9A9A9A", SHADOW_DARK, SCRIM),
  },
  // 全 0 无例外（spec §4）。base.css 那条 `*{border-radius:0}` 是同一条主张的
  // 兜底写法：浏览器给 input / button 的默认圆角也得压掉。
  radius: {
    "radius-sm": "0",
    "radius-md": "0",
    "radius-lg": "0",
    "radius-pill": "0",
  },
  family: {
    cjk: CJK,
    ui: `system-ui, -apple-system, "Segoe UI", var(--cjk), sans-serif`,
    // IBM Plex Mono 是随包发的 woff2（base.css 的 @font-face），
    // 所以排在系统等宽之前——等宽字是这个界面里读数字的地方，不能听系统的。
    mono: `"IBM Plex Mono", var(--cjk), ui-monospace, Menlo, Consolas, monospace`,
  },
  fontSize: {
    "fs-50": "8.5px",
    "fs-100": "9px",
    "fs-200": "10px",
    "fs-300": "11px",
    "fs-350": "11.5px",
    "fs-400": "12px",
    "fs-450": "12.5px",
    "fs-500": "13px",
    "fs-550": "13.5px",
    "fs-600": "14px",
    "fs-700": "15px",
    "fs-800": "16px",
    "fs-900": "17px",
    "fs-1000": "19px",
    "fs-1100": "20px",
  },
  lineHeight: {
    "lh-none": "1",
    "lh-tight": "1.1",
    "lh-snug": "1.4",
    "lh-body": "1.5",
    "lh-normal": "1.6",
    "lh-loose": "1.65",
    "lh-200": "15px",
    "lh-300": "16px",
  },
  size: {
    row: "30px",
    "row-toolbar": "40px",
    "row-sm": "26px",
    "row-xs": "24px",
    rail: "184px",
    "rail-narrow": "44px",
    icon: "16px",
    "icon-lg": "22px",
  },
  space: {
    "sp-1": "2px",
    "sp-2": "4px",
    "sp-3": "6px",
    "sp-4": "8px",
    "sp-5": "10px",
    "sp-6": "12px",
    "sp-7": "16px",
    "sp-8": "20px",
    "sp-9": "24px",
    "sp-10": "32px",
  },
  stroke: {
    stroke: "1px",
    "stroke-thick": "2px",
    "stroke-thicker": "3px",
    "stroke-thickest": "4px",
  },
  // 环画在控件外面，与控件之间留 2px 空（spec §5.4）。贴边会和控件自己的
  // 1px 边框糊成「边框变粗了」，看不出是焦点。
  focus: {
    "focus-width": "2px",
    "focus-offset": "2px",
  },
  motion: {
    "t-pop": "90ms",
    "t-color": "120ms",
    "t-arrive": "150ms",
    "t-enter": "240ms",
    "t-shape": "440ms",
    "t-shape-out": "320ms",
    ease: "ease",
    "ease-out": "ease-out",
    "ease-in-out": "ease-in-out",
    "ease-shape": "cubic-bezier(0.3, 0, 0.1, 1)",
  },
  // `as const` 是为了把 `id` 留成字面量类型：设计语言的清单（`ThemeId`、
  // `DESIGN_OPTIONS`）全部从 `THEMES` 推导，推导链的源头必须是字面量。
} as const satisfies Design;
