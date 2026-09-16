/**
 * 主题层：一个平台一整套设计语言（2026-09-16 第三版）。
 *
 * 第二版只有一条全局中性灰阶（`RAMP`），所有平台共用，发行版身份仅由
 * `--accent` 承担。第三版把中性色也纳入主题：`Theme` 持有完整的一套 token，
 * `applyTheme` 从选中的那套里取 `tokens[mode]` 写进 `:root`。
 *
 * 换掉全局灰阶的理由是各平台的中性色惯例本来就不一样，套同一条公式算出来的
 * 界面「哪个平台都不像」。Windows 用户每天看见的是 Fluent 那几档灰，
 * 不是按 OKLCH 推出来的近似色。
 *
 * **身份与设计语言是两件事，类型上不合并。** 本文件只回答「用哪套设计语言」；
 * 方块上的字、品牌色、accent 是发行版身份，在 distro.ts 里。两者将来会分叉：
 * Ubuntu 装 KDE 应当是 Ubuntu 的橙色方块配 Breeze 的中性色。
 *
 * 通用主题的数值与第二版**逐字相同**——`RAMP` 原样保留，`fromRamp` 生成的
 * CSS 串与第二版 `applyTheme` 的输出完全一致，因此 Linux 与 macOS 的渲染零变化。
 */

export type Mode = "dark" | "light";

/** 中性色 token 名。与 CSS 里的 `--ground`、`--surface` … 一一对应，改名等于全面重排。 */
export type NeutralToken =
  | "ground"
  | "surface"
  | "surface-2"
  | "surface-3"
  | "line"
  | "line-strong"
  | "sel"
  | "ink-3"
  | "ink-2"
  | "ink";

/**
 * 一套完整的界面色。
 *
 * 值直接就是**最终的 CSS 颜色串**，不是明度数字：通用主题存 `oklch(L% 0 0)`，
 * Fluent 存十六进制原值。两种写法必须能并存，否则 Fluent 的官方色值就只能
 * 换算一遍再写回去，而换算的近似色正是这一版要摆脱的东西。
 *
 * 键的顺序就是「页底 → 面板 → 分隔 → 正文」的深浅顺序，合集页按这个顺序排色卡。
 */
export interface TokenSet extends Readonly<Record<NeutralToken, string>> {
  /** 主题色。认得出机器时会被发行版身份覆盖，见 useTheme.ts 的 `applyTheme` */
  readonly accent: string;
}

export interface Theme {
  /** 设计语言的 id，不是平台 id——将来 Breeze 会同时服务好几个发行版 */
  readonly id: string;
  readonly name: string;
  readonly tokens: { readonly light: TokenSet; readonly dark: TokenSet };
}

/** token 名 → OKLCH 明度百分比 */
export type Ramp = Readonly<Record<NeutralToken, number>>;

/**
 * 通用主题的中性色明度阶（2026-08-29 第二版原值，一个数字都没动）。
 *
 * 第一版曾把发行版色相派生进整套中性色。已退役：界面灰一律 C=0，
 * 发行版身份改由 `--accent` 承担（见 distro.ts）。
 *
 * 明度数值全部经过对比度实算，**不要凭感觉改**：
 * - 暗色 ground 22.4% 是双层阴影的落脚点，再暗阴影就消失了，两者绑定；
 * - 亮色 surface 93%（不是纯白）：面板是屏幕上面积最大的东西，L99.5 刺眼；
 * - `ink` 钳在 L92 / L24，纯白正文在暗底上会产生光晕。
 *
 * 这三条是**通用主题**的结论，不是全局公理。Fluent 那套另有出处，见下。
 */
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
 * 明度阶 → CSS 颜色串。
 *
 * `toFixed(1)` 是第二版 `applyTheme` 写 `:root` 时用的格式，逐字保留：
 * 这一版换的是取值的地方，不是取出来之后长什么样。
 */
function fromRamp(ramp: Ramp, accent: string): TokenSet {
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
  };
}

/**
 * 通用主题：认不出平台、或者认得出但还没有专属设计语言时用这一套。
 *
 * Linux 与 macOS 目前全部落在这里，渲染结果与第二版完全一致。
 * accent 取的是「认不出」那档灰，与 `UNKNOWN_DISTRO.accent` 同值——
 * 认得出发行版时 `applyTheme` 会用发行版自己的那档盖掉它。
 * 两处数值的一致由 tokens.test.ts 守着，不要单独改其中一边。
 */
