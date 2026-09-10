import { useQuery } from "@tanstack/react-query";
import { api } from "@/api/client";
import type { components } from "@/api/schema";
import { useSession } from "@/session/useSession";

type MetricQueryResp = components["schemas"]["MetricQueryResp"];

/** 时间范围五档（roadmap/08 §8.2 的自动选层表）。 */
export type RangeKey = "60s" | "1h" | "24h" | "7d" | "30d";

export const RANGES: readonly { key: RangeKey; label: string; secs: number; step: number }[] = [
  { key: "60s", label: "60 秒", secs: 60, step: 2 },
  { key: "1h", label: "1 时", secs: 3_600, step: 30 },
  { key: "24h", label: "24 时", secs: 86_400, step: 600 },
  { key: "7d", label: "7 天", secs: 604_800, step: 3_600 },
  { key: "30d", label: "30 天", secs: 2_592_000, step: 43_200 },
];

export function rangeOf(key: RangeKey) {
  const found = RANGES.find((r) => r.key === key);
  if (!found) throw new Error(`未知的时间范围: ${key}`);
  return found;
}

/** 一条序列整理后的 band 数据。 */
export interface BandSeries {
  xs: number[];
  min: (number | null)[];
  max: (number | null)[];
  avg: (number | null)[];
  med: (number | null)[];
}

export interface HistoryResult {
  layer: string;
  /** 序列表达式（请求时的写法）→ 数据。查无此序列的键不存在。 */
  bySeries: ReadonlyMap<string, BandSeries>;
}

/** 目标渲染桶数：原始点多于它就客户端再分桶（live 层查 1 小时会有 1800 点）。 */
const TARGET_BUCKETS = 160;

function rebucket(src: BandSeries, buckets: number): BandSeries {
  const n = src.xs.length;
  if (n <= buckets) return src;
  const out: BandSeries = { xs: [], min: [], max: [], avg: [], med: [] };
  const first = src.xs[0];
  const last = src.xs[n - 1];
  if (first === undefined || last === undefined) return src;
  const span = Math.max(1, last - first);
  const width = span / buckets;
  let i = 0;
  for (let b = 0; b < buckets; b++) {
    const end = first + (b + 1) * width;
    let mn = Number.POSITIVE_INFINITY;
    let mx = Number.NEGATIVE_INFINITY;
    let sum = 0;
    let cnt = 0;
    const meds: number[] = [];
    while (i < n) {
      const x = src.xs[i];
      if (x === undefined || (x > end && b < buckets - 1)) break;
      const lo = src.min[i];
      const hi = src.max[i];
      const av = src.avg[i];
      const md = src.med[i];
      if (typeof lo === "number" && lo < mn) mn = lo;
      if (typeof hi === "number" && hi > mx) mx = hi;
      if (typeof av === "number") {
        sum += av;
        cnt++;
      }
      if (typeof md === "number") meds.push(md);
      i++;
    }
    if (cnt === 0) {
      // 洞就是洞：不零填充（08 §「有洞就是真的没数据」）
      out.xs.push(first + (b + 0.5) * width);
      out.min.push(null);
      out.max.push(null);
      out.avg.push(null);
      out.med.push(null);
    } else {
      meds.sort((a, z) => a - z);
      out.xs.push(first + (b + 0.5) * width);
      out.min.push(mn);
      out.max.push(mx);
      out.avg.push(sum / cnt);
      out.med.push(meds[meds.length >> 1] ?? null);
    }
  }
  return out;
}

/**
 * 查一组序列的历史。`exprs` 为 `metric` 或 `metric{k=v}` 形式。
 * 返回实际选中的层名（必须展示——用户需要知道自己看的是什么粒度）。
 */
export function useHistory(exprs: readonly string[], range: RangeKey, enabled: boolean) {
  const open = useSession((st) => st.status) === "open";
  const r = rangeOf(range);
  return useQuery({
    queryKey: ["metrics", "history", exprs.join(","), range],
    queryFn: async (): Promise<HistoryResult> => {
      const now = Math.floor(Date.now() / 1000);
      const { data, error } = await api.GET("/api/v1/metrics/query", {
        params: {
          query: { series: exprs.join(","), from: now - r.secs, to: now, step: r.step },
        },
      });
      if (error) throw error;
      const resp: MetricQueryResp = data;
      const by = new Map<string, BandSeries>();
      for (const sr of resp.series ?? []) {
        const key = sr.meta.labels ? `${sr.meta.metric}{${sr.meta.labels}}` : sr.meta.metric;
        const bs: BandSeries = { xs: [], min: [], max: [], avg: [], med: [] };
        for (const p of sr.points ?? []) {
          bs.xs.push(p.ts);
          bs.min.push(p.min);
          bs.max.push(p.max);
          bs.avg.push(p.cnt > 0 ? p.sum / p.cnt : null);
          bs.med.push(p.med);
        }
        by.set(key, rebucket(bs, TARGET_BUCKETS));
      }
      return { layer: resp.layer, bySeries: by };
    },
    enabled: enabled && open && exprs.length > 0,
    refetchInterval: range === "60s" ? undefined : 60_000,
    placeholderData: (prev) => prev,
  });
}
