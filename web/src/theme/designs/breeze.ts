import type { Design } from "../design";

/**
 * Breeze：KDE Plasma 的设计语言。
 *
 * 取值来自 KDE 的三处上游：配色方案 `breeze/colors/*.colors`（INI）、
 * Qt 控件样式的度量表 `breeze/kstyle/breezemetrics.h`（`struct Metrics`），
 * 以及 KDE HIG 与它所指向的 Kirigami 单位表。**每一条注释都标了它是「原值」
 * 还是「折算」**：
 *
 * - **原值**：直接抄自上游源码或官方文档，给出文件与标识符。
 * - **折算**：上游没有这条轴，按其发布的常量或 HIG 推出来的，写清依据与推法。
 *
 * 颜色、圆角、焦点几何、动画时长这几条对得出原值；「Web 的字阶」「行高」
 * 「三档阴影」在 Qt 那边没有对应物，只能折算，下面逐条写明推法。
 *
 * ## 三处上游的事实需要先说清楚，否则下面的取值看着像抄错了
 *
 * **一、没有 `Breeze.colors` 这个文件。** `colors/` 下只有
 * `BreezeLight.colors`、`BreezeDark.colors`、`BreezeClassic.colors` 三个。
 * Plasma 的默认档是 **BreezeLight**（`plasma-workspace` 的
 * `lookandfeel/org.kde.breeze/contents/defaults` 写着 `ColorScheme=BreezeLight`），
 * 所以本文件的亮色取 BreezeLight，暗色取 BreezeDark。
 *
 * **二、广为流传的 `#31363B` / `#BDC3C7` 是 BreezeClassic 的值，不是默认档。**
 * 那是仍然随包发的旧方案。凭记忆写 Breeze 很容易写成那一套，这里没有。
 *
 * **三、`.colors` 里没有描边色这一条。** 分隔线与控件边框都是算出来的：
 * `breezehelper.cpp` 的 `Helper::separatorColor()` = `KColorUtils::mix(Window,
 * WindowText, frameIntensityBias())`，而 `frameIntensityBias()` 取
 * `KColorScheme::frameContrast()`，其默认值在 `kcolorscheme.cpp` 里是 **0.2**。
 * 本文件的 `--line` 就是这个式子算出来的。
 *
 * ## 亮暗两档里 Window 与 View 的深浅关系是反的
 *
 * 亮色：`[Colors:Window] BackgroundNormal` #EFF0F1 比
 * `[Colors:View] BackgroundNormal` #FFFFFF **深**；
 * 暗色：Window #202326 比 View #141618 **浅**。
 *
 * 本项目的结构不变量是「面板浮在页底之上」（`--ground` 比 `--surface` 暗，
 * tokens.test.ts 有断言）。所以两档各取 Window / View 里**较深的那个**作
 * `--ground`：亮色 ground=Window / surface=View，暗色 ground=View / surface=Window。
 * **色值全是原值，折算的只是角色的分配**，而且反转这件事是 Breeze 自己做的。
 * Adwaita 那套也是同一个情况，处理方式一致。
 *
 * ## 为什么亮色的 accent 不是 `DecorationFocus`
 *
 * Breeze 的主题色是 `#3DAEE9`（`DecorationFocus` / `Selection` 的底色，
 * 亮暗两档同值）。它对 Breeze 自己的亮色面板 #FFFFFF 只有 **2.18:1**，
 * 过不了 tokens.test.ts 那条「accent 对自己的面板 ≥3:1」——那条判据来自
 * WCAG 1.4.11 的非文字对比度，`--accent` 在本项目里全部用在填充与描边上。
 *
 * **判据不动，调这套语言自己的取值**：亮色改取同一份方案里的
 * `[Colors:View] ForegroundLink` = `#2980B9`（4.30:1）。它仍然是 BreezeLight
 * 发布的原值，仍然是同一族蓝，只是 KDE 为「需要读得清的蓝」准备的那一档。
 * 暗色不存在这个问题，照取 `DecorationFocus` `#3DAEE9`（6.34:1）。
 *
 * ## 对比度实算（WCAG 相对亮度，下同）
 *
 * | | 亮 | 暗 |
 * |---|---|---|
 * | 正文 `--ink` 对面板 | 15.21:1 | 15.39:1 |
 * | `--accent` 对面板 | 4.30:1 | 6.34:1 |
 * | `--ink-2` 对面板 | 7.94:1 | 10.49:1 |
 * | `--ink-3` 对面板 | 4.21:1 | 6.64:1 |
 * | `--line-strong` 对面板 | 2.66:1 | 3.66:1 |
 *
 * `--line-strong` 亮色 2.66:1 低于 WCAG 1.4.11 的 3:1。这是 Breeze 自己的分寸：
 * 它的控件边界是 `mix(Window, WindowText, contrast)` 算出来的浅灰，靠的是填充
 * 与阴影而不是描边。测试里没有这条断言，不构成失败；再往深里调就不是 Breeze 了。
 */