export const GENERIC_THEME: Theme = {
  id: "generic",
  name: "通用",
  tokens: {
    light: fromRamp(RAMP.light, "#5A5A5A"),
    dark: fromRamp(RAMP.dark, "#9A9A9A"),
  },
};

/**
 * Fluent：Windows 的设计语言。
 *
 * 全部取自 Fluent 2 官方发布的设计 token（`@fluentui/tokens`，
 * `src/global/colors.ts` 的 `grey` 灰阶与 `src/alias/{light,dark}Color.ts`
 * 的别名表；别名表由 Microsoft 的 token pipeline 生成，文件头写着不要手改）。
 * 表里每一档都注明了别名与 `Global.Color.Grey.N` 档号，**不要凭记忆改**。
 *
 * ## 亮色（面板 #F5F5F5）
 *
 * | token | 值 | 出处 | 对面板 |
 * |---|---|---|---|
 * | ground | #E6E6E6 | Grey.90 `colorNeutralBackground6` | 1.14:1 |
 * | surface | #F5F5F5 | Grey.96 `colorNeutralBackground3` | — |
 * | surface-2 | #EBEBEB | Grey.92 `colorNeutralBackground3Hover` | 1.09:1 |
 * | surface-3 | #E0E0E0 | Grey.88 `colorNeutralBackground3Selected` | 1.21:1 |
 * | sel | #D6D6D6 | Grey.84 `colorNeutralBackground3Pressed` | 1.33:1 |
 * | line | #D1D1D1 | Grey.82 `colorNeutralStroke1` | 1.40:1 |
 * | line-strong | #8A8A8A | Grey.54 | 3.17:1 |
 * | ink-3 | #616161 | Grey.38 `colorNeutralForeground3` | 5.68:1 |
 * | ink-2 | #424242 | Grey.26 `colorNeutralForeground2` | 9.22:1 |
 * | ink | #242424 | Grey.14 `colorNeutralForeground1` | 14.24:1 |
 * | accent | #0078D4 | Brand.80（Communication Blue） | 4.15:1 |
 *
 * **面板用 #F5F5F5 而不是 Fluent 的 `colorNeutralBackground1`（纯白）**：
 * 面板是屏幕上面积最大的东西，纯白刺眼。这一条是产品决定，不是 Fluent 的默认。
 * 面板既然落在 `colorNeutralBackground3` 这一层，行的三个交互态就直接取
 * Fluent 为**这一层**定义的 Hover / Selected / Pressed，不必自己推。
 * Fluent 的 Selected 比 Pressed 浅，而本项目「选中」是最强的一档，
 * 所以 `surface-3`（划过）取 Selected、`sel`（选中）取 Pressed——
 * 取的是档位值，不是 Fluent 对这两个词的用法。
 *
 * ## 暗色（面板 #292929）
 *
 * | token | 值 | 出处 | 对面板 |
 * |---|---|---|---|
 * | ground | #1F1F1F | Grey.12 `colorNeutralBackground2` | 1.13:1 |
 * | surface | #292929 | Grey.16 `colorNeutralBackground1` | — |
 * | surface-2 | #333333 | Grey.20 `colorNeutralBackground6` | 1.15:1 |
 * | surface-3 | #3D3D3D | Grey.24 `colorNeutralBackground1Hover` | 1.34:1 |
 * | sel | #474747 | Grey.28 | 1.57:1 |
 * | line | #525252 | Grey.32 `colorNeutralStroke2` | 1.86:1 |
 * | line-strong | #757575 | Grey.46 `colorNeutralStroke1Hover` | 3.16:1 |
 * | ink-3 | #ADADAD | Grey.68 `colorNeutralForeground3` | 6.48:1 |
 * | ink-2 | #D6D6D6 | Grey.84 `colorNeutralForeground2` | 10.01:1 |
 * | ink | #FFFFFF | `Global.Color.White` `colorNeutralForeground1` | 14.55:1 |
 * | accent | #2B88D8 | Tint10 | 3.89:1 |
 *
 * 暗色的四级底取的是 Grey 灰阶的 16 / 20 / 24 / 28，与亮色的 96 / 92 / 88 / 84
 * 步长一致（每档 4)。Fluent 暗色没有一组单调升上去的别名可用：
 * `colorNeutralBackground1Hover`（Grey.24）比 `…Selected`（Grey.22）还浅，
 * 而本项目要求「选中 > 划过」，照抄别名会让阶梯反过来。
 *
 * ## 两档分隔线为什么不是对称取的
 *
 * `--line-strong` 是控件边界（按钮、输入框、分段控件、工具条、对话框，
 * 以及滚动条滑块），适用 WCAG 1.4.11 的非文字对比度 3:1。
 * Fluent 的 `colorNeutralStroke1`（#D1D1D1 / #666666）对本项目的面板只有
 * 1.40:1 / 2.53:1，达不到；`colorNeutralStrokeAccessible`（#616161 / #ADADAD）
 * 达标，但它与 `colorNeutralForeground3` 同值，而滚动条滑块的静止态用
 * `--line-strong`、划过态用 `--ink-3`（base.css），同值等于划过没反应。
 * 所以两档都退到 Grey 灰阶里最接近 3:1 的那一档（Grey.54 / Grey.46）。
 *
 * `--line` 是同层之间的细分隔：亮色取 `colorNeutralStroke1`（1.40:1）；
 * 暗色的 `colorNeutralStroke1` 已经有 2.53:1，与 `--line-strong` 的 3.16:1
 * 分不开，退到 `colorNeutralStroke2`（1.86:1）。Fluent 的暗色描边整体比亮色重，
 * 两档本来就不是镜像关系。
 *
 * ## 与通用主题冲突的两处，都按 Fluent 来
 *
 * - 暗色正文取纯白。通用主题把它钳在 L92 以避开光晕，而 Windows 自己的
 *   暗色正文就是纯白（`colorNeutralForeground1` = `Global.Color.White`）。
 * - 亮色面板 L96 左右，高于通用主题给自己定的 L94 上限。用户已确认
 *   #F5F5F5 就是要的那一档。
 */
