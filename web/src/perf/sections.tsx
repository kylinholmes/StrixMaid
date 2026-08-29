import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "@/api/client";
import { Segmented } from "@/components";
import { cssVar, Plot, type PlotSeries, withAlpha } from "@/components/Plot";
import { fmtBytes, fmtPct, fmtRateBits, fmtUptime } from "@/lib/fmt";
import { labelValue, useDiscovery } from "@/metrics/discovery";
import { type RangeKey, rangeOf, useHistory } from "@/metrics/history";
import { latestOf, latestSum, type Ring, seriesKey, useLive } from "@/metrics/live";
import { useSession } from "@/session/useSession";
import { useTheme } from "@/theme/useTheme";
import { bandData, bandFill, bandSeries, liveSeries, liveSingle, liveSum, sumAvg } from "./chart";
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

/** 图下的 `<details>` 表格（spec §10）。 */
function ReadingsTable({
  xs,
  cols,
  unitFmt,
}: {
  xs: readonly (number | null | undefined)[];
  cols: readonly { name: string; vs: readonly (number | null | undefined)[] }[];
  unitFmt: (v: number) => string;
}) {
  const rows: { t: string; vals: string[] }[] = [];
  for (let i = xs.length - 1; i >= 0 && rows.length < 8; i--) {
    const t = xs[i];
    if (typeof t !== "number") continue;
    rows.push({
      t: new Date(t * 1000).toLocaleTimeString("zh-CN", { hour12: false }),
      vals: cols.map((c) => {
        const v = c.vs[i];
        return typeof v === "number" ? unitFmt(v) : "—";
      }),
    });
  }
  return (
    <details className={s.tblView}>
      <summary>表格</summary>
      <table>
        <thead>
          <tr>
            <th>时刻</th>
            {cols.map((c) => (
              <th key={c.name}>{c.name}</th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.t}>
              <td>{r.t}</td>
              {r.vals.map((v, i) => (
                <td key={cols[i]?.name ?? i}>{v}</td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </details>
  );
}

function Legend({ entries }: { entries: readonly { color: string; name: string }[] }) {
  return (
    <div className={s.legend}>
      {entries.map((e) => (
        <span key={e.name}>
          <i style={{ background: e.color }} />
          {e.name}
        </span>
      ))}
    </div>
  );
}

function Numbers({ items }: { items: readonly { k: string; v: string; sub?: string }[] }) {
  return (
    <div className={s.numbers}>
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

  const corner = {
    cornerTR: yMax !== undefined ? unitFmt(yMax) : latest !== undefined ? unitFmt(latest) : "",
    cornerBL: rangeOf(range).label,
    cornerBR: "0",
  };

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
  let tableXs: readonly (number | null | undefined)[];
  let tableCols: { name: string; vs: readonly (number | null | undefined)[] }[];

  if (live || !bs) {
    data = liveSingle(ring);
    const xs = data[0] as number[];
    tableXs = xs;
    tableCols = [{ name: metric, vs: data[1] as number[] }];
    if (secondary) {
      const ys2 = alignByTs(xs, ring2);
      data = [xs, data[1], ys2] as typeof data;
      plotSeries = [...liveSeries(hue), { stroke: hue2, width: 1.25 }];
      tableCols.push({ name: secondary.metric, vs: ys2 });
    }
  } else {
    data = bandData(bs);
    plotSeries = bandSeries(hue);
    bands = bandFill(hue);
    tableXs = bs.xs;
    tableCols = [{ name: metric, vs: bs.avg }];
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
      tableCols.push({ name: secondary.metric, vs: ys2 });
    }
  }

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
        {...corner}
      />
      {secondary && (
        <Legend
          entries={[
            { color: hue, name: metric },
            { color: hue2, name: secondary.name },
          ]}
        />
      )}
      <ReadingsTable xs={tableXs} cols={tableCols} unitFmt={unitFmt} />
    </div>
  );
}

/**
 * 组页聚合图：每组按「速率求和」（08 §6.2）成一条线，最多两组（读/写、收/发）。
 * 历史档只画各组 avg 合计。
 */
function SumChart({
  groups,
  title,
  tone,
  height,
  range,
  onLayer,
  unitFmt,
}: {
  groups: readonly { name: string; metrics: readonly string[]; exprs: readonly string[] }[];
  title: string;
  tone: string;
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

  const perGroup = groups.map((g) => {
    if (live) {
      const groupRings: (Ring | undefined)[] = [];
      for (const [key, r] of rings) {
        if (g.metrics.some((m) => key.startsWith(`${m}|`))) groupRings.push(r);
      }
      const d = liveSum(groupRings);
      return { name: g.name, xs: d[0] as number[], vs: d[1] as (number | null)[] };
    }
    const list = [...(hist.data?.bySeries.entries() ?? [])]
      .filter(([k]) => g.exprs.includes(k))
      .map(([, v]) => v);
    const d = sumAvg(list);
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

  const latest = groups.reduce(
    (acc, g) => acc + g.metrics.reduce((a, m) => a + (latestSum(rings, m) ?? 0), 0),
    0,
  );
  const colors = [hue, hue2];
  const plotSeries: PlotSeries[] = perGroup.map((_, i) => ({
    stroke: colors[i] ?? hue,
    width: i === 0 ? 2 : 1.25,
    fill: i === 0 && live && perGroup.length === 1 ? withAlpha(hue, 0.18) : undefined,
  }));

  return (
    <div className={s.chartBlock} key={mode}>
      <div className={s.chartTitle}>
        <b>{title}</b>
        <span>{unitFmt(latest)}</span>
      </div>
      <Plot
        data={[xs, ...ys] as Parameters<typeof Plot>[0]["data"]}
        series={plotSeries}
        height={height}
        tone={tone}
        cornerBL={rangeOf(range).label}
        cornerBR="0"
      />
      {perGroup.length > 1 && (
        <Legend entries={perGroup.map((g, i) => ({ color: colors[i] ?? hue, name: g.name }))} />
      )}
      <ReadingsTable
        xs={xs}
        cols={perGroup.map((g, i) => ({ name: g.name, vs: ys[i] ?? [] }))}
        unitFmt={unitFmt}
      />
    </div>
  );
}

/* ================= CPU ================= */

export function CpuSection({ range, rangeSecs, onLayer }: SectionProps & { rangeSecs: number }) {
  const [view, setView] = useState<"all" | "cores">("all");
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();

  const cores = discovery.data?.members("cpu.core.usage", "core") ?? [];

  const numberItems: { k: string; v: string }[] = [];
  for (const [k, label] of [
    ["cpu.system", "内核态"],
    ["cpu.iowait", "IO 等待"],
    ["cpu.irq", "中断"],
    ["cpu.steal", "被偷走"],
    ["psi.cpu.some", "停滞"],
  ] as const) {
    const v = latestOf(rings, seriesKey(k, ""));
    if (v !== null) numberItems.push({ k: label, v: fmtPct(v / 100) });
  }
  const procs = latestOf(rings, seriesKey("procs.total", ""));
  if (procs !== null) numberItems.push({ k: "进程", v: String(Math.round(procs)) });
  const running = latestOf(rings, seriesKey("procs.running", ""));
  if (running !== null) numberItems.push({ k: "运行中", v: String(Math.round(running)) });
  const load = latestOf(rings, seriesKey("load.1m", ""));
  if (load !== null) numberItems.push({ k: "负载 1 分钟", v: load.toFixed(2) });

  return (
    <>
      {cores.length > 0 && (
        <div style={{ marginBottom: 12 }}>
          <Segmented
            label="视图"
            value={view}
            onChange={setView}
            options={[
              { value: "all", label: "总体" },
              { value: "cores", label: `逻辑处理器 ${cores.length}` },
            ]}
          />
        </div>
      )}

      {view === "all" ? (
        <>
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
            secondary={
              discovery.data?.has("cpu.system")
                ? { expr: "cpu.system", metric: "cpu.system", labels: "", name: "cpu.system" }
                : undefined
            }
          />
          {numberItems.length > 0 && <Numbers items={numberItems} />}
          {info.data && (
            <Numbers
              items={[
                { k: "型号", v: info.data.cpu.model },
                {
                  k: "插槽 / 物理核",
                  v: `${(info.data.cpu.packages ?? []).length || 1} / ${
                    info.data.cpu.physical_cores ?? info.data.cpu.logical_cores
                  }`,
                },
                { k: "逻辑核", v: String(info.data.cpu.logical_cores) },
                { k: "虚拟化", v: info.data.virtualization ?? "未检出" },
                { k: "运行时间", v: fmtUptime(info.data.uptime_secs) },
              ]}
            />
          )}
        </>
      ) : cores.length <= 32 ? (
        <div className={s.cores}>
          {cores.map((core) => {
            const key = seriesKey("cpu.core.usage", `core=${core}`);
            const ring = rings.get(key);
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
                  height={54}
                  tone="--cpu"
                  noCursor
                />
              </div>
            );
          })}
        </div>
      ) : (
        <CoreHeatmap cores={cores} />
      )}
    </>
  );
}

/** >32 核：单张 canvas 热力图（08 §6.6），单色相顺序色阶。 */
function CoreHeatmap({ cores }: { cores: readonly string[] }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rings = useLive((st) => st.rings);
  const mode = useTheme((t) => t.mode);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const hostW = canvas.parentElement?.clientWidth ?? 600;
    const cols = [32, 16, 8].find((c) => hostW / c >= 22) ?? 8;
    const cellW = Math.floor(hostW / cols);
    const cellH = Math.min(cellW, 30);
    const rows = Math.ceil(cores.length / cols);
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
      const ring = rings.get(seriesKey("cpu.core.usage", `core=${core}`));
      const v = ring?.v[ring.v.length - 1] ?? 0;
      const col = i % cols;
      const row = Math.floor(i / cols);
      ctx.fillStyle = withAlpha(hue, 0.1 + 0.9 * Math.min(1, v / 100));
      ctx.fillRect(col * cellW + 1, row * cellH + 1, cellW - 2, cellH - 2);
    });
  }, [cores, rings, mode]);

  return (
    <div className={s.heat}>
      <canvas ref={canvasRef} title="逻辑处理器占用" />
    </div>
  );
}

/* ================= 内存 ================= */

export function MemSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const total = latestSum(rings, "mem.total");
  const items: { k: string; v: string }[] = [];
  for (const [metric, label] of [
    ["mem.used", "已用"],
    ["mem.available", "可用"],
    ["mem.cached", "缓存"],
    ["mem.swap_used", "交换已用"],
    ["mem.swap_total", "交换总量"],
  ] as const) {
    const v = latestOf(rings, seriesKey(metric, ""));
    if (v !== null) items.push({ k: label, v: fmtBytes(v) });
  }
  if (total !== null) items.push({ k: "总量", v: fmtBytes(total) });

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
      <Numbers items={items} />
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
                height={82}
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

interface MountGroup {
  dev: string | null;
  mounts: string[];
}

/** 挂载点按后端设备去重（08 §6.3）：同容器的卷共享物理空间，容量只算一次。 */
function groupMounts(
  mounts: readonly string[],
  filesystems: readonly { mount_point: string; backing_dev?: string | null }[] | undefined,
): MountGroup[] {
  const devOf = new Map<string, string>();
  for (const f of filesystems ?? []) {
    if (f.backing_dev) devOf.set(f.mount_point, f.backing_dev);
  }
  const groups = new Map<string, MountGroup>();
  const solo: MountGroup[] = [];
  for (const m of mounts) {
    const dev = devOf.get(m);
    if (!dev) {
      solo.push({ dev: null, mounts: [m] });
      continue;
    }
    const g = groups.get(dev);
    if (g) g.mounts.push(m);
    else groups.set(dev, { dev, mounts: [m] });
  }
  return [...groups.values(), ...solo];
}

export function DiskSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const devs = discovery.data?.members("disk.util", "dev") ?? [];
  const hasRates = discovery.data?.has("disk.") ?? false;
  const mounts = discovery.data?.members("fs.used", "mount") ?? [];

  return (
    <>
      {hasRates && (
        <SumChart
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

      {devs.length > 0 && (
        <>
          <Numbers
            items={devs.flatMap((d) => {
              const util = latestOf(rings, seriesKey("disk.util", `dev=${d}`));
              const iops = latestOf(rings, seriesKey("disk.iops", `dev=${d}`));
              const await_ = latestOf(rings, seriesKey("disk.await", `dev=${d}`));
              return [
                { k: `${d} 繁忙`, v: util !== null ? fmtPct(util / 100) : "—" },
                { k: `${d} IOPS`, v: iops !== null ? String(Math.round(iops)) : "—" },
                { k: `${d} 等待`, v: await_ !== null ? `${await_.toFixed(1)} ms` : "—" },
              ];
            })}
          />
          {(info.data?.disks ?? []).length > 0 && (
            <Numbers
              items={(info.data?.disks ?? []).map((dsk) => ({
                k: dsk.name,
                v: dsk.model || (dsk.rotational ? "HDD" : "SSD"),
                sub: [
                  fmtBytes(dsk.size_bytes),
                  dsk.rotational ? "HDD" : "SSD",
                  dsk.smart_healthy === false ? "SMART 异常" : null,
                ]
                  .filter(Boolean)
                  .join(" · "),
              }))}
            />
          )}
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
              return disk.rotational ? "HDD" : "SSD";
            }}
          />
        </>
      )}

      {mounts.length > 0 && (
        <details className={s.mts}>
          <summary>
            挂载点 {mounts.length} · 设备 {groupMounts(mounts, info.data?.filesystems).length} ·
            最满 {(() => {
              let worst = 0;
              for (const m of mounts) {
                const u = latestOf(rings, seriesKey("fs.used", `mount=${m}`));
                const t = latestOf(rings, seriesKey("fs.total", `mount=${m}`));
                if (u !== null && t) worst = Math.max(worst, u / t);
              }
              return fmtPct(worst);
            })()}
          </summary>
          <Numbers
            items={groupMounts(mounts, info.data?.filesystems).map((g) => {
              const first = g.mounts[0] ?? "";
              const used = latestOf(rings, seriesKey("fs.used", `mount=${first}`));
              const total = latestOf(rings, seriesKey("fs.total", `mount=${first}`));
              return {
                k: g.dev ? `容器 ${g.dev}` : first,
                v: used !== null && total ? fmtPct(used / total) : "—",
                sub:
                  used !== null && total
                    ? `${fmtBytes(used)} / ${fmtBytes(total)} · ${g.mounts.join(" ")}`
                    : undefined,
              };
            })}
          />
        </details>
      )}
    </>
  );
}

/* ================= 网络 ================= */

export function NetSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const all = discovery.data?.members("net.tx_bytes", "iface") ?? [];
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
      <SumChart
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
      <Numbers
        items={[
          { k: "接收", v: fmtRateBits(latestSum(rings, "net.rx_bytes") ?? 0) },
          { k: "发送", v: fmtRateBits(latestSum(rings, "net.tx_bytes") ?? 0) },
          { k: "错误", v: `${Math.round(latestSum(rings, "net.errors") ?? 0)}/s` },
          { k: "接口", v: String(all.length) },
        ]}
      />
      {(info.data?.networks ?? []).filter((n) => n.carrier).length > 0 && (
        <Numbers
          items={(info.data?.networks ?? [])
            .filter((n) => n.carrier)
            .slice(0, 8)
            .map((n) => ({
              k: n.name,
              v: n.speed_mbps ? `${n.speed_mbps} Mb/s` : "已连接",
              sub: [n.mac, n.mtu ? `MTU ${n.mtu}` : null].filter(Boolean).join(" · "),
            }))}
        />
      )}
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

export function GpuSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const gpus = discovery.data?.members("gpu.usage", "gpu") ?? [];
  const only = gpus.length === 1 ? gpus[0] : undefined;
  const lbl = only !== undefined ? `gpu=${only}` : "";
  const expr = (m: string) => (only !== undefined ? `${m}{gpu=${only}}` : m);

  return (
    <>
      <SeriesChart
        expr={expr("gpu.usage")}
        metric="gpu.usage"
        labels={lbl}
        title="GPU · gpu.usage"
        tone="--gpu"
        yMax={100}
        height={H_MAIN}
        range={range}
        onLayer={onLayer}
        unitFmt={(v) => fmtPct(v / 100)}
      />
      {discovery.data?.has("gpu.mem_used") && (
        <SeriesChart
          expr={expr("gpu.mem_used")}
          metric="gpu.mem_used"
          labels={lbl}
          title="GPU · gpu.mem_used"
          tone="--gpu"
          height={H_SUB}
          range={range}
          onLayer={onLayer}
          unitFmt={fmtBytes}
        />
      )}
      {(info.data?.gpus ?? []).length > 0 && (
        <Numbers
          items={(info.data?.gpus ?? []).map((g) => ({
            k: g.card || "GPU",
            v: g.model || "—",
            sub: [g.driver, g.vram_bytes ? fmtBytes(g.vram_bytes) : null]
              .filter(Boolean)
              .join(" · "),
          }))}
        />
      )}
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
  const numberItems: { k: string; v: string }[] = [];
  const facts: { k: string; v: string; sub?: string }[] = [];

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
    const iops = num("disk.iops");
    const aw = num("disk.await");
    if (iops !== null) numberItems.push({ k: "IOPS", v: String(Math.round(iops)) });
    if (aw !== null) numberItems.push({ k: "等待", v: `${aw.toFixed(1)} ms` });
    const d = (info.data?.disks ?? []).find((x) => x.name === member);
    if (d) {
      facts.push(
        { k: "型号", v: d.model || "—" },
        { k: "容量", v: fmtBytes(d.size_bytes) },
        { k: "介质", v: d.rotational ? "HDD" : "SSD" },
      );
      if (d.smart_healthy !== null && d.smart_healthy !== undefined)
        facts.push({ k: "SMART", v: d.smart_healthy ? "健康" : "异常" });
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
    const g = (info.data?.gpus ?? [])[Number(member)] ?? (info.data?.gpus ?? [])[0];
    if (g) {
      facts.push({ k: "型号", v: g.model || "—" });
      if (g.driver) facts.push({ k: "驱动", v: g.driver });
      if (g.vram_bytes) facts.push({ k: "显存", v: fmtBytes(g.vram_bytes) });
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
      {numberItems.length > 0 && <Numbers items={numberItems} />}
      {facts.length > 0 && <Numbers items={facts} />}
    </>
  );
}
