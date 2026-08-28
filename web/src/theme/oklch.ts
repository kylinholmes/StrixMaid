/**
 * OKLCH 色彩工具。
 *
 * 中性色由发行版主色派生（spec §2.2）：只取主色的**色相**，彩度与明度全部丢弃，
 * 再按固定的明度阶铺开。所以这里唯一需要从十六进制反推的量就是色相。
 */

function srgbToLinear(c: number): number {
  const x = c / 255;
  return x <= 0.04045 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4;
}

/** 取十六进制颜色在 OKLCH 里的色相角（0–360）。 */
export function hueOf(hex: string): number {
  const h = hex.replace("#", "");
  const r = srgbToLinear(Number.parseInt(h.slice(0, 2), 16));
  const g = srgbToLinear(Number.parseInt(h.slice(2, 4), 16));
  const b = srgbToLinear(Number.parseInt(h.slice(4, 6), 16));

  const l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
  const m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
  const s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;

  const l_ = Math.cbrt(l);
  const m_ = Math.cbrt(m);
  const s_ = Math.cbrt(s);

  const a = 1.9779984951 * l_ - 2.428592205 * m_ + 0.4505937099 * s_;
  const bb = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.808675766 * s_;

  return ((Math.atan2(bb, a) * 180) / Math.PI + 360) % 360;
}

/** 拼一个 CSS `oklch()` 字符串。 */
export function oklch(lightness: number, chroma: number, hue: number): string {
  return `oklch(${lightness.toFixed(1)}% ${chroma.toFixed(4)} ${hue.toFixed(1)})`;
}