export const BREEZE = {
  id: "breeze",
  name: "Breeze",
  color: {
    light: {
      // 原值：`colors/BreezeLight.colors` → `[Colors:Window] BackgroundNormal=239,240,241`
      ground: "#EFF0F1",
      // 原值：`[Colors:View] BackgroundNormal=255,255,255`
      surface: "#FFFFFF",
      // 原值：`[Colors:View] BackgroundAlternate=247,247,247`。
      // 这一档不必折算角色——它在 Qt 里的用途就是**表格的交替行底**，
      // 与本项目的斑马纹是同一件事。
      "surface-2": "#F7F7F7",
      // 原值：`[Colors:Window] BackgroundAlternate=227,229,231`。
      // 折算的是角色：Breeze 的划过态是 `DecorationHover`（蓝），不是一档中性灰；
      // 本项目的划过是「底色再深一级」，所以按明度顺序取它自己的下一档中性面板色。
      "surface-3": "#E3E5E7",
      // 原值：`[Colors:Selection] BackgroundAlternate=163,212,250`。
      // 折算的是角色：Breeze 真正的选中底是 `[Colors:Selection] BackgroundNormal`
      // `#3DAEE9`，亮暗两档同值，配的是 `ForegroundNormal`（亮色白字 / 暗色近白字）。
      // 本项目的选中行没有「选中态文字色」这条轴，行内文字一律沿用 `--ink`，
      // 而**暗色的 `--ink` 是 #FCFCFC，压在 #3DAEE9 上只有 2.55:1**，读不动。
      // `BackgroundAlternate` 是同一节里「选中区内交替行」的底色，Breeze 为亮暗
      // 两档各发了一个（亮 #A3D4FA / 暗 #1E5774），正好一个承深字一个承浅字：
      // 亮色 9.67:1、暗色 7.66:1。两档取同一个键，结构才对称。
      // 选中底带蓝是 Breeze 的主张，这里保留了。
      sel: "#A3D4FA",
      // 折算（公式是原值）：`breezehelper.cpp` 的
      // `Helper::separatorColor() = KColorUtils::mix(Window, WindowText, frameIntensityBias())`，
      // `frameIntensityBias()` = `KColorScheme::frameContrast()`，默认 **0.2**
      // （`kcolorscheme.cpp` 的 `default_value = 0.2`）。
      // mix(#EFF0F1, #232629, 0.20) = #C6C8C9。
      line: "#C6C8C9",
      // 折算（两个常数都是原值）：同一个 mix 公式，比例改用方案文件自己写着的
      // `[KDE] contrast=4` —— `kcolorscheme.cpp` 的 `contrastF = 0.1 * contrast` = **0.4**。
      // mix(#EFF0F1, #232629, 0.40) = #9D9FA1。
      // 为什么不用 0.2 那一档：`--line-strong` 在本项目兼着控件边界、表头下沿与
      // 滚动条滑块，0.2 那档对面板只有 1.68:1，滑块会消失。
      "line-strong": "#9D9FA1",
      // 原值：`[Colors:View] ForegroundNormal=35,38,41`
      ink: "#232629",
      // 折算：Breeze 只发布两档文字色（`ForegroundNormal` 与 `ForegroundInactive`），
      // 本项目要三档。中间这档取两者在 sRGB 上的中点，依据是上游没有第三档可引。
      "ink-2": "#4A525A",
      // 原值：`[Colors:View] ForegroundInactive=112,125,138`
      "ink-3": "#707D8A",
      // 原值：`[Colors:View] ForegroundLink=41,128,185`。
      // **不是** `DecorationFocus`，理由见模块文档「为什么亮色的 accent 不是
      // DecorationFocus」。
      accent: "#2980B9",
      // 折算（几何是原值）：`kdecoration/breezedecoration.cpp` 的
      // `s_shadowParams[]`，`ShadowSmall` = 外偏移 (0,4)，第一层 (0,0) 半径 16、
      // 第二层 (0,-2) 半径 8（净偏移 (0,2)）。偏移与半径照抄。
      //
      // **不透明度是折算**：上游的 1.0 / 0.4 是 KWin 两层高斯蒙版做加法合成时的
      // 系数，与 CSS 的 `box-shadow` 不是同一套数学——照抄会得到一块纯黑。
      // 这里给整组乘一个统一的比例：亮色 0.20、暗色 0.55，层与层之间的比例
      // （1.0 : 0.4 / 0.9 : 0.3 / 0.8 : 0.2）保持上游原样。
      "shadow-raised": "0 4px 16px rgba(0, 0, 0, 0.2), 0 2px 8px rgba(0, 0, 0, 0.08)",
      // 折算：同上，`ShadowMedium` = 外偏移 (0,8)，(0,0) 半径 32 / (0,-4) 半径 16。
      "shadow-pop": "0 8px 32px rgba(0, 0, 0, 0.18), 0 4px 16px rgba(0, 0, 0, 0.06)",
      // 折算：同上，`ShadowLarge` = 外偏移 (0,12)，(0,0) 半径 48 / (0,-6) 半径 24。
      // Large 是 `breeze.kcfg` 里 `ShadowSize` 的默认档，对应本项目最重的对话框。
      "shadow-dialog": "0 12px 48px rgba(0, 0, 0, 0.16), 0 6px 24px rgba(0, 0, 0, 0.04)",
      // 折算：Breeze 没有发布模态蒙层的取值。取它自己的混合系数
      // `Metrics::Blend_Value = 0.3`（`breezemetrics.h`，注释写着「用于划过等效果的
      // 前后景混合值」）配纯黑，不另编数字。
      scrim: "rgba(0, 0, 0, 0.3)",
      // 折算：Breeze 的焦点是**两件事叠起来**——控件自己的 1px 边框换成 highlight
      // 原色（`Helper::renderButtonFrame` 里 `penBrush = highlightColor`），
      // 外面再罩一圈 2px、`Metrics::Blend_Value`（0.3）不透明度的同色环
      // （`Style::drawFocusFrame`）。本项目的焦点只有一个 `outline` 通道，
      // 两件事只能取其一；取的是**原色**那一件——30% 的一圈单独存在时太弱。
      // 写成 `var(--accent)` 而不是抄一份色值：发行版身份会在运行时覆盖 accent。
      focus: "var(--accent)",
      // 本语言的焦点环是单层。轴要填满，所以填透明。
      "focus-inner": "transparent",
    },
    dark: {
      // 原值：`colors/BreezeDark.colors` → `[Colors:View] BackgroundNormal=20,22,24`。
      // 暗色下 View 比 Window **深**，所以它在这里当页底，见模块文档。
      ground: "#141618",
      // 原值：`[Colors:Window] BackgroundNormal=32,35,38`
      surface: "#202326",
      // 原值：`[WM] activeBackground=39,44,49`。
      // 折算的是角色：暗色方案里刚好只有这一档落在 Window 与 Button 之间，
      // 斑马纹的偶数行需要「比面板浅一点点」的那一级。
      "surface-2": "#272C31",
      // 原值：`[Colors:Button] BackgroundNormal=41,44,48`
      //（`[Colors:Window] BackgroundAlternate` 与 `[Colors:Header] BackgroundNormal` 同值）
      "surface-3": "#292C30",
      // 原值：`[Colors:Selection] BackgroundAlternate=30,87,116`。
      // 与亮色同一条理由（见那边的长注释）：本项目的选中行沿用 `--ink`，
      // 暗色下是 #FCFCFC，需要一档能承住近白字的选中底。这一档 7.66:1。
      sel: "#1E5774",
      // 折算（公式是原值）：mix(#202326, #FCFCFC, 0.20)，同亮色那条 `frameContrast` 0.2。
      line: "#4C4E51",
      // 折算（公式是原值）：mix(#202326, #FCFCFC, 0.40)，同亮色那条 `[KDE] contrast=4`。
      "line-strong": "#787A7C",
      // 原值：`[Colors:View] ForegroundNormal=252,252,252`。
      // 不是纯白——Breeze 的正文就是这一档近白。
      ink: "#FCFCFC",
      // 折算：同亮色，`ForegroundNormal` 与 `ForegroundInactive` 的中点。
      "ink-2": "#CFD3D7",
      // 原值：`[Colors:View] ForegroundInactive=161,169,177`
      "ink-3": "#A1A9B1",
      // 原值：`[Colors:View] DecorationFocus=61,174,233`。
      // 暗色下这一档对面板 6.34:1，不需要像亮色那样退到 `ForegroundLink`。
      accent: "#3DAEE9",
      // 折算：几何同亮色（`ShadowSmall` / `Medium` / `Large`），
      // 整组比例改用 0.55——暗底上不加深阴影就看不见。
      "shadow-raised": "0 4px 16px rgba(0, 0, 0, 0.55), 0 2px 8px rgba(0, 0, 0, 0.22)",
      "shadow-pop": "0 8px 32px rgba(0, 0, 0, 0.5), 0 4px 16px rgba(0, 0, 0, 0.17)",
      "shadow-dialog": "0 12px 48px rgba(0, 0, 0, 0.44), 0 6px 24px rgba(0, 0, 0, 0.11)",
      // 折算：同亮色。上游没给蒙层写暗色覆盖，两档同值。
      scrim: "rgba(0, 0, 0, 0.3)",
      // 同亮色，见那边的注释。
      focus: "var(--accent)",
      "focus-inner": "transparent",
    },
  },
  /**
   * 圆角。
   *
   * Breeze 整套**只有一档圆角**：`kstyle/breezemetrics.h` 的
   * `Frame_FrameRadius = 5`，按钮、输入框、菜单、工具提示、橡皮筋全都用它
   * （`Helper::renderMenuFrame()` 里 `qreal radius(Metrics::Frame_FrameRadius)`）。
   * Kirigami 的 `cornerRadius` 也是 5（`src/platform/units.cpp`），QML 与
   * QtWidgets 两侧一致。
   *
   * - sm ← `CheckBox_Radius = Frame_FrameRadius - 1` = 4px（原值）
   * - md ← `Frame_FrameRadius` = 5px（原值）
   * - lg ← 同 md（**折算：档位归并**）。上游没有单独的对话框圆角——窗口装饰那档
   *   是算出来的（`Frame_FrameRadius * smallSpacing` 再对齐像素格），不是常量。
   * - pill ← 同 md（**折算：档位归并**）。Breeze 没有胶囊形状，这一档填成同一个 5px，
   *   等于这套语言不做胶囊，与 StrixMaid 四档全 0 是同一类表态。
   */
  radius: {
    "radius-sm": "4px",
    "radius-md": "5px",
    "radius-lg": "5px",
    "radius-pill": "5px",
  },
  /**
   * 字族。
   *
   * 头一个名字是**原值**：`plasma-workspace` 的 `kcms/fonts/fontssettings.kcfg`
   * 里，`font` 默认 `QFont("Noto Sans", 10)`、`fixed` 默认 `QFont("Hack", 10)`，
   * 都是 Regular。工具条字体与菜单字体也是 Noto Sans 10。
   *
   * **后面的回退链是折算**：上游发布的是字体名，不是 Web 的字族栈。
   * 中文链插在通用族之前，位置与 StrixMaid 那套一致。
   *
   * **这两个头名现在是随包发的 woff2**（`styles/base.css` 的 `@font-face`，
   * 拉丁子集，出处与许可见 `assets/fonts/NOTICE.txt`）。Noto Sans 在 OFL 1.1
   * 之下，Hack 在 MIT 加 Bitstream Vera 之下，两者都允许随产品再分发，
   * 于是这一套换到哪台机器上都真的换字形。后面的回退链原样留着。
   */
  family: {
    cjk: '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"',
    ui: `"Noto Sans", var(--cjk), sans-serif`,
    mono: `Hack, var(--cjk), ui-monospace, monospace`,
  },
  /**
   * 字阶。**基准与倍数是原值，px 与合并是折算。**
   *
   * Plasma 发布两个绝对字号：正文 `font` = Noto Sans **10pt**，
   * 小字 `smallestReadableFont` = Noto Sans **8pt**
   * （`plasma-workspace/kcms/fonts/fontssettings.kcfg`）。折算成 px 的公式是
   * 96/72 dpi：10pt = 13.33px → 13px，8pt = 10.67px → 11px。
   *
   * 标题的倍数来自 Kirigami 的 `Heading.qml`（KDE HIG 的文本页明文把标题交给
   * `Kirigami.Heading`，本身不给数字）：`level 1 → 1.35`、`2 → 1.20`、
   * `3 → 1.15`、`4 → 1.10`、`5 → 1`，乘在 `defaultFont.pointSize` 上。
   * 10pt 基准下得 13.5 / 12 / 11.5 / 11 pt，折成 px 是 18 / 16 / 15 / 15。
   *
   * 本项目的 15 档压进这 5 档，边界与 Adwaita 那套同一条：fs-350 以下是次要文字，
   * 落小字档；fs-400 起是正文，落正文档。
   *
   * KDE HIG 的排版页已经下线（`/hig/typography/` 404，旧站 301 回首页），
   * 所以没有「标题 +2pt」之类的规范可引，只有上面这些源码常量。
   */
  fontSize: {
    "fs-50": "11px",
    "fs-100": "11px",
    "fs-200": "11px",
    "fs-300": "11px",
    "fs-350": "11px",
    "fs-400": "13px",
    "fs-450": "13px",
    "fs-500": "13px",
    "fs-550": "13px",
    "fs-600": "13px",
    "fs-700": "15px",
    "fs-800": "15px",
    "fs-900": "16px",
    "fs-1000": "18px",
    "fs-1100": "18px",
  },
  /**
   * 行高。**八档全部是折算。**
   *
   * Breeze、Plasma 的字体默认值、KDE HIG 三处都没有任何行高或字距的取值——
   * Qt 的行盒由字体的 `QFontMetrics` 决定，不是主题的参数。所以这八档沿用
   * 本项目取值，**不假称有官方出处**。后两档的固定像素同理（徽章与提示框在
   * 本项目里靠行高定盒高，中英混排下不钉死会差 1px）。
   */
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
  /**
   * 控件尺寸。
   *
   * - `row` 34px（**折算**）。Breeze **没有按钮最小高度这个常量**
   *   （`breezemetrics.h` 里只有 `Button_MinWidth = 80`，而且只在有文字时生效）；
   *   `Style::pushButtonSizeFromContents` 的算法是「文字尺寸 →
   *   `expandSize(Button_MarginWidth=6)` → `expandSize(Frame_FrameWidth=2)`」，
   *   即高度 = 一行文字 + 12 + 4。一行文字取 Kirigami 的 `gridUnit = 18`
   *   （`units.cpp`；KDE HIG 的布局页明文「固定尺寸的元素用 gridUnit 的倍数」），
   *   得 18 + 12 + 4 = **34**。这个数与 `TabBar_StaticTabMinHeight = 34` 吻合，
   *   两条独立的路径对上同一个值。
   * - `row-toolbar` 46px（**折算**）：34px 的控件 + 上下各
   *   `Layout_ChildMarginWidth = 6`。上游没有工具条高度这条常量。
   * - `row-sm` 30px ← `TabBar_TabMinHeight = 30`（**原值**），Breeze 唯一发布的
   *   控件条高度，对应本项目工具条里的紧凑控件。
   * - `row-xs` 20px ← `CheckBox_Size = 20`（**原值**；`Slider_ControlThickness`
   *   与 `ScrollBar_MinSliderHeight` 也是 20），Breeze 的最小可点方块。
   * - `icon` 16px ← Kirigami `IconSizes::small() = 16`（**原值**，`units.cpp`）。
   *   HIG 的布局页：菜单项与凸起按钮里的图标用 `IconSizes.small`。
   * - `icon-lg` 22px ← Kirigami `IconSizes::smallMedium() = 22`（**原值**）。
   *   HIG：扁平/工具条按钮与无副标题的列表项用这一档。
   * - `rail` / `rail-narrow` 是**折算**：Breeze 没有侧栏宽度这条轴
   *   （`breezemetrics.h` 里连 `PM_SmallIconSize` 都是交还给父样式的）。
   *   沿用本项目取值，理由同 Fluent：侧栏宽度是本产品的信息密度决定的。
   */
  size: {
    row: "34px",
    "row-toolbar": "46px",
    "row-sm": "30px",
    "row-xs": "20px",
    rail: "216px",
    "rail-narrow": "44px",
    icon: "16px",
    "icon-lg": "22px",
  },
  /**
   * 间距。八档原值 + 两档折算。
   *
   * KDE HIG 的布局页给的是**语义单位**（`smallSpacing` / `mediumSpacing` /
   * `largeSpacing` / `cornerRadius` / `gridUnit`），数值在 Kirigami 的
   * `src/platform/units.cpp` 里：`gridUnit(18)`、`smallSpacing(4)`、
   * `mediumSpacing(6)`、`largeSpacing(8)`、`cornerRadius(5)`。
   * Qt 控件那边另有一套：`Layout_DefaultSpacing = 6`、
   * `Layout_ChildMarginWidth = 6`、`Layout_TopLevelMarginWidth = 10`。
   * 两套在 6 上一致，在小端不一致——下面逐档标了取的是哪一套。
   *
   * | 档 | 值 | 出处 |
   * |---|---|---|
   * | sp-1 | 2px | `MenuItem_ItemSpacing = 2`（原值） |
   * | sp-2 | 4px | Kirigami `smallSpacing`（原值） |
   * | sp-3 | 6px | Kirigami `mediumSpacing` / `Layout_DefaultSpacing`（原值） |
   * | sp-4 | 8px | Kirigami `largeSpacing`（原值） |
   * | sp-5 | 10px | `Layout_TopLevelMarginWidth = 10`（原值） |
   * | sp-6 | 12px | 折算：2×`mediumSpacing`；与 `SplitterProxyWidth = 12` 吻合 |
   * | sp-7 | 16px | `MenuItem_AcceleratorSpace = 16`（原值） |
   * | sp-8 | 18px | Kirigami `gridUnit`（原值） |
   * | sp-9 | 24px | 折算：3×`largeSpacing` |
   * | sp-10 | 36px | 折算：2×`gridUnit` |
   *
   * 这条阶梯与本项目原本那条几乎重合（只有 sp-8 由 20 变 18、sp-10 由 32 变 36）。
   * **不是巧合**：Breeze 与 StrixMaid 都是密排的桌面界面，都落在 2/4/6/8 这条
   * 通行的小端阶梯上。取值相同不等于轴可以省，见 design.ts 的模块文档。
   */
  space: {
    "sp-1": "2px",
    "sp-2": "4px",
    "sp-3": "6px",
    "sp-4": "8px",
    "sp-5": "10px",
    "sp-6": "12px",
    "sp-7": "16px",
    "sp-8": "18px",
    "sp-9": "24px",
    "sp-10": "36px",
  },
  /**
   * 描边。四档全部是原值，取自 `breezemetrics.h`：
   *
   * - `stroke` 1px ← `PenWidth::Frame = 1.001`（那 0.001 是 Qt 里让笔宽走
   *   「余弦画法」的老把戏，落到 CSS 上就是 1px）
   * - `stroke-thick` 2px ← `Frame_FrameWidth = 2`
   * - `stroke-thicker` 3px ← `TabBar_ActiveEffectSize = 3`（选中标签下的粗线），
   *   正对应本项目选中行左缘那道 3px 边条；`ToolTip_FrameWidth` 也是 3
   * - `stroke-thickest` 4px ← `MenuItem_HighlightGap = 4`（也等于 Kirigami 的
   *   `smallSpacing`），本项目用它画侧栏顶部的色带
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
   * 宽度 2px 是**原值**：`Style::drawFocusFrame`（`breezestyle.cpp`）里
   * `hmargin` / `vmargin` 取 `PM_FocusFrameHMargin` / `PM_FocusFrameVMargin`，
   * 两者都是 2，环就是 `outerRect = innerRect.adjusted(-2, -2, 2, 2)` 这一圈。
   *
   * 偏移 0 是**折算**：Breeze 先把内矩形收进 1px 再往外扩 2px，实际覆盖的是
   * 控件边缘的 -1px 到 +2px。CSS 的 `outline` 只有一个偏移量，给不出这种
   * 「跨着边缘」的环，取 0 让环紧贴控件外缘——三种做法（StrixMaid 外推 2px、
   * Fluent 内收 2px、Breeze 贴边）靠这一条轴分叉，不需要作用域覆盖。
   */
  focus: {
    "focus-width": "2px",
    "focus-offset": "0px",
  },
  /**
   * 动效。**时长是原值，落到哪一档是折算；缓动一条原值三条折算。**
   *
   * Breeze 的动画时长在 `kstyle/breeze.kcfg` 里：`AnimationsDuration` 默认
   * **100**（ms）。Kirigami 那边有一整排：`veryShortDuration 50`、
   * `shortDuration 100`、`longDuration 200`、`veryLongDuration 400`
   * （`src/platform/units.cpp`）。两套在 100 上一致。
   *
   * | 档 | 取值 | 出处 |
   * |---|---|---|
   * | t-pop | 100ms | `AnimationsDuration` / Kirigami `shortDuration` |
   * | t-color | 100ms | 同上——Breeze 的划过、按下就是这一档 |
   * | t-arrive | 200ms | Kirigami `longDuration` |
   * | t-enter | 200ms | Kirigami `longDuration` |
   * | t-shape | 400ms | Kirigami `veryLongDuration` |
   * | t-shape-out | 200ms | Kirigami `longDuration`，比展开短 |
   *
   * **缓动：`ease` 取 `linear` 是原值，而且是上游明写的。**
   * `kstyle/animations/breezeanimation.h` 的 `Breeze::Animation` 只设了时长，
   * 没设曲线，`breezestyle.cpp` / `breezehelper.cpp` 里 `QEasingCurve` 零命中；
   * 窗口装饰那边的注释把这件事说死了——
   * `// Linear to have the same easing as Breeze animations` 后面紧跟
   * `m_animation->setEasingCurve(QEasingCurve::Linear);`。
   *
   * 另外三条是**折算**：装饰的阴影动画用 `QEasingCurve::OutCubic` / `InCubic`
   * （`breezedecoration.cpp`），Qt 的这两条换成 CSS 的三次贝塞尔近似分别是
   * `cubic-bezier(0.215, 0.61, 0.355, 1)` 与 `cubic-bezier(0.55, 0.055, 0.675, 0.19)`；
   * `ease-in-out` 上游没有，取前者的出段与后者的入段合成
   * `cubic-bezier(0.645, 0.045, 0.355, 1)`。
   * **没有引入关键帧或形变动画**，与 StrixMaid 的取舍一致。
   */
  motion: {
    "t-pop": "100ms",
    "t-color": "100ms",
    "t-arrive": "200ms",
    "t-enter": "200ms",
    "t-shape": "400ms",
    "t-shape-out": "200ms",
    ease: "linear",
    "ease-out": "cubic-bezier(0.215, 0.61, 0.355, 1)",
    "ease-in-out": "cubic-bezier(0.645, 0.045, 0.355, 1)",
    "ease-shape": "cubic-bezier(0.215, 0.61, 0.355, 1)",
  },
  // `as const` 的理由见 strixmaid.ts
} as const satisfies Design;
