import { afterEach, describe, expect, it, vi } from "vitest";
import type { Mode, TokenName } from "./design";
import { MODE_TOKENS, SHARED_TOKENS, TOKEN_NAMES } from "./design";
import { UNKNOWN_DISTRO } from "./distro";
import { GENERIC_THEME, THEMES } from "./tokens";

/**
 * 设计语言这一层的两条硬约束。
 *
 * 一、**每套语言必须填满每一条轴**。这是「加一套语言只写一个文件」的保证：
 * 组件与 CSS 里出现的每一个 `var(--x)`，在任何一套语言下都必须解析得出值，
 * 否则换过去就是一处塌掉的样式，而且塌在哪儿要等有人截图才知道。
 * 类型上 `Design` 的每一组都是 `Record<该轴全部 token, string>`，漏一条编译不过；
 * 这里再从运行时验一遍，挡住 `as unknown as` 之类的绕过。
 *
 * 二、**StrixMaid 零视觉变化**。第四版把圆角、字阶、控件尺寸、间距、描边、
 * 阴影、动效从 CSS 搬进 token，搬而不改。本机做不了视觉回归，所以把
 * `applyTheme` 落到 `:root` 上的**每一个**变量与一张写死的「今天的取值」表
 * 逐 key 对齐：少一条、多一条、值不同，三种都失败。
 *
 * 这张表是手抄的，抄的是 2026-09-16 重构之前 `tokens.css` / `base.css` /
 * 各 CSS 模块里的那些字面量。它**不是**从 `STRIXMAID` 生成的——从被测对象
 * 生成期望值等于什么都没验。
 */

const MODES: readonly Mode[] = ["light", "dark"];

/** 与模式无关的那 64 条。两档模式下必须一模一样。 */
const SHARED_EXPECTED: Readonly<Record<string, string>> = {
  // 圆角：spec §4，全 0 无例外
  "radius-sm": "0",
  "radius-md": "0",
  "radius-lg": "0",
  "radius-pill": "0",
  // 字族：原 tokens.css 的 --cjk / --ui / --mono
  cjk: '"PingFang SC", "Microsoft YaHei UI", "Microsoft YaHei", "Hiragino Sans GB", "Noto Sans CJK SC", "Source Han Sans SC"',
  ui: 'system-ui, -apple-system, "Segoe UI", var(--cjk), sans-serif',
  mono: '"IBM Plex Mono", var(--cjk), ui-monospace, Menlo, Consolas, monospace',
  // 字阶：15 档，逐条来自重构前 CSS 里的字面量
  "fs-50": "8.5px", // Gallery .ramp span
  "fs-100": "9px", // Perf .cellTag / Proc、Svc .thArrow
  "fs-200": "10px", // 大写小标签、计数徽章、表头
  "fs-300": "11px", // mono 元信息
  "fs-350": "11.5px", // Field .hint / Meter .value / States .cmd
  "fs-400": "12px", // 小号正文
  "fs-450": "12.5px", // 密排正文：表格、对话框正文
  "fs-500": "13px", // base.css 的 body
  "fs-550": "13.5px", // States .title
  "fs-600": "14px", // Overlay .head
  "fs-700": "15px", // 页标题
  "fs-800": "16px", // Perf .numbers .v
  "fs-900": "17px", // Login h1
  "fs-1000": "19px", // Gallery h2
  "fs-1100": "20px", // Perf .cellVal
  // 行高
  "lh-none": "1",
  "lh-tight": "1.1",
  "lh-snug": "1.4",
  "lh-body": "1.5",
  "lh-normal": "1.6", // base.css 的 body
  "lh-loose": "1.65",
  "lh-200": "15px", // NavRail .count / Status .tag 的盒高
  "lh-300": "16px", // Plot .tip / Perf .legend 的行盒
  // 控件尺寸：前四条来自原 tokens.css 的 --row / --row-toolbar 与 CSS 里的 26 / 24
  row: "30px",
  "row-toolbar": "40px",
  "row-sm": "26px",
  "row-xs": "24px",
  // 侧栏展开宽度是**重构之后有意加宽的**（184 → 216px），不是搬运出的偏差。
  // 这张表的其余每一条都必须与重构前逐字相同，只有这条是例外，所以单独标出来：
  // 184px 在装下「图标 + 中文标签 + 底部那行两个按钮」之后已经很挤，
  // 横排的分段控件在那个宽度里根本放不下。216 = 27×8，仍在 8 的栅格上。
  rail: "216px",
  "rail-narrow": "44px",
  icon: "16px",
  "icon-lg": "22px",
  // 间距
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
  // 描边
  stroke: "1px",
  "stroke-thick": "2px",
  "stroke-thicker": "3px",
  "stroke-thickest": "4px",
  // 焦点环几何：base.css 原来的 `outline: 2px solid var(--accent); outline-offset: 2px`
  "focus-width": "2px",
  "focus-offset": "2px",
  // 动效：前两条来自原 tokens.css，其余来自 Rail / Shell / PageChrome 里的字面量
  "t-pop": "90ms",
  "t-color": "120ms",
  "t-arrive": "150ms",
  "t-enter": "240ms",
  "t-shape": "440ms",
  "t-shape-out": "320ms",
  ease: "ease",
  "ease-out": "ease-out",
  "ease-in-out": "ease-in-out",
  "ease-shape": "cubic-bezier(0.3, 0, 0.1, 1)",
};

