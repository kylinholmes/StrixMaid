import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "@/api/client";
import { cssVar, Plot, type PlotSeries, withAlpha } from "@/components/Plot";
import { cx } from "@/lib/cx";
import { fmtBytes, fmtPct, fmtRateBits, fmtUptime } from "@/lib/fmt";
import { labelValue, useDiscovery } from "@/metrics/discovery";
import { type RangeKey, rangeOf, useHistory } from "@/metrics/history";
import {
  latestMax,
  latestOf,
  latestSum,
  liveMembers,
  type Ring,
  seriesKey,
  useLive,
} from "@/metrics/live";
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
} from "./chart";
import { MountTable } from "./MountTable";
import type { ResourceDef } from "./model";
import s from "./Perf.module.css";

/*
 * 图表布局遵循任务管理器的经验：同量纲合图，异量纲分层。
 * 网络的收/发、磁盘的读/写是同一张图里的两条线（一粗一细 + 图例），
 * 不为每个指标单开整高图；errors 这类几乎恒零的计数进数字区，不占图。
 */
const H_MAIN = 180;
const H_SUB = 110;

interface SectionProps {
  range: RangeKey;
  onLayer: (layer: string | null) => void;
}

/* ================= 通用小件 ================= */

function Legend({
  entries,
}: {
  entries: readonly { color: string; name: string; dash?: boolean }[];
}) {
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

interface NumItem {
  k: string;
  v: string;
  sub?: string;
}

function Numbers({ items, fact }: { items: readonly NumItem[]; fact?: boolean }) {
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
function StatFact({ stats, facts }: { stats: readonly NumItem[]; facts: readonly NumItem[] }) {
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

/** 单序列图（可挂第二条同量纲序列）：60 秒走 live 环，其余档走历史 band。 */
function SeriesChart({
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
}: {
  expr: string;
  metric: string;
  labels: string;
  title: string;
  tone: string;
  yMax?: number;
  height: number;
  range: RangeKey;
  onLayer: (layer: string | null) => void;
  unitFmt: (v: number) => string;
  secondary?: { expr: string; metric: string; labels: string; name: string };
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
  let legendEntries: { color: string; name: string; dash?: boolean }[] = [];
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
    <div className={s.chartBlock} key={mode}>
      <div className={s.chartTitle}>
        <b>{title}</b>
        <span>{latest !== undefined ? unitFmt(latest) : "—"}</span>
      </div>
      <Plot
        data={data}
        series={plotSeries}
        bands={bands}
        yMax={yMax}
        height={height}
        tone={tone}
        cornerTR={axisMax > 0 ? unitFmt(axisMax) : undefined}
        cornerBL={rangeOf(range).label}
        cornerBR="0"
        tip={{ names: tipNames, unitFmt }}
      />
      {legendEntries.length > 0 && <Legend entries={legendEntries} />}
    </div>
  );
}

/** 数据里全部 y 值的最大者（不含 x 列）。 */
function dataMax(data: ReturnType<typeof liveSingle>): number {
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
function AggChart({
  groups,
  agg = "sum",
  title,
  tone,
  yMax,
  height,
  range,
  onLayer,
  unitFmt,
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
  height: number;
  range: RangeKey;
  onLayer: (layer: string | null) => void;
  unitFmt: (v: number) => string;
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
    <div className={s.chartBlock} key={mode}>
      <div className={s.chartTitle}>
        <b>{title}</b>
        <span>{unitFmt(latest)}</span>
      </div>
      <Plot
        data={[xs, ...ys] as Parameters<typeof Plot>[0]["data"]}
        series={plotSeries}
        yMax={yMax}
        height={height}
        tone={tone}
        cornerTR={axisMax > 0 ? unitFmt(axisMax) : undefined}
        cornerBL={rangeOf(range).label}
        cornerBR="0"
        tip={{ names: perGroup.map((g) => g.name), unitFmt }}
      />
      <Legend entries={perGroup.map((g, i) => ({ color: colors[i] ?? hue, name: g.name }))} />
    </div>
  );
}

/**
 * PSI 压力小图:some 主线 + full 细线(有才画)。不定死 y 上限——
 * 压力通常是个位数,按 0–100 画就成一条贴地直线,看不出波动。
 */
function PsiChart({
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

/* ================= CPU ================= */

export type CpuView = "all" | "cores";

/**
 * CPU 段。视图切换只换图表区（总体大图 ⇄ 逻辑处理器网格/热力图）,
 * 数字与静态事实不随之消失;切换器由页头渲染（靠近时间档）,状态在 PerfPage。
 */
export function CpuSection({
  range,
  rangeSecs,
  view,
  onLayer,
}: SectionProps & { rangeSecs: number; view: CpuView }) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();

  const cores = discovery.data?.members("cpu.core.usage", "core") ?? [];
  const ringOf = (core: string): Ring | undefined =>
    rings.get(seriesKey("cpu.core.usage", `core=${core}`));

  // 数字区:动态读数(任务管理器左栏)。探测不到的项整个不出现(spec §6)
  const stats: NumItem[] = [];
  for (const [k, label] of [
    ["cpu.usage", "利用率"],
    ["cpu.system", "内核态"],
    ["cpu.iowait", "IO 等待"],
    ["cpu.irq", "中断"],
    ["cpu.steal", "被偷走"],
    ["psi.cpu.some", "CPU 停滞"],
  ] as const) {
    const v = latestOf(rings, seriesKey(k, ""));
    if (v !== null) stats.push({ k: label, v: fmtPct(v / 100) });
  }
  const procs = latestOf(rings, seriesKey("procs.total", ""));
  if (procs !== null) stats.push({ k: "进程", v: String(Math.round(procs)) });
  const running = latestOf(rings, seriesKey("procs.running", ""));
  if (running !== null) stats.push({ k: "运行队列", v: String(Math.round(running)) });
  const load = latestOf(rings, seriesKey("load.1m", ""));
  if (load !== null) stats.push({ k: "负载 1 分钟", v: load.toFixed(2) });

  // 静态事实(任务管理器右栏)
  const facts: NumItem[] = [];
  if (info.data) {
    const c = info.data.cpu;
    facts.push({ k: "型号", v: c.model });
    if (c.mhz) facts.push({ k: "主频", v: `${(c.mhz / 1000).toFixed(2)} GHz` });
    facts.push({
      k: "插槽 / 物理核",
      v: `${(c.packages ?? []).length || 1} / ${c.physical_cores ?? "—"}`,
    });
    facts.push({ k: "逻辑处理器", v: String(c.logical_cores) });
    if (c.quota_cores) facts.push({ k: "配额", v: `${c.quota_cores} 核` });
    if (c.numa_nodes && c.numa_nodes > 1) facts.push({ k: "NUMA 节点", v: String(c.numa_nodes) });
    facts.push({ k: "架构", v: info.data.arch });
    facts.push({ k: "虚拟化", v: info.data.virtualization ?? "未检出" });
    facts.push({ k: "运行时间", v: fmtUptime(info.data.uptime_secs) });
  }

  return (
    <>
      {view === "all" || cores.length === 0 ? (
        <SeriesChart
          expr="cpu.usage"
          metric="cpu.usage"
          labels=""
          title="CPU · cpu.usage"
          tone="--cpu"
          yMax={100}
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />
      ) : (
        /* 与总体图同一副骨架(标题行 + H_MAIN 内容区),切换前后区块高度不变,
           下方数字|静态事实不跳。格子高度适应容器,装不下才转热力图(CoresView) */
        <div className={s.chartBlock}>
          <div className={s.chartTitle}>
            <b>CPU · cpu.core.usage</b>
            <span>{cores.length} 逻辑处理器</span>
          </div>
          <CoresView cores={cores} ringOf={ringOf} height={H_MAIN} rangeSecs={rangeSecs} />
        </div>
      )}

      {/* 副图行:主图之外的第二层信息。视图切换(总体⇄逐核)不影响这里 */}
      {(() => {
        const kernelParts = (
          [
            ["cpu.system", "内核态"],
            ["cpu.iowait", "IO 等待"],
            ["cpu.irq", "中断"],
            ["cpu.steal", "被偷走"],
          ] as const
        ).filter(([m]) => discovery.data?.has(m));
        const hasPsi = discovery.data?.has("psi.cpu.some") ?? false;
        if (kernelParts.length === 0 && !hasPsi) return null;
        return (
          <div className={s.subRow}>
            {kernelParts.length > 0 && (
              <AggChart
                groups={kernelParts.map(([m, name]) => ({ name, metrics: [m], exprs: [m] }))}
                title="CPU · 内核细分"
                tone="--cpu"
                height={H_SUB}
                range={range}
                onLayer={onLayer}
                unitFmt={(v) => fmtPct(v / 100)}
              />
            )}
            {hasPsi && (
              <PsiChart
                kind="cpu"
                title="CPU · psi.cpu（压力）"
                tone="--cpu"
                range={range}
                onLayer={onLayer}
              />
            )}
          </div>
        );
      })()}

      <StatFact stats={stats} facts={facts} />
    </>
  );
}

/* 逐核视图:格子尺寸是**写死的**(样稿密度),不随容器伸缩 */
const CORE_GAP = 4;
const CORE_CELL_W = 132; // 固定格宽
const CORE_PLOT_H = 54; // 固定小图高(样稿值)
const CORE_CELL_H = 74; // 标签 15 + 小图 54 + 边框内边距 5

/**
 * 逐核视图:容器高度固定(与总体图等高),**格子反过来适应容器**——
 * 行数由高度定(每行给 mockup 密度的一格),列数摊开全部核;
 * 摊出来的格子太窄(< CORE_MIN_W)说明这块面积装不下逐核小图,转热力图。
 * 「超多核」不再是写死的 32,是几何上装不装得下。
 */
function CoresView({
  cores,
  ringOf,
  height,
  rangeSecs,
}: {
  cores: readonly string[];
  ringOf: (core: string) => Ring | undefined;
  height: number;
  rangeSecs: number;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const [w, setW] = useState(0);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const ro = new ResizeObserver(() => setW(host.clientWidth));
    ro.observe(host);
    setW(host.clientWidth);
    return () => ro.disconnect();
  }, []);

  // 格子固定 132×74:一行放几个 = 容器宽度整除,行数 = 核数除上去;
  // 固定高度的容器里摆不下这些行 → 转热力图。剩余空间留白,不拉伸格子。
  let grid: { cols: number } | null = null;
  if (w > 0) {
    const cols = Math.max(
      1,
      Math.min(cores.length, Math.floor((w + CORE_GAP) / (CORE_CELL_W + CORE_GAP))),
    );
    const rows = Math.ceil(cores.length / cols);
    if (rows * (CORE_CELL_H + CORE_GAP) - CORE_GAP <= height) grid = { cols };
  }

  return (
    <>
      <div ref={hostRef} className={s.coreArea} style={{ height }}>
        {w === 0 ? null : grid ? (
          <div
            className={s.cores}
            style={{ gridTemplateColumns: `repeat(${grid.cols}, ${CORE_CELL_W}px)` }}
          >
            {cores.map((core) => {
              const ring = ringOf(core);
              const latest = ring?.v[ring.v.length - 1];
              return (
                <div key={core} className={s.core}>
                  <div className={s.coreLabel}>
                    <span>核 {core}</span>
                    <span>{typeof latest === "number" ? fmtPct(latest / 100) : "—"}</span>
                  </div>
                  <Plot
                    data={liveSingle(ring, Math.min(rangeSecs, 180))}
                    series={liveSeries(cssVar("--cpu"))}
                    yMax={100}
                    height={CORE_PLOT_H}
                    tone="--cpu"
                    noCursor
                  />
                </div>
              );
            })}
          </div>
        ) : (
          <CoreHeatmap cores={cores} ringOf={ringOf} maxHeight={height} />
        )}
      </div>
      {/* 与总体图的图例行同高,保证两种视图区块高度逐像素一致 */}
      <Legend
        entries={[
          {
            color: cssVar("--cpu"),
            name: grid ? "cpu.core.usage · 每核一图" : "cpu.core.usage · 色深 = 当前占用",
          },
        ]}
      />
    </>
  );
}

/** 逐核小图装不下时的形态（08 §6.6）：单张 canvas 热力图，单色相顺序色阶。 */
function CoreHeatmap({
  cores,
  ringOf,
  maxHeight,
}: {
  cores: readonly string[];
  ringOf: (core: string) => Ring | undefined;
  maxHeight: number;
}) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const tipRef = useRef<HTMLDivElement>(null);
  /** 最近一次绘制的几何,悬停时用它反算鼠标落在哪个核上 */
  const geomRef = useRef({ cols: 1, cellW: 1, cellH: 1 });
  const mode = useTheme((t) => t.mode);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const hostW = canvas.parentElement?.clientWidth ?? 600;
    const cols = [32, 16, 8].find((c) => hostW / c >= 22) ?? 8;
    const rows = Math.ceil(cores.length / cols);
    const cellW = Math.floor(hostW / cols);
    // 高度同样封顶在容器内:格高受宽、30px 上限、容器均分三者约束(下限 4px 保底可见)
    const cellH = Math.max(4, Math.min(cellW, 30, Math.floor(maxHeight / rows)));
    geomRef.current = { cols, cellW, cellH };
    const dpr = devicePixelRatio;
    canvas.width = hostW * dpr;
    canvas.height = rows * cellH * dpr;
    canvas.style.width = `${hostW}px`;
    canvas.style.height = `${rows * cellH}px`;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    ctx.scale(dpr, dpr);
    void mode; // 主题切换时资源色变化，需要重画
    const hue = cssVar("--cpu");
    ctx.clearRect(0, 0, hostW, rows * cellH);
    cores.forEach((core, i) => {
      const ring = ringOf(core);
      const v = ring?.v[ring.v.length - 1] ?? 0;
      const col = i % cols;
      const row = Math.floor(i / cols);
      ctx.fillStyle = withAlpha(hue, 0.1 + 0.9 * Math.min(1, v / 100));
      ctx.fillRect(col * cellW + 1, row * cellH + 1, cellW - 2, cellH - 2);
    });
  }, [cores, ringOf, mode, maxHeight]);

  // 悬停提示:鼠标坐标 → 格子 → 「核 N · 占用」,直接改 DOM,不为每次移动走 React
  const hideTip = () => {
    if (tipRef.current) tipRef.current.style.display = "none";
  };
  const moveTip = (e: React.MouseEvent<HTMLCanvasElement>) => {
    const el = tipRef.current;
    const canvas = canvasRef.current;
    if (!el || !canvas) return;
    const rect = canvas.getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    const { cols, cellW, cellH } = geomRef.current;
    const core = cores[Math.floor(y / cellH) * cols + Math.floor(x / cellW)];
    if (core === undefined) {
      hideTip();
      return;
    }
    const ring = ringOf(core);
    const v = ring?.v[ring.v.length - 1];
    el.textContent = `核 ${core} · ${typeof v === "number" ? fmtPct(v / 100) : "—"}`;
    el.style.display = "block";
    const w = el.offsetWidth;
    const hostW = canvas.parentElement?.clientWidth ?? rect.width;
    el.style.left = `${x + 12 + w > hostW - 4 ? Math.max(2, x - w - 12) : x + 12}px`;
    el.style.top = `${y + 14}px`;
  };

  return (
    <div className={s.heat}>
      <canvas ref={canvasRef} onMouseMove={moveTip} onMouseLeave={hideTip} />
      <div className={s.heatTip} ref={tipRef} />
    </div>
  );
}

/* ================= 内存 ================= */

/**
 * 内存组成条(Win10 任务管理器「内存组成」):使用中 | 缓存 | 空闲 分段横条,
 * 有交换分区时再加一根细条。空闲 = 总量 − 使用中 − 缓存,是条上的底色不画段。
 */
function MemComposition() {
  const rings = useLive((st) => st.rings);
  const mode = useTheme((t) => t.mode);
  void mode; // 主题切换时 cssVar 解析值变化,需要重渲染
  const total = latestOf(rings, seriesKey("mem.total", ""));
  const used = latestOf(rings, seriesKey("mem.used", ""));
  if (total === null || used === null || total <= 0) return null;
  const cached = latestOf(rings, seriesKey("mem.cached", ""));
  const swapTotal = latestOf(rings, seriesKey("mem.swap_total", ""));
  const swapUsed = latestOf(rings, seriesKey("mem.swap_used", ""));

  const hue = cssVar("--mem");
  const u = Math.min(used, total);
  const c = Math.max(0, Math.min(cached ?? 0, total - u));
  const free = total - u - c;
  const w = (v: number) => `${((v / total) * 100).toFixed(2)}%`;
  const swap =
    swapTotal !== null && swapTotal > 0 && swapUsed !== null ? { u: swapUsed, t: swapTotal } : null;

  const entries: { color: string; name: string }[] = [
    { color: hue, name: `使用中 ${fmtBytes(u)}` },
  ];
  if (cached !== null) entries.push({ color: withAlpha(hue, 0.45), name: `缓存 ${fmtBytes(c)}` });
  entries.push({ color: "var(--surface-3)", name: `空闲 ${fmtBytes(free)}` });
  if (swap)
    entries.push({
      color: withAlpha(hue, 0.75),
      name: `交换 ${fmtBytes(swap.u)} / ${fmtBytes(swap.t)}`,
    });

  return (
    <div className={s.chartBlock}>
      <div className={s.chartTitle}>
        <b>内存组成</b>
        <span>
          {fmtPct(u / total)} 已用 · 共 {fmtBytes(total)}
        </span>
      </div>
      <div className={s.compBar}>
        <i style={{ width: w(u), background: hue }} title={`使用中 ${fmtBytes(u)}`} />
        {c > 0 && (
          <i
            style={{ width: w(c), background: withAlpha(hue, 0.45) }}
            title={`缓存 ${fmtBytes(c)}`}
          />
        )}
      </div>
      {swap && (
        <div className={cx(s.compBar, s.compSwap)}>
          <i
            style={{
              width: `${((swap.u / swap.t) * 100).toFixed(2)}%`,
              background: withAlpha(hue, 0.75),
            }}
            title={`交换已用 ${fmtBytes(swap.u)}`}
          />
        </div>
      )}
      <Legend entries={entries} />
    </div>
  );
}

export function MemSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const total = latestSum(rings, "mem.total");

  const stats: NumItem[] = [];
  const used = latestOf(rings, seriesKey("mem.used", ""));
  if (used !== null)
    stats.push({
      k: "使用中",
      v: fmtBytes(used),
      sub: total ? `（${fmtPct(used / total)}）` : undefined,
    });
  for (const [metric, label] of [
    ["mem.available", "可用"],
    ["mem.cached", "缓存"],
    ["mem.swap_used", "交换已用"],
  ] as const) {
    const v = latestOf(rings, seriesKey(metric, ""));
    if (v !== null) stats.push({ k: label, v: fmtBytes(v) });
  }
  for (const [metric, label] of [
    ["psi.memory.some", "内存停滞"],
    ["psi.memory.full", "彻底停滞"],
  ] as const) {
    const v = latestOf(rings, seriesKey(metric, ""));
    if (v !== null) stats.push({ k: label, v: fmtPct(v / 100) });
  }

  const facts: NumItem[] = [];
  if (info.data) {
    facts.push({ k: "总容量", v: fmtBytes(info.data.memory.total_bytes) });
    facts.push({
      k: "交换分区",
      v: info.data.memory.swap_total_bytes ? fmtBytes(info.data.memory.swap_total_bytes) : "未启用",
    });
  }

  return (
    <>
      <SeriesChart
        expr="mem.used"
        metric="mem.used"
        labels=""
        title="内存 · mem.used"
        tone="--mem"
        yMax={total ?? undefined}
        height={H_MAIN}
        range={range}
        onLayer={onLayer}
        unitFmt={fmtBytes}
      />
      <MemComposition />
      {(() => {
        const swapTotal = latestOf(rings, seriesKey("mem.swap_total", ""));
        const hasSwap = (discovery.data?.has("mem.swap_used") ?? false) && (swapTotal ?? 0) > 0;
        const hasPsi = discovery.data?.has("psi.memory.some") ?? false;
        if (!hasSwap && !hasPsi) return null;
        return (
          <div className={s.subRow}>
            {hasSwap && (
              <SeriesChart
                expr="mem.swap_used"
                metric="mem.swap_used"
                labels=""
                title="内存 · mem.swap_used"
                tone="--mem"
                yMax={swapTotal ?? undefined}
                height={H_SUB}
                range={range}
                onLayer={onLayer}
                unitFmt={fmtBytes}
              />
            )}
            {hasPsi && (
              <PsiChart
                kind="memory"
                title="内存 · psi.memory（压力）"
                tone="--mem"
                range={range}
                onLayer={onLayer}
              />
            )}
          </div>
        );
      })()}
      <StatFact stats={stats} facts={facts} />
    </>
  );
}

