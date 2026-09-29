import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { api } from "@/api/client";
import { cssVar, Plot, type PlotSeries, withAlpha } from "@/components/Plot";
import { cx } from "@/lib/cx";
import { fmtPct } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { type RangeKey, rangeOf, useHistory } from "@/metrics/history";
import { type Ring, seriesKey, useLive } from "@/metrics/live";
import { useSession } from "@/session/useSession";
import { useTheme } from "@/theme/useTheme";
import {
  type Agg,
  aggAvg,
  bandData,
  bandFill,
  bandSeries,
  liveAgg,
  liveSeries,
  liveSingle,
} from "../chart";
import s from "../Perf.module.css";

/*
 * 图表布局遵循任务管理器的经验：同量纲合图，异量纲分层。
 * 网络的收/发、磁盘的读/写是同一张图里的两条线（一粗一细 + 图例），
 * 不为每个指标单开整高图；errors 这类几乎恒零的计数进数字区，不占图。
 *
 * 高度只有副图这一个定值。每个资源的**首图**不写死高度：它是 `.stack` 里唯一
 * 会长的弹性项，吃掉一屏里其余区块用剩的全部竖向空间（任务管理器的主图也是
 * 这样占据主要高度的）。视口高矮、有没有副图、数字区几行，都会改变这个值，
 * 换成另一个写死的数字只会在别的窗口尺寸上重新留白或者被截断。
 */
export const H_SUB = 110;

export interface SectionProps {
  range: RangeKey;
  onLayer: (layer: string | null) => void;
}

/**
 * 图表区的两种视图：总体一张大图 ⇄ 每个成员一张小图。
 * 只有成员数 ≥ 2 的资源给这个切换（见 `PerfPage` 的 `VIEW_SPLIT`）。
 */
export type PerfView = "all" | "each";

/* ================= 通用小件 ================= */

export interface LegendEntry {
  color: string;
  name: string;
  dash?: boolean;
}

export function Legend({ entries }: { entries: readonly LegendEntry[] }) {
  return (
    <div className={s.legend}>
      {entries.map((e) => (
        <span key={e.name}>
          <i
            className={e.dash ? s.legendDash : undefined}
            style={e.dash ? { borderTopColor: e.color } : { background: e.color }}
          />
          {e.name}
        </span>
      ))}
    </div>
  );
}

export interface NumItem {
  k: string;
  v: string;
  sub?: string;
}

export function Numbers({ items, fact }: { items: readonly NumItem[]; fact?: boolean }) {
  return (
    <div className={cx(s.numbers, fact && s.numbersFact)}>
      {items.map((it) => (
        <div key={it.k}>
          <div className={s.k}>{it.k}</div>
          <div className={s.v}>
            {it.v}
            {it.sub && <small> {it.sub}</small>}
          </div>
        </div>
      ))}
    </div>
  );
}

/**
 * 「数字 | 静态事实」两栏(08 §6.1 骨架的文本层,任务管理器同款):
 * 左边是动态读数(大号等宽),右边是这台机器的静态事实(小一号)。
 */
export function StatFact({
  stats,
  facts,
}: {
  stats: readonly NumItem[];
  facts: readonly NumItem[];
}) {
  if (stats.length === 0 && facts.length === 0) return null;
  if (stats.length === 0) return <Numbers items={facts} fact />;
  if (facts.length === 0) return <Numbers items={stats} />;
  return (
    <div className={s.grids}>
      <Numbers items={stats} />
      <Numbers items={facts} fact />
    </div>
  );
}

export function useSystemInfo() {
  const open = useSession((st) => st.status) === "open";
  return useQuery({
    queryKey: ["system", "info"],
    queryFn: async () => {
      const { data, error } = await api.GET("/api/v1/system/info");
      if (error) throw error;
      return data;
    },
    staleTime: 30_000,
    enabled: open,
  });
}

/* ================= 图 ================= */