export const FLUENT_THEME: Theme = {
  id: "fluent",
  name: "Fluent",
  tokens: {
    light: {
      ground: "#E6E6E6",
      surface: "#F5F5F5",
      "surface-2": "#EBEBEB",
      "surface-3": "#E0E0E0",
      line: "#D1D1D1",
      "line-strong": "#8A8A8A",
      sel: "#D6D6D6",
      "ink-3": "#616161",
      "ink-2": "#424242",
      ink: "#242424",
      accent: "#0078D4",
    },
    dark: {
      ground: "#1F1F1F",
      surface: "#292929",
      "surface-2": "#333333",
      "surface-3": "#3D3D3D",
      line: "#525252",
      "line-strong": "#757575",
      sel: "#474747",
      "ink-3": "#ADADAD",
      "ink-2": "#D6D6D6",
      ink: "#FFFFFF",
      accent: "#2B88D8",
    },
  },
};

export const THEMES: readonly Theme[] = [GENERIC_THEME, FLUENT_THEME];

/**
 * 挑主题所依据的那台机器的身份。
 *
 * 字段都是可选/可空的：前端拿到 capabilities 之前什么都不知道，
 * 这时候 `pickTheme` 必须给出通用主题，而不是抛错。
 */
export interface PlatformIdentity {
  /** 后端 `SystemInfo.os.id`，今天就有 */
  readonly osId?: string | null;
  /**
   * 桌面环境（`gnome` / `kde` / `xfce` …）。**今天后端还不报这个字段**，
   * 所以恒为 undefined，行为与只看 `osId` 完全一致。
   */
  readonly desktop?: string | null;
}

/** 桌面环境 → 设计语言。今天是空的，Adwaita / Breeze / Yaru 将来加在这里。 */
const THEME_BY_DESKTOP: Readonly<Record<string, Theme>> = {};

/** 操作系统 → 设计语言。只有 Windows 有专属的一套，其余落回通用。 */
const THEME_BY_OS: Readonly<Record<string, Theme>> = { windows: FLUENT_THEME };

/**
 * 选一套设计语言。**这是唯一的入口**，加维度只改这里，不动调用点。
 *
 * 优先级是**桌面环境 > 操作系统 > 通用**。桌面环境排在前面，因为界面长什么样
 * 由桌面环境决定，不由发行版决定：Ubuntu 装 KDE 该用 Breeze，
 * Fedora 装 GNOME 该用 Adwaita，而没有桌面的服务器两者都不是，落回通用。
 * Windows 上没有这个分叉，`osId` 一档就够。
 *
 * 认不出时返回通用主题，不假装认识这台机器——与 `findDistro` 的同一条约定。
 */
export function pickTheme(identity: PlatformIdentity): Theme {
  const desktop = identity.desktop?.toLowerCase();
  if (desktop && THEME_BY_DESKTOP[desktop]) return THEME_BY_DESKTOP[desktop];
  const os = identity.osId?.toLowerCase();
  if (os && THEME_BY_OS[os]) return THEME_BY_OS[os];
  return GENERIC_THEME;
}