/* ================= 成员格 ================= */

function MemberGrid({
  resource,
  members,
  cellValue,
  tagOf,
}: {
  resource: ResourceDef;
  members: readonly string[];
  cellValue: (m: string) => { big: string; small?: string };
  tagOf?: (m: string) => string | null;
}) {
  const rings = useLive((st) => st.rings);
  const navigate = useNavigate();
  const hue = cssVar(resource.tone);
  const metric = resource.memberMetric ?? "";
  const isPct = metric === "disk.util" || metric === "gpu.usage";

  return (
    <div className={s.members}>
      {members.map((m) => {
        const ring = rings.get(seriesKey(metric, `${resource.memberLabel}=${m}`));
        const val = cellValue(m);
        const tag = tagOf?.(m);
        return (
          <button
            key={m}
            type="button"
            className={s.cell}
            style={{ "--cell-tone": `var(${resource.tone})` } as React.CSSProperties}
            onClick={() => navigate(`/performance/${resource.id}/${encodeURIComponent(m)}`)}
          >
            <div className={s.cellPlot}>
              <Plot
                data={liveSingle(ring, 90)}
                series={[{ stroke: hue, width: 1.5, fill: withAlpha(hue, 0.14) }]}
                yMax={isPct ? 100 : undefined}
                height={80}
                tone={resource.tone}
                noCursor
              />
            </div>
            <span className={s.cellName}>{m}</span>
            {tag && <span className={s.cellTag}>{tag}</span>}
            <span className={s.cellVal}>
              {val.big}
              {val.small && <small className={s.cellSub}>{val.small}</small>}
            </span>
          </button>
        );
      })}
    </div>
  );
}

