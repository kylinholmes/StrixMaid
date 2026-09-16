import { describe, expect, it } from "vitest";
import { DISTROS, UNKNOWN_DISTRO } from "./distro";
import {
  asDesign,
  DEFAULT_DESIGN,
  DESIGN_OPTIONS,
  type DesignPreference,
  FLUENT_THEME,
  GENERIC_THEME,
  type Mode,
  type NeutralToken,
  pickTheme,
  RAMP,
  THEMES,
  type Theme,
  type TokenSet,
} from "./tokens";

const MODES: readonly Mode[] = ["light", "dark"];
const NEUTRALS = Object.keys(RAMP.light) as NeutralToken[];

/** accent 的对比度门槛。为什么是 3 不是 4.5，见本文件末尾那条断言上面的长注释。 */
const MIN_RATIO = 3;

/**
 * 只对**通用主题**成立的那几条。
 *
 * 第二版把它们写成了全局公理，其实它们是那一条灰阶自己的结论：
 * 亮色面板的上限是配着那条阶梯算出来的，暗色 ground 的下限绑着那套阴影，
 * 正文的钳位是那套明度下的光晕实测。换一套设计语言，前提就换了。
 */
describe("通用主题的中性灰阶", () => {
  it("正文钳在 L92 / L24，纯白正文在暗底上会产生光晕", () => {
    expect(RAMP.dark.ink).toBe(92);
    expect(RAMP.light.ink).toBe(24);
  });

  it("亮色面板不得接近纯白——面板是屏幕上面积最大的东西，L99.5 会刺眼", () => {
    // 这一条**不对 Fluent 生效**：Fluent 的面板 #F5F5F5 换算过去约 L96，
    // 会撞上这个上限。用户已确认那一档就是要的，见 tokens.ts 里 Fluent 那段。
    expect(RAMP.light.surface).toBeLessThanOrEqual(94);
  });

  it("暗色底不低于 22%——双层阴影的落脚点，再暗阴影就消失了", () => {
    expect(RAMP.dark.ground).toBeGreaterThanOrEqual(22);
  });

  // 下面两条是「Linux 与 macOS 渲染零变化」的结构性保证：本机无法做视觉回归，
  // 所以把第二版 applyTheme 的输出逐字钉住——中性色是同样的 `oklch(L% 0 0)` 串，
  // accent 与「认不出」那档同值（认得出发行版时由发行版自己的那档覆盖）。
  it("通用主题写出去的中性色串与第二版逐字相同", () => {
    for (const mode of MODES) {
      for (const token of NEUTRALS) {
        expect(GENERIC_THEME.tokens[mode][token]).toBe(
          `oklch(${RAMP[mode][token].toFixed(1)}% 0 0)`,
        );
      }
    }
  });

  it("通用主题的 accent 与「认不出」那档同值，不要单独改其中一边", () => {
    expect(GENERIC_THEME.tokens.light.accent).toBe(UNKNOWN_DISTRO.accent.light);
    expect(GENERIC_THEME.tokens.dark.accent).toBe(UNKNOWN_DISTRO.accent.dark);
  });
});

/**
 * 对**每一套**设计语言都成立的那几条。
 *
 * 「面板浮在页底之上」「选中比划过深」这些是设计意图，不是某条灰阶的特产，
 * 任何主题都该满足。Fluent 那套填完同样要过，过不了说明档位填错了。
 */
