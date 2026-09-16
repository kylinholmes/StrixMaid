import type { Design } from "../design";

/**
 * Yaru：Ubuntu 的设计语言。
 *
 * ## 取值的优先级（先读这一段，否则下面的出处看着是从两个地方东拼西凑）
 *
 * Yaru 是一套 GTK / GNOME Shell 主题，**它是 Adwaita 的派生**：仓库里没有
 * libadwaita 样式表，`gtk/src/default/gtk-4.0/` 是旧版 GTK4 Default/Adwaita 的
 * 一个分叉。所以「Yaru 的取值」分三类，本文件逐条标明属于哪一类：
 *
 * 1. **Yaru 自己改过的**——颜色、圆角。这些是这套语言真正的主张，标「原值」。
 * 2. **Yaru 树里有、但从 Adwaita 一字未改继承下来的**——控件最小高度、字阶、
 *    焦点环几何。标「继承」：是 Yaru 的实际渲染结果，但不是 Yaru 的主张。
 * 3. **Yaru 完全没有的**——字族（仓库里一处 `font-family` 都没有，桌面字体由
 *    GNOME 设置决定）、间距阶、动效阶、阴影、描边阶。这些取 Canonical 自己发布的
 *    **Vanilla Framework**（`canonical/vanilla-framework`）。
 *
 * **第 3 类必须说清楚：Vanilla 是 Canonical 的网页设计系统，Yaru 是桌面主题，
 * 两者只共享品牌橙与 Ubuntu 字体，不是同一个子系统。** 取它是因为
 * (a) 这几条轴 Yaru 那边确实没有对应物，(b) StrixMaid 的界面本身就是网页，
 * (c) 编一个出处比引一个同品牌的邻近系统更糟。每一条都注明了它来自 Vanilla。
 *
 * ## 亮暗两档里 bg 与 base 的深浅关系是反的
 *
 * 亮色：`$bg_color` #FAFAFA 比 `$base_color` #ffffff **深**；
 * 暗色：`$bg_color` #2c2c2c 比 `$base_color` #272727 **浅**。
 *
 * 本项目的结构不变量是「面板浮在页底之上」（tokens.test.ts 有断言）。
 * 两档各取其中**较深的那个**作 `--ground`：亮色 ground=bg / surface=base，
 * 暗色 ground=base / surface=bg。**色值全是原值，折算的只是角色的分配**。
 * Adwaita 与 Breeze 那两套是同一个情况，处理方式一致。
 *
 * ## 对比度实算（WCAG 相对亮度，下同）
 *
 * | | 亮 | 暗 |
 * |---|---|---|
 * | 正文 `--ink` 对面板 | 10.86:1 | 13.04:1 |
 * | `--accent` 对面板 | 3.65:1 | 3.83:1 |
 * | `--ink-2` 对面板 | 6.58:1 | 8.70:1 |
 * | `--ink-3` 对面板 | 3.59:1 | 4.49:1 |
 * | `--line-strong` 对面板 | 1.88:1 | 2.12:1 |
 *
 * `--accent` 两档都在 3:1 到 4.5:1 之间，过得了 tokens.test.ts 那条判据
 * （WCAG 1.4.11 的非文字对比度 3:1，`--accent` 在本项目里一次都没用在文字上）。
 * Ubuntu 橙本来就是这个量级——Yaru 自己另发了一档「当文字用」的
 * `$yaru_accent_color`（亮 #d33e05 / 暗 #fe6823），本项目用不上。
 */
