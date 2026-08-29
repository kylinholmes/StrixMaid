import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api } from "@/api/client";
import { Segmented } from "@/components";
import { cssVar, Plot, withAlpha } from "@/components/Plot";
import { fmtBytes, fmtPct, fmtRateBits } from "@/lib/fmt";
import { labelValue, useDiscovery } from "@/metrics/discovery";
import { type RangeKey, rangeOf, useHistory } from "@/metrics/history";
import { latestOf, latestSum, type Ring, seriesKey, useLive } from "@/metrics/live";
import { useSession } from "@/session/useSession";
import { useTheme } from "@/theme/useTheme";
import { bandData, bandFill, bandSeries, liveSeries, liveSingle, liveSum, sumAvg } from "./chart";
import type { ResourceDef } from "./model";
import s from "./Perf.module.css";

interface SectionProps {
  range: RangeKey;
  onLayer: (layer: string | null) => void;
}

/** 单序列图：60 秒走 live 环，其余档走历史 band。 */
function SeriesChart({
  expr,
  metric,
  labels,
  title,
  sub,
  tone,
  yMax,
  height,
  range,
  onLayer,
  unitFmt,
}: {
  /** 历史查询表达式（`metric` 或 `metric{k=v}`） */
  expr: string;
  metric: string;
  labels: string;
  title: string;
  sub?: string;
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
  const live = range === "60s";
  const hist = useHistory([expr], range, !live);

  useEffect(() => {
    onLayer(live ? null : (hist.data?.layer ?? null));
  }, [live, hist.data?.layer, onLayer]);

  const ring = rings.get(seriesKey(metric, labels));
  const bs = hist.data?.bySeries.get(expr);
  const latest = ring?.v[ring.v.length - 1];

  const rangeLabel = rangeOf(range)?.label ?? "";
  const corner = {
    cornerTR: yMax !== undefined ? unitFmt(yMax) : latest !== undefined ? unitFmt(latest) : "",
    cornerBL: rangeLabel,
    cornerBR: "0",
  };

  return (
    <div className={s.chartBlock} key={mode}>
      <div className={s.chartTitle}>
        <b>{title}</b>
        <span>{sub ?? (latest !== undefined ? unitFmt(latest) : "—")}</span>
      </div>
      {live || !bs ? (
        <Plot
          data={liveSingle(ring)}
          series={liveSeries(hue)}
          yMax={yMax}
          height={height}
          tone={tone}
          {...corner}
        />
      ) : (
        <Plot
          data={bandData(bs)}
          series={bandSeries(hue)}
          bands={bandFill(hue)}
          yMax={yMax}
          height={height}
          tone={tone}
          {...corner}
        />
      )}
    </div>
  );
}

/** 组页聚合图：多序列求和。60 秒实时求和；历史档只画 avg 合计线（合成假 band 是错的）。 */
function SumChart({
  exprs,
  metrics,
  title,
  tone,
  height,
  range,
  onLayer,
  unitFmt,
}: {
  exprs: readonly string[];
  metrics: readonly string[];
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
  const live = range === "60s";
  const hist = useHistory(exprs, range, !live);

  useEffect(() => {
    onLayer(live ? null : (hist.data?.layer ?? null));
  }, [live, hist.data?.layer, onLayer]);

  const liveRings: (Ring | undefined)[] = [];
  for (const [key, r] of rings) {
    if (metrics.some((m) => key.startsWith(`${m}|`))) liveRings.push(r);
  }
  const latest = metrics.reduce((acc, m) => acc + (latestSum(rings, m) ?? 0), 0);

  const data = live ? liveSum(liveRings) : sumAvg([...(hist.data?.bySeries.values() ?? [])]);

  return (
    <div className={s.chartBlock} key={mode}>
      <div className={s.chartTitle}>
        <b>{title}</b>
        <span>{unitFmt(latest)}</span>
      </div>
      <Plot
        data={data}
        series={[{ stroke: hue, width: 2, fill: live ? withAlpha(hue, 0.18) : undefined }]}
        height={height}
        tone={tone}
        cornerBL={rangeOf(range)?.label ?? ""}
        cornerBR="0"
      />
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

function useSystemInfo() {
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
  ] as const) {
    const v = latestOf(rings, seriesKey(k, ""));
    if (v !== null) numberItems.push({ k: label, v: fmtPct(v / 100) });
  }
  const procs = latestOf(rings, seriesKey("procs.total", ""));
  if (procs !== null) numberItems.push({ k: "进程", v: String(Math.round(procs)) });
  const load =
    latestOf(rings, seriesKey("load.1min", "")) ?? latestOf(rings, seriesKey("load1", ""));
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
            height={220}
            range={range}
            onLayer={onLayer}
            unitFmt={(v) => fmtPct(v / 100)}
          />
          {numberItems.length > 0 && <Numbers items={numberItems} />}
          {info.data && (
            <Numbers
              items={[
                { k: "型号", v: info.data.cpu.model },
                { k: "逻辑核", v: String(info.data.cpu.logical_cores) },
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

/** >32 核：单张 canvas 热力图（08 §6.6）。颜色 = 单色相顺序色阶。 */
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
    void mode; // 主题切换时资源色值变化，需要重画
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
      <canvas ref={canvasRef} title="逻辑处理器占用热力图" />
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
        height={220}
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

export function DiskSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const devs = discovery.data?.members("disk.util", "dev") ?? [];
  const hasRates = discovery.data?.has("disk.") ?? false;
  const mounts = discovery.data?.members("fs.used", "mount") ?? [];

  return (
    <>
      {hasRates ? (
        <SumChart
          exprs={devs.flatMap((d) => [`disk.read_bytes{dev=${d}}`, `disk.write_bytes{dev=${d}}`])}
          metrics={["disk.read_bytes", "disk.write_bytes"]}
          title="磁盘 · 合计吞吐（读 + 写 · 求和）"
          tone="--disk"
          height={220}
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => `${fmtBytes(v)}/s`}
        />
      ) : (
        <p className={s.note}>此平台未提供块设备级 I/O 速率，以下为文件系统容量。</p>
      )}

      {devs.length > 0 && (
        <>
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
        <>
          <h2 className={s.sectionTitle}>挂载点</h2>
          <Numbers
            items={mounts.map((m) => {
              const used = latestOf(rings, seriesKey("fs.used", `mount=${m}`));
              const total = latestOf(rings, seriesKey("fs.total", `mount=${m}`));
              return {
                k: m,
                v: used !== null && total ? fmtPct(used / total) : "—",
                sub: used !== null && total ? `${fmtBytes(used)} / ${fmtBytes(total)}` : undefined,
              };
            })}
          />
        </>
      )}
    </>
  );
}

/* ================= 网络 ================= */

export function NetSection({ range, onLayer }: SectionProps) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();
  const ifaces = discovery.data?.members("net.tx_bytes", "iface") ?? [];

  return (
    <>
      <SumChart
        exprs={ifaces.flatMap((i) => [`net.rx_bytes{iface=${i}}`, `net.tx_bytes{iface=${i}}`])}
        metrics={["net.rx_bytes", "net.tx_bytes"]}
        title="网络 · 合计吞吐（收 + 发 · 求和）"
        tone="--net"
        height={220}
        range={range}
        onLayer={onLayer}
        unitFmt={fmtRateBits}
      />
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
                small: errs ? `错误 ${Math.round(errs)}/s` : undefined,
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
  const gpus = discovery.data?.members("gpu.usage", "gpu") ?? [];

  return (
    <>
      <SeriesChart
        expr="gpu.usage"
        metric="gpu.usage"
        labels={gpus.length === 1 ? `gpu=${gpus[0]}` : ""}
        title="GPU · gpu.usage（组内取最大）"
        tone="--gpu"
        yMax={100}
        height={220}
        range={range}
        onLayer={onLayer}
        unitFmt={(v) => fmtPct(v / 100)}
      />
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
            const mt = latestOf(rings, seriesKey("gpu.mem_total", `gpu=${g}`));
            return {
              big: u !== null ? fmtPct(u / 100) : "—",
              small: mu !== null && mt !== null ? `${fmtBytes(mu)} / ${fmtBytes(mt)}` : undefined,
            };
          }}
        />
      )}
    </>
  );
}

/* ================= 成员详情 ================= */

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
  const labelKey = resource.memberLabel ?? "";
  const metrics =
    discovery.data?.all
      .filter((m) => labelValue(m.labels, labelKey) === member)
      .map((m) => m.metric)
      .filter((v, i, arr) => arr.indexOf(v) === i) ?? [];

  const fmtFor = (metric: string): ((v: number) => string) => {
    if (metric.endsWith("util") || metric.endsWith("usage")) return (v) => fmtPct(v / 100);
    if (metric.includes("bytes") && resource.id === "net") return fmtRateBits;
    if (metric.includes("bytes")) return (v) => `${fmtBytes(v)}/s`;
    if (metric.startsWith("fs.") || metric.includes("mem")) return fmtBytes;
    return (v) => v.toFixed(1);
  };

  if (metrics.length === 0) {
    return <p className={s.note}>「{member}」没有任何序列——它可能刚被拔掉，或已过保留期。</p>;
  }

  return (
    <>
      {metrics.map((metric) => (
        <SeriesChart
          key={metric}
          expr={`${metric}{${labelKey}=${member}}`}
          metric={metric}
          labels={`${labelKey}=${member}`}
          title={`${member} · ${metric}`}
          tone={resource.tone}
          yMax={metric.endsWith("util") || metric.endsWith("usage") ? 100 : undefined}
          height={160}
          range={range}
          onLayer={onLayer}
          unitFmt={fmtFor(metric)}
        />
      ))}
    </>
  );
}