describe.each(THEMES)("$name 主题", (theme: Theme) => {
  it("每个 token 都有值，且是 #RRGGBB 或 oklch(L% 0 0)", () => {
    for (const mode of MODES) {
      const set = theme.tokens[mode];
      for (const token of [...NEUTRALS, "accent"] as (keyof TokenSet)[]) {
        expect(set[token], `${theme.id}.${mode}.${token}`).toMatch(
          /^(#[0-9A-F]{6}|oklch\(\d+(?:\.\d+)?% 0 0\))$/i,
        );
      }
    }
  });

  it("暗色四级底的明度阶梯严格递增：奇行 < 偶行 < 划过 < 选中", () => {
    const l = (token: NeutralToken) => relLum(srgbOf(theme.tokens.dark[token]));
    expect(l("surface")).toBeLessThan(l("surface-2"));
    expect(l("surface-2")).toBeLessThan(l("surface-3"));
    expect(l("surface-3")).toBeLessThan(l("sel"));
  });

  it("亮色四级底的明度阶梯严格递减：面板 > 偶行 > 划过 > 选中", () => {
    const l = (token: NeutralToken) => relLum(srgbOf(theme.tokens.light[token]));
    expect(l("surface")).toBeGreaterThan(l("surface-2"));
    expect(l("surface-2")).toBeGreaterThan(l("surface-3"));
    expect(l("surface-3")).toBeGreaterThan(l("sel"));
  });

  it("页底比面板暗，面板才浮得起来", () => {
    for (const mode of MODES) {
      const l = (token: NeutralToken) => relLum(srgbOf(theme.tokens[mode][token]));
      expect(l("ground"), `${theme.id}.${mode}`).toBeLessThan(l("surface"));
    }
  });

  // `--ink` 上面是真文字，适用的是正文门槛 4.5:1，不是 accent 那条 3:1。
  it("正文对自己的面板 ≥4.5:1", () => {
    for (const mode of MODES) {
      const set = theme.tokens[mode];
      const ratio = contrast(relLum(srgbOf(set.ink)), relLum(srgbOf(set.surface)));
      expect(
        ratio,
        `${theme.id}.${mode} 的正文 ${set.ink} 只有 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(4.5);
    }
  });

  it("主题自带的 accent 对自己的面板 ≥3:1", () => {
    for (const mode of MODES) {
      const set = theme.tokens[mode];
      const ratio = contrast(relLum(srgbOf(set.accent)), relLum(srgbOf(set.surface)));
      expect(
        ratio,
        `${theme.id}.${mode} 的 accent ${set.accent} 只有 ${ratio.toFixed(2)}:1`,
      ).toBeGreaterThanOrEqual(MIN_RATIO);
    }
  });

  // 滚动条滑块静止态用 `--line-strong`、划过态用 `--ink-3`（base.css）。
  // 两者同值等于划过没反应——Fluent 的 colorNeutralStrokeAccessible 正好踩中这个坑。
  it("强分隔线与三级正文不同值，否则滚动条的划过态会消失", () => {
    for (const mode of MODES) {
      expect(theme.tokens[mode]["line-strong"], `${theme.id}.${mode}`).not.toBe(
        theme.tokens[mode]["ink-3"],
      );
    }
  });
});

describe("pickTheme（挑设计语言）", () => {
  it("Windows 用 Fluent，其余一律通用", () => {
    expect(pickTheme({ osId: "windows" })).toBe(FLUENT_THEME);
    for (const d of DISTROS.filter((x) => x.id !== "windows")) {
      expect(pickTheme({ osId: d.id }), d.id).toBe(GENERIC_THEME);
    }
  });

  it("认不出、没传、传空都落回通用，不假装认识这台机器", () => {
    expect(pickTheme({})).toBe(GENERIC_THEME);
    expect(pickTheme({ osId: null })).toBe(GENERIC_THEME);
    expect(pickTheme({ osId: "" })).toBe(GENERIC_THEME);
    expect(pickTheme({ osId: "haiku" })).toBe(GENERIC_THEME);
  });

  it("os id 大小写不敏感——后端给什么形状都认", () => {
    expect(pickTheme({ osId: "Windows" })).toBe(FLUENT_THEME);
  });

  // 桌面环境是留给 Adwaita / Breeze / Yaru 的扩展点，后端今天还不报这个字段。
  // 这一条钉住「字段缺省时行为与只看 osId 完全一致」，将来加档位不能破坏它。
  it("桌面环境缺省或认不出时，结果与只看 osId 一致", () => {
    expect(pickTheme({ osId: "windows", desktop: undefined })).toBe(FLUENT_THEME);
    expect(pickTheme({ osId: "windows", desktop: null })).toBe(FLUENT_THEME);
    expect(pickTheme({ osId: "ubuntu", desktop: "kde" })).toBe(GENERIC_THEME);
  });
});

/**
 * 设计语言偏好这一维。
 *
 * 上面那一组 `pickTheme` 的断言全部只传一个参数，钉的是**偏好缺省**时的行为；
 * 缺省档是「系统」，所以那一组的意图没有变：它们问的仍然是「这台机器该用什么」。
 */
describe("pickTheme（设计语言偏好）", () => {
  it("默认是「系统」——不传偏好与显式传「系统」结果相同", () => {
    expect(DEFAULT_DESIGN).toBe("system");
    expect(pickTheme({ osId: "windows" })).toBe(pickTheme({ osId: "windows" }, "system"));
    expect(pickTheme({ osId: "ubuntu" })).toBe(pickTheme({ osId: "ubuntu" }, "system"));
  });

  it("选「系统」时跟随机器：Windows 拿 Fluent，其余通用", () => {
    expect(pickTheme({ osId: "windows" }, "system")).toBe(FLUENT_THEME);
    for (const d of DISTROS.filter((x) => x.id !== "windows")) {
      expect(pickTheme({ osId: d.id }, "system"), d.id).toBe(GENERIC_THEME);
    }
  });

  // 「强制某一套」对**每一套**已注册语言都成立，不是只对 StrixMaid 那套。
  // 遍历 THEMES 而不是列举，加一套语言时这条自动覆盖它。
  it("指名某一套时不看机器：任何机器都拿到指名的那一套", () => {
    for (const t of THEMES) {
      for (const osId of ["windows", "ubuntu", "macos", "haiku", "", null]) {
        expect(pickTheme({ osId }, t.id), `${t.id} ← ${osId}`).toBe(t);
      }
    }
  });

  // 偏好排在机器前面判。将来 THEME_BY_DESKTOP 填上 Adwaita / Breeze 之后，
  // 桌面环境同样不得绕过这一档——这条现在就钉住，那时不必回头补。
  it("指名某一套时连桌面环境这一档也不看", () => {
    for (const t of THEMES) {
      expect(pickTheme({ osId: "ubuntu", desktop: "kde" }, t.id), t.id).toBe(t);
      expect(pickTheme({ osId: "windows", desktop: "gnome" }, t.id), t.id).toBe(t);
    }
  });

  // 类型上挡得住的事情运行时也要挡：存储里可能留着旧 id，或者某套语言被删掉了。
  it("指名一套不存在的语言时回落到「系统」，不抛错也不给空白", () => {
    const stale = "breeze" as DesignPreference;
    expect(pickTheme({ osId: "windows" }, stale)).toBe(FLUENT_THEME);
    expect(pickTheme({ osId: "ubuntu" }, stale)).toBe(GENERIC_THEME);
  });
});

/**
 * 设置里的那份清单。
 *
 * 这一组才是「加一套设计语言不用回头改 UI」的保证：清单由注册表生成，
 * 且每一档都真的切得过去。列一个选了没反应的选项等于在界面上编能力。
 */
describe("DESIGN_OPTIONS（设置里的清单）", () => {
  it("就是「系统」+ 注册表里的每一套，顺序一致", () => {
    expect(DESIGN_OPTIONS.map((o) => o.value)).toEqual(["system", ...THEMES.map((t) => t.id)]);
  });

  it("显示的名字取自每套语言自己的 name，不在界面里另写一份", () => {
    expect(DESIGN_OPTIONS.map((o) => o.label)).toEqual(["系统", ...THEMES.map((t) => t.name)]);
  });

  it("每一档都真的切得过去——没有选了没反应、或静默回落的选项", () => {
    for (const o of DESIGN_OPTIONS) {
      if (o.value === "system") continue;
      expect(
        THEMES.some((t) => t.id === o.value),
        o.value,
      ).toBe(true);
      // 连的是哪台机器都不影响：指名了就一定拿到那一套
      expect(pickTheme({ osId: "windows" }, o.value).id, o.value).toBe(o.value);
      expect(pickTheme({}, o.value).id, o.value).toBe(o.value);
    }
  });

  it("「系统」排在第一档，且就是默认档", () => {
    expect(DESIGN_OPTIONS[0]?.value).toBe("system");
    expect(DEFAULT_DESIGN).toBe("system");
  });
});

describe("asDesign（偏好的回落）", () => {
  it("控件上的每一档都原样返回", () => {
    for (const o of DESIGN_OPTIONS) expect(asDesign(o.value)).toBe(o.value);
  });

  // 存储里只可能有本项目自己写进去的值，所以是精确匹配，不做大小写归一：
  // 对不上就说明那是手改的、或旧版本留下的，回落比猜用户想要什么稳妥。
  it("读不到、空值、大小写不对、指向不存在的语言时一律回落到「系统」", () => {
    for (const v of [null, undefined, "", "Generic", "FLUENT", "strixmaid", "breeze", 0, {}]) {
      expect(asDesign(v), String(v)).toBe("system");
    }
  });
});

describe("发行版 accent", () => {
  it("每个发行版都有亮暗两档，格式为 #RRGGBB", () => {
    for (const d of [...DISTROS, UNKNOWN_DISTRO]) {
      expect(d.accent.light).toMatch(/^#[0-9A-F]{6}$/i);
      expect(d.accent.dark).toMatch(/^#[0-9A-F]{6}$/i);
    }
  });

  it("认不出的 accent 是纯灰（RGB 三通道相等）——不假装认识这台机器", () => {
    for (const v of [UNKNOWN_DISTRO.accent.light, UNKNOWN_DISTRO.accent.dark]) {
      const [r, g, b] = [v.slice(1, 3), v.slice(3, 5), v.slice(5, 7)];
      expect(r).toBe(g);
      expect(g).toBe(b);
    }
  });

  it("认不出的 accent 与同主题的面板灰明度错开（否则它就消失了）", () => {
    // 认不出 ⇒ 通用主题：亮面板 L93 → accent 必须显著更深；暗面板 L26.4 → 必须显著更浅
    expect(Number.parseInt(UNKNOWN_DISTRO.accent.light.slice(1, 3), 16)).toBeLessThan(0x80);
    expect(Number.parseInt(UNKNOWN_DISTRO.accent.dark.slice(1, 3), 16)).toBeGreaterThan(0x80);
  });

  // 上面几条只看格式，看不出一个凭感觉填的颜色是否真的达标——这条才是执行者。
  //
  // **对每一套设计语言的面板都验，不是只验「它实际会拿到的那一套」。**
  // 设计语言现在是用户选的（`DesignPreference`），而 accent 是机器身份、不跟着选，
  // 于是同一个 accent 可能落在通用面板（oklch 93% / 26.4%）上，
  // 也可能落在 Fluent 面板（#F5F5F5 / #292929）上：Windows 机器选「系统」是后者，
  // 选「StrixMaid」是前者。只验其中一套，另一套就成了没人看过的组合。
  // 将来加设计语言时这条自动覆盖新面板（遍历的是 `THEMES`），不必回头补。
  //
  // **门槛是 3:1 而不是 4.5:1**，因为 `--accent` 在本项目里一次都没用在文字上：
  // 4px 上边框（Rail）、3px 内阴影边条（NavRail / Table）、边框色（Field /
  // Toolbar）、焦点轮廓（base.css），以及进度条填充（States）——上面都没有字。
  // 适用的是 WCAG 1.4.11 非文字对比度 3:1，不是正文的 4.5:1。
  //
  // **这条依赖是活的**：哪天 accent 被用到 `color:` 上（链接、标签、图例），
  // 门槛必须退回 4.5:1，届时 Windows 的两档 Fluent 原色需要重选
  // （见 distro.ts 里那条的注释）。改这个数字之前先 grep 一遍 `var(--accent`。
  it(`每个 accent 对每一套设计语言的面板都 ≥${MIN_RATIO}:1`, () => {
    for (const d of [...DISTROS, UNKNOWN_DISTRO]) {
      for (const theme of THEMES) {
        for (const mode of MODES) {
          const panel = relLum(srgbOf(theme.tokens[mode].surface));
          const ratio = contrast(relLum(hexToSrgb(d.accent[mode])), panel);
          expect(
            ratio,
            `${d.id} 的 ${mode} accent ${d.accent[mode]} 对 ${theme.id} 的面板只有 ${ratio.toFixed(2)}:1`,
          ).toBeGreaterThanOrEqual(MIN_RATIO);
        }
      }
    }
  });
});

/** token 值 → sRGB 三分量。两种写法都要认：Fluent 是十六进制，通用主题是 OKLCH 纯灰。 */
function srgbOf(value: string): [number, number, number] {
  if (value.startsWith("#")) return hexToSrgb(value);
  const m = /^oklch\((\d+(?:\.\d+)?)% 0 0\)$/.exec(value);
  if (!m) throw new Error(`无法解析的 token 值：${value}`);
  return oklchGrey(Number.parseFloat(m[1]!));
}

/** sRGB 十六进制 → 0..1 三分量。 */
function hexToSrgb(hex: string): [number, number, number] {
  const h = hex.replace("#", "");
  return [0, 2, 4].map((i) => Number.parseInt(h.slice(i, i + 2), 16) / 255) as [
    number,
    number,
    number,
  ];
}

/** OKLCH 的中性灰（C=0）→ sRGB 三分量。C=0 时三通道相等，只需解一次传递函数。 */
function oklchGrey(lPercent: number): [number, number, number] {
  // L' = a' = b' = L（C=0），立方还原线性分量后三通道同值。
  const lin = (lPercent / 100) ** 3;
  const c = lin <= 0.0031308 ? 12.92 * lin : 1.055 * lin ** (1 / 2.4) - 0.055;
  return [c, c, c];
}

/** WCAG 相对亮度。 */
function relLum([r, g, b]: [number, number, number]): number {
  const f = (c: number) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  return 0.2126 * f(r) + 0.7152 * f(g) + 0.0722 * f(b);
}

/** WCAG 对比度。 */
function contrast(a: number, b: number): number {
  const [hi, lo] = a > b ? [a, b] : [b, a];
  return (hi + 0.05) / (lo + 0.05);
}
