import { describe, expect, it } from "vitest";
import { CHROMA, chromaFor, RAMP } from "./tokens";

describe("chromaFor", () => {
  it("认不出发行版时每个 token 都是纯灰", () => {
    // 这条曾经漏过：sel 的彩度下限把颜色带回了本该纯灰的界面
    for (const token of Object.keys(RAMP.dark)) {
      expect(chromaFor(token, 0)).toBe(0);
    }
  });

  it("文字端彩度低于面板端——高彩度的深色文字会显脏", () => {
    const base = CHROMA.dark;
    expect(chromaFor("ink", base)).toBeLessThan(chromaFor("surface", base));
    expect(chromaFor("ink-2", base)).toBeLessThan(base);
  });

  it("选中底的彩度高于面板，否则在低彩度下看不出来", () => {
    const base = CHROMA.dark;
    expect(chromaFor("sel", base)).toBeGreaterThan(base);
  });

  it("彩度停留在实测的安全区间内（C ≤ 0.008 六个发行版全部分得开）", () => {
    expect(CHROMA.dark).toBeLessThanOrEqual(0.008);
    expect(CHROMA.light).toBeLessThanOrEqual(0.008);
    // 亮底对低彩度更敏感，取值必须比暗色更低
    expect(CHROMA.light).toBeLessThan(CHROMA.dark);
  });
});

describe("RAMP", () => {
  it("正文钳在 L92 / L24，纯白正文在暗底上会产生光晕", () => {
    expect(RAMP.dark.ink).toBe(92);
    expect(RAMP.light.ink).toBe(24);
  });

  it("亮色面板不得接近纯白——面板是屏幕上面积最大的东西，L99.5 会刺眼", () => {
    expect(RAMP.light.surface!).toBeLessThanOrEqual(94);
  });

  it("亮色四级底的明度阶梯严格递减：面板 > 偶行 > 划过 > 选中", () => {
    const r = RAMP.light;
    expect(r.surface!).toBeGreaterThan(r["surface-2"]!);
    expect(r["surface-2"]!).toBeGreaterThan(r["surface-3"]!);
    expect(r["surface-3"]!).toBeGreaterThan(r.sel!);
  });

  it("页底比面板暗，面板才浮得起来", () => {
    expect(RAMP.light.ground!).toBeLessThan(RAMP.light.surface!);
    expect(RAMP.dark.ground!).toBeLessThan(RAMP.dark.surface!);
  });

  it("暗色四级底的明度阶梯严格递增：奇行 < 偶行 < 划过 < 选中", () => {
    const r = RAMP.dark;
    expect(r.surface!).toBeLessThan(r["surface-2"]!);
    expect(r["surface-2"]!).toBeLessThan(r["surface-3"]!);
    expect(r["surface-3"]!).toBeLessThan(r.sel!);
  });

  it("暗色底不低于 22%——再暗下去双层阴影就没有落脚点了", () => {
    expect(RAMP.dark.ground!).toBeGreaterThanOrEqual(22);
  });
});