/* ================= 磁盘 ================= */

export function DiskSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const devs = liveMembers(
    rings,
    "disk.util",
    "dev",
    discovery.data?.members("disk.util", "dev") ?? [],
  );
  const hasRates = discovery.data?.has("disk.") ?? false;

  return (
    <>
      {hasRates && (
        <AggChart
          groups={[
            {
              name: "读",
              metrics: ["disk.read_bytes"],
              exprs: devs.map((d) => `disk.read_bytes{dev=${d}}`),
            },
            {
              name: "写",
              metrics: ["disk.write_bytes"],
              exprs: devs.map((d) => `disk.write_bytes{dev=${d}}`),
            },
          ]}
          title="磁盘 · 吞吐"
          tone="--disk"
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => `${fmtBytes(v)}/s`}
        />
      )}
      {discovery.data?.has("psi.io.some") && (
        <div className={s.subRow}>
          <PsiChart
            kind="io"
            title="磁盘 · psi.io（压力）"
            tone="--disk"
            range={range}
            onLayer={onLayer}
          />
        </div>
      )}

      {devs.length > 0 && (
        <>
          {(() => {
            // 数字区按 §6.2 聚合:饱和度取最大并说明是谁,速率与 IOPS 求和,
            // await 是每次 IO 的平均等待——跨盘合计没有物理意义,宁可标「不可合计」
            const stats: NumItem[] = [];
            let busiest: { d: string; v: number } | null = null;
            let iops: number | null = null;
            for (const d of devs) {
              const u = latestOf(rings, seriesKey("disk.util", `dev=${d}`));
              if (u !== null && (busiest === null || u > busiest.v)) busiest = { d, v: u };
              const io = latestOf(rings, seriesKey("disk.iops", `dev=${d}`));
              if (io !== null) iops = (iops ?? 0) + io;
            }
            if (busiest !== null)
              stats.push({
                k: "最忙",
                v: fmtPct(busiest.v / 100),
                sub: devs.length > 1 ? busiest.d : undefined,
              });
            const rd = latestSum(rings, "disk.read_bytes");
            const wr = latestSum(rings, "disk.write_bytes");
            if (rd !== null) stats.push({ k: "合计读取", v: `${fmtBytes(rd)}/s` });
            if (wr !== null) stats.push({ k: "合计写入", v: `${fmtBytes(wr)}/s` });
            if (iops !== null) stats.push({ k: "合计 IOPS", v: String(Math.round(iops)) });
            stats.push({ k: "平均响应", v: "—", sub: "不可合计" });
            const psi = latestOf(rings, seriesKey("psi.io.some", ""));
            if (psi !== null) stats.push({ k: "IO 停滞", v: fmtPct(psi / 100) });

            const facts: NumItem[] = [];
            const disks = info.data?.disks ?? [];
            if (disks.length > 0) {
              facts.push({ k: "设备数", v: String(disks.length) });
              facts.push({
                k: "总容量",
                v: fmtBytes(disks.reduce((a, x) => a + x.size_bytes, 0)),
              });
              const media = new Map<string, number>();
              for (const x of disks) {
                const m = x.rotational ? "HDD" : x.name.startsWith("nvme") ? "NVMe" : "SSD";
                media.set(m, (media.get(m) ?? 0) + 1);
              }
              facts.push({
                k: "介质",
                v: [...media.entries()].map(([m, n]) => `${m} ×${n}`).join(" · "),
              });
              const bad = disks.filter((x) => x.smart_healthy === false).length;
              const known = disks.filter(
                (x) => x.smart_healthy !== null && x.smart_healthy !== undefined,
              ).length;
              facts.push({
                k: "SMART",
                v: known === 0 ? "未检测" : bad === 0 ? "全部正常" : `${bad} 块异常`,
              });
            }
            const fsCount = (info.data?.filesystems ?? []).length;
            if (fsCount > 0) facts.push({ k: "挂载点", v: String(fsCount) });
            return <StatFact stats={stats} facts={facts} />;
          })()}
          <h2 className={s.sectionTitle}>块设备</h2>
          <MemberGrid
            resource={{
              id: "disk",
              label: "磁盘",
              tone: "--disk",
              probes: ["disk."],
              memberLabel: "dev",
              memberMetric: "disk.util",
            }}
            members={devs}
            cellValue={(d) => {
              const util = latestOf(rings, seriesKey("disk.util", `dev=${d}`));
              const rd = latestOf(rings, seriesKey("disk.read_bytes", `dev=${d}`)) ?? 0;
              const wr = latestOf(rings, seriesKey("disk.write_bytes", `dev=${d}`)) ?? 0;
              return {
                big: util !== null ? fmtPct(util / 100) : "—",
                small: `${fmtBytes(rd + wr)}/s`,
              };
            }}
            tagOf={(d) => {
              const disk = (info.data?.disks ?? []).find((x) => x.name === d);
              if (!disk) return null;
              // §5.5:rotational → HDD;名字 nvme 开头 → NVMe;其余 SSD
              return disk.rotational ? "HDD" : d.startsWith("nvme") ? "NVMe" : "SSD";
            }}
          />
        </>
      )}

      <MountTable
        filesystems={info.data?.filesystems ?? []}
        rings={rings}
        liveDevs={devs}
        caption="全部挂载点"
      />
    </>
  );
}

