import { useState } from "react";
import { Navigate, useNavigate, useParams } from "react-router-dom";
import chrome from "@/app/PageChrome.module.css";
import { Segmented, Sparkline, TableSkeleton } from "@/components";
import { cx } from "@/lib/cx";
import { fmtBytes, fmtPct, fmtRateBits } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { RANGES, type RangeKey } from "@/metrics/history";
import { latestSum, type Ring, seriesKey, useLive } from "@/metrics/live";
import { type ResourceDef, visibleResources } from "./model";
import s from "./Perf.module.css";
import {
  CpuSection,
  DiskSection,
  GpuSection,
  MemberDetail,
  MemSection,
  NetSection,
  useSystemInfo,
} from "./sections";

/** 二级导航行的 sparkline 数据：按指标名把各标签序列求和后取窗口。 */
function railSpark(rings: ReadonlyMap<string, Ring>, metrics: readonly string[]): number[] {
  const byTs = new Map<number, number>();
  for (const metric of metrics) {
    for (const [key, r] of rings) {
      if (!key.startsWith(`${metric}|`)) continue;
      for (let i = 0; i < r.ts.length; i++) {
        const t = r.ts[i];
        const v = r.v[i];
        if (t !== undefined && v !== undefined) byTs.set(t, (byTs.get(t) ?? 0) + v);
      }
    }
  }
  return [...byTs.keys()].sort((a, b) => a - b).map((t) => byTs.get(t) ?? 0);
}

/** 某指标各标签成员里的最大值。 */
function latestMax(rings: ReadonlyMap<string, Ring>, metric: string): number | null {
  let max: number | null = null;
  for (const [key, r] of rings) {
    if (!key.startsWith(`${metric}|`)) continue;
    const v = r.v[r.v.length - 1];
    if (typeof v === "number" && (max === null || v > max)) max = v;
  }
  return max;
}

function railSummary(r: ResourceDef, rings: ReadonlyMap<string, Ring>): string {
  switch (r.id) {
    case "cpu": {
      const v = latestSum(rings, "cpu.usage");
      return v === null ? "—" : fmtPct(v / 100);
    }
    case "mem": {
      const used = latestSum(rings, "mem.used");
      const total = latestSum(rings, "mem.total");
      return used !== null && total
        ? `${fmtBytes(used)} / ${fmtBytes(total)}（${fmtPct(used / total)}）`
        : "—";
    }
    case "disk": {
      const rd = latestSum(rings, "disk.read_bytes");
      const wr = latestSum(rings, "disk.write_bytes");
      const busiest = latestMax(rings, "disk.util");
      if (rd !== null || wr !== null) {
        const parts: string[] = [];
        if (busiest !== null) parts.push(`最忙 ${fmtPct(busiest / 100)}`);
        parts.push(`合计 ${fmtBytes((rd ?? 0) + (wr ?? 0))}/s`);
        return parts.join(" · ");
      }
      if (rd === null && wr === null) {
        // 没有速率时报「最满」而不是求和——APFS 同容器的卷共享空间，求和会数多遍
        let worst: number | null = null;
        for (const [key, r] of rings) {
          if (!key.startsWith("fs.used|mount=")) continue;
          const total = rings.get(key.replace("fs.used", "fs.total"));
          const u = r.v[r.v.length - 1];
          const t = total?.v[total.v.length - 1];
          if (typeof u === "number" && typeof t === "number" && t > 0) {
            const pct = u / t;
            if (worst === null || pct > worst) worst = pct;
          }
        }
        return worst === null ? "—" : `最满 ${fmtPct(worst)}`;
      }
      return "—";
    }
    case "net": {
      const rx = latestSum(rings, "net.rx_bytes");
      const tx = latestSum(rings, "net.tx_bytes");
      if (rx === null && tx === null) return "—";
      const errs = latestSum(rings, "net.errors");
      const base = `↓ ${fmtRateBits(rx ?? 0)} · ↑ ${fmtRateBits(tx ?? 0)}`;
      return errs && errs >= 1 ? `${base} · 异常 ${Math.round(errs)}/s` : base;
    }
    case "gpu": {
      const v = latestMax(rings, "gpu.usage");
      const mem = latestSum(rings, "gpu.mem_used");
      if (v === null) return "—";
      return mem !== null
        ? `最忙 ${fmtPct(v / 100)} · 显存 ${fmtBytes(mem)}`
        : `最忙 ${fmtPct(v / 100)}`;
    }
  }
}

const RAIL_SPARK_METRICS: Record<string, readonly string[]> = {
  cpu: ["cpu.usage"],
  mem: ["mem.used"],
  disk: ["disk.read_bytes", "disk.write_bytes"],
  net: ["net.rx_bytes", "net.tx_bytes"],
  gpu: ["gpu.usage"],
};

const RAIL_SPARK_MAX: Record<string, number | undefined> = {
  cpu: 100,
  gpu: 100,
  mem: undefined,
  disk: undefined,
  net: undefined,
};

