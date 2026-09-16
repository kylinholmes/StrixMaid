import type { Design } from "../design";

/**
 * Adwaita：GNOME 的设计语言。
 *
 * 取值来自 libadwaita 的样式表源码（`src/stylesheet/`，核对过 1.7.12、1.10.0
 * 与 main 三处，三者一致）与 GNOME HIG。**每一条注释都标了它是「原值」还是
 * 「折算」**：
 *
 * - **原值**：直接抄自上游源码或官方文档，给出文件与标识符。
 * - **折算**：上游没有这条轴，按其发布的常量或 HIG 的规范推出来的，写清依据与推法。
 *
 * 这条界线在这套语言上比 Fluent 那套重要得多。Fluent 有微软发布的 Web token 包，
 * 逐条照抄即可；Adwaita 是一套 GTK 主题，颜色、圆角、控件最小高度、焦点环几何
 * 都能对出原值，但「Web 的字阶」「十档间距」「三档阴影」这些轴在 GTK 那边**没有
 * 对应物**——libadwaita 的字号是相对基准字体的百分比，间距是散在各控件里的
 * 字面量，阴影是逐控件写死的 `box-shadow`。这几条只能折算，下面逐条写明推法。
 *
 * ## 两处上游的事实需要先说清楚，否则下面的取值看着像抄错了
 *
 * **一、`_defaults.scss` 已经不存在。** 具名颜色现在全部在
 * `src/stylesheet/_colors.scss` 里，以 `@define-color` 与 CSS 自定义属性的形式
 * 发布，不再是 SCSS 变量。下面引用的一律是 `@define-color` 的名字。
 *
 * **二、默认字体不是 Cantarell。** GNOME 48/49 起
 * `org.gnome.desktop.interface font-name` 的默认值是 `Adwaita Sans 11`
 * （gsettings-desktop-schemas），HIG 的排版页也明文说「GNOME 的默认字体是
 * Adwaita Sans，Inter 的一个定制变体」。Cantarell 只能当回退。
 *
 * ## 亮暗两档里 window 与 view 的深浅关系是反的
 *
 * 亮色：`window_bg_color` #fafafb 比 `view_bg_color` #ffffff **深**；
 * 暗色：`window_bg_color` #222226 比 `view_bg_color` #1d1d20 **浅**。
 *
 * 本项目的结构不变量是「面板浮在页底之上」（`--ground` 比 `--surface` 暗，
 * tokens.test.ts 有断言）。所以两档各取 window / view 里**较深的那个**作
 * `--ground`、较浅的那个作 `--surface`：亮色 ground=window / surface=view，
 * 暗色 ground=view / surface=window。**色值全是原值，折算的只是角色的分配**，
 * 而且反转这件事是 Adwaita 自己做的，不是这里编的。
 *
 * ## 对比度实算（WCAG 相对亮度，下同）
 *
 * | | 亮 | 暗 |
 * |---|---|---|
 * | 正文 `--ink` 对面板 | 12.56:1 | 15.85:1 |
 * | `--accent` 对面板 | 3.77:1 | 4.21:1 |
 * | `--ink-2` 对面板 | 6.17:1 | 9.98:1 |
 * | `--ink-3` 对面板 | 3.23:1 | 5.78:1 |
 * | `--line-strong` 对面板 | 2.84:1 | 5.04:1 |
 *
 * `--ink-3` 亮色只有 3.23:1、`--line-strong` 亮色只有 2.84:1，两条都低于各自
 * 理想的门槛（正文 4.5:1 / 非文字 3:1）。**这两条是 Adwaita 自己的主张，不是这里
 * 算错**：它的次要文字就是 `--dim-opacity: 55%` 压出来的，它的控件边界就是
 * `currentColor 15%`（本项目取的已经是高对比档的 50%）。测试里没有这两条断言，
 * 所以不构成失败；要往深里调就不是 Adwaita 了。
 */
