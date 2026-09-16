/**
 * 主题层的**入口**：挑哪一套设计语言，以及用户的那一档偏好（2026-09-16 第四版）。
 *
 * 第三版这个文件同时装着三件事：token 的类型、两套语言的全部取值、挑语言的逻辑。
 * 加第三套语言时它会变成一千行，而那一千行里 90% 是别人的官方色号。
 * 第四版把前两件拆出去：
 *
 * - `design.ts` —— 词汇表：有哪些轴、每条轴有哪些档、`Design` 与 `Theme` 的类型。
 * - `designs/<name>.ts` —— 一套设计语言一个文件，只有取值与出处，没有逻辑。
 * - `designs/index.ts` —— 注册表，汇总成 `THEMES`。
 * - 本文件 —— 挑语言：`pickTheme`、偏好 `DesignPreference` 与设置里的那份清单。
 *
 * 为了让调用点与测试不必跟着改导入路径，类型与注册表在这里**原样再导出一次**。
 * 这个文件仍然是主题层对外的那一扇门。
 *
 * **身份与设计语言是两件事，类型上不合并。** 本文件只回答「用哪套设计语言」；
 * 方块上的字、品牌色、accent 是发行版身份，在 distro.ts 里。两者将来会分叉：
 * Ubuntu 装 KDE 应当是 Ubuntu 的橙色方块配 Breeze 的中性色。
 */

import type { Theme } from "./design";
import {
  ADWAITA_THEME,
  BREEZE_THEME,
  FLUENT_THEME,
  GENERIC_THEME,
  MACOS_THEME,
  THEMES,
  YARU_THEME,
} from "./designs";

export type {
  Design,
  FamilyToken,
  FocusToken,
  FontSizeToken,
  LineHeightToken,
  Mode,
  ModeToken,
  MotionToken,
  NeutralToken,
  RadiusToken,
  SizeToken,
  SpaceToken,
  StrokeToken,
  Theme,
  TokenName,
  TokenSet,
} from "./design";
export {
  buildTheme,
  FAMILY_TOKENS,
  FOCUS_TOKENS,
  FONT_SIZE_TOKENS,
  LINE_HEIGHT_TOKENS,
  MODE_TOKENS,
  MOTION_TOKENS,
  NEUTRAL_TOKENS,
  RADIUS_TOKENS,
  SHARED_TOKENS,
  SIZE_TOKENS,
  SPACE_TOKENS,
  STROKE_TOKENS,
  TOKEN_NAMES,
} from "./design";
export type { Ramp } from "./designs/strixmaid";
/** 通用主题的中性色明度阶。属于 StrixMaid 那套语言，从它那里再导出。 */
export { RAMP } from "./designs/strixmaid";
export {
  ADWAITA_THEME,
  BREEZE_THEME,
  FLUENT_THEME,
  GENERIC_THEME,
  MACOS_THEME,
  THEMES,
  YARU_THEME,
};

/** 已注册的设计语言 id。从 `THEMES` 推导，不要另写一份。 */
export type ThemeId = (typeof THEMES)[number]["id"];

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
   * 桌面环境。取的是 Linux 上 `XDG_CURRENT_DESKTOP` 那个值的形状，
   * 所以既可能是 `KDE` 这样的单值，也可能是 `ubuntu:GNOME` 这样的冒号分隔表。
   *
   * **今天后端还不报这个字段**，所以恒为 undefined，行为与只看 `osId` 完全一致。
   * 这三套桌面语言（Adwaita / Breeze / Yaru）今天只能在设置里手动选中，
   * 自动识别那条路要等后端补上这个字段——那是 `crates/` 的改动，单独一轮。
   */
  readonly desktop?: string | null;
}