/** 页头副标题：这台机器上该资源的一句话概述。 */
function subtitleOf(
  id: string,
  info: ReturnType<typeof useSystemInfo>["data"],
  d: ReturnType<typeof useDiscovery>["data"],
): string {
  switch (id) {
    case "cpu":
      return info ? `${info.cpu.model} · ${info.cpu.logical_cores} 逻辑处理器` : "";
    case "mem":
      return info ? fmtBytes(info.memory.total_bytes) : "";
    case "gpu":
      return (info?.gpus ?? [])
        .map((g) => g.model)
        .filter(Boolean)
        .join(" · ");
    case "disk": {
      const n = d?.members("disk.util", "dev").length ?? 0;
      return n > 0 ? `${n} 个块设备` : "";
    }
    case "net": {
      const n = d?.members("net.tx_bytes", "iface").length ?? 0;
      return n > 0 ? `${n} 个接口` : "";
    }
    default:
      return "";
  }
}

function psiTone(v: number): string {
  if (v >= 40) return "var(--crit)";
  if (v >= 12) return "var(--warn)";
  return "var(--ok)";
}

export function PerfPage() {
  const { res, member } = useParams<{ res?: string; member?: string }>();
  const navigate = useNavigate();
  const discovery = useDiscovery();
  const rings = useLive((st) => st.rings);
  const [range, setRange] = useState<RangeKey>("60s");
  const [layer, setLayer] = useState<string | null>(null);

  const visible = visibleResources(discovery.data);
  const info = useSystemInfo();

  if (discovery.isPending) {
    return (
      <>
        <header className={chrome.head}>
          <h1>性能</h1>
        </header>
        <div className={chrome.body}>
          <TableSkeleton rows={4} />
        </div>
      </>
    );
  }

  const first = visible[0];
  if (!first) {
    return (
      <>
        <header className={chrome.head}>
          <h1>性能</h1>
        </header>
        <div className={chrome.body}>
          <p className={s.note}>没有探测到任何指标序列。采集器可能尚未完成第一轮采集。</p>
        </div>
      </>
    );
  }
  const current = visible.find((r) => r.id === res);
  if (!current) return <Navigate to={`/performance/${first.id}`} replace />;

  const rangeDef = RANGES.find((r) => r.key === range);

  return (
    <>
      <header className={chrome.head}>
        <span className={s.crumb}>
          <b>性能</b>
          {" / "}
          {current.label}
          {member ? ` / ${member}` : ""}
          <span className={s.crumbSub}>{subtitleOf(current.id, info.data, discovery.data)}</span>
        </span>
        <span className={chrome.spacer} />
        <span className={s.layerNote} title="区间带 = min–max · 实线 = avg · 虚线 = med">
          {range === "60s"
            ? "live · 2s 采集"
            : layer
              ? `层 ${layer} · 带 min–max · 实线 avg · 虚线 med`
              : "自动选层中…"}
        </span>
        <Segmented
          label="时间范围"
          value={range}
          onChange={(v) => setRange(v as RangeKey)}
          options={RANGES.map((r) => ({ value: r.key, label: r.label }))}
        />
      </header>

      <div className={s.page}>
        <nav className={s.rrail} aria-label="资源">
          {visible.map((r) => {
            const psiRing = r.psi ? rings.get(seriesKey(r.psi, "")) : undefined;
            const psiVal = psiRing?.v[psiRing.v.length - 1];
            const memberCount =
              r.memberLabel && r.memberMetric && discovery.data
                ? discovery.data.members(r.memberMetric, r.memberLabel).length
                : 0;
            return (
              <button
                key={r.id}
                type="button"
                className={cx(s.rrow, r.id === current.id && s.rrowOn)}
                style={{ "--rr-tone": `var(${r.tone})` } as React.CSSProperties}
                onClick={() => navigate(`/performance/${r.id}`)}
              >
                <Sparkline
                  data={railSpark(rings, RAIL_SPARK_METRICS[r.id] ?? [])}
                  tone={r.tone}
                  max={RAIL_SPARK_MAX[r.id]}
                  width={72}
                  height={34}
                  label={`${r.label} 走势`}
                />
                <span className={s.rrText}>
                  <span className={s.rrName}>
                    {r.label}
                    {memberCount > 1 && <span className={s.rrCount}>{memberCount}</span>}
                  </span>
                  <span className={s.rrSummary}>{railSummary(r, rings)}</span>
                </span>
                {r.psi && typeof psiVal === "number" && (
                  <span className={s.psi} aria-hidden="true">
                    <i
                      style={{
                        width: `${Math.min(100, psiVal)}%`,
                        background: psiTone(psiVal),
                      }}
                    />
                  </span>
                )}
              </button>
            );
          })}
        </nav>

        <div className={s.main}>
          <div className={s.body}>
            {member && current.memberLabel ? (
              <MemberDetail resource={current} member={member} range={range} onLayer={setLayer} />
            ) : (
              <>
                {current.id === "cpu" && (
                  <CpuSection range={range} rangeSecs={rangeDef?.secs ?? 60} onLayer={setLayer} />
                )}
                {current.id === "mem" && <MemSection range={range} onLayer={setLayer} />}
                {current.id === "disk" && <DiskSection range={range} onLayer={setLayer} />}
                {current.id === "net" && <NetSection range={range} onLayer={setLayer} />}
                {current.id === "gpu" && <GpuSection range={range} onLayer={setLayer} />}
              </>
            )}
          </div>
        </div>
      </div>
    </>
  );
}
