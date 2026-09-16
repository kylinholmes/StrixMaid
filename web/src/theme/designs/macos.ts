import type { Design } from "../design";

/**
 * macOS：Apple 的设计语言。
 *
 * ## 先说清楚 Apple 发布什么、不发布什么，否则下面的出处看着薄
 *
 * 这四套桌面语言里，macOS 是**可引的数值最少**的一套。Apple 在 HIG 的
 * 「Color」页上把话说死了：
 *
 * > Avoid hard-coding system color values in your app. Documented color values are
 * > for your reference during the app design process. The actual color values may
 * > fluctuate from release to release……
 *
 * 于是：
 *
 * - **发布了数值的**：系统色板（12 个具名色的 R/G/B）、系统灰阶（Gray 1–6，
 *   浅深两列）、macOS 的整张字阶表（字号 + 行高 + 字距）、Core Animation 的
 *   五条具名缓动曲线的控制点与隐式时长、SwiftUI 的几个默认时长、
 *   无障碍页的控件目标尺寸与控件间距、菜单栏高度、`NSBezierPath` 的默认线宽、
 *   `NSStackView` 的默认间距、`NSTableView` 的默认行高。
 * - **只发布名字、不发布数值的**：全部 35 个 macOS 语义色
 *   （`windowBackgroundColor`、`labelColor`、`separatorColor`……）。
 * - **一个字都没有的**：圆角、阴影、模态蒙层的不透明度、焦点环的宽度、
 *   macOS 11 之后的工具条高度、图标尺寸、成体系的布局边距。
 *
 * 所以本文件的每条注释标了三类：
 *
 * - **原值**：Apple 自己发布的数值，给出页面与标识符。
 * - **折算**：Apple 没有这条轴，按它发布的别的常量或规范推出来的，写清推法。
 * - **第三方**：Apple 一个数都没有、但存在一份被广泛使用的重实现，引它并
 *   **明说它不是 Apple 的**。只在圆角与蒙层两处用到，因为那两处连折算的依据都没有。
 *
 * 命名上这套叫「macOS」而不是 Aqua 或 Liquid Glass：前者指向早已退役的观感，
 * 后者把它钉死在 26 及以后的某一代。这套语言要跟着这个平台走，不跟着某一代走。
 *
 * ## 中性色是怎么搭出来的
 *
 * Apple 只发布**两列**灰：浅色外观那一列（Gray 6 #F2F2F7 最浅 → Gray 1 #8E8E93
 * 最深）与深色外观那一列（Gray 6 #1C1C1E 最深 → Gray 1 #636366 最浅）。
 * 一套 token 里既要有面板色又要有文字色，同一列凑不齐——浅色那列最深的
 * #8E8E93 拿来当正文只有 3.26:1。
 *
 * 所以：**亮色主题的面板取浅色那一列、文字取深色那一列；暗色主题反过来。**
 * 两列都是 Apple 发布的原值，折算的只是哪一列落在哪个角色上。
 *
 * 还有一层折算要交代：这张灰阶表与系统色板，Apple 挂在 iOS / iPadOS /
 * visionOS 名下；macOS 那一节只有语义色的名字。取它们是因为**这是 Apple
 * 唯一发布过数值的中性色阶与主题色**，不是因为它是 macOS 专用的。
 *
 * ## 对比度实算（WCAG 相对亮度，下同）
 *
 * | | 亮 | 暗 |
 * |---|---|---|
 * | 正文 `--ink` 对面板 | 17.01:1 | 12.49:1 |
 * | `--accent` 对面板 | 3.52:1 | 4.31:1 |
 * | `--ink-2` 对面板 | 9.12:1 | 8.28:1 |
 * | `--ink-3` 对面板 | 5.99:1 | 6.30:1 |
 * | `--line-strong` 对面板 | 3.26:1 | 4.27:1 |
 *
 * 这一套是四套桌面语言里唯一**每条都达标**的：正文过 4.5:1，accent 与控件
 * 边界都过 WCAG 1.4.11 的 3:1。不是刻意挑的，是 Apple 的灰阶本来就是按
 * 对比度排的（它的无障碍页同时发布了 4.5:1 / 3:1 两条门槛）。
 */
