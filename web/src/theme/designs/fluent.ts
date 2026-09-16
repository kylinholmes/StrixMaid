import type { Design } from "../design";

/**
 * Fluent：Windows 的设计语言。
 *
 * 全部取自 Fluent 2 官方发布的设计 token（`microsoft/fluentui` 仓库
 * `packages/tokens/src/`），每一条都标了来源文件与标识符，**不要凭记忆改**。
 * 官方 token 表里没有的轴（侧栏宽度）另行注明，不编出处。
 *
 * 引用格式统一成「文件 → 标识符」，例如
 * `global/borderRadius.ts → borderRadiusMedium`。组件侧的取值（按钮高度、
 * 对话框圆角）引到具体的样式文件，因为那才是 Fluent 对「一个按钮该多高」的
 * 正式回答——全局 token 表里没有高度这条轴。
 *
 * ## 颜色
 *
 * 取自 `global/colors.ts` 的 `grey` 灰阶与 `alias/{light,dark}Color.ts` 的别名表；
 * 别名表由 Microsoft 的 token pipeline 生成，文件头写着不要手改。
 *
 * ### 亮色（面板 #F5F5F5）
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
 * ### 暗色（面板 #292929）
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
 * | ink | #E6E6E6 | Grey.90 | 11.66:1 |
 * | accent | #2B88D8 | Tint10 | 3.89:1 |
 *
 * 暗色的四级底取的是 Grey 灰阶的 16 / 20 / 24 / 28，与亮色的 96 / 92 / 88 / 84
 * 步长一致（每档 4）。Fluent 暗色没有一组单调升上去的别名可用：
 * `colorNeutralBackground1Hover`（Grey.24）比 `…Selected`（Grey.22）还浅，
 * 而本项目要求「选中 > 划过」，照抄别名会让阶梯反过来。
 *
 * ### 两档分隔线为什么不是对称取的
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
 * ### 与 StrixMaid 冲突的两处
 *
 * **暗色正文没有取 Fluent 的纯白。** `colorNeutralForeground1` 在暗色下是
 * `Global.Color.White`，而 StrixMaid 把正文钳在 L92 以避开暗底上的光晕。
 * 这一处两边可以同时满足：那条 L92 换算成 sRGB 是 `#E4E4E4`，
 * 而 Fluent 的 `Grey.90` 是 `#E6E6E6`——几乎同一亮度。取 Grey.90 之后
 * 它**既是 Fluent 官方色阶里的值，又落在防光晕的钳位上**，不必二选一。
 * 对面板 11.66:1，远高于正文所需的 4.5:1。
 *
 * 想换回纯白只改这一行（`color.dark.ink`），测试不会挡；中间档是
 * `Grey.94` `#F0F0F0`（12.77:1）。
 *
 * **亮色面板 L96 左右**，高于 StrixMaid 给自己定的 L94 上限。这一处没有折中的
 * 余地，用户已确认 `#F5F5F5` 就是要的那一档。
 */