/* ================= 网络 ================= */

export function NetSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const all = liveMembers(
    rings,
    "net.tx_bytes",
    "iface",
    discovery.data?.members("net.tx_bytes", "iface") ?? [],
  );
  // 有流量的接口排前面；一排 0 b/s 的虚拟接口沉底
  const hasTraffic = (i: string) => {
    for (const m of ["net.rx_bytes", "net.tx_bytes"]) {
      const r = rings.get(seriesKey(m, `iface=${i}`));
      if (r?.v.some((v) => v > 0)) return true;
    }
    return false;
  };
  const ifaces = [...all].sort((a, b) => Number(hasTraffic(b)) - Number(hasTraffic(a)));

  return (
    <>
      <AggChart
        groups={[
          {
            name: "接收",
            metrics: ["net.rx_bytes"],
            exprs: all.map((i) => `net.rx_bytes{iface=${i}}`),
          },
          {
            name: "发送",
            metrics: ["net.tx_bytes"],
            exprs: all.map((i) => `net.tx_bytes{iface=${i}}`),
          },
        ]}
        title="网络 · 吞吐"
        tone="--net"
        height={H_MAIN}
        range={range}
        onLayer={onLayer}
        unitFmt={fmtRateBits}
      />
      {(() => {
        const stats: NumItem[] = [
          { k: "合计接收", v: fmtRateBits(latestSum(rings, "net.rx_bytes") ?? 0) },
          { k: "合计发送", v: fmtRateBits(latestSum(rings, "net.tx_bytes") ?? 0) },
        ];
        const errs = latestSum(rings, "net.errors");
        if (errs !== null) stats.push({ k: "异常包", v: `${Math.round(errs)}/s` });

        const facts: NumItem[] = [];
        const nets = info.data?.networks ?? [];
        facts.push({ k: "接口数", v: String(all.length) });
        if (nets.length > 0) {
          const up = nets.filter((n) => n.carrier);
          facts.push({ k: "有载波", v: String(up.length) });
          const speeds = new Map<string, number>();
          for (const n of up) {
            if (!n.speed_mbps) continue;
            const label =
              n.speed_mbps >= 1000 ? `${Math.round(n.speed_mbps / 1000)}G` : `${n.speed_mbps}M`;
            speeds.set(label, (speeds.get(label) ?? 0) + 1);
          }
          if (speeds.size > 0)
            facts.push({
              k: "链路",
              v: [...speeds.entries()].map(([sp, n]) => `${sp} ×${n}`).join(" · "),
            });
          const mtus = [...new Set(up.map((n) => n.mtu))];
          if (mtus.length > 0)
            facts.push({ k: "MTU", v: mtus.length === 1 ? String(mtus[0]) : mtus.join(" / ") });
        }
        return <StatFact stats={stats} facts={facts} />;
      })()}
      {ifaces.length > 0 && (
        <>
          <h2 className={s.sectionTitle}>接口</h2>
          <MemberGrid
            resource={{
              id: "net",
              label: "网络",
              tone: "--net",
              probes: ["net."],
              memberLabel: "iface",
              memberMetric: "net.tx_bytes",
            }}
            members={ifaces}
            cellValue={(i) => {
              const rx = latestOf(rings, seriesKey("net.rx_bytes", `iface=${i}`)) ?? 0;
              const tx = latestOf(rings, seriesKey("net.tx_bytes", `iface=${i}`)) ?? 0;
              const errs = latestOf(rings, seriesKey("net.errors", `iface=${i}`));
              return {
                big: fmtRateBits(rx + tx),
                small: errs && errs >= 1 ? `错误 ${Math.round(errs)}/s` : undefined,
              };
            }}
            tagOf={(i) => {
              const n = (info.data?.networks ?? []).find((x) => x.name === i);
              return n?.speed_mbps
                ? n.speed_mbps >= 10_000
                  ? "10G"
                  : `${Math.round(n.speed_mbps / 1000)}G`
                : null;
            }}
          />
        </>
      )}
    </>
  );
}

