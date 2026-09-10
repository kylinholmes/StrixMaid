import { describe, expect, it } from "vitest";
import { DISTROS, UNKNOWN_DISTRO } from "./distro";
import { RAMP } from "./tokens";

describe("RAMP（纯灰明度阶）", () => {
  it("正文钳在 L92 / L24，纯白正文在暗底上会产生光晕", () => {
    expect(RAMP.dark.ink).toBe(92);
    expect(RAMP.light.ink).toBe(24);
  });

  it("亮色面板不得接近纯白——面板是屏幕上面积最大的东西，L99.5 会刺眼", () => {
    expect(RAMP.light.surface!).toBeLessThanOrEqual(94);
  });

  it("暗色四级底的明度阶梯严格递增：奇行 < 偶行 < 划过 < 选中", () => {
    const r = RAMP.dark;
    expect(r.surface!).toBeLessThan(r["surface-2"]!);
    expect(r["surface-2"]!).toBeLessThan(r["surface-3"]!);
    expect(r["surface-3"]!).toBeLessThan(r.sel!);
  });

  it("亮色四级底的明度阶梯严格递减：面板 > 偶行 > 划过 > 选中", () => {
    const r = RAMP.light;
    expect(r.surface!).toBeGreaterThan(r["surface-2"]!);
    expect(r["surface-2"]!).toBeGreaterThan(r["surface-3"]!);
    expect(r["surface-3"]!).toBeGreaterThan(r.sel!);
  });

  it("页底比面板暗，面板才浮得起来；暗色底不低于 22%（双层阴影的落脚点）", () => {
    expect(RAMP.light.ground!).toBeLessThan(RAMP.light.surface!);
    expect(RAMP.dark.ground!).toBeLessThan(RAMP.dark.surface!);
    expect(RAMP.dark.ground!).toBeGreaterThanOrEqual(22);
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
    // 亮面板 L93 → accent 必须显著更深；暗面板 L26.4 → accent 必须显著更浅
    expect(Number.parseInt(UNKNOWN_DISTRO.accent.light.slice(1, 3), 16)).toBeLessThan(0x80);
    expect(Number.parseInt(UNKNOWN_DISTRO.accent.dark.slice(1, 3), 16)).toBeGreaterThan(0x80);
  });
});