/**
 * 图表外框：标题行 + 绘图区 + 图例行。绘图区的像素高度有两种来源——
 * `height` 是调用方给定的定值（副图），`grow` 是先由弹性布局撑开、再量出来的值
 * （每个资源的首图）。uPlot 与 canvas 都只认确定的像素数，撑开的那一种必须先量
 * 后画，量不到（首帧、隐藏在别的路由里）就先不画，绝不拿一个猜的高度顶上。
 *
 * 标题行与图例行都可以不要：一条折线需要说明它画的是哪个指标、区间带是什么，
 * 而逐核 / 逐设备网格每一格自己就写着名字与读数，再加一行标题一行图例是白说。
 * 省掉它们不影响「切换前后区块高度不变」——那条约束落在 `.chartGrow` 的
 * `flex: 1 0 0` + `min-height` 上，与框里装了什么无关。
 */
export function ChartFrame({
  title,
  value,
  grow,
  height,
  legend,
  noFrame,
  children,
}: {
  /** 不给就不渲染标题行 */
  title?: string;
  value?: string;
  grow?: boolean;
  height?: number;
  legend?: readonly LegendEntry[];
  /** 去掉外框的边框与填充色（里面自带格子时用），见 `.chartNoFrame` */
  noFrame?: boolean;
  children: (height: number) => React.ReactNode;
}) {
  const areaRef = useRef<HTMLDivElement>(null);
  const [measured, setMeasured] = useState(0);

  useEffect(() => {
    const el = areaRef.current;
    if (!grow || !el) return;
    const ro = new ResizeObserver(() => setMeasured(el.clientHeight));
    ro.observe(el);
    setMeasured(el.clientHeight);
    return () => ro.disconnect();
  }, [grow]);

  const h = grow ? measured : (height ?? 0);
  return (
    <div className={cx(s.chartBlock, grow && s.chartGrow, noFrame && s.chartNoFrame)}>
      {title !== undefined && (
        <div className={s.chartTitle}>
          <b>{title}</b>
          <span>{value}</span>
        </div>
      )}
      <div ref={areaRef} className={grow ? s.plotArea : undefined}>
        {h > 0 && children(h)}
      </div>
      {legend && legend.length > 0 && <Legend entries={legend} />}
    </div>
  );
}