/**
 * 桌面环境 → 设计语言。
 *
 * 键是**小写之后**的 `XDG_CURRENT_DESKTOP` 的某一段，匹配在 `pickTheme` 里做，
 * 见那里对冒号分隔的处理。
 *
 * 收哪些键是按「这个值在真机上真的会出现」来定的，不求全：
 *
 * - GNOME 在不同发行版上会报 `GNOME`、`GNOME-Classic`、`GNOME-Flashback`；
 * - KDE Plasma 报 `KDE`；`plasma` 是 wayland 会话名里常见的另一种拼法；
 * - Ubuntu 的 GNOME 会话报 `ubuntu:GNOME`，`ubuntu` 那一段排在前面，
 *   所以 Ubuntu 装 GNOME 拿到的是 Yaru 而不是 Adwaita——这正是要的：
 *   那台机器上的 GNOME 长的就是 Yaru 的样子。Unity 会话报 `Unity`，同样归 Yaru。
 *
 * 认不出的桌面（XFCE、Cinnamon、MATE、i3……）不收：它们各有各的主题，
 * 硬塞给其中一套等于在界面上假装认识这台机器。落回通用主题。
 */
const THEME_BY_DESKTOP: Readonly<Record<string, Theme>> = {
  gnome: ADWAITA_THEME,
  "gnome-classic": ADWAITA_THEME,
  "gnome-flashback": ADWAITA_THEME,
  kde: BREEZE_THEME,
  "kde-plasma": BREEZE_THEME,
  plasma: BREEZE_THEME,
  ubuntu: YARU_THEME,
  unity: YARU_THEME,
};

/**
 * 操作系统 → 设计语言。
 *
 * macOS 不走桌面环境那一档：它只有一套界面，`osId` 一档就够。
 * Linux 反过来，界面长什么样由桌面环境决定、不由发行版决定，所以 Linux 的
 * 各个 `osId` 一个都不在这张表里，全靠 `THEME_BY_DESKTOP`。
 */
const THEME_BY_OS: Readonly<Record<string, Theme>> = {
  windows: FLUENT_THEME,
  macos: MACOS_THEME,
};

/**
 * 设计语言偏好：**用户自己选的那一档**，不是被管机器的属性。
 *
 * - `system`：跟随被管机器的平台（Windows → Fluent，macOS → macOS，
 *   Linux 按桌面环境 → Adwaita / Breeze / Yaru）；
 * - 其余取值是某套设计语言的 `Theme.id`：强制用那一套，不看对面是什么机器。
 *
 * 可选的具体语言**从 `THEMES` 推导**，不是写死的联合类型。界面上只应当出现
 * 真的实现了的那几套：列一个选了没反应、或者静默回落的选项，等于在界面上编能力。
 *
 * 默认取 `system`。**桌面环境那一档今天在真机上永远不命中**（后端还不报
 * `desktop`），所以 Linux 这一档的结果仍然是 StrixMaid 那套，与加这个设置之前
 * 零差异；Windows 用户看见 Fluent，macOS 用户看见 macOS 那套。
 * 想在 Linux 上用 Adwaita / Breeze / Yaru，今天只能在设置里手动选。
 *
 * **这一档不影响 accent。** accent 回答的是「正在看哪台机器」，属于身份层
 * （distro.ts），与界面长什么样是两件事：选了 StrixMaid 的用户连上 Windows，
 * 拿到的仍然是 Fluent 蓝的方块与蓝色 accent，只是中性色换成 StrixMaid 那套。
 * 由此每个发行版的 accent 都可能落在**任意一套**已注册语言的面板上，
 * 对比度因此要对全部面板都验，见 tokens.test.ts。
 */
export type DesignPreference = "system" | ThemeId;

/** 读不到偏好、或存着的值非法时用这一档，理由见 `DesignPreference`。 */
export const DEFAULT_DESIGN: DesignPreference = "system";

export interface DesignOption {
  readonly value: DesignPreference;
  readonly label: string;
}