export const FLUENT = {
  id: "fluent",
  name: "Fluent",
  color: {
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
      // 三档阴影：`utils/shadows.ts → createShadowTokens` 展开后的 shadow8 /
      // shadow16 / shadow64，环境色与主光色取 `alias/lightColor.ts` 的
      // `colorNeutralShadowAmbient`（rgba(0,0,0,0.12)）与
      // `colorNeutralShadowKey`（rgba(0,0,0,0.14)）。
      // 档位的归属来自 Fluent 自己的组件：提示框 shadow8
      // （`react-tooltip → useTooltipStyles.styles.ts`）、菜单与弹层 shadow16
      // （`react-menu → useMenuPopoverStyles.styles.ts`）、对话框 shadow64
      // （`react-dialog → useDialogSurfaceStyles.styles.ts`）。
      "shadow-raised": "0 0 2px rgba(0,0,0,0.12), 0 4px 8px rgba(0,0,0,0.14)",
      "shadow-pop": "0 0 2px rgba(0,0,0,0.12), 0 8px 16px rgba(0,0,0,0.14)",
      "shadow-dialog": "0 0 8px rgba(0,0,0,0.12), 0 32px 64px rgba(0,0,0,0.14)",
      // `alias/lightColor.ts → colorBackgroundOverlay` = BlackAlpha.40
      scrim: "rgba(0, 0, 0, 0.4)",
      // 焦点环两色，`alias/lightColor.ts`：`colorStrokeFocus2` 是黑、
      // `colorStrokeFocus1` 是白（暗色下对调）。Fluent 的焦点环不跟主题色走，
      // 走这一对互为反色的黑白——任何底色上都有一层看得见，这是它与 StrixMaid
      // 「焦点用 accent」的实质分歧。
      //
      // **档位的配对是折算**：`createFocusOutlineStyle` 只画了外环这一层
      // （2px `colorStrokeFocus2`），内环那一层官方是逐控件叠的
      // （例如 `react-button` 再加一道 1px 的 inset 内阴影）。
      // 这里把 `colorStrokeFocus1` 认成内环色，依据是这两个 token 本来就作为
      // 互为反色的一对发布——一对反色只有画成内外两层才有意义。
      focus: "#000000",
      "focus-inner": "#FFFFFF",
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
      // Grey.90。不是 `colorNeutralForeground1`（纯白），理由见模块文档
      // 「与 StrixMaid 冲突的两处」——这一档同时满足 Fluent 与防光晕的钳位。
      ink: "#E6E6E6",
      accent: "#2B88D8",
      // 同亮色，环境色与主光色换成 `alias/darkColor.ts` 的
      // rgba(0,0,0,0.24) / rgba(0,0,0,0.28)——暗底上阴影要更重才看得见。
      "shadow-raised": "0 0 2px rgba(0,0,0,0.24), 0 4px 8px rgba(0,0,0,0.28)",
      "shadow-pop": "0 0 2px rgba(0,0,0,0.24), 0 8px 16px rgba(0,0,0,0.28)",
      "shadow-dialog": "0 0 8px rgba(0,0,0,0.24), 0 32px 64px rgba(0,0,0,0.28)",
      // `alias/darkColor.ts → colorBackgroundOverlay` = BlackAlpha.50
      scrim: "rgba(0, 0, 0, 0.5)",
      // 暗色下两色对调：`colorStrokeFocus2` 是白，`colorStrokeFocus1` 是黑。
      focus: "#FFFFFF",
      "focus-inner": "#000000",
    },
  },
  /**
   * 圆角：`global/borderRadius.ts`。
   *
   * - sm ← `borderRadiusSmall` 2px
   * - md ← `borderRadiusMedium` 4px。Fluent 的按钮
   *   （`react-button → useButtonStyles.styles.ts`）、菜单弹层、提示框、
   *   弹层面板用的都是这一档。
   * - lg ← `borderRadiusXLarge` 8px。对话框专用，来自
   *   `react-dialog → useDialogSurfaceStyles.styles.ts` 的 `borderRadiusXLarge`。
   * - pill ← `borderRadiusCircular` 10000px。
   *
   * `borderRadiusLarge`（6px）没有进表：本项目没有介于菜单与对话框之间的第三种
   * 面板，立一档没有调用点的轴只是噪音。将来真有了再补，那时也有官方值可取。
   */
  radius: {
    "radius-sm": "2px",
    "radius-md": "4px",
    "radius-lg": "8px",
    "radius-pill": "10000px",
  },
  /**
   * 字族：`global/fonts.ts` 的 `fontFamilyBase` 与 `fontFamilyMonospace`，
   * 逐字照抄，**只多插了一段 `var(--cjk)`**。
   *
   * Fluent 的字族 token 里没有中文回退——它的目标语言里没有 CJK。
   * 本项目必须有，所以把项目自己的中文链插在 `sans-serif` / `monospace`
   * 这两个通用族之前，位置与 StrixMaid 那套一致。这一处是**折算**，
   * 不是原值：Fluent 没说过中文该回退到哪里。
   */
  family: {
    cjk: '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"',
    ui: `"Segoe UI", "Segoe UI Web (West European)", -apple-system, BlinkMacSystemFont, Roboto, "Helvetica Neue", var(--cjk), sans-serif`,
    mono: `Consolas, "Courier New", Courier, var(--cjk), monospace`,
  },
  /**
   * 字阶：`global/fonts.ts` 的 `fontSizeBase100`…`fontSizeBase600`。
   *
   * 本项目的 15 档压进 Fluent 的 6 档，映射如下（左 StrixMaid，右 Fluent）：
   *
   * | 档 | StrixMaid | Fluent | 官方 token |
   * |---|---|---|---|
   * | fs-50 / 100 / 200 | 8.5 / 9 / 10px | 10px | `fontSizeBase100` |
   * | fs-300 / 350 / 400 / 450 | 11 / 11.5 / 12 / 12.5px | 12px | `fontSizeBase200` |
   * | fs-500 / 550 / 600 | 13 / 13.5 / 14px | 14px | `fontSizeBase300` |
   * | fs-700 / 800 | 15 / 16px | 16px | `fontSizeBase400` |
   * | fs-900 / 1000 | 17 / 19px | 20px | `fontSizeBase500` |
   * | fs-1100 | 20px | 24px | `fontSizeBase600` |
   *
   * 合并的位置是**折算**，取的值是原值。合并的依据是 Fluent 自己对这几档的
   * 用法：`fontSizeBase300`（14px）是正文，`Base200`（12px）是次要文字与
   * 密排表格，`Base100`（10px）是徽章与角标。本项目的 12.5px 表格落在
   * 「密排表格」这一类，所以跟 12px 走而不是跟 14px 走；13px 正文落在
   * 「正文」这一类，跟 14px 走。两者因此在 Fluent 下分得比今天开，
   * 那正是 Fluent 的排法。
   *
   * 8.5px 在 Fluent 里没有对应物——它的最小字号就是 10px，只能向上取。
   */
  fontSize: {
    "fs-50": "10px",
    "fs-100": "10px",
    "fs-200": "10px",
    "fs-300": "12px",
    "fs-350": "12px",
    "fs-400": "12px",
    "fs-450": "12px",
    "fs-500": "14px",
    "fs-550": "14px",
    "fs-600": "14px",
    "fs-700": "16px",
    "fs-800": "16px",
    "fs-900": "20px",
    "fs-1000": "20px",
    "fs-1100": "24px",
  },
  /**
   * 行高。
   *
   * 后两档是原值：`global/fonts.ts → lineHeightBase100`（14px）与
   * `lineHeightBase200`（16px），正好与 `fs-200` / `fs-300` 在 Fluent 下取到的
   * 字号配对——Fluent 的行高 token 本来就是按字号一一配对发布的。
   *
   * 前六档是**折算**：Fluent 没有无单位的行高倍数，它的行高一律是与字号绑死的
   * 像素值。本项目的正文行高是 1.6 这类倍数（中英混排下比固定像素稳），
   * 换成 Fluent 的固定像素会让 CJK 行盒被截。因此这六档沿用本项目取值，
   * **不假称有官方出处**。
   */
  lineHeight: {
    "lh-none": "1",
    "lh-tight": "1.1",
    "lh-snug": "1.4",
    "lh-body": "1.5",
    "lh-normal": "1.6",
    "lh-loose": "1.65",
    "lh-200": "14px",
    "lh-300": "16px",
  },
  /**
   * 控件尺寸。
   *
   * - `row` 34px ← `react-table → useTableCellStyles.styles.ts` 的 `small`
   *   档（medium 44px / small 34px / extra-small 24px）。本项目的表是密排表，
   *   对应 Fluent 的 small。
   * - `row-toolbar` 40px ← `react-toolbar → useToolbarStyles.styles.ts` 的
   *   medium 档 `padding: 4px 8px`，加上里面 medium 按钮的 32px：4+32+4=40。
   *   这一条是**折算**（按官方样式推出来的），Fluent 没有「工具条高度」这条 token。
   * - `row-sm` 24px ← `react-button → useButtonStyles.styles.ts` 的 small 档：
   *   `useRootIconOnlyStyles.small` 写死 24px，文字按钮同高
   *   （3px+3px padding + `lineHeightBase200` 16px + 2×`strokeWidthThin`）。
   * - `row-xs` 24px ← `useTableCellStyles.styles.ts` 的 `extra-small` 档。
   * - `rail` / `rail-narrow`：**Fluent 没有对应的官方 token**。侧栏宽度是本产品
   *   的信息密度决定的，不是设计语言的主张，沿用本项目取值，不编出处。
   * - `icon` 20px ← `useButtonStyles.styles.ts → useIconBaseClassName`
   *   （medium 按钮的图标 20×20）。
   * - `icon-lg` 24px ← 同文件 `useIconStyles.large`。
   */
  size: {
    row: "34px",
    "row-toolbar": "40px",
    "row-sm": "24px",
    "row-xs": "24px",
    rail: "184px",
    "rail-narrow": "44px",
    icon: "20px",
    "icon-lg": "24px",
  },
  /**
   * 间距：`global/spacings.ts` 的 `spacings`（`horizontalSpacings` /
   * `verticalSpacings` 两组都由它展开，横竖同值）。
   *
   * 十档与 StrixMaid 逐档相同——`xxs` 2 / `xs` 4 / `sNudge` 6 / `s` 8 /
   * `mNudge` 10 / `m` 12 / `l` 16 / `xl` 20 / `xxl` 24 / `xxxl` 32。
   * 取值相同不是省掉这条轴的理由：Adwaita 的间距阶是 6 的倍数，到时候要改的
   * 是这一个文件，不是 23 个 CSS 模块。
   */
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
  /** 描边：`global/strokeWidths.ts` 的 Thin / Thick / Thicker / Thickest。 */
  stroke: {
    stroke: "1px",
    "stroke-thick": "2px",
    "stroke-thicker": "3px",
    "stroke-thickest": "4px",
  },
  /**
   * 焦点环的几何：`react-tabster → createFocusOutlineStyle.ts`。
   *
   * 宽度 2px 是那里写死的（源码里还留着一条注释说明为什么没直接用
   * `strokeWidthThick`）。偏移 -2px 来自同文件的 `getOutlinePosition`：
   * 没有显式 offset 时四边取 `calc(outlineWidth * -1)`，也就是把环收进元素内侧，
   * 正好压在元素自己的边框上（那条边框同时被置成 transparent）。
   *
   * 与 StrixMaid 正好相反——那一套把环推到外面留 2px 空。两种做法靠这一条轴
   * 分叉，不需要作用域覆盖。
   */
  focus: {
    "focus-width": "2px",
    "focus-offset": "-2px",
  },
  /**
   * 动效：时长取 `global/durations.ts`，缓动取 `global/curves.ts`。
   *
   * | 档 | StrixMaid | Fluent | 官方 token |
   * |---|---|---|---|
   * | t-pop | 90ms | 100ms | `durationFaster` |
   * | t-color | 120ms | 100ms | `durationFaster` |
   * | t-arrive | 150ms | 150ms | `durationFast` |
   * | t-enter | 240ms | 250ms | `durationGentle` |
   * | t-shape | 440ms | 400ms | `durationSlower` |
   * | t-shape-out | 320ms | 300ms | `durationSlow` |
   *
   * `t-color` 取 `durationFaster` 有直接依据：Fluent 的按钮就是用
   * `durationFaster` + `curveEasyEase` 过渡 background / border / color
   * （`react-button → useButtonStyles.styles.ts`）。`t-arrive` 与 `t-enter`
   * 取 `durationFast` / `durationGentle`，对应 Fluent 自己的两个淡入预设
   * `FadeSnappy` / `FadeRelaxed`（`react-motion-components-preview → Fade.ts`）。
   * 其余几档按「取最接近的那一档官方时长」折算。
   *
   * 缓动四条都是原值：`curveEasyEase` / `curveDecelerateMid` /
   * `curveEasyEaseMax` / `curveDecelerateMin`。**没有引入关键帧或形变动画**——
   * 这个产品不需要，轴留着够将来填就行。
   */
  motion: {
    "t-pop": "100ms",
    "t-color": "100ms",
    "t-arrive": "150ms",
    "t-enter": "250ms",
    "t-shape": "400ms",
    "t-shape-out": "300ms",
    ease: "cubic-bezier(0.33,0,0.67,1)",
    "ease-out": "cubic-bezier(0,0,0,1)",
    "ease-in-out": "cubic-bezier(0.8,0,0.2,1)",
    "ease-shape": "cubic-bezier(0.33,0,0.1,1)",
  },
  // `as const` 的理由见 strixmaid.ts
} as const satisfies Design;