export const MACOS = {
  id: "macos",
  name: "macOS",
  color: {
    light: {
      // 原值：HIG「Color」→ 系统灰阶表 Gray (4) Light = R-209, G-209, B-214。
      // 折算的是角色：macOS 的 `windowBackgroundColor` 只有名字没有数值，
      // 这里按「窗口底比内容底深」这条 Apple 自己的层级关系，取灰阶里的一档。
      ground: "#D1D1D6",
      // 折算：灰阶表的**上端**。Apple 的最浅一档灰是 Gray (6) #F2F2F7，再往上是白；
      // `textBackgroundColor` / `controlBackgroundColor` 这一族只有名字没有数值。
      // 面板取白、偶数行取 Gray (6)，正好对上 macOS 的
      // `alternatingContentBackgroundColors`（白 + 一档极浅灰）这个语义色的构造。
      surface: "#FFFFFF",
      // 原值：Gray (6) Light = R-242, G-242, B-247。角色见上一条。
      "surface-2": "#F2F2F7",
      // 原值：Gray (5) Light = R-229, G-229, B-234。划过态。
      "surface-3": "#E5E5EA",
      // 原值：Gray (3) Light = R-199, G-199, B-204。
      // 折算的是角色：对应 `unemphasizedSelectedContentBackgroundColor`
      //（无焦点时的选中底，macOS 用中性灰而不是主题色）。
      // 本项目的选中行沿用 `--ink` 当文字色，深字压上去 10.10:1。
      sel: "#C7C7CC",
      // 原值：Gray (4) Light。角色对应 `separatorColor`（只有名字没有数值）。
      // 与 `--ground` 同值不是笔误：Apple 只发了六档灰，而本项目的中性色有十档，
      // 撞档不可避免。两者从不相邻——ground 在面板外，line 在面板内。
      line: "#D1D1D6",
      // 原值：Gray (1) Light = R-142, G-142, B-147。
      // 对面板 3.26:1，过 WCAG 1.4.11 的非文字门槛——`--line-strong` 在本项目里
      // 是控件边界、表头下沿与滚动条滑块，正是那条门槛管的东西。
      "line-strong": "#8E8E93",
      // 原值：Gray (6) **Dark** = R-28, G-28, B-30。
      // 亮色主题的文字取深色那一列，理由见模块文档「中性色是怎么搭出来的」。
      // 对应 `labelColor`（只有名字没有数值）。
      ink: "#1C1C1E",
      // 原值：Gray (2) Dark = R-72, G-72, B-76。对应 `secondaryLabelColor`。
      "ink-2": "#48484A",
      // 原值：Gray (1) Dark = R-99, G-99, B-102。对应 `tertiaryLabelColor`。
      // 比 `--line-strong` 深，所以滚动条滑块的划过态（`--ink-3`）比静止态
      //（`--line-strong`）更明显，方向是对的。
      "ink-3": "#636366",
      // 原值：HIG「Color」→ 系统色板 Blue (Light) = R-0, G-136, B-255。
      // **这是 2025-06-09 之后的取值**，不是流传更广的那个 #007AFF；
      // 凭记忆写很容易写成旧值。对面板 3.52:1，过 3:1。
      // 色板本身 Apple 挂在 iOS / iPadOS / visionOS 名下，见模块文档。
      accent: "#0088FF",
      // 折算：**Apple 一条阴影规格都没有发布**（整份 HIG 里 shadow 附近没有任何
      // 数值，`NSShadow.shadowBlurRadius` 的「默认 0」是空 API 的初值不是系统规格）。
      // 三档沿用本项目 StrixMaid 那套双层阴影，不编出处；三档同值的理由也与那套
      // 一样——没有可引的分档依据时，浮层只分「浮起来了」与「没浮」。
      "shadow-raised": "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)",
      "shadow-pop": "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)",
      "shadow-dialog": "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)",
      // **第三方**：Apple 只说「sheet 出现时父窗口会变暗」（HIG「Sheets」的
      // macOS 一节），没有不透明度、没有颜色，实现藏在私有类里。
      // 取 macos_ui（Flutter 的 macOS 控件重实现）`macos_sheet.dart` 的
      // barrier opacity 0.6。**这不是 Apple 的数值。**
      //
      // 顺带记一个坑：整份 HIG 里唯一的不透明度数字是「Materials」页 Liquid Glass
      // 一节的 35%，那是给**你自己垫在透明玻璃后面**的压暗层，不是模态蒙层，
      // 不要拿来当这一条。
      scrim: "rgba(0, 0, 0, 0.6)",
      // 折算：Apple 发布了 `keyboardFocusIndicatorColor` 这个名字，没有数值。
      // 它的语义就是「键盘焦点用当前的主题色」，所以取 `var(--accent)`——
      // 写成变量而不是抄一份色值，因为发行版身份会在运行时把 accent 盖掉。
      focus: "var(--accent)",
      // 本语言的焦点环是单层。轴要填满，所以填透明。
      "focus-inner": "transparent",
    },
    dark: {
      // 原值：Gray (6) Dark = R-28, G-28, B-30
      ground: "#1C1C1E",
      // 原值：Gray (5) Dark = R-44, G-44, B-46
      surface: "#2C2C2E",
      // 原值：Gray (4) Dark = R-54, G-54, B-56
      "surface-2": "#363638",
      // 原值：Gray (3) Dark = R-58, G-58, B-60
      "surface-3": "#3A3A3C",
      // 原值：Gray (2) Dark = R-72, G-72, B-76。近白字压上去 8.18:1。
      sel: "#48484A",
      // 原值：Gray (3) Dark。与 `--surface-3` 撞档，理由同亮色那条 `line`。
      line: "#3A3A3C",
      // 原值：Gray (1) **Light** = R-142, G-142, B-147。
      // 暗色这一档取浅色那一列：深色列里最浅的 #636366 对面板只有 2.4:1，
      // 滚动条滑块会看不见。对面板 4.27:1，且比 `--ink-3` 深，划过的方向是对的。
      "line-strong": "#8E8E93",
      // 原值：Gray (6) Light = R-242, G-242, B-247。
      // 暗色主题的文字取浅色那一列。**不是纯白**——这一档同时满足 Apple 的色阶
      // 与 StrixMaid 那条「避开暗底光晕」的钳位（见 strixmaid.ts），不必二选一。
      ink: "#F2F2F7",
      // 原值：Gray (3) Light = R-199, G-199, B-204
      "ink-2": "#C7C7CC",
      // 原值：Gray (2) Light = R-174, G-174, B-178
      "ink-3": "#AEAEB2",
      // 原值：系统色板 Blue (Dark) = R-0, G-145, B-255。对面板 4.31:1。
      accent: "#0091FF",
      // 折算：同亮色，沿用本项目那套双层阴影的暗色档（不透明度整体加深，
      // 暗底上浅阴影看不见）。Apple 没有可引的阴影规格。
      "shadow-raised": "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)",
      "shadow-pop": "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)",
      "shadow-dialog": "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)",
      // 第三方：同亮色。上游没给蒙层写暗色覆盖，两档同值。
      scrim: "rgba(0, 0, 0, 0.6)",
      // 折算：同亮色，见那边的注释。
      focus: "var(--accent)",
      "focus-inner": "transparent",
    },
  },
  /**
   * 圆角。**四档全部是第三方或折算——Apple 一个 macOS 圆角数值都没有发布。**
   *
   * 整份 HIG 里只有三处圆角数字，分别属于 visionOS、iOS 与 watchOS；macOS 一处没有。
   * 唯一能在 macOS 上够到的 Apple 数值是 `PKPaymentButton.cornerRadius`
   *（默认 4.0），但那是一个**品牌按钮**的属性，不是系统控件的规格，不采用。
   *
   * 所以取 macos_ui（Flutter 的 macOS 控件重实现，`github.com/macosui/macos_ui`）
   * 的常量。**这些不是 Apple 的数值**，而且它们早于 macOS 26 那一代观感：
   *
   * - sm 2px ← `buttons/push_button.dart` 的 `_kSmallButtonRadius`（第三方）
   * - md 5px ← 同文件 `_kRegularButtonRadius`；`buttons/popup_button.dart` 的
   *   `_kSideRadius` 同为 5.0（第三方）
   * - lg 12px ← `sheets/macos_sheet.dart` 的 `_kSheetBorderRadius` 与
   *   `dialogs/macos_alert_dialog.dart` 的 `_kDialogBorderRadius`（第三方）
   * - pill 9999px ← **折算**。本项目用这一档的只有滚动条滑块与一个浮起来的
   *   小圆钮，macOS 的滚动条滑块是胶囊形；上游没有数值，填 9999px 只是把形状
   *   说成「胶囊」，不是在引用某个数。
   */
  radius: {
    "radius-sm": "2px",
    "radius-md": "5px",
    "radius-lg": "12px",
    "radius-pill": "9999px",
  },
  /**
   * 字族。两条都取**Apple 自己站点上跑着的那份 CSS**，这是能引到的最完整的栈：
   *
   * - `ui` ← developer.apple.com 的 `--typography-html-font`：
   *   `"SF Pro Text", system-ui, -apple-system, BlinkMacSystemFont,
   *   "Helvetica Neue", "Helvetica", "Arial", sans-serif`
   * - `mono` ← 同一份 CSS 的 `code { font-family: SF Mono, SFMono-Regular,
   *   ui-monospace, Menlo, monospace }`
   *
   * 两条**逐字照抄，只多插了一段 `var(--cjk)`**（Apple 没说过中文该回退到哪里）。
   *
   * WebKit 2015 年那篇《Using the System Font in Web Content》只推荐了
   * `-apple-system` 这一个值，没有给栈；没有后续文章。所以引的是 Apple 自家
   * 站点的实际取值，不是那篇文章。
   *
   * **SF Mono 只能当本地字体引，不能随包发。** 它不随 macOS 安装
   *（Apple 的「macOS 内置字体」支持文档里没有它），而 Apple 字体的许可写着
   * 只可用于「为运行在 Apple 系统上的软件制作界面原型」。栈里写它的名字是让
   * 装了 Xcode 的机器用上，不是要内嵌。
   */
  family: {
    cjk: '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"',
    ui: `"SF Pro Text", system-ui, -apple-system, BlinkMacSystemFont, "Helvetica Neue", "Helvetica", "Arial", var(--cjk), sans-serif`,
    mono: `"SF Mono", SFMono-Regular, ui-monospace, Menlo, var(--cjk), monospace`,
  },
  /**
   * 字阶。**档位与数值全是原值**，这是四套桌面语言里唯一一条能逐档照抄的轴。
   *
   * 出处：HIG「Typography」→ Specifications → macOS built-in text styles。
   * macOS 没有 Dynamic Type，所以这张表是固定的一份：
   *
   * | 样式 | 字号 pt | 行高 pt |
   * |---|---|---|
   * | Large Title | 26 | 32 |
   * | Title 1 | 22 | 26 |
   * | Title 2 | 17 | 22 |
   * | Title 3 | 15 | 20 |
   * | Headline / Body | 13 | 16 |
   * | Callout | 12 | 15 |
   * | Subheadline | 11 | 14 |
   * | Footnote / Caption 1 / Caption 2 | 10 | 13 |
   *
   * macOS 的缩放因子是 @1x 与 @2x（HIG「Images」），所以 1pt = 1px，**不用换算**。
   *
   * 本项目的 15 档几乎与这张表一一对上——Apple 的字阶正好落在本项目的密度上，
   * 只有 8.5 / 9px 两档无处可去（Apple 的无障碍页把 macOS 的最小字号定在 10pt），
   * 向上取到 10px。
   *
   * 字距（tracking）Apple 也按字号发布了（10pt +0.12pt、13pt -0.08pt、
   * 17pt -0.43pt …），但本项目没有字距这条轴，没有立——轴要覆盖需求，
   * 不是覆盖别人的 token 表。
   */
  fontSize: {
    "fs-50": "10px",
    "fs-100": "10px",
    "fs-200": "10px",
    "fs-300": "11px",
    "fs-350": "11px",
    "fs-400": "12px",
    "fs-450": "12px",
    "fs-500": "13px",
    "fs-550": "13px",
    "fs-600": "13px",
    "fs-700": "15px",
    "fs-800": "17px",
    "fs-900": "17px",
    "fs-1000": "22px",
    "fs-1100": "26px",
  },
  /**
   * 行高。后两档是原值，前六档是原值换算出来的比值。
   *
   * Apple 的字阶表把行高与字号**成对**发布，所以本项目的无单位倍数可以直接从
   * 那些对里除出来：
   *
   * - Title 1 26/22 = **1.18**（最紧的一对）
   * - Body 16/13 = **1.23**
   * - Callout 15/12 = 1.25，Subheadline 14/11 = 1.27
   * - Caption 1 / Footnote 13/10 = **1.30**（最松的一对）
   *
   * `lh-normal` 取 1.23 不是随手挑的：`base.css` 的 `body` 用
   * `font-size: var(--fs-500)` 配 `line-height: var(--lh-normal)`，而 `fs-500`
   * 在这套语言下正是 Body 的 13px，所以行高必须取 Body 那一档的 16/13。
   * 三档同为 1.23 是这条约束的结果——轴在，取值重合。
   *
   * `lh-none`（1）是**折算**：Apple 没有这一档，它服务的是自己撑 padding 的控件。
   *
   * 后两档是**原值**且配对精确：`lh-200` 与 `fs-200`（10px = Caption 1）配对，
   * 取 Caption 1 的行高 13pt；`lh-300` 与 `fs-300`（11px = Subheadline）配对，
   * 取 Subheadline 的行高 14pt。
   *
   * 整套行高比其余几套语言紧（1.18–1.30 对 1.4–1.65），这正是 macOS 的排版——
   * 它的行距一向比 GNOME、KDE 收得紧。
   */
  lineHeight: {
    "lh-none": "1",
    "lh-tight": "1.18",
    "lh-snug": "1.23",
    "lh-body": "1.23",
    "lh-normal": "1.23",
    "lh-loose": "1.3",
    "lh-200": "13px",
    "lh-300": "14px",
  },
  /**
   * 控件尺寸。
   *
   * Apple **没有发布**按钮高度、列表行高、工具条高度或图标尺寸。它发布的与
   * 尺寸有关的只有三处，都在 HIG 里：
   *
   * - `row` 28px ← HIG「Accessibility」→ Mobility 的平台表，macOS 一行的
   *   **默认控件尺寸 28×28 pt**（原值）。**注意这是「可点目标」的推荐尺寸，
   *   不是按钮的绘制高度**——Apple 自己就是这么措辞的。用在 `--row` 上恰好合适：
   *   本项目的 `--row` 是表格行与导航项的**点击目标**高度，不是按钮的边框盒。
   *
   *   另一个候选是 `NSTableView.rowHeight`，文档写着「The default row height is
   *   16.0.（仅当 `rowSizeStyle` 为 `custom` 时生效——而那正是默认值）」。
   *   **没有取它**：16pt 是行的**绘制**高度，不含 `intercellSpacing`，
   *   而且装不下 13pt 的 Body 正文再加任何内边距；Apple 自己的无障碍页要求
   *   28pt 的点击目标，两条一起看，28 才是这一档该有的值。
   * - `row-xs` 20px ← 同一张表的**最小控件尺寸 20×20 pt**（原值）。
   * - `row-sm` 24px ← HIG「The Menu Bar」：「The menu bar's height is 24 pt.」
   *   （原值）。**角色是折算**：那是屏幕顶部那条系统菜单栏的高度，Apple 没说它
   *   是「紧凑控件的高度」；取它是因为这是 macOS 发布过的最密的一条可交互条带，
   *   而本项目的 `row-sm` 服务的正是工具条里的紧凑控件。
   * - `row-toolbar` 40px ← **折算**。上游没有工具条高度。沿用本项目取值；
   *   它与「28pt 的控件 + 上下各 6pt」吻合（12pt 是 HIG 无障碍页给的带边框控件
   *   之间的间距，这里上下分摊），所以换到这套语言时工具条高度不变。
   * - `icon` 16px / `icon-lg` 22px ← **折算**。Apple 没有发布过 macOS 的图标点数。
   *   沿用本项目取值，不编出处。
   * - `rail` / `rail-narrow` ← **折算**。上游没有侧栏宽度这条轴，沿用本项目取值，
   *   理由同 Fluent：侧栏宽度是本产品的信息密度决定的，不是设计语言的主张。
   */
  size: {
    row: "28px",
    "row-toolbar": "40px",
    "row-sm": "24px",
    "row-xs": "20px",
    rail: "216px",
    "rail-narrow": "44px",
    icon: "16px",
    "icon-lg": "22px",
  },
  /**
   * 间距。**三档原值，七档折算。**
   *
   * Apple 没有发布成体系的间距阶，HIG 也没有 macOS 的布局边距页
   *（那一页只有两条定性建议：别把控件放在窗口底边、别放在摄像头凹口后面）。
   * 能引的只有三个孤立的数：
   *
   * - `sp-4` 8px ← `NSStackView.spacing`：「The default value for the spacing
   *   property is 8.0 points.」这是 AppKit 里「相邻控件之间该留多少」的默认答案。
   * - `sp-6` 12px 与 `sp-9` 24px ← HIG「Accessibility」→ Mobility：
   *   「In general, it works well to add about 12 points of padding around
   *   elements that include a bezel. For elements without a bezel, about 24
   *   points of padding works well around the element's visible edges.」
   *
   * 三个数正好落在本项目原本那条阶梯的第 4、6、9 档上，一个都不用挪。
   * 其余七档**折算**：沿用本项目取值，**不编出处**。这一条与 Fluent 的处理一致：
   * 上游没有的轴，老老实实说没有，不去别处凑一个像样的数字回来。
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
  /**
   * 描边。
   *
   * 第一档是**原值**：`NSBezierPath.defaultLineWidth` —— 「The default line
   * width, measured in points in the user coordinate space, or 1.0 if no other
   * value has been set.」这是 AppKit 绘图的默认笔宽，Apple 没有单独发布过
   * 「控件边框该多粗」，但这是能引到的最近的一条。
   *
   * 后三档是**折算**：macOS 没有更粗的描边这条轴，沿用本项目取值。
   * 本项目用它们画选中行左缘的 3px 边条与侧栏顶部的 4px 色带。
   */
  stroke: {
    stroke: "1px",
    "stroke-thick": "2px",
    "stroke-thicker": "3px",
    "stroke-thickest": "4px",
  },
  /**
   * 焦点环的几何。
   *
   * 宽度 3px 是**第三方**：Apple 一处都没有发布焦点环的宽度（真实取值在私有
   * SPI 里）。WebKit 的 `Source/WebCore/rendering/RenderTheme.h` 里
   * `RenderTheme::platformFocusRingWidth()` 返回 3，上面那行注释写着
   * 「on macOS this matches AppKit」——**这是 WebKit 工程师的断言，不是 Apple 文档**。
   *
   * 偏移 0 是**折算**：Apple 只确立了方向。技术问答 QA1785 讲图层背衬时说
   * 「焦点环无法延伸到该图层之外……结果是一圈又细又不对的环」，可见环本来是
   * 向**外**长的；但没有任何数值。取 0 让 3px 的环紧贴控件外缘。
   *
   * 环的颜色在 color 那一节（`keyboardFocusIndicatorColor` 只有名字没有数值，
   * 取 `var(--accent)`）。
   */
  focus: {
    "focus-width": "3px",
    "focus-offset": "0px",
  },
  /**
   * 动效。**六档时长与四条曲线全部是原值**——这一条 Apple 发布得比谁都全。
   *
   * 时长：
   *
   * | 档 | 取值 | 出处 |
   * |---|---|---|
   * | t-pop | 250ms | Core Animation 的隐式时长：「…… or .25 seconds if no transaction duration is specified」 |
   * | t-color | 250ms | 同上 |
   * | t-arrive | 250ms | 同上 |
   * | t-enter | 350ms | SwiftUI `Animation.easeInOut`：「has a default duration of 0.35 seconds」（easeIn / easeOut / linear 同值） |
   * | t-shape | 500ms | SwiftUI `Animation.smooth(duration:extraBounce:)` 的声明式默认 `duration: TimeInterval = 0.5` |
   * | t-shape-out | 350ms | SwiftUI 的 easeInOut 默认时长，比展开短 |
   *
   * **`t-color` 用 250ms 是有意的**：Apple 的隐式动画时长就是这一个数，
   * 它没有发布过更短的一档。换到这套语言时划过态的过渡会比 StrixMaid（120ms）
   * 明显慢，那是这套语言的节奏，不是漏配。
   *
   * 缓动四条都取 `CAMediaTimingFunctionName` 各档文档里写着的贝塞尔控制点：
   *
   * - `.default` (0.25, 0.1) 与 (0.25, 1.0) —— 文档原话「使用这条以确保你的动画
   *   与大多数系统动画的节奏一致」
   * - `.easeOut` (0.0, 0.0) 与 (0.58, 1.0)
   * - `.easeInEaseOut` (0.42, 0.0) 与 (0.58, 1.0)
   * - 形变那一条也取 `.easeInEaseOut`：SwiftUI 现在的默认是弹簧
   *  （response 0.55 / dampingFraction 1.0），CSS 表达不了弹簧，取它在
   *   iOS 17 / macOS 14 之前的默认 easeInOut。
   *
   * **没有引入关键帧或形变动画**，与 StrixMaid 的取舍一致。
   */
  motion: {
    "t-pop": "250ms",
    "t-color": "250ms",
    "t-arrive": "250ms",
    "t-enter": "350ms",
    "t-shape": "500ms",
    "t-shape-out": "350ms",
    ease: "cubic-bezier(0.25, 0.1, 0.25, 1)",
    "ease-out": "cubic-bezier(0, 0, 0.58, 1)",
    "ease-in-out": "cubic-bezier(0.42, 0, 0.58, 1)",
    "ease-shape": "cubic-bezier(0.42, 0, 0.58, 1)",
  },
  // `as const` 的理由见 strixmaid.ts
} as const satisfies Design;