/** 单序列图（可挂第二条同量纲序列）：60 秒走 live 环，其余档走历史 band。 */
export function SeriesChart({
  expr,
  metric,
  labels,
  title,
  tone,
  yMax,
  height,
  range,
  onLayer,
  unitFmt,
  secondary,
  grow,
}: {
  expr: string;
  metric: string;
  labels: string;
  title: string;
  tone: string;
  yMax?: number;
  /** 定值高度（副图）。与 `grow` 二选一 */
  height?: number;
  range: RangeKey;
  onLayer: (layer: string | null) => void;
  unitFmt: (v: number) => string;
  secondary?: { expr: string; metric: string; labels: string; name: string };
  /** 首图：高度由弹性布局给，吃掉页面剩余的竖向空间 */
  grow?: boolean;
}) {
  const rings = useLive((st) => st.rings);
  const mode = useTheme((t) => t.mode);
  const hue = cssVar(tone);
  const hue2 = withAlpha(hue, 0.55);
  const live = range === "60s";
  const exprs = secondary ? [expr, secondary.expr] : [expr];
  const hist = useHistory(exprs, range, !live);

  useEffect(() => {
    onLayer(live ? null : (hist.data?.layer ?? null));
  }, [live, hist.data?.layer, onLayer]);

  const ring = rings.get(seriesKey(metric, labels));
  const ring2 = secondary ? rings.get(seriesKey(secondary.metric, secondary.labels)) : undefined;
  const bs = hist.data?.bySeries.get(expr);
  const bs2 = secondary ? hist.data?.bySeries.get(secondary.expr) : undefined;
  const latest = ring?.v[ring.v.length - 1];

  const alignByTs = (xs: number[], src: Ring | undefined) => {
    const by = new Map<number, number>();
    if (src)
      for (let i = 0; i < src.ts.length; i++) {
        const t = src.ts[i];
        const v = src.v[i];
        if (t !== undefined && v !== undefined) by.set(t, v);
      }
    return xs.map((t) => by.get(t) ?? null);
  };

  let data: ReturnType<typeof liveSingle>;
  let plotSeries: PlotSeries[] = liveSeries(hue);
  let bands: ReturnType<typeof bandFill> | undefined;
  let legendEntries: LegendEntry[] = [];
  let tipNames: (string | null)[] = [metric];

  if (live || !bs) {
    data = liveSingle(ring);
    // 单线也给图例:所有图表统一带图例行,区块高度可预测(CPU 双视图逐像素对齐靠它)
    legendEntries = [{ color: hue, name: metric }];
    if (secondary) {
      const xs = data[0] as number[];
      const ys2 = alignByTs(xs, ring2);
      data = [xs, data[1], ys2] as typeof data;
      plotSeries = [...liveSeries(hue), { stroke: hue2, width: 1.25 }];
      legendEntries = [
        { color: hue, name: metric },
        { color: hue2, name: secondary.name },
      ];
      tipNames = [metric, secondary.name];
    }
  } else {
    data = bandData(bs);
    plotSeries = bandSeries(hue);
    bands = bandFill(hue);
    legendEntries = [
      { color: hue, name: "avg" },
      { color: hue, name: "med", dash: true },
      { color: withAlpha(hue, 0.34), name: "min–max 区间带" },
    ];
    tipNames = ["max", "min", "avg", "med"];
    if (secondary && bs2) {
      const by = new Map<number, number>();
      for (let i = 0; i < bs2.xs.length; i++) {
        const t = bs2.xs[i];
        const v = bs2.avg[i];
        if (t !== undefined && typeof v === "number") by.set(t, v);
      }
      const xs = data[0] as number[];
      const ys2 = xs.map((t) => by.get(t) ?? null);
      data = [...data, ys2] as typeof data;
      plotSeries = [...plotSeries, { stroke: hue2, width: 1.25 }];
      legendEntries.push({ color: hue2, name: `${secondary.name}（avg）` });
      tipNames.push(`${secondary.name}（avg）`);
    }
  }

  // 右上角 = 轴上限（08 §8.1）：固定上限直接标,自动缩放标当前的 max×1.15
  const axisMax = yMax ?? dataMax(data) * 1.15;

  return (
    <ChartFrame
      key={mode}
      title={title}
      value={latest !== undefined ? unitFmt(latest) : "—"}
      grow={grow}
      height={height}
      legend={legendEntries}
    >
      {(h) => (
        <Plot
          data={data}
          series={plotSeries}
          bands={bands}
          yMax={yMax}
          height={h}
          tone={tone}
          cornerTR={axisMax > 0 ? unitFmt(axisMax) : undefined}
          cornerBL={rangeOf(range).label}
          cornerBR="0"
          tip={{ names: tipNames, unitFmt }}
        />
      )}
    </ChartFrame>
  );
}

/** 数据里全部 y 值的最大者（不含 x 列）。 */
export function dataMax(data: ReturnType<typeof liveSingle>): number {
  let max = 0;
  for (let i = 1; i < data.length; i++) {
    const ys = data[i];
    if (!ys) continue;
    for (const v of ys) if (typeof v === "number" && v > max) max = v;
  }
  return max;
}

/**
 * 组页聚合图：每组按 §6.2 的语义聚合成一条线——速率求和,饱和度取组内最大。
 * 最多两组（读/写、收/发）。历史档只画各组 avg 的聚合。
 */
