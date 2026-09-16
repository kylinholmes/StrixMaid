/**
 * 设计语言的**词汇表**（2026-09-16 第四版）。
 *
 * 第三版的 `Theme.tokens` 只有颜色，圆角、字阶、控件尺寸、间距、描边、阴影、动效
 * 全部写死在 23 个 CSS 模块里。于是「换一套设计语言」实际只换得动中性灰，
 * 界面骨架仍然是同一副：方角、13px 正文、30px 行高。Fluent 在 Windows 上
 * 之所以是 Fluent，圆角与字阶的分量不比灰阶小。
 *
 * 这一版把那些轴一并收进 token，目标是**一套设计语言 = 一个文件**：
 * `designs/<name>.ts` 填满本文件定义的每一条轴，`buildTheme` 压成 `Theme`，
 * 组件与 CSS 一行都不必动。加 Adwaita / Breeze / Yaru 时只多一个文件。
 *
 * ## 为什么轴按「存在」而不是按「有差异」来划
 *
 * 两套语言在某条轴上取值相同，这条轴仍然要有。`--stroke` 今天两边都是 1px，
 * `--sp-*` 两边完全一致——把它们省掉，第三套语言需要 2px 描边时就是又一次
 * 全库重构。轴的成本是一行常量，漏轴的成本是一轮返工。
 *
 * ## 分组的依据是「随什么变」，不是「长得像什么」
 *
 * `color` 与其余的分开，是因为只有它随亮暗两档变；阴影与蒙层虽然不是颜色，
 * 但它们的不透明度在暗色下要整体加深（暗底上浅阴影看不见），所以归在 `color`
 * 这一组里跟着模式走。圆角、字阶、尺寸、间距、描边、动效与模式无关，
 * 一套语言只填一份，`buildTheme` 复制进亮暗两份 `TokenSet`。
 *
 * ## token 名就是 CSS 变量名
 *
 * 这里的每个字符串去掉引号前面加 `--` 就是 CSS 里那个变量，`applyTheme`
 * 直接 `setProperty(\`--${name}\`, value)`，中间不做任何映射。改名等于全库重排，
 * 所以名字一旦定下就不要动。
 */

export type Mode = "dark" | "light";

/**
 * 中性色。键的顺序就是「页底 → 面板 → 分隔 → 正文」的深浅顺序，
 * 合集页按这个顺序排色卡。
 */
export const NEUTRAL_TOKENS = [
  "ground",
  "surface",
  "surface-2",
  "surface-3",
  "line",
  "line-strong",
  "sel",
  "ink-3",
  "ink-2",
  "ink",
] as const;
export type NeutralToken = (typeof NEUTRAL_TOKENS)[number];

/**
 * 随亮暗两档变化的全部轴：中性色 + 主题色 + 三档阴影 + 蒙层 + 焦点环两色。
 *
 * 阴影与蒙层在这一组，理由见模块文档。焦点环的两色也在这一组：Fluent 的外环是
 * 亮色下的黑、暗色下的白（`colorStrokeFocus2`），内环正好相反，两者都随模式翻转。
 */
export const MODE_TOKENS = [
  ...NEUTRAL_TOKENS,
  /** 主题色。认得出机器时会被发行版身份覆盖，见 useTheme.ts 的 `applyTheme` */
  "accent",
  /** 贴着面板浮起来的一档：提示框 */
  "shadow-raised",
  /** 脱离面板的一档：菜单、弹层、抽屉 */
  "shadow-pop",
  /** 压住整页的一档：模态对话框 */
  "shadow-dialog",
  /** 模态对话框身后的蒙层 */
  "scrim",
  /** 焦点环的外环色 */
  "focus",
  /** 焦点环的内环色。只有画双描边的语言用得上，单环的语言填 transparent */
  "focus-inner",
] as const;
export type ModeToken = (typeof MODE_TOKENS)[number];

/**
 * 圆角。
 *
 * 四档对应本项目真的存在的四种形状：小徽章、标准控件、模态对话框、胶囊。
 * 没有第五档，因为界面里没有第五种形状——轴要覆盖需求，不是覆盖别人的 token 表。
 */
export const RADIUS_TOKENS = [
  /** 徽章、计数、标签、进度条这类小方块 */
  "radius-sm",
  /** 按钮、输入框、菜单、提示框、面板 —— 绝大多数控件 */
  "radius-md",
  /** 模态对话框 */
  "radius-lg",
  /** 胶囊：圆角大于自身高度的一半 */
  "radius-pill",
] as const;
export type RadiusToken = (typeof RADIUS_TOKENS)[number];

/** 字族。`ui` 与 `mono` 里都插着 `var(--cjk)`，中文回退链只写一份。 */
export const FAMILY_TOKENS = ["ui", "mono", "cjk"] as const;
export type FamilyToken = (typeof FAMILY_TOKENS)[number];

