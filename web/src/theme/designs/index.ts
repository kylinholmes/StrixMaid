import { buildTheme } from "../design";
import { ADWAITA } from "./adwaita";
import { BREEZE } from "./breeze";
import { FLUENT } from "./fluent";
import { MACOS } from "./macos";
import { STRIXMAID } from "./strixmaid";
import { YARU } from "./yaru";

/**
 * 设计语言注册表。
 *
 * **加一套语言 = 加一个文件 + 这里多两行**（一行 import、一行进 `THEMES`）。
 * 设置里的档位、对比度测试要验的面板、合集页的色卡，全部从 `THEMES` 推导，
 * 界面与测试都不必回头改。第五版加 macOS / Adwaita / Breeze / Yaru 四套时，
 * 这条承诺兑现了：四个新文件、本文件八行，组件与 CSS 一行没动。
 *
 * `buildTheme` 把书写形式（按轴分组）压成落盘形式（每档模式一张平表）。
 * 压平放在这里而不是放在各自的文件里，是为了让设计语言的文件只剩取值，
 * 没有一行逻辑——那样非前端的人也改得动。
 *
 * 通用主题：认不出平台、或者认得出但还没有专属设计语言时用这一套。
 * 没有桌面环境的服务器（绝大多数被管机器）落在这里。
 */
export const GENERIC_THEME = buildTheme(STRIXMAID);

/** Fluent：Windows 的设计语言。取值与出处见 `./fluent.ts`。 */
export const FLUENT_THEME = buildTheme(FLUENT);

/** macOS：Apple 的设计语言。取值与出处见 `./macos.ts`。 */
export const MACOS_THEME = buildTheme(MACOS);

/** Adwaita：GNOME 的设计语言。取值与出处见 `./adwaita.ts`。 */
export const ADWAITA_THEME = buildTheme(ADWAITA);

/** Breeze：KDE Plasma 的设计语言。取值与出处见 `./breeze.ts`。 */
export const BREEZE_THEME = buildTheme(BREEZE);

/** Yaru：Ubuntu 的设计语言。取值与出处见 `./yaru.ts`。 */
export const YARU_THEME = buildTheme(YARU);

/**
 * **这是唯一的一份清单。**
 *
 * `as const` 让 `ThemeId` 能收成字面量联合，于是「选了一套还没实现的语言」
 * 在类型上就通不过，不必等到运行时回落。
 *
 * 顺序就是设置里那列单选的顺序：本项目自己的那套排头（它同时是回落档），
 * 然后按平台排——Windows、macOS，再是三套 Linux 桌面。
 */
export const THEMES = [
  GENERIC_THEME,
  FLUENT_THEME,
  MACOS_THEME,
  ADWAITA_THEME,
  BREEZE_THEME,
  YARU_THEME,
] as const;