export const ADWAITA = {
  id: "adwaita",
  name: "Adwaita",
  color: {
    light: {
      // 原值：`_colors.scss` → `@define-color window_bg_color #fafafb`
      ground: "#FAFAFB",
      // 原值：`_colors.scss` → `@define-color view_bg_color #ffffff`
      // （亮色下 `card_bg_color`、`popover_bg_color`、`headerbar_bg_color` 也都是 #ffffff）
      surface: "#FFFFFF",
      // 原值：`_colors.scss` → `@define-color secondary_sidebar_bg_color #f3f3f5`
      // 折算的是角色：上游没有「斑马纹偶数行」这个概念，本项目的四级底按
      // 明度顺序去认领 Adwaita 自己发布的那几档面板色。
      "surface-2": "#F3F3F5",
      // 原值：`_colors.scss` → `@define-color sidebar_bg_color #ebebed`
      "surface-3": "#EBEBED",
      // 折算：`_colors.scss` 的 `$view_selected_color`
      // = `color-mix(in srgb, var(--accent-bg-color) 25%, transparent)`
      // （`widgets/_lists.scss` 把它用在 `row:selected` 上），压到 `view_bg_color`
      // 上得 #CDE0F8。alpha 与 accent 都是原值，压平是折算——本项目的 token 必须是
      // 不透明色（tokens.test.ts 要求 `#RRGGBB`）。
      // **选中底带蓝，是 Adwaita 的主张**：它的选中行本来就是 accent 染的，
      // 不像 StrixMaid 只靠明度。亮色下深字压在上面 9.4:1，读得动。
      sel: "#CDE0F8",
      // 折算：`_colors.scss` 的 `--border-color`
      // = `color-mix(in srgb, currentColor var(--border-opacity), transparent)`，
      // `--border-opacity: 15%`；亮色的 `currentColor` 是 `view_fg_color`
      // = `rgb(0 0 6 / 80%)`，两重 alpha 相乘得 0.12，压到 #ffffff 上是 #E0E0E1。
      line: "#E0E0E1",
      // 折算：同上，取上游的**高对比档** `--border-opacity: 50%`
      // （`_colors.scss` 的 `@media (prefers-contrast: more)`），0.8×0.5=0.4。
      // 为什么不取常规档：`--line-strong` 在本项目是控件边界 + 表头下沿 +
      // 滚动条滑块，常规档压出来对面板只有 1.32:1，滑块会消失。高对比档 2.84:1
      // 仍低于 WCAG 1.4.11 的 3:1，见模块文档末尾那段。
      "line-strong": "#99999B",
      // 折算：`view_fg_color` = `rgb(0 0 6 / 80%)` 压到面板上。
      // 不是纯黑——Adwaita 的正文本来就带 20% 的透。
      ink: "#333338",
      // 折算：上游只发布**两档**文字明度（原色与 `--dim-opacity: 55%`），
      // 本项目要三档。中间这档取两者 alpha 的中点 0.775（0.8×0.775=0.62），
      // 依据是 Adwaita 的 dim 语义只到一档，再分就没有出处可引了。
      "ink-2": "#616165",
      // 折算：`_colors.scss` 的 `--dim-opacity: 55%`（`.dimmed`，旧名 `.dim-label`）
      // 叠在 80% 的 `view_fg_color` 上 = 0.44，压到面板上。alpha 是原值。
      "ink-3": "#8F8F91",
      // 原值：`src/adw-accent-color.c` → `adw_accent_color_to_rgba()`，
      // `ADW_ACCENT_COLOR_BLUE` = `#3584e4`，文档注明这是默认值。
      // 亮暗两档同值——`accent_bg_color` 不随模式变，随模式变的是另一个
      // `accent_color`（独立使用的那档，亮 #0461be / 暗 #81d0ff）。
      // 这里取 `accent_bg_color`，因为本项目的 `--accent` 全部用在**填充与描边**
      // 上（4px 上边框、3px 边条、边框色、焦点环、进度条），一次都没用在文字上。
      accent: "#3584E4",
      // 原值：`widgets/_misc.scss` 的 `.card`
      "shadow-raised":
        "0 0 0 1px rgba(0, 0, 6, 0.03), 0 1px 3px 1px rgba(0, 0, 6, 0.07), 0 2px 6px 2px rgba(0, 0, 6, 0.03)",
      // 原值：`widgets/_popovers.scss` 的 popover
      "shadow-pop":
        "0 0 0 1px rgba(0, 0, 0, 0.05), 0 1px 5px 1px rgba(0, 0, 0, 0.09), 0 2px 14px 3px rgba(0, 0, 0, 0.05)",
      // 折算（档位归并）：上游只发布了 card 与 popover 两档阴影，对话框那一档
      // 没有单独的取值。取 popover 那一档，不另编数字。
      "shadow-dialog":
        "0 0 0 1px rgba(0, 0, 0, 0.05), 0 1px 5px 1px rgba(0, 0, 0, 0.09), 0 2px 14px 3px rgba(0, 0, 0, 0.05)",
      // 折算：上游没有发布模态蒙层的取值。取 libadwaita 自己最重的那一档 shade
      // （`@define-color card_shade_color` / `headerbar_shade_color` 的暗色档
      // `rgb(0 0 6 / 36%)`），底色用它的 shade 基色 `rgb(0 0 6)`。
      scrim: "rgba(0, 0, 6, 0.36)",
      // 原值（不透明度）+ 折算（用哪个 accent）：`_drawing.scss` 的 `focus-ring`
      // mixin 用 `$focus_border_color`
      // = `color-mix(in srgb, var(--accent-color) $focus_border_opacity, transparent)`，
      // `$focus_border_opacity: 50%`。上游混的是独立档的 `--accent-color`，这里混
      // `var(--accent)`：本项目的 accent 会被发行版身份在运行时覆盖，抄一份就跟不上了。
      focus: "color-mix(in srgb, var(--accent) 50%, transparent)",
      // 本语言的焦点环是单层。轴要填满，所以填透明（双环只在 Fluent 那一处覆盖里画）。
      "focus-inner": "transparent",
    },
    dark: {
      // 原值：`_colors.scss` 的暗色档 → `view_bg_color #1d1d20`。
      // 暗色下 view 比 window **深**，所以它在这里当页底，见模块文档。
      ground: "#1D1D20",
      // 原值：`_colors.scss` 的暗色档 → `window_bg_color #222226`
      surface: "#222226",
      // 原值：`_colors.scss` 的暗色档 → `secondary_sidebar_bg_color #28282c`
      "surface-2": "#28282C",
      // 原值：`_colors.scss` 的暗色档 → `sidebar_bg_color #2e2e32`
      // （`headerbar_bg_color` 暗色也是 #2e2e32）
      "surface-3": "#2E2E32",
      // 原值：`_colors.scss` 的暗色档 → `popover_bg_color` / `dialog_bg_color` #36363a。
      // 暗色这一档**不用**亮色那种 accent 染的选中底：25% 的蓝压在 #222226 上
      // 得到的深蓝比 `surface-3` 还暗，四级底的阶梯会倒过来。改用上游自己的
      // 最浅一档中性面板色，阶梯与「选中最亮」两件事同时成立。
      sel: "#36363A",
      // 折算：同亮色，`--border-opacity: 15%`；暗色的 `currentColor` 是
      // `view_fg_color` = `white`（alpha 1），所以直接 15% 白压在面板上。
      line: "#434347",
      // 折算：同亮色，取高对比档 50%。暗色下得 5.04:1，比亮色那档宽裕得多——
      // Adwaita 的暗色描边本来就比亮色重。
      "line-strong": "#919193",
      // 原值：`_colors.scss` 的暗色档 → `window_fg_color: white`。
      // 这一处与 StrixMaid 的主张冲突：那套把正文钳在 L92 以避开暗底上的光晕
      // （见 strixmaid.ts）。但那是**那条灰阶自己的结论**，不是全局公理，
      // Adwaita 的正文就是纯白。对面板 15.85:1。
      ink: "#FFFFFF",
      // 折算：同亮色，alpha 取原色与 `--dim-opacity` 的中点 0.775。
      "ink-2": "#CDCDCE",
      // 折算：`--dim-opacity: 55%` 的白压在面板上。alpha 是原值。
      "ink-3": "#9C9C9D",
      // 原值：同亮色。`accent_bg_color` 不随模式变。
      accent: "#3584E4",
      // 折算：几何与层数是亮色那三层的原值，**不透明度按上游自己的 shade 档
      // 同比提高**——`@define-color shade_color` 亮色 7%、暗色 25%，比值 25/7≈3.57，
      // 三层的 3% / 7% / 3% 照此得 11% / 25% / 11%。暗底上不加深阴影就看不见。
      "shadow-raised":
        "0 0 0 1px rgba(0, 0, 6, 0.11), 0 1px 3px 1px rgba(0, 0, 6, 0.25), 0 2px 6px 2px rgba(0, 0, 6, 0.11)",
      // 折算：同上，popover 的 5% / 9% / 5% ×3.57 得 18% / 32% / 18%。
      "shadow-pop":
        "0 0 0 1px rgba(0, 0, 0, 0.18), 0 1px 5px 1px rgba(0, 0, 0, 0.32), 0 2px 14px 3px rgba(0, 0, 0, 0.18)",
      // 折算（档位归并）：同亮色，与 pop 同值。
      "shadow-dialog":
        "0 0 0 1px rgba(0, 0, 0, 0.18), 0 1px 5px 1px rgba(0, 0, 0, 0.32), 0 2px 14px 3px rgba(0, 0, 0, 0.18)",
      // 折算：同亮色。上游没给蒙层写暗色覆盖，两档同值。
      scrim: "rgba(0, 0, 6, 0.36)",
      // 同亮色，见那边的注释。
      focus: "color-mix(in srgb, var(--accent) 50%, transparent)",
      "focus-inner": "transparent",
    },
  },
  /**
   * 圆角：`src/stylesheet/_common.scss` 与各控件文件，全部是原值。
   *
   * - sm ← `widgets/_checks.scss` 的 `$check_radius: 6px`。上游没有比这更小的一档，
   *   本项目的徽章、进度槽、状态方块落在这里。
   * - md ← `_common.scss` 的 `$button_radius: 9px`（`widgets/_buttons.scss` 的
   *   `button`、`widgets/_menus.scss` 的 `modelbutton`、`widgets/_tooltip.scss`
   *   用的都是这一档；`$menu_radius` 同为 9px）。
   * - lg ← `_common.scss` 的 `$dialog_radius: $button_radius + 6` = 15px
   *   （`widgets/_dialogs.scss` 的 `floating-sheet > sheet`；`$popover_radius`
   *   与 `--window-radius` 同为 15px）。
   * - pill ← `widgets/_buttons.scss` 的 `&.pill { border-radius: 9999px }`。
   *
   * `$card_radius: 12px` 没有进表：本项目没有介于按钮与对话框之间的第三种圆角，
   * 立一档没有调用点的轴只是噪音。
   */
  radius: {
    "radius-sm": "6px",
    "radius-md": "9px",
    "radius-lg": "15px",
    "radius-pill": "9999px",
  },
  /**
   * 字族。
   *
   * `ui` / `mono` 的**头一个名字是原值**：
   * `org.gnome.desktop.interface` 的 `font-name` 默认 `Adwaita Sans 11`、
   * `monospace-font-name` 默认 `Adwaita Mono 11`
   * （gsettings-desktop-schemas 的 `org.gnome.desktop.interface.gschema.xml.in`）。
   * HIG 排版页：「GNOME 的默认字体是 Adwaita Sans，Inter 的一个定制变体」。
   *
   * **后面的回退链是折算**：上游发布的是一个**字体名**，不是 Web 的字族栈。
   * Inter 排在 Adwaita Sans 之后，因为官方文档说前者是后者的母体；Cantarell 再后，
   * 因为它是 GNOME 48 之前的默认值，装着旧系统的机器上还有。中文链插在通用族之前，
   * 位置与 StrixMaid 那套一致——Adwaita 没说过中文该回退到哪里。
   *
   * **这两个头名现在是随包发的 woff2**（`styles/base.css` 的 `@font-face`，
   * 拉丁子集，出处与许可见 `assets/fonts/NOTICE.txt`）。上游两份字体都在
   * OFL 1.1 之下，允许随产品再分发，于是这一套换到哪台机器上都真的换字形，
   * 不是只换了颜色。后面整条回退链原样留着，woff2 取不到时还有东西顶上。
   */
  family: {
    cjk: '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"',
    ui: `"Adwaita Sans", Inter, Cantarell, var(--cjk), sans-serif`,
    mono: `"Adwaita Mono", var(--cjk), ui-monospace, monospace`,
  },
  /**
   * 字阶。**档位是原值，px 与合并是折算。**
   *
   * libadwaita 的字号是相对基准字体的百分比（`widgets/_labels.scss`），
   * 基准是 `Adwaita Sans 11`（11pt）。折算成 px 的公式是 96/72 dpi：
   * 11pt = 14.67px。各档：
   *
   * | 类 | 百分比（原值） | px（折算） |
   * |---|---|---|
   * | `.caption` / `.caption-heading` | 82% | 12 |
   * | `.body`（基准） | 100% | 15 |
   * | `.title-4` | 118% | 17 |
   * | `.title-3` / `.title-2` | 136% | 20 |
   * | `.title-1` | 181% | 27 |
   *
   * 本项目的 15 档压进这 5 档，边界在 fs-350 与 fs-400 之间：
   * fs-350 以下是 hint、元信息、表头这类**次要文字**，落 `.caption`；
   * fs-400 起是小号正文、表格正文、界面正文，落 `.body`。
   *
   * **表头（fs-200）与表格正文（fs-450）因此分到了两档**，这一点与 Fluent 一致；
   * 但 8.5 / 9 / 10 / 11 / 11.5px 五档在 Adwaita 下**全部同为 12px**——它的字阶
   * 在正文以下只有 `.caption` 一档，没有第二档可分。GNOME 的界面本来就不靠字号
   * 分层，靠留白与字重。
   *
   * HIG 的排版页只给了这些类的**名字**，一个数字都没有；数字只在 libadwaita 的
   * 样式表里。所以这里引的是样式表，不是 HIG。
   */
  fontSize: {
    "fs-50": "12px",
    "fs-100": "12px",
    "fs-200": "12px",
    "fs-300": "12px",
    "fs-350": "12px",
    "fs-400": "15px",
    "fs-450": "15px",
    "fs-500": "15px",
    "fs-550": "15px",
    "fs-600": "15px",
    "fs-700": "17px",
    "fs-800": "17px",
    "fs-900": "20px",
    "fs-1000": "20px",
    "fs-1100": "27px",
  },
  /**
   * 行高。
   *
   * 中间四档是**原值**：`widgets/_labels.scss` 给 `.body` / `.caption` /
   * `.document` 写的 `line-height: 140%`，也就是 1.4。上游只发布了这一个数，
   * 标题类一条都没有，所以四档同值——轴在，取值重合。
   *
   * `lh-none`（1）与 `lh-tight`（1.1）是**折算**：上游没有这两档，沿用本项目取值，
   * 不假称有官方出处。`lh-none` 服务的是自己撑 padding 的控件（按钮、分段控件），
   * 那类控件在 GTK 里根本不用行高定高。
   *
   * 后两档是**折算**：徽章与提示框靠行高定盒高，本项目需要固定像素。
   * 取 `.caption` 的 12px × 140% = 16.8 → 17px，两档同值。
   */
  lineHeight: {
    "lh-none": "1",
    "lh-tight": "1.1",
    "lh-snug": "1.4",
    "lh-body": "1.4",
    "lh-normal": "1.4",
    "lh-loose": "1.4",
    "lh-200": "17px",
    "lh-300": "17px",
  },
  /**
   * 控件尺寸。前四档是原值，取自 libadwaita 各控件的 `min-height`。
   *
   * - `row` 34px ← `widgets/_entries.scss` 的 `entry { min-height: 34px }`。
   *   这是 Adwaita 的标准控件高度（按钮 `min-height: 24px` 加上 5px 的上下
   *   padding 与 2×1px 边框，渲染出来也是 34px）。本项目的 `--row` 是表格行与
   *   列表行的盒高，取标准控件高度。
   *   **没有取 `widgets/_lists.scss` 的 `.rich-list > row`（32px + 8px 上下
   *   padding = 48px）或 `AdwActionRow`（50px）**：那两个是「标题 + 副标题」的
   *   两行式行，本项目的表格是密排单行表，不是那个东西。
   * - `row-toolbar` 47px ← `widgets/_header-bar.scss` 的
   *   `headerbar { min-height: 47px }`。
   * - `row-sm` 32px ← `widgets/_menus.scss` 的 `modelbutton { min-height: 32px }`，
   *   Adwaita 里最密的一类可点行，对应本项目工具条里的紧凑控件。
   * - `row-xs` 24px ← `widgets/_buttons.scss` 的 `button { min-height: 24px }`
   *   （不含 padding 的内容高度），Adwaita 里最矮的一档。
   * - `icon` 16px ← `_common.scss` 的 `.normal-icons { -gtk-icon-size: 16px }`；
   *   HIG 的 UI 图标页也明文「符号图标按 16×16px 画」。
   * - `icon-lg` 32px ← `_common.scss` 的 `.large-icons { -gtk-icon-size: 32px }`。
   *   HIG 只认 16 / 32 / 64 / 128 四档，**没有 24px**，所以不取本项目的 22px 近邻。
   * - `rail` / `rail-narrow` 是**折算**：libadwaita 确实发布了侧栏宽度
   *   （`adw-navigation-split-view.c` 的 `min_sidebar_width = 180`、
   *   `max_sidebar_width = 280`、`sidebar_width_fraction = 0.25`，单位是 sp），
   *   但 180 装不下本项目的「图标 + 中文标签 + 底部两个按钮」——184px 时就已经挤了。
   *   216px 落在上游发布的 180–280 区间内，但**不是**它的默认值；取本项目值的
   *   理由同 Fluent：侧栏宽度是本产品的信息密度决定的，不是设计语言的主张。
   */
  size: {
    row: "34px",
    "row-toolbar": "47px",
    "row-sm": "32px",
    "row-xs": "24px",
    rail: "216px",
    "rail-narrow": "44px",
    icon: "16px",
    "icon-lg": "32px",
  },
  /**
   * 间距。**十档全部是折算。**
   *
   * libadwaita 没有间距 token，只有散在各控件里的字面量；现行的 GNOME HIG
   * 也**没有间距页**（指南目录里根本没有这一项，站内搜 spacing 零命中）。
   * 能引的规范只有归档的 GNOME 3 Wiki 布局页，原文：
   *
   * > leave space between user interface components in increments of 6 pixels
   * > …… Between labels and associated components, leave 12 horizontal pixels.
   * > …… For vertical spacing between groups of components, 18 pixels is adequate.
   * > …… Leave a 12-pixel border between the edge of the window and the nearest controls.
   *
   * 样式表里的实际用量与它一致：`border-spacing` 出现 6px 三十次、12px 八次、
   * 24px 七次、18px 四次、36px 两次。所以本项目的十档按**最近的 6 的倍数**归并，
   * 最小那档取 libadwaita 自己在用的 3px（样式表里出现 59 次）。
   *
   * 归并的结果是 2/4/6/8 四档并成 3/6/6/6，10/12 并成 12/12——GNOME 的界面
   * 比本项目原本的排布松，这正是换到这套语言该有的变化。
   */
  space: {
    "sp-1": "3px",
    "sp-2": "6px",
    "sp-3": "6px",
    "sp-4": "6px",
    "sp-5": "12px",
    "sp-6": "12px",
    "sp-7": "18px",
    "sp-8": "24px",
    "sp-9": "24px",
    "sp-10": "36px",
  },
  /**
   * 描边。
   *
   * 第一档是**原值**：libadwaita 通篇 1px——`widgets/_lists.scss` 的
   * `border-bottom: 1px solid $border_color`、`widgets/_window.scss` 的
   * `outline: 1px solid $window_outline_color`、`.card` 的 1px 扩散阴影。
   *
   * 后三档是**折算**：Adwaita 没有更粗的描边这条轴（它的强调靠填充色，不靠加粗线），
   * 沿用本项目取值。本项目用它们画选中行的 3px 边条与侧栏顶部的 4px 色带。
   */
  stroke: {
    stroke: "1px",
    "stroke-thick": "2px",
    "stroke-thicker": "3px",
    "stroke-thickest": "4px",
  },
  /**
   * 焦点环的几何，两条都是**原值**：`src/stylesheet/_drawing.scss` 的
   * `focus-ring` mixin，`$width: 2px`、`$offset: -$width`（即 -2px，画在控件内侧）。
   *
   * 与 Fluent 巧合地同值，与 StrixMaid 正好相反（那套外推 2px 留空）。
   * 三种做法靠这一条轴分叉，不需要作用域覆盖。
   */
  focus: {
    "focus-width": "2px",
    "focus-offset": "-2px",
  },
  /**
   * 动效。**时长与曲线都是原值，落到哪一档是折算。**
   *
   * libadwaita 的整个动效词汇表就在 `_common.scss` 头七行：
   * `$ease-out-quad: cubic-bezier(0.25, 0.46, 0.45, 0.94)`、
   * `$backdrop_transition: 200ms ease-out`、
   * `$focus_transition` / `$button_transition` 均为 `200ms $ease-out-quad`。
   * 把样式表里每一处 `transition:` 都数一遍，200ms 是绝对多数，
   * 其次是 `150ms ease-in-out`（`widgets/_tab-view.scss` 的淡入）与
   * bottom sheet 的 `100ms` / `250ms`。
   *
   * | 档 | 取值 | 出处 |
   * |---|---|---|
   * | t-pop | 200ms | 上游没有「浮层出现」这条，取它的通用时长 |
   * | t-color | 200ms | `$button_transition`，正是颜色/边框的过渡 |
   * | t-arrive | 150ms | `widgets/_tab-view.scss` 的 `opacity 150ms ease-in-out` |
   * | t-enter | 200ms | 通用时长 |
   * | t-shape | 250ms | `widgets/_bottom-sheet.scss` 的 `box-shadow 250ms` |
   * | t-shape-out | 200ms | 通用时长，比展开短 |
   *
   * GNOME HIG **没有动效页**，所以没有规范可引，只能引样式表。
   * 缓动四条里三条是 `$ease-out-quad`——上游只有这一条自定义曲线，
   * `ease-in-out` 那条是它在 tab-view 里字面写的 CSS 关键字。
   * **没有引入关键帧或形变动画**，与 StrixMaid 的取舍一致。
   */
  motion: {
    "t-pop": "200ms",
    "t-color": "200ms",
    "t-arrive": "150ms",
    "t-enter": "200ms",
    "t-shape": "250ms",
    "t-shape-out": "200ms",
    ease: "cubic-bezier(0.25, 0.46, 0.45, 0.94)",
    "ease-out": "cubic-bezier(0.25, 0.46, 0.45, 0.94)",
    "ease-in-out": "ease-in-out",
    "ease-shape": "cubic-bezier(0.25, 0.46, 0.45, 0.94)",
  },
  // `as const` 的理由见 strixmaid.ts
} as const satisfies Design;
