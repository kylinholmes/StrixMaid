import { buildTheme } from "../design";
import { FLUENT } from "./fluent";
import { STRIXMAID } from "./strixmaid";

/**
 * 设计语言注册表。
 *
 * **加一套语言 = 加一个文件 + 这里多两行**（一行 import、一行进 `THEMES`）。
 * 设置里的档位、对比度测试要验的面板、合集页的色卡，全部从 `THEMES` 推导，
 * 界面与测试都不必回头改。
 *
 * `buildTheme` 把书写形式（按轴分组）压成落盘形式（每档模式一张平表）。
 * 压平放在这里而不是放在各自的文件里，是为了让设计语言的文件只剩取值，
 * 没有一行逻辑——那样非前端的人也改得动。
 *
 * 通用主题：认不出平台、或者认得出但还没有专属设计语言时用这一套。
 * Linux 与 macOS 目前全部落在这里。
 */
export const GENERIC_THEME = buildTheme(STRIXMAID);

/** Fluent：Windows 的设计语言。取值与出处见 `./fluent.ts`。 */
export const FLUENT_THEME = buildTheme(FLUENT);

/**
 * **这是唯一的一份清单。**
 *
 * `as const` 让 `ThemeId` 能收成字面量联合，于是「选了一套还没实现的语言」
 * 在类型上就通不过，不必等到运行时回落。
 */
export const THEMES = [GENERIC_THEME, FLUENT_THEME] as const;
