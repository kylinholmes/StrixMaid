import type { AlignedData } from "uplot";
import type { PlotBand, PlotSeries } from "@/components/Plot";
import { withAlpha } from "@/components/Plot";
import type { BandSeries } from "@/metrics/history";
import type { Ring } from "@/metrics/live";

/** 60 秒实时视图：单序列原始点，实线 + 面积填充（08 §8.2 表首行）。 */
export function liveSingle(ring: Ring | undefined, windowSecs = 60) {
  const now = Math.floor(Date.now() / 1000);
  const xs: number[] = [];
  const vs: number[] = [];
  if (ring) {
    for (let i = 0; i < ring.ts.length; i++) {
      const t = ring.ts[i];
      const v = ring.v[i];
      if (t !== undefined && v !== undefined && t >= now - windowSecs) {
        xs.push(t);
        vs.push(v);
      }
    }
  }
  return [xs, vs] as AlignedData;
}

/** 组页聚合方式（08 §6.2）：速率求和；饱和度取组内最大。 */
export type Agg = "sum" | "max";

/** 60 秒实时视图：多序列按 ts 聚合成一条。 */
export function liveAgg(rings: readonly (Ring | undefined)[], agg: Agg, windowSecs = 60) {
  const now = Math.floor(Date.now() / 1000);
  const byTs = new Map<number, number>();
  for (const r of rings) {
    if (!r) continue;
    for (let i = 0; i < r.ts.length; i++) {
      const t = r.ts[i];
      const v = r.v[i];
      if (t !== undefined && v !== undefined && t >= now - windowSecs) {
        const cur = byTs.get(t);
        byTs.set(t, cur === undefined ? v : agg === "sum" ? cur + v : Math.max(cur, v));
      }
    }
  }
  const xs = [...byTs.keys()].sort((a, b) => a - b);
  const vs = xs.map((t) => byTs.get(t) ?? 0);
  return [xs, vs] as AlignedData;
}

export function liveSeries(hue: string): PlotSeries[] {
  return [{ stroke: hue, width: 2, fill: withAlpha(hue, 0.18) }];
}

/** band 视图数据：[xs, max, min, avg, med]（08 §8.2）。 */
export function bandData(bs: BandSeries): AlignedData {
  return [bs.xs, bs.max, bs.min, bs.avg, bs.med] as AlignedData;
}

export function bandSeries(hue: string): PlotSeries[] {
  return [
    { stroke: "transparent", width: 0 },
    { stroke: "transparent", width: 0 },
    { stroke: hue, width: 2 },
    { stroke: hue, width: 1.5, dash: [4, 3] },
  ];
}

export function bandFill(hue: string): PlotBand[] {
  return [{ series: [1, 2], fill: withAlpha(hue, 0.22) }];
}

/** 多条 BandSeries 的 avg 按桶聚合（组页聚合历史：只画聚合 avg，不合成假 band）。 */
export function aggAvg(list: readonly BandSeries[], agg: Agg): AlignedData {
  const byTs = new Map<number, number>();
  for (const bs of list) {
    for (let i = 0; i < bs.xs.length; i++) {
      const t = bs.xs[i];
      const v = bs.avg[i];
      if (t !== undefined && typeof v === "number") {
        const cur = byTs.get(t);
        byTs.set(t, cur === undefined ? v : agg === "sum" ? cur + v : Math.max(cur, v));
      }
    }
  }
  const xs = [...byTs.keys()].sort((a, b) => a - b);
  return [xs, xs.map((t) => byTs.get(t) ?? 0)] as AlignedData;
}