/**
 * 字阶。
 *
 * 档号按 Fluent 的 `fontSizeBaseNNN` 的排法取整百，中间插了三档 `*50`：
 * 本项目今天用着 11.5 / 12.5 / 13.5 三个半像素档，它们既不是相邻整数档的别名
 * （半个像素在 1x 屏上看得出来），又确实各自只服务一两处。这一轮的规矩是
 * 「只搬不并」，所以原样立成档；要不要收敛是下一件事，见 README 之外的交付报告。
 *
 * 一套新语言填这张表时**不必逐档给不同的值**：Fluent 就把 15 档压进了官方的
 * 6 档字号里，相邻几档取同值。档位在、映射清楚，比档档有别重要。
 */
export const FONT_SIZE_TOKENS = [
  "fs-50",
  "fs-100",
  "fs-200",
  "fs-300",
  "fs-350",
  "fs-400",
  "fs-450",
  /** 正文基准。`body` 取的就是这一档 */
  "fs-500",
  "fs-550",
  "fs-600",
  "fs-700",
  "fs-800",
  "fs-900",
  "fs-1000",
  "fs-1100",
] as const;
export type FontSizeToken = (typeof FONT_SIZE_TOKENS)[number];

/**
 * 行高。
 *
 * 前六档是无单位倍数，跟着字号走；后两档是**固定像素**，它们撑的不是行距而是
 * 盒高——徽章（`lh-200`）与提示框（`lh-300`）靠行高定高度，中英文混排时
 * CJK 回退字体的行盒更高，不钉死会让相邻区块差 1px。两类不能互换。
 */
export const LINE_HEIGHT_TOKENS = [
  /** 1：文字盒即字高，用在自己撑 padding 的控件上（按钮、分段控件） */
  "lh-none",
  "lh-tight",
  "lh-snug",
  "lh-body",
  /** 正文基准。`body` 取的就是这一档 */
  "lh-normal",
  "lh-loose",
  /** 固定像素：徽章的盒高，与 `fs-200` 配对 */
  "lh-200",
  /** 固定像素：提示框的行盒，与 `fs-300` 配对 */
  "lh-300",
] as const;
export type LineHeightToken = (typeof LINE_HEIGHT_TOKENS)[number];

/** 控件尺寸：四档高度 + 侧栏两档宽度 + 两档图标。 */
export const SIZE_TOKENS = [
  /** 列表行、导航项、表格单元格 */
  "row",
  /** 工具条、页头 */
  "row-toolbar",
  /** 紧凑控件：工具条里的搜索框、下拉钮、小号输入框 */
  "row-sm",
  /** 分组标题这类只占一行文字的条带 */
  "row-xs",
  "rail",
  "rail-narrow",
  /** 行内图标（Lucide、程序图标、发行版方块） */
  "icon",
  /** 大一号的图标（登录门里的发行版方块） */
  "icon-lg",
] as const;
export type SizeToken = (typeof SIZE_TOKENS)[number];

/**
 * 间距。十档，2 / 4 / 6 / 8 / 10 / 12 / 16 / 20 / 24 / 32。
 *
 * 这条阶梯本项目与 Fluent 逐档相同，不是巧合：8 的倍数加两个半档（6、10）
 * 是当代界面间距的通行排法。**取值相同不等于轴可以省**，见模块文档。
 */
export const SPACE_TOKENS = [
  "sp-1",
  "sp-2",
  "sp-3",
  "sp-4",
  "sp-5",
  "sp-6",
  "sp-7",
  "sp-8",
  "sp-9",
  "sp-10",
] as const;
export type SpaceToken = (typeof SPACE_TOKENS)[number];

/** 描边宽度。四档：发丝线、强调线、身份色边条、侧栏顶色带。 */
export const STROKE_TOKENS = [
  "stroke",
  "stroke-thick",
  "stroke-thicker",
  "stroke-thickest",
] as const;
export type StrokeToken = (typeof STROKE_TOKENS)[number];

/**
 * 焦点环的几何。颜色在 `MODE_TOKENS` 里（随亮暗翻转），这两条不随。
 *
 * 分出来单立一组而不是并进 `STROKE_TOKENS`，是因为 `focus-offset` 允许负值：
 * 正值把环推到控件外面（本项目的做法，留空避免与控件自身的 1px 边框糊成一条），
 * 负值把环收进控件里面（Fluent 的做法，环压在自己的边框上）。
 * 这条轴不立起来，两种做法就只能靠作用域覆盖分叉。
 */
export const FOCUS_TOKENS = ["focus-width", "focus-offset"] as const;
export type FocusToken = (typeof FOCUS_TOKENS)[number];

/**
 * 动效：六档时长 + 四条缓动曲线。
 *
 * **刻意只做到这一层。** 复杂的动效与视效在这个产品里意义不大——它是一块盯着
 * 看的仪表盘，不是需要引导注意力的消费级界面。留下时长与缓动两条轴是为了
 * 将来某套语言真的需要时有地方填，不是为了现在填满：这里没有关键帧体系、
 * 没有形变动画，也不打算加。
 */