/**
 * 控件上的各档：「系统」+ 注册表里的每一套，顺序就是控件里的顺序。
 *
 * **由 `THEMES` 生成，不要写死。** 加一套语言就是加一个文件、在注册表里多一行；
 * 清单硬编码在界面里的话，每加一套都得回来改 UI，那个结构就白做了。
 * 显示用的名字取 `Theme.name`，每套语言自己带着。
 */
export const DESIGN_OPTIONS: readonly DesignOption[] = [
  { value: "system", label: "系统" },
  ...THEMES.map((t) => ({ value: t.id, label: t.name })),
];

/**
 * 任意值 → 偏好。认不出一律回落到默认档。
 *
 * 入口收在这里而不是在读 localStorage 的地方判断，是因为合法值的清单只应当有
 * 一份：`DESIGN_OPTIONS` 跟着注册表长，存储层不必跟着改。旧版本存下的、
 * 手改过的、指向已经删掉的那套语言的值，到这里一律回落。
 */
export function asDesign(value: unknown): DesignPreference {
  return DESIGN_OPTIONS.some((o) => o.value === value)
    ? (value as DesignPreference)
    : DEFAULT_DESIGN;
}

/**
 * 选一套设计语言。**这是唯一的入口**，加维度只改这里，不动调用点。
 *
 * 两个维度的优先级是**用户偏好 > 机器**：偏好指名了某一套就直接给那一套，
 * 连机器是什么都不必看——这一档的语义是「不要跟着对面变」，
 * 把它放在最前面，调用点就不需要为这个设置写任何 if。
 *
 * 偏好为 `system` 时才按机器挑，优先级是**桌面环境 > 操作系统 > 通用**。
 * 桌面环境排在前面，因为界面长什么样由桌面环境决定，不由发行版决定：
 * Ubuntu 装 KDE 该用 Breeze，Fedora 装 GNOME 该用 Adwaita，
 * 而没有桌面的服务器两者都不是，落回通用。Windows 与 macOS 上没有这个分叉，
 * `osId` 一档就够。
 *
 * 认不出时返回通用主题，不假装认识这台机器——与 `findDistro` 的同一条约定。
 *
 * **注意：后端今天还不报 `desktop`**，所以桌面环境那一档在真机上恒不命中，
 * Adwaita / Breeze / Yaru 只能在设置里手动选。详见 `PlatformIdentity.desktop`。
 *
 * `design` 有默认值而不是必传：默认档的结果与加这个参数之前逐字相同，
 * 于是「只问机器该用什么」的调用点（测试、将来的预览）可以继续只传一个参数。
 */
export function pickTheme(
  identity: PlatformIdentity,
  design: DesignPreference = DEFAULT_DESIGN,
): Theme {
  // 指名的那套认不出来（存储里是旧 id、那套语言已经删掉）就当成「系统」，
  // 而不是抛错或给一块空白。`asDesign` 在入口处已经拦过一道，这里是第二道。
  const forced = design === "system" ? undefined : THEMES.find((t) => t.id === design);
  if (forced) return forced;
  // `XDG_CURRENT_DESKTOP` 是**冒号分隔的一张表**（`ubuntu:GNOME`、
  // `X-Cinnamon`），不是单值。按顺序试每一段，第一个认得出的算数：
  // Ubuntu 的 GNOME 会话报 `ubuntu:GNOME`，`ubuntu` 在前，于是拿到 Yaru
  // 而不是 Adwaita——那台机器上的 GNOME 长的就是 Yaru 的样子。
  // 大小写一并归一，后端将来原样透传环境变量也认得出。
  for (const part of identity.desktop?.toLowerCase().split(":") ?? []) {
    const byDesktop = THEME_BY_DESKTOP[part.trim()];
    if (byDesktop) return byDesktop;
  }
  const os = identity.osId?.toLowerCase();
  if (os && THEME_BY_OS[os]) return THEME_BY_OS[os];
  return GENERIC_THEME;
}