export const YARU = {
  id: "yaru",
  name: "Yaru",
  color: {
    light: {
      // 原值：`gtk/src/default/gtk-4.0/_colors.scss` → `$light_bg_color: #FAFAFA`
      ground: "#FAFAFA",
      // 原值：同文件 `$base_color: #ffffff`
      surface: "#FFFFFF",
      // 原值：同文件 `$menu_selected_color`（亮色解析为 #ebebeb）。
      // 折算的是角色：Yaru 没有「斑马纹偶数行」这个概念，本项目的四级底按明度
      // 顺序去认领 Yaru 自己发布的那几档中性面板色。
      "surface-2": "#EBEBEB",
      // 原值：同文件 `$dark_fill`（亮色解析为 #e3e3e3）
      "surface-3": "#E3E3E3",
      // 原值：`gtk/src/default/gtk-4.0/_common.scss` 的菜单/弹层划过色
      // `darken($menu_selected_color, 5%)`（亮色解析为 #dedede）。
      // **没有取 Yaru 真正的选中底 `$selected_bg_color` = #E95420**：本项目的
      // 选中行沿用 `--ink` 当文字色，深字压在 Ubuntu 橙上只有 3.0:1，读不动；
      // 而 Yaru 自己是配白字的，那需要第二条「选中行文字色」的轴，本项目没有。
      // 选中的身份表达改由左缘那道 3px `--accent` 边条承担（spec §5.3），
      // 橙色仍然在，只是不铺满整行。
      sel: "#DEDEDE",
      // 原值：`_colors.scss` → `$borders_color`（亮色解析为 #cccccc）
      line: "#CCCCCC",
      // 原值：`_colors.scss` → `$alt_borders_color`（亮色解析为 #bdbdbd）。
      // 这一条在 Yaru 里的用途正是「更强的那档边框」，与本项目 `--line-strong`
      // 的角色一一对应。对面板只有 1.88:1，低于 WCAG 1.4.11 的 3:1——
      // 这是 Yaru 的分寸（它的按钮靠填充分层，不靠描边），测试里没有这条断言。
      "line-strong": "#BDBDBD",
      // 原值：`_colors.scss` → `$fg_color: $inkstone` = #3D3D3D
      // （`_palette.scss` 的 `$inkstone: #3D3D3D`）
      ink: "#3D3D3D",
      // 原值：`_palette.scss` → `$slate: #5D5D5D`。
      // 折算的是角色：Yaru 的语义色里没有「二级正文」这一档
      //（`$backdrop_fg_color` #636363 是失焦态，不是层级），
      // 取调色板里正好落在 `$inkstone` 与 `$ash` 之间的那档灰。
      "ink-2": "#5D5D5D",
      // 原值：`_palette.scss` → `$ash: #878787`。
      // **没有取 `$insensitive_fg_color`（#9c9c9c）**：那一档比 `--line-strong`
      // 还浅，而滚动条滑块的静止态用 `--line-strong`、划过态用 `--ink-3`
      //（base.css），浅过去等于划过时滑块变淡。`$ash` 比 `$alt_borders_color` 深，
      // 划过的方向才是对的。
      "ink-3": "#878787",
      // 原值：`gtk/src/default/gtk-4.0/_palette.scss` → `$orange: #E95420`，
      // 同文件 `$accent_bg_color: $orange`；`common/accent-colors.scss.in` 的
      // `get_accent_color()` 里 `default` 档也是 `#E95420`。
      // design.ubuntu.com 的品牌色板同样写着 Ubuntu orange `#E95420`（Pantone 1665）。
      // 仓库里**没有** `$ubuntu_orange` 这个标识符，凭记忆写容易写错名字。
      accent: "#E95420",
      // 原值：Vanilla `scss/_settings_placeholders.scss` 的 `$box-shadow`。
      // 见模块文档第 3 类：Yaru 没有阴影这条轴（只有一个
      // `$shadow_color: rgba(0,0,0,0.1)`，不带几何）。
      "shadow-raised":
        "0 1px 1px 0 rgba(0, 0, 0, 0.15), 0 2px 2px -1px rgba(0, 0, 0, 0.15), 0 0 3px 0 rgba(0, 0, 0, 0.2)",
      // 原值：Vanilla 同文件的 `$box-shadow--deep: 0 0 2rem 0 rgba($color-x-dark, 0.2)`，
      // 按 Vanilla 自己的 16px 根字号折成 32px。
      "shadow-pop": "0 0 32px 0 rgba(0, 0, 0, 0.2)",
      // 折算（档位归并）：Vanilla 只发布两档阴影，对话框那一档没有单独的取值。
      // 取 deep 那一档，不另编数字。
      "shadow-dialog": "0 0 32px 0 rgba(0, 0, 0, 0.2)",
      // 折算：Yaru 与 Vanilla 都没有模态蒙层的取值。沿用本项目取值，不编出处
      //（与 Fluent 处理侧栏宽度是同一条规矩）。
      scrim: "rgba(0, 0, 0, 0.45)",
      // 原值：`gtk/src/default/gtk-4.0/_colors.scss` → `$focus_border_color:
      // rgba(239, 134, 97, 0.7)`，亮暗两档同值。
      // **这一档不跟 `--accent` 走**，与 Fluent 的黑白双环是同一类主张：
      // Yaru 的焦点环是一圈淡了的橙，色值写死在主题里。于是在非 Ubuntu 的机器上
      // 选了这套语言时，焦点是 Ubuntu 橙、accent 是那台机器的身份色——这正是
      // 「设计语言与身份是两件事」该有的样子（见 tokens.ts 的模块文档）。
      focus: "rgba(239, 134, 97, 0.7)",
      // 本语言的焦点环是单层。轴要填满，所以填透明。
      // （Yaru 另有一档 `$alt_focus_border_color`，那是画在彩色底上的替代环，
      // 不是内环；本项目的内环只在 Fluent 那一处作用域覆盖里画。）
      "focus-inner": "transparent",
    },
    dark: {
      // 原值：`_colors.scss` → `$base_color: lighten($jet, 6%)` = #272727。
      // 暗色下 base 比 bg **深**，所以它在这里当页底，见模块文档。
      ground: "#272727",
      // 原值：`_colors.scss` → `$dark_bg_color: lighten($jet, 8%)` = #2c2c2c
      surface: "#2C2C2C",
      // 原值：`_colors.scss` → `$backdrop_bg_color`（暗色解析为 #343434）
      "surface-2": "#343434",
      // 原值：`_colors.scss` → `$menu_selected_color`（暗色解析为 #3c3c3c）
      "surface-3": "#3C3C3C",
      // 原值：`_common.scss` 的菜单/弹层划过色
      // `lighten($menu_selected_color, 5%)`（暗色解析为 #484848）。
      // 不取 `$selected_bg_color`（Ubuntu 橙）的理由同亮色。
      sel: "#484848",
      // 原值：`_colors.scss` → `$borders_color`（暗色解析为 #131313）。
      // 暗色的分隔线比面板**更暗**，这是 Yaru 的做法，不是抄反了。
      line: "#131313",
      // 原值：`_palette.scss` → `$slate: #5D5D5D`。
      // **偏离上游：Yaru 暗色的 `$alt_borders_color` 是纯黑 #000000。**
      // 纯黑在 #2c2c2c 的面板上做滚动条滑块等于看不见，而 `--line-strong`
      // 在本项目里同时兼着滑块与控件边界（base.css / Button / Field）。
      // 取调色板里的 `$slate`——仍然是 Yaru 自己的灰，且划过态 `--ink-3`
      //（#929292）比它浅，滑块的划过反馈才成立。
      "line-strong": "#5D5D5D",
      // 原值：`_colors.scss` → `$fg_color: $porcelain` = #F7F7F7
      //（`_palette.scss` 的 `$porcelain: #F7F7F7`）。不是纯白。
      ink: "#F7F7F7",
      // 原值：`_palette.scss` → `$silk: #CCC`。角色折算同亮色。
      "ink-2": "#CCCCCC",
      // 原值：`_colors.scss` → `$insensitive_fg_color`（暗色解析为 #929292）。
      // 暗色这一档可以直接用：它比 `--line-strong` 浅，滑块的划过方向是对的。
      "ink-3": "#929292",
      // 原值：同亮色。Yaru 的 `$accent_bg_color` 不随模式变。
      accent: "#E95420",
      // 折算：几何与层数是 Vanilla `$box-shadow` 的原值，**不透明度 ×3**——
      // Vanilla 只发布亮色主题的阴影，暗底上不加深就看不见。
      // 倍数取 3 是让第一层落在 0.45、与本项目另外几套语言的暗色阴影同量级。
      "shadow-raised":
        "0 1px 1px 0 rgba(0, 0, 0, 0.45), 0 2px 2px -1px rgba(0, 0, 0, 0.45), 0 0 3px 0 rgba(0, 0, 0, 0.6)",
      // 折算：同上，`$box-shadow--deep` 的 0.2 ×3 = 0.6。
      "shadow-pop": "0 0 32px 0 rgba(0, 0, 0, 0.6)",
      // 折算（档位归并）：同亮色，与 pop 同值。
      "shadow-dialog": "0 0 32px 0 rgba(0, 0, 0, 0.6)",
      // 折算：同亮色，沿用本项目取值。
      scrim: "rgba(0, 0, 0, 0.45)",
      // 原值：同亮色，`$focus_border_color` 亮暗两档同值。
      focus: "rgba(239, 134, 97, 0.7)",
      "focus-inner": "transparent",
    },
  },
  /**
   * 圆角。**这是 Yaru 真正改过 Adwaita 的地方之一**，四档里三档是原值。
   *
   * `gtk/src/default/gtk-4.0/_common.scss`（GTK3 那份同值），源码里每一行都带着
   * Yaru 自己的注释 `// Yaru change: sync radius with Gtk4`：
   *
   * - `$button_radius: 6px`（上游 Adwaita 是 5px）
   * - `$menu_radius: 8px`（上游 5px）
   * - `$window_radius: 15px`（上游 `$button_radius + 3` = 8px）
   * - `$popover_radius: $window_radius` = 15px（上游 `$button_radius + 4` = 9px）
   *
   * - sm ← `$button_radius` 6px（**折算：档位归并**。Yaru 没有比按钮更小的一档，
   *   本项目的徽章、进度槽、状态方块落在同一档上）
   * - md ← `$button_radius` 6px（原值）
   * - lg ← `$window_radius` / `$popover_radius` 15px（原值）
   * - pill ← 9999px（**继承**：`.pill` 这个样式类来自 libadwaita
   *   `widgets/_buttons.scss` 的 `border-radius: 9999px`，Yaru 不改它）
   *
   * `$menu_radius` 8px 没有进表：本项目没有介于按钮与对话框之间的第三种圆角。
   */
  radius: {
    "radius-sm": "6px",
    "radius-md": "6px",
    "radius-lg": "15px",
    "radius-pill": "9999px",
  },
  /**
   * 字族。见模块文档第 3 类：**Yaru 仓库里一处 `font-family` 都没有**，
   * 桌面字体由 GNOME 设置决定，不由主题决定。
   *
   * 所以取 Canonical 自己发布的那一份 Web 字族栈——Vanilla Framework 的
   * `scss/_settings_font.scss`：
   *
   * - `$font-base-family: '"Ubuntu variable", "Ubuntu", -apple-system, "Segoe UI",
   *   "Roboto", "Oxygen", "Cantarell", "Fira Sans", "Droid Sans",
   *   "Helvetica Neue", sans-serif'`
   * - `$font-monospace: '"Ubuntu Mono variable", "Ubuntu Mono", Consolas, Monaco,
   *   Courier, monospace'`
   *
   * 两条照抄，改动只有两处：多插了一段 `var(--cjk)`（位置与其余几套语言一致，
   * Canonical 没说过中文该回退到哪里），以及把 `"Ubuntu"` / `"Ubuntu Mono"`
   * 提到了各自那条栈的最前面。
   *
   * **为什么要提前。** 这两个名字现在指的是随包发的 woff2 子集
   * （`styles/base.css` 的 `@font-face`，出处与许可见
   * `assets/fonts/NOTICE.txt`），而 `"Ubuntu variable"` 只存在于装了 Ubuntu
   * 字体的机器上。原来的顺序意味着「谁的机器上装了什么，就看到什么」：
   * 同一套设计语言在 Ubuntu 机器上是 Canonical 那份变量字体、在 Windows 上
   * 一路落到 Segoe UI。自托管的那份排头才谈得上「到哪台机器上都是这个字形」。
   *
   * `"Ubuntu variable"` 与后面整条本机回落一个都没删：woff2 取不到时
   * （离线、被拦、资源没随包发出去），还得有东西顶上。
   *
   * 注意 vanillaframework.io 的文档页是**旧的**（还写着
   * `Ubuntu, Arial, "libra sans", sans-serif`），与仓库里的 SCSS 不一致；
   * 这里引的是 SCSS。
   */
  family: {
    cjk: '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"',
    ui: `"Ubuntu", "Ubuntu variable", -apple-system, "Segoe UI", "Roboto", "Oxygen", "Cantarell", "Fira Sans", "Droid Sans", "Helvetica Neue", var(--cjk), sans-serif`,
    mono: `"Ubuntu Mono", "Ubuntu Mono variable", Consolas, Monaco, Courier, var(--cjk), monospace`,
  },
  /**
   * 字阶。**继承**（见模块文档第 2 类）+ px 折算。
   *
   * Yaru 的排版类与上游 Adwaita 逐字节相同，一档都没改。GTK3 那份给的是绝对
   * 点数（`gtk/src/default/gtk-3.0/`）：`large-title` 24pt、`title-1` 20pt、
   * `title-2` / `title-3` 15pt、`title-4` 13pt、`heading` / `body` 11pt、
   * `caption-heading` / `caption` 9pt。折成 px 的公式是 96/72 dpi。
   *
   * | 类 | pt（继承） | px（折算） |
   * |---|---|---|
   * | `.caption` | 9 | 12 |
   * | `.body`（基准） | 11 | 15 |
   * | `.title-4` | 13 | 17 |
   * | `.title-3` / `.title-2` | 15 | 20 |
   * | `.title-1` | 20 | 27 |
   *
   * 本项目的 15 档压进这 5 档，边界在 fs-350 与 fs-400 之间：以下是次要文字，
   * 落 `.caption`；以上是正文，落 `.body`。
   *
   * **这张表与 adwaita.ts 那张逐档相同，那是事实而不是复制粘贴**：Yaru 的排版
   * 就是 Adwaita 的排版。两套语言的差别在颜色、圆角、字族、焦点色与动效上。
   *
   * 没有取 Vanilla 的字阶（正文 1rem = 16px、小字 0.875rem = 14px、
   * 标题 1.5rem / 2.625rem）：那是网页版式的尺度，正文 16px 配本项目 32px 的
   * 行高会挤破表格；而 Yaru 树里确实有一份可引的桌面字阶，第 2 类优先于第 3 类。
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
   * 行高。前六档**原值**，后两档**折算**。
   *
   * Vanilla 的版式表把行高与字号成对发布（`scss/_settings_spacing.scss`，
   * 行高以 `$sp-unit` = 0.5rem 的倍数给）：正文 `1rem / 3×0.5rem` = **1.5**，
   * 小字 `0.875rem / 2.5×0.5rem` ≈ **1.43**，超小 `0.75rem / 2×0.5rem` ≈ **1.33**，
   * 标题 `1.5rem / 4×0.5rem` ≈ **1.33**，h1 `2.625rem / 6×0.5rem` ≈ **1.14**。
   * 六档按「越大的字行距越紧」的顺序落在这几个比值上。
   *
   * 后两档是固定像素（徽章与提示框在本项目里靠行高定盒高）：取 Vanilla 小字那档
   * 的行高 `2.5 × 0.5rem` = 20px 与超小那档的 `2 × 0.5rem` = 16px。
   * 这一步是折算——Vanilla 的行高是配它自己的字号发布的，本项目这两档的字号
   * 落在 `.caption` 的 12px 上，不是 Vanilla 的 12/14px。
   */
  lineHeight: {
    "lh-none": "1",
    "lh-tight": "1.14",
    "lh-snug": "1.33",
    "lh-body": "1.43",
    "lh-normal": "1.5",
    "lh-loose": "1.5",
    "lh-200": "16px",
    "lh-300": "20px",
  },
  /**
   * 控件尺寸。前四档是**继承**（见模块文档第 2 类）：Yaru 的 GTK4
   * `_common.scss` 里这几条与上游 Adwaita 逐字相同，Yaru 自己的尺寸改动只落在
   * 窗口按钮与滑块上（`_tweaks.scss`），与本项目的轴无关。
   *
   * - `row` 32px ← `entry { min-height: 32px }`（`_common.scss`）。
   *   本项目的 `--row` 是表格行与列表行的盒高，取标准输入框高度。
   * - `row-toolbar` 46px ← `headerbar { min-height: 46px }`
   * - `row-sm` 26px ← `modelbutton.flat { min-height: 26px }`，最密的一类可点行
   * - `row-xs` 24px ← `button { min-height: 24px }`（不含 padding 的内容高度）
   * - `icon` 16px / `icon-lg` 32px ← GNOME 的符号图标尺寸
   *   （`.normal-icons` 16px / `.large-icons` 32px，Yaru 不改）。
   *   GNOME HIG 的 UI 图标页只认 16 / 32 / 64 / 128，**没有 24px**。
   * - `rail` / `rail-narrow` 是**折算**：Yaru 与 Vanilla 都没有侧栏宽度这条轴。
   *   沿用本项目取值，理由同 Fluent。
   */
  size: {
    row: "32px",
    "row-toolbar": "46px",
    "row-sm": "26px",
    "row-xs": "24px",
    rail: "216px",
    "rail-narrow": "44px",
    icon: "16px",
    "icon-lg": "32px",
  },
  /**
   * 间距。见模块文档第 3 类：Yaru 没有间距阶（它继承的 Adwaita 也没有 token，
   * 只有散在各控件里的字面量），所以取 Vanilla 的那一条。
   *
   * Vanilla `scss/_settings_spacing.scss`：`$sp-unit: 1rem * 0.5` = **0.5rem（8px）**，
   * 文档页原文「Everything in Vanilla adheres to a .5rem baseline grid」。
   * 具名档：`$spv--x-small: 0.25rem`（4px）、`$spv--small: 0.5rem`（8px）、
   * `$spv--medium: 0.75rem`（12px）、`$spv--large: 1rem`（16px）、
   * `$spv--x-large: 1.5rem`（24px）；栅格的槽宽 `$sp-unit * 4` = 2rem（32px）。
   *
   * | 档 | 值 | 出处 |
   * |---|---|---|
   * | sp-1 / sp-2 | 4px | `$spv--x-small`（原值） |
   * | sp-3 / sp-4 | 8px | `$spv--small` = `$sp-unit`（原值） |
   * | sp-5 / sp-6 | 12px | `$spv--medium`（原值） |
   * | sp-7 | 16px | `$spv--large`（原值） |
   * | sp-8 / sp-9 | 24px | `$spv--x-large`（原值） |
   * | sp-10 | 32px | 栅格槽宽 `$sp-unit * 4`（原值） |
   *
   * 十档压进六个值。8 的倍数比本项目原本那条（含 6 与 10 两个半档）齐整，
   * 代价是 6px 的小缝一律变 8px、10px 的中缝一律变 12px——Ubuntu 的界面
   * 比本项目原本的排布松一点，这正是换到这套语言该有的变化。
   */
  space: {
    "sp-1": "4px",
    "sp-2": "4px",
    "sp-3": "8px",
    "sp-4": "8px",
    "sp-5": "12px",
    "sp-6": "12px",
    "sp-7": "16px",
    "sp-8": "24px",
    "sp-9": "24px",
    "sp-10": "32px",
  },
  /**
   * 描边。
   *
   * - `stroke` 1px（**继承**）：Yaru 的 GTK 样式表通篇 1px 边框。
   *   **没有取 Vanilla 的 `$input-border-thickness: 1.5px`**：那是网页表单
   *   描边的厚度，而本项目的 `--stroke` 同时是表格分隔线与表头下沿，
   *   1.5px 在 1x 屏上会糊成一条 2px 的灰带。这是一处有意的偏离，
   *   第 2 类（Yaru 树里的 1px）优先于第 3 类（Vanilla 的 1.5px）。
   * - `stroke-thick` 2px（**折算**）：两边都没有这一档，沿用本项目取值。
   * - `stroke-thicker` 3px（**原值**）：Vanilla
   *   `scss/_settings_placeholders.scss` 的 `$bar-thickness: 0.1875rem`
   *   （源码注释写着「3px at 16px fontsize」）。本项目用它画选中行左缘的边条。
   * - `stroke-thickest` 4px（**折算**）：沿用本项目取值，用在侧栏顶部的色带上。
   */
  stroke: {
    stroke: "1px",
    "stroke-thick": "2px",
    "stroke-thicker": "3px",
    "stroke-thickest": "4px",
  },
  /**
   * 焦点环的几何。两条都是**继承**：Yaru 的 GTK4 分叉沿用 Adwaita 的
   * `focus-ring` 做法（`$focus_transition` 过渡的正是 `outline-width` 与
   * `outline-offset`），Adwaita 的 `_drawing.scss` 里那个 mixin 是
   * `$width: 2px`、`$offset: -$width`——2px 宽、内收 2px。
   *
   * 环的**颜色**才是 Yaru 自己的（`$focus_border_color`，见上面 color 那一节）。
   */
  focus: {
    "focus-width": "2px",
    "focus-offset": "-2px",
  },
  /**
   * 动效。见模块文档第 3 类。
   *
   * Yaru 树里的 `$button_transition: all 200ms $ease-out-quad` 是从 Adwaita
   * 一字未改继承来的，不是 Yaru 的主张；而 Canonical **自己发布过**一套时长与
   * 缓动——Vanilla 的 `scss/_settings_animations.scss`：
   *
   * - `$animation-duration: (snap: 0.1s, fast: 0.165s, brisk: 0.333s,
   *   slow: 0.5s, sleepy: 1s)`
   * - `$animation-easing: (out: cubic-bezier(0.215, 0.61, 0.355, 1),
   *   in: cubic-bezier(0.55, 0.055, 0.675, 0.19))`
   * - `@mixin vf-transition($property: all, $duration: brisk, $easing: out)`
   *   ——默认过渡就是 `brisk` + `out`。
   *
   * | 档 | 取值 | 出处 |
   * |---|---|---|
   * | t-pop | 100ms | `snap`（原值） |
   * | t-color | 165ms | `fast`（原值） |
   * | t-arrive | 165ms | `fast`（原值） |
   * | t-enter | 333ms | `brisk`，Vanilla 的默认过渡时长（原值） |
   * | t-shape | 500ms | `slow`（原值） |
   * | t-shape-out | 333ms | `brisk`，比展开短（原值） |
   *
   * 缓动三条取 `out` 那条原值；`ease-in-out` 上游没有，取 `in` 的入段与 `out`
   * 的出段合成 `cubic-bezier(0.55, 0.055, 0.355, 1)`，**这一条是折算**。
   * `sleepy`（1s）没有进表：本项目没有需要一秒的过渡。
   * **没有引入关键帧或形变动画**，与 StrixMaid 的取舍一致。
   */
  motion: {
    "t-pop": "100ms",
    "t-color": "165ms",
    "t-arrive": "165ms",
    "t-enter": "333ms",
    "t-shape": "500ms",
    "t-shape-out": "333ms",
    ease: "cubic-bezier(0.215, 0.61, 0.355, 1)",
    "ease-out": "cubic-bezier(0.215, 0.61, 0.355, 1)",
    "ease-in-out": "cubic-bezier(0.55, 0.055, 0.355, 1)",
    "ease-shape": "cubic-bezier(0.215, 0.61, 0.355, 1)",
  },
  // `as const` 的理由见 strixmaid.ts
} as const satisfies Design;