export function AggChart({
  groups,
  agg = "sum",
  title,
  tone,
  yMax,
  height,
  range,
  onLayer,
  unitFmt,
  grow,
}: {
  /** match:自定义 live 环匹配(多标签序列按前缀分不开组时用);缺省按 metrics 前缀 */
  groups: readonly {
    name: string;
    metrics: readonly string[];
    exprs: readonly string[];
    match?: (key: string) => boolean;
  }[];
  agg?: Agg;
  title: string;
  tone: string;
  yMax?: number;
  /** 定值高度（副图）。与 `grow` 二选一 */
  height?: number;
  range: RangeKey;
  onLayer: (layer: string | null) => void;
  unitFmt: (v: number) => string;
  /** 首图：高度由弹性布局给，吃掉页面剩余的竖向空间 */
  grow?: boolean;
}) {
  const rings = useLive((st) => st.rings);
  const mode = useTheme((t) => t.mode);
  const hue = cssVar(tone);
  const hue2 = withAlpha(hue, 0.55);
  const live = range === "60s";
  const allExprs = groups.flatMap((g) => [...g.exprs]);
  const hist = useHistory(allExprs, range, !live);

  useEffect(() => {
    onLayer(live ? null : (hist.data?.layer ?? null));
  }, [live, hist.data?.layer, onLayer]);

  const ringsOf = (g: (typeof groups)[number]): Ring[] => {
    const out: Ring[] = [];
    for (const [key, r] of rings) {
      const ok = g.match ? g.match(key) : g.metrics.some((m) => key.startsWith(`${m}|`));
      if (ok) out.push(r);
    }
    return out;
  };

  const perGroup = groups.map((g) => {
    if (live) {
      const d = liveAgg(ringsOf(g), agg);
      return { name: g.name, xs: d[0] as number[], vs: d[1] as (number | null)[] };
    }
    const list = [...(hist.data?.bySeries.entries() ?? [])]
      .filter(([k]) => g.exprs.includes(k))
      .map(([, v]) => v);
    const d = aggAvg(list, agg);
    return { name: g.name, xs: d[0] as number[], vs: d[1] as (number | null)[] };
  });

  const xsSet = new Set<number>();
  for (const g of perGroup) for (const t of g.xs) xsSet.add(t);
  const xs = [...xsSet].sort((a, b) => a - b);
  const ys = perGroup.map((g) => {
    const by = new Map<number, number | null>();
    g.xs.forEach((t, i) => {
      by.set(t, g.vs[i] ?? null);
    });
    return xs.map((t) => by.get(t) ?? null);
  });

  // 标题右侧的当前值:组内按 agg 合并,组间求和(读+写、收+发的「合计」语义)
  const latest = groups.reduce((acc, g) => {
    let v: number | null = null;
    for (const r of ringsOf(g)) {
      const last = r.v[r.v.length - 1];
      if (typeof last !== "number") continue;
      v = v === null ? last : agg === "max" ? Math.max(v, last) : v + last;
    }
    return acc + (v ?? 0);
  }, 0);
  // 单色相多线:主线全色,其余按透明度递减区分(spec §4 数据色不引入新色相)
  const colors = [hue, hue2, withAlpha(hue, 0.8), withAlpha(hue, 0.35)];
  const plotSeries: PlotSeries[] = perGroup.map((_, i) => ({
    stroke: colors[i] ?? hue,
    width: i === 0 ? 2 : 1.25,
    fill: i === 0 && live && perGroup.length === 1 ? withAlpha(hue, 0.18) : undefined,
  }));

  let dmax = 0;
  for (const col of ys) for (const v of col) if (typeof v === "number" && v > dmax) dmax = v;
  const axisMax = yMax ?? dmax * 1.15;

  return (
    <ChartFrame
      key={mode}
      title={title}
      value={unitFmt(latest)}
      grow={grow}
      height={height}
      legend={perGroup.map((g, i) => ({ color: colors[i] ?? hue, name: g.name }))}
    >
      {(h) => (
        <Plot
          data={[xs, ...ys] as Parameters<typeof Plot>[0]["data"]}
          series={plotSeries}
          yMax={yMax}
          height={h}
          tone={tone}
          cornerTR={axisMax > 0 ? unitFmt(axisMax) : undefined}
          cornerBL={rangeOf(range).label}
          cornerBR="0"
          tip={{ names: perGroup.map((g) => g.name), unitFmt }}
        />
      )}
    </ChartFrame>
  );
}

/**
 * PSI 压力小图:some 主线 + full 细线(有才画)。不定死 y 上限——
 * 压力通常是个位数,按 0–100 画就成一条贴地直线,看不出波动。
 */
export function PsiChart({
  kind,
  title,
  tone,
  range,
  onLayer,
}: SectionProps & { kind: "cpu" | "memory" | "io"; title: string; tone: string }) {
  const discovery = useDiscovery();
  const some = `psi.${kind}.some`;
  const full = `psi.${kind}.full`;
  return (
    <SeriesChart
      expr={some}
      metric={some}
      labels=""
      title={title}
      tone={tone}
      height={H_SUB}
      range={range}
      onLayer={onLayer}
      unitFmt={(v) => fmtPct(v / 100)}
      secondary={
        discovery.data?.has(full) ? { expr: full, metric: full, labels: "", name: full } : undefined
      }
    />
  );
}