/* ================= GPU ================= */

/**
 * GPU 内存条(与内存组成条同款):每张卡一根 使用中/分母 横条。
 * 分母优先 `gpu.mem_total`(真显存);统一内存没有总量,退到 `gpu.mem_alloc`
 * (GPU 已向系统申请的量)。两者都测不到就整块不出现(spec §6)。
 */
function GpuComposition({ gpus }: { gpus: readonly string[] }) {
  const rings = useLive((st) => st.rings);
  const mode = useTheme((t) => t.mode);
  void mode; // 主题切换时 cssVar 解析值变化,需要重渲染
  const rows = gpus.flatMap((g) => {
    const used = latestOf(rings, seriesKey("gpu.mem_used", `gpu=${g}`));
    const total = latestOf(rings, seriesKey("gpu.mem_total", `gpu=${g}`));
    const alloc = latestOf(rings, seriesKey("gpu.mem_alloc", `gpu=${g}`));
    const denom = total ?? alloc;
    if (used === null || denom === null || denom <= 0) return [];
    return [{ g, used: Math.min(used, denom), denom, real: total !== null }];
  });
  if (rows.length === 0) return null;

  const hue = cssVar("--gpu");
  const usedSum = rows.reduce((a, r) => a + r.used, 0);
  const denomSum = rows.reduce((a, r) => a + r.denom, 0);
  // 全组只要有一张卡是 alloc 分母,措辞就得说「已分配」,不能冒充显存总量
  const denomKind = rows.every((r) => r.real) ? "显存总量" : "已分配";

  return (
    <div className={s.chartBlock}>
      <div className={s.chartTitle}>
        <b>GPU 内存</b>
        <span>
          {fmtPct(usedSum / denomSum)} 已用 · {denomKind} {fmtBytes(denomSum)}
        </span>
      </div>
      {rows.map((r) => (
        <div key={r.g} className={s.compRow}>
          {rows.length > 1 && <span className={s.compLabel}>{r.g}</span>}
          <div className={s.compBar}>
            <i
              style={{ width: `${((r.used / r.denom) * 100).toFixed(2)}%`, background: hue }}
              title={`使用中 ${fmtBytes(r.used)}`}
            />
          </div>
        </div>
      ))}
      <Legend
        entries={[
          { color: hue, name: `使用中 ${fmtBytes(usedSum)}` },
          { color: "var(--surface-3)", name: `${denomKind} ${fmtBytes(denomSum)}` },
        ]}
      />
    </div>
  );
}

