import { describe, expect, it } from "vitest";
import { fmtBytes, fmtPct, fmtRateBits, fmtUptime } from "./fmt";

describe("fmtBytes（1024 进制，spec §9）", () => {
  it("单位阶梯", () => {
    expect(fmtBytes(0)).toBe("0 B");
    expect(fmtBytes(1024)).toBe("1.0 KiB");
    expect(fmtBytes(9.7 * 1024 ** 3)).toBe("9.7 GiB");
    expect(fmtBytes(2 * 1024 ** 4)).toBe("2.0 TiB");
  });
  it("三位数不带小数", () => {
    expect(fmtBytes(500 * 1024 ** 2)).toBe("500 MiB");
  });
  it("非法输入给占位符", () => {
    expect(fmtBytes(Number.NaN)).toBe("—");
    expect(fmtBytes(-1)).toBe("—");
  });
});

describe("fmtRateBits（网络例外：1000 进制比特）", () => {
  it("字节每秒转比特每秒", () => {
    expect(fmtRateBits(125)).toBe("1.0 Kb/s"); // 125 B/s = 1000 b/s
    expect(fmtRateBits(125_000_000)).toBe("1.0 Gb/s");
  });
});

describe("fmtUptime", () => {
  it("按量级挑单位", () => {
    expect(fmtUptime(90)).toBe("1 分");
    expect(fmtUptime(3_700)).toBe("1 时 1 分");
    expect(fmtUptime(90_000)).toBe("1 天 1 时");
  });
});

describe("fmtPct", () => {
  it("分数转百分比", () => {
    expect(fmtPct(0.235)).toBe("24%");
    expect(fmtPct(Number.NaN)).toBe("—");
  });
});