export const MOTION_TOKENS = [
  /** 浮层出现 */
  "t-pop",
  /** 颜色、边框的状态过渡 */
  "t-color",
  /** 数据到达时的一次淡入 */
  "t-arrive",
  /** 整块内容替换时的淡入 */
  "t-enter",
  /** 侧栏展开 */
  "t-shape",
  /** 侧栏合上。比展开短——关箱子要干脆 */
  "t-shape-out",
  /** 通用缓动 */
  "ease",
  /** 进入类：先快后慢 */
  "ease-out",
  /** 往复类：两头慢 */
  "ease-in-out",
  /** 形变类：侧栏宽度这种大位移 */
  "ease-shape",
] as const;
export type MotionToken = (typeof MOTION_TOKENS)[number];

/** 与模式无关的全部轴。一套语言只填一份，亮暗两档共用。 */
export const SHARED_TOKENS = [
  ...RADIUS_TOKENS,
  ...FAMILY_TOKENS,
  ...FONT_SIZE_TOKENS,
  ...LINE_HEIGHT_TOKENS,
  ...SIZE_TOKENS,
  ...SPACE_TOKENS,
  ...STROKE_TOKENS,
  ...FOCUS_TOKENS,
  ...MOTION_TOKENS,
] as const;
export type SharedToken = (typeof SHARED_TOKENS)[number];

/** 落到 `:root` 上的全部 CSS 变量名（不含 `--` 前缀）。 */
export const TOKEN_NAMES = [...MODE_TOKENS, ...SHARED_TOKENS] as const;
export type TokenName = (typeof TOKEN_NAMES)[number];

/**
 * 一套完整的界面 token（某一档模式下的）。
 *
 * 值直接就是**最终的 CSS 值串**，不是明度数字或像素数：通用主题的中性色存
 * `oklch(L% 0 0)`，Fluent 存十六进制原值，尺寸存 `30px`，缓动存 `ease-out`。
 * 两种以上的写法必须能并存，否则各家官方的原值就只能换算一遍再写回去，
 * 而换算的近似值正是这一版要摆脱的东西。
 */
export type TokenSet = Readonly<Record<TokenName, string>>;

export interface Theme {
  /** 设计语言的 id，不是平台 id——Breeze 一套要服务装着 KDE 的所有发行版 */
  readonly id: string;
  readonly name: string;
  readonly tokens: { readonly light: TokenSet; readonly dark: TokenSet };
}

/**
 * 一套设计语言的**书写形式**。
 *
 * 与 `Theme` 分开是为了让写文件的人按轴填，而不是把七八十个键平铺两遍：
 * 随模式变的填 `color`，其余按轴各填一组，`buildTheme` 负责压平。
 * 每一组都是 `Record<该轴的全部 token, string>`，漏一条编译就过不去——
 * 「加一套语言只写一个文件」的保证在类型上，不在文档上。
 */
export interface Design {
  readonly id: string;
  readonly name: string;
  readonly color: {
    readonly light: Readonly<Record<ModeToken, string>>;
    readonly dark: Readonly<Record<ModeToken, string>>;
  };
  readonly radius: Readonly<Record<RadiusToken, string>>;
  readonly family: Readonly<Record<FamilyToken, string>>;
  readonly fontSize: Readonly<Record<FontSizeToken, string>>;
  readonly lineHeight: Readonly<Record<LineHeightToken, string>>;
  readonly size: Readonly<Record<SizeToken, string>>;
  readonly space: Readonly<Record<SpaceToken, string>>;
  readonly stroke: Readonly<Record<StrokeToken, string>>;
  readonly focus: Readonly<Record<FocusToken, string>>;
  readonly motion: Readonly<Record<MotionToken, string>>;
}

/**
 * 书写形式 → 落盘形式。
 *
 * 泛型不是摆设：`Design.id` 声明成 `string`，但 `ThemeId`、`DESIGN_OPTIONS`
 * 这条推导链的源头必须是字面量类型，否则「选了一套没实现的语言」在类型上
 * 就挡不住了。`D extends Design` 让 `design.id` 保留调用点传进来的字面量。
 */
export function buildTheme<D extends Design>(
  design: D,
): Theme & { readonly id: D["id"]; readonly name: D["name"] } {
  const shared = {
    ...design.radius,
    ...design.family,
    ...design.fontSize,
    ...design.lineHeight,
    ...design.size,
    ...design.space,
    ...design.stroke,
    ...design.focus,
    ...design.motion,
  };
  return {
    id: design.id,
    name: design.name,
    tokens: {
      light: { ...shared, ...design.color.light },
      dark: { ...shared, ...design.color.dark },
    },
  };
}