export function GpuSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const gpus = liveMembers(
    rings,
    "gpu.usage",
    "gpu",
    discovery.data?.members("gpu.usage", "gpu") ?? [],
  );
  const only = gpus.length === 1 ? gpus[0] : undefined;

  return (
    <>
      {only !== undefined ? (
        <SeriesChart
          expr={`gpu.usage{gpu=${only}}`}
          metric="gpu.usage"
          labels={`gpu=${only}`}
          title="GPU · gpu.usage"
          tone="--gpu"
          yMax={100}
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />
      ) : (
        /* 多卡:饱和度不能求和也不能平均,取组内最大（08 §6.2） */
        <AggChart
          groups={[
            {
              name: "最忙",
              metrics: ["gpu.usage"],
              exprs: gpus.map((g) => `gpu.usage{gpu=${g}}`),
            },
          ]}
          agg="max"
          title="GPU · gpu.usage（组内取最大）"
          tone="--gpu"
          yMax={100}
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />
      )}
      <GpuComposition gpus={gpus} />
      {/* 副图行:引擎细分 + 显存 + 温度。显存是容量按 §6.2 求和,温度取组内最热 */}
      {(() => {
        const engines = discovery.data?.members("gpu.engine.usage", "engine") ?? [];
        const hasMem = discovery.data?.has("gpu.mem_used") ?? false;
        const hasTemp = discovery.data?.has("gpu.temp") ?? false;
        if (engines.length === 0 && !hasMem && !hasTemp) return null;
        return (
          <div className={s.subRow}>
            {engines.length > 0 && (
              <AggChart
                groups={engines.map((e) => ({
                  name: e,
                  metrics: ["gpu.engine.usage"],
                  exprs: gpus.map((g) => `gpu.engine.usage{engine=${e},gpu=${g}}`),
                  match: (key) =>
                    key.startsWith("gpu.engine.usage|") && key.includes(`engine=${e}`),
                }))}
                agg="max"
                title={gpus.length > 1 ? "GPU · 引擎细分（组内取最大）" : "GPU · 引擎细分"}
                tone="--gpu"
                yMax={100}
                height={H_SUB}
                range={range}
                onLayer={onLayer}
                unitFmt={(v) => fmtPct(v / 100)}
              />
            )}
            {hasMem &&
              (only !== undefined ? (
                <SeriesChart
                  expr={`gpu.mem_used{gpu=${only}}`}
                  metric="gpu.mem_used"
                  labels={`gpu=${only}`}
                  title="GPU · gpu.mem_used"
                  tone="--gpu"
                  height={H_SUB}
                  range={range}
                  onLayer={onLayer}
                  unitFmt={fmtBytes}
                />
              ) : (
                <AggChart
                  groups={[
                    {
                      name: "显存合计",
                      metrics: ["gpu.mem_used"],
                      exprs: gpus.map((g) => `gpu.mem_used{gpu=${g}}`),
                    },
                  ]}
                  title="GPU · gpu.mem_used（合计）"
                  tone="--gpu"
                  height={H_SUB}
                  range={range}
                  onLayer={onLayer}
                  unitFmt={fmtBytes}
                />
              ))}
            {hasTemp &&
              (only !== undefined ? (
                <SeriesChart
                  expr={`gpu.temp{gpu=${only}}`}
                  metric="gpu.temp"
                  labels={`gpu=${only}`}
                  title="GPU · gpu.temp"
                  tone="--gpu"
                  height={H_SUB}
                  range={range}
                  onLayer={onLayer}
                  unitFmt={(v) => `${Math.round(v)} °C`}
                />
              ) : (
                <AggChart
                  groups={[
                    {
                      name: "最热",
                      metrics: ["gpu.temp"],
                      exprs: gpus.map((g) => `gpu.temp{gpu=${g}}`),
                    },
                  ]}
                  agg="max"
                  title="GPU · gpu.temp（组内取最大）"
                  tone="--gpu"
                  height={H_SUB}
                  range={range}
                  onLayer={onLayer}
                  unitFmt={(v) => `${Math.round(v)} °C`}
                />
              ))}
          </div>
        );
      })()}
      {(() => {
        const stats: NumItem[] = [];
        const u = latestMax(rings, "gpu.usage");
        if (u !== null) stats.push({ k: gpus.length > 1 ? "最忙" : "利用率", v: fmtPct(u / 100) });
        const mem = latestSum(rings, "gpu.mem_used");
        const memTotal = latestSum(rings, "gpu.mem_total");
        if (mem !== null)
          stats.push({
            k: gpus.length > 1 ? "显存合计" : "显存",
            v: fmtBytes(mem),
            sub: memTotal ? `/ ${fmtBytes(memTotal)}` : undefined,
          });
        const alloc = latestSum(rings, "gpu.mem_alloc");
        if (alloc !== null) stats.push({ k: "已分配", v: fmtBytes(alloc) });
        const temp = latestMax(rings, "gpu.temp");
        if (temp !== null)
          stats.push({ k: gpus.length > 1 ? "最高温度" : "温度", v: `${Math.round(temp)} °C` });
        if (gpus.length > 1) stats.push({ k: "卡数", v: String(gpus.length) });

        const facts: NumItem[] = (info.data?.gpus ?? []).map((g) => ({
          k: g.card || "GPU",
          v: g.model || "—",
          sub: [g.driver, g.vram_bytes ? fmtBytes(g.vram_bytes) : null, g.bus]
            .filter(Boolean)
            .join(" · "),
        }));
        return <StatFact stats={stats} facts={facts} />;
      })()}
      {gpus.length > 1 && (
        <MemberGrid
          resource={{
            id: "gpu",
            label: "GPU",
            tone: "--gpu",
            probes: ["gpu."],
            memberLabel: "gpu",
            memberMetric: "gpu.usage",
          }}
          members={gpus}
          cellValue={(g) => {
            const u = latestOf(rings, seriesKey("gpu.usage", `gpu=${g}`));
            const mu = latestOf(rings, seriesKey("gpu.mem_used", `gpu=${g}`));
            return {
              big: u !== null ? fmtPct(u / 100) : "—",
              small: mu !== null ? fmtBytes(mu) : undefined,
            };
          }}
        />
      )}
    </>
  );
}