/** 随模式变的那 17 条。中性色是 RAMP 那套明度阶算出来的串，这里写死结果。 */
const MODE_EXPECTED: Readonly<Record<Mode, Readonly<Record<string, string>>>> = {
  light: {
    ground: "oklch(88.0% 0 0)",
    surface: "oklch(93.0% 0 0)",
    "surface-2": "oklch(89.5% 0 0)",
    "surface-3": "oklch(85.0% 0 0)",
    line: "oklch(82.0% 0 0)",
    "line-strong": "oklch(64.0% 0 0)",
    sel: "oklch(78.0% 0 0)",
    "ink-3": "oklch(48.0% 0 0)",
    "ink-2": "oklch(34.0% 0 0)",
    ink: "oklch(24.0% 0 0)",
    accent: "#5A5A5A",
    // 重构前只有一档 --shadow-pop，三档都得是它，否则浮层的观感就变了
    "shadow-raised": "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)",
    "shadow-pop": "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)",
    "shadow-dialog": "0 1px 1px rgb(0 0 0 / 0.1), 0 9px 26px rgb(0 0 0 / 0.16)",
    scrim: "rgb(0 0 0 / 0.45)",
    focus: "var(--accent)",
    "focus-inner": "transparent",
  },
  dark: {
    ground: "oklch(22.4% 0 0)",
    surface: "oklch(26.4% 0 0)",
    "surface-2": "oklch(30.4% 0 0)",
    "surface-3": "oklch(35.4% 0 0)",
    line: "oklch(37.4% 0 0)",
    "line-strong": "oklch(52.4% 0 0)",
    sel: "oklch(44.0% 0 0)",
    "ink-3": "oklch(63.0% 0 0)",
    "ink-2": "oklch(79.0% 0 0)",
    ink: "oklch(92.0% 0 0)",
    accent: "#9A9A9A",
    "shadow-raised": "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)",
    "shadow-pop": "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)",
    "shadow-dialog": "0 1px 1px rgb(0 0 0 / 0.5), 0 9px 26px rgb(0 0 0 / 0.62)",
    // 重构前 tokens.css 没给 --scrim 写暗色覆盖，两档同值
    scrim: "rgb(0 0 0 / 0.45)",
    focus: "var(--accent)",
    "focus-inner": "transparent",
  },
};

/** 最小 document：落下去的 CSS 变量收进 map，断言直接看这里。 */
function fakeDocument(vars: Map<string, string>) {
  return {
    documentElement: {
      style: { setProperty: (k: string, v: string) => void vars.set(k, v) },
      dataset: {} as Record<string, string>,
    },
  };
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("轴的完整性", () => {
  it("token 名没有重名——两条轴撞名字会让后写的那条静默盖掉前一条", () => {
    expect(new Set(TOKEN_NAMES).size).toBe(TOKEN_NAMES.length);
  });

  it("随模式变的与不随模式变的，两组不重叠", () => {
    const shared = new Set<string>(SHARED_TOKENS);
    for (const t of MODE_TOKENS) expect(shared.has(t), t).toBe(false);
  });

  // 这一条才是「加一套语言只写一个文件」的执行者：遍历 THEMES，
  // 加一套语言时自动覆盖它，不必回头补测试。
  it.each(THEMES)("$name 填满了每一条轴，没有空值", (theme) => {
    for (const mode of MODES) {
      const set = theme.tokens[mode];
      for (const name of TOKEN_NAMES) {
        const value = set[name as TokenName];
        expect(value, `${theme.id}.${mode}.${name} 缺值`).toBeTypeOf("string");
        expect(value.trim(), `${theme.id}.${mode}.${name} 是空串`).not.toBe("");
      }
    }
  });

  it.each(THEMES)("$name 没有多余的 token——键集与轴的清单严格一致", (theme) => {
    const expected = [...TOKEN_NAMES].sort();
    for (const mode of MODES) {
      expect(Object.keys(theme.tokens[mode]).sort(), `${theme.id}.${mode}`).toEqual(expected);
    }
  });

  it.each(THEMES)("$name 的不随模式变的那些轴，亮暗两档取值相同", (theme) => {
    for (const name of SHARED_TOKENS) {
      expect(theme.tokens.light[name], `${theme.id}.${name}`).toBe(theme.tokens.dark[name]);
    }
  });
});

describe("StrixMaid 零视觉变化", () => {
  it("期望表本身覆盖了每一条轴——漏抄一条就等于那条没人验", () => {
    const covered = [...Object.keys(SHARED_EXPECTED), ...Object.keys(MODE_EXPECTED.light)].sort();
    expect(covered).toEqual([...TOKEN_NAMES].sort());
    expect(Object.keys(MODE_EXPECTED.dark).sort()).toEqual([...MODE_TOKENS].sort());
  });

  // 不只比 `theme.tokens`，而是比 applyTheme 真的写到 :root 上的东西：
  // 中间少写一个、名字前缀拼错，只有走这条路径才验得出来。
  it.each(MODES)("%s 档：applyTheme 写出去的每一个变量都与重构前逐字相同", async (mode) => {
    const vars = new Map<string, string>();
    vi.resetModules();
    vi.stubGlobal("document", fakeDocument(vars));
    const { applyTheme } = await import("./useTheme");

    // 用「认不出的机器」，这样 accent 留主题自己那一档，不被发行版身份盖掉
    applyTheme(mode, GENERIC_THEME, UNKNOWN_DISTRO);

    const expected = { ...SHARED_EXPECTED, ...MODE_EXPECTED[mode] };
    expect([...vars.keys()].sort()).toEqual(
      Object.keys(expected)
        .map((k) => `--${k}`)
        .sort(),
    );
    for (const [name, value] of Object.entries(expected)) {
      expect(vars.get(`--${name}`), `--${name}`).toBe(value);
    }
  });
});