/* ================= 成员详情 ================= */

/**
 * 成员详情页。同量纲合图：吞吐两条线一张图，有界百分比一张矮图，
 * 计数类进数字区。
 */
export function MemberDetail({
  resource,
  member,
  range,
  onLayer,
}: {
  resource: ResourceDef;
  member: string;
  range: RangeKey;
  onLayer: (layer: string | null) => void;
}) {
  const discovery = useDiscovery();
  const rings = useLive((st) => st.rings);
  const info = useSystemInfo();
  const labelKey = resource.memberLabel ?? "";
  const lbl = `${labelKey}=${member}`;
  const metrics =
    discovery.data?.all
      .filter((m) => labelValue(m.labels, labelKey) === member)
      .map((m) => m.metric)
      .filter((v, i, arr) => arr.indexOf(v) === i) ?? [];

  if (metrics.length === 0) {
    return <p className={s.note}>没有可用序列。</p>;
  }

  const has = (m: string) => metrics.includes(m);
  const num = (m: string) => latestOf(rings, seriesKey(m, lbl));

  const charts: React.ReactNode[] = [];
  const numberItems: NumItem[] = [];
  const facts: NumItem[] = [];

  if (resource.id === "net") {
    if (has("net.rx_bytes"))
      charts.push(
        <SeriesChart
          key="thru"
          expr={`net.rx_bytes{${lbl}}`}
          metric="net.rx_bytes"
          labels={lbl}
          title={`${member} · 吞吐`}
          tone="--net"
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={fmtRateBits}
          secondary={
            has("net.tx_bytes")
              ? {
                  expr: `net.tx_bytes{${lbl}}`,
                  metric: "net.tx_bytes",
                  labels: lbl,
                  name: "net.tx_bytes",
                }
              : undefined
          }
        />,
      );
    const rx = num("net.rx_bytes");
    const tx = num("net.tx_bytes");
    const er = num("net.errors");
    if (rx !== null) numberItems.push({ k: "接收", v: fmtRateBits(rx) });
    if (tx !== null) numberItems.push({ k: "发送", v: fmtRateBits(tx) });
    if (er !== null) numberItems.push({ k: "错误", v: `${Math.round(er)}/s` });
    const n = (info.data?.networks ?? []).find((x) => x.name === member);
    if (n) {
      facts.push({ k: "链路", v: n.carrier ? "已连接" : "无载波" });
      if (n.speed_mbps) facts.push({ k: "速率", v: `${n.speed_mbps} Mb/s` });
      if (n.mac) facts.push({ k: "MAC", v: n.mac });
      if (n.mtu) facts.push({ k: "MTU", v: String(n.mtu) });
      for (const a of (n.addrs ?? []).slice(0, 4)) facts.push({ k: "地址", v: a });
    }
  } else if (resource.id === "disk") {
    if (has("disk.read_bytes"))
      charts.push(
        <SeriesChart
          key="thru"
          expr={`disk.read_bytes{${lbl}}`}
          metric="disk.read_bytes"
          labels={lbl}
          title={`${member} · 吞吐`}
          tone="--disk"
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => `${fmtBytes(v)}/s`}
          secondary={
            has("disk.write_bytes")
              ? {
                  expr: `disk.write_bytes{${lbl}}`,
                  metric: "disk.write_bytes",
                  labels: lbl,
                  name: "disk.write_bytes",
                }
              : undefined
          }
        />,
      );
    if (has("disk.util"))
      charts.push(
        <SeriesChart
          key="util"
          expr={`disk.util{${lbl}}`}
          metric="disk.util"
          labels={lbl}
          title={`${member} · disk.util`}
          tone="--disk"
          yMax={100}
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />,
      );
    const util = num("disk.util");
    if (util !== null) numberItems.push({ k: "活动时间", v: fmtPct(util / 100) });
    const rd = num("disk.read_bytes");
    const wr = num("disk.write_bytes");
    if (rd !== null) numberItems.push({ k: "读取", v: `${fmtBytes(rd)}/s` });
    if (wr !== null) numberItems.push({ k: "写入", v: `${fmtBytes(wr)}/s` });
    const iops = num("disk.iops");
    const aw = num("disk.await");
    if (iops !== null) numberItems.push({ k: "IOPS", v: String(Math.round(iops)) });
    if (aw !== null) numberItems.push({ k: "平均响应", v: `${aw.toFixed(1)} ms` });
    const d = (info.data?.disks ?? []).find((x) => x.name === member);
    if (d) {
      facts.push(
        { k: "型号", v: d.model || "—" },
        { k: "容量", v: fmtBytes(d.size_bytes) },
        {
          k: "介质",
          v: d.rotational ? "HDD" : member.startsWith("nvme") ? "NVMe" : "SSD",
        },
      );
      if (d.removable) facts.push({ k: "可移动", v: "是" });
      facts.push({
        k: "SMART",
        v:
          d.smart_healthy === null || d.smart_healthy === undefined
            ? "未检测"
            : d.smart_healthy
              ? "健康"
              : "异常",
      });
      const mine = (info.data?.filesystems ?? []).filter((f) => f.backing_dev === member);
      if (mine.length > 0) facts.push({ k: "挂载点", v: String(mine.length) });
    }
  } else if (resource.id === "gpu") {
    if (has("gpu.usage"))
      charts.push(
        <SeriesChart
          key="usage"
          expr={`gpu.usage{${lbl}}`}
          metric="gpu.usage"
          labels={lbl}
          title={`${member} · gpu.usage`}
          tone="--gpu"
          yMax={100}
          height={H_MAIN}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />,
      );
    if (has("gpu.mem_used"))
      charts.push(
        <SeriesChart
          key="mem"
          expr={`gpu.mem_used{${lbl}}`}
          metric="gpu.mem_used"
          labels={lbl}
          title={`${member} · gpu.mem_used`}
          tone="--gpu"
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={fmtBytes}
        />,
      );
    const gu = num("gpu.usage");
    if (gu !== null) numberItems.push({ k: "利用率", v: fmtPct(gu / 100) });
    const gm = num("gpu.mem_used");
    const gmt = num("gpu.mem_total");
    if (gm !== null)
      numberItems.push({
        k: "显存",
        v: fmtBytes(gm),
        sub: gmt !== null ? `/ ${fmtBytes(gmt)}` : undefined,
      });
    const gt = num("gpu.temp");
    if (gt !== null) numberItems.push({ k: "温度", v: `${Math.round(gt)} °C` });
    // member 是 gpu 标签值（卡名）,按 card 名对齐;对不上宁可不显示,不能拿别的卡凑数
    const g = (info.data?.gpus ?? []).find((x) => x.card === member);
    if (g) {
      facts.push({ k: "型号", v: g.model || "—" });
      if (g.driver) facts.push({ k: "驱动", v: g.driver });
      if (g.vram_bytes) facts.push({ k: "显存", v: fmtBytes(g.vram_bytes) });
      if (g.bus) facts.push({ k: "总线", v: g.bus });
    }
  } else {
    for (const metric of metrics) {
      charts.push(
        <SeriesChart
          key={metric}
          expr={`${metric}{${lbl}}`}
          metric={metric}
          labels={lbl}
          title={`${member} · ${metric}`}
          tone={resource.tone}
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => v.toFixed(1)}
        />,
      );
    }
  }

  return (
    <>
      {charts}
      {resource.id === "disk" && (
        /* 这块盘上的挂载点（08 §6.3 的第二处）。挂载点列在它所在的盘里,
           「共享设备级 IO 计数器」就不言自明。自己页内不再跳自己,行不可点。 */
        <MountTable
          filesystems={(info.data?.filesystems ?? []).filter((f) => f.backing_dev === member)}
          rings={rings}
          liveDevs={[]}
          caption="这块盘上的挂载点"
          defaultOpen
        />
      )}
      <StatFact stats={numberItems} facts={facts} />
    </>
  );
}
