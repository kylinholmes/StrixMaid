import { useState } from "react";
import { Navigate, useNavigate, useParams } from "react-router-dom";
import chrome from "@/app/PageChrome.module.css";
import { Segmented, Sparkline, TableSkeleton } from "@/components";
import { cx } from "@/lib/cx";
import { fmtBytes, fmtPct, fmtRateBits } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { RANGES, type RangeKey } from "@/metrics/history";
import { latestMax, latestSum, liveMembers, type Ring, seriesKey, useLive } from "@/metrics/live";
import { type ResourceDef, visibleResources } from "./model";
import s from "./Perf.module.css";
import {
  CpuSection,
  DiskSection,
  GpuSection,
  MemberDetail,
  MemSection,
  NetSection,
  type PerfView,
  useSystemInfo,
} from "./sections";

/**
 * 「总体 ⇄ 逐设备」切换器：成员从哪条指标的哪个标签里来、「逐设备」那一档的字样，
 * 以及少于几个成员就不给切换（`min`）。资源不在这张表里 = 不给切换。
 *
 * 三类设备的 `min` 是 1：那一档放的是成员格，一格一设备，**点进去是该设备的详情页**，
 * 只有一块盘时它也是进详情的唯一入口，不能因为「只有一个」就把路堵死。
 * CPU 的 `min` 是 2：逐核网格点不进去，只有一个核时两档画的是同一条线。
 */
const VIEW_SPLIT: Record<string, { metric: string; labelKey: string; each: string; min: number }> =
  {
    cpu: { metric: "cpu.core.usage", labelKey: "core", each: "逻辑处理器", min: 2 },
    disk: { metric: "disk.util", labelKey: "dev", each: "块设备", min: 1 },
    net: { metric: "net.tx_bytes", labelKey: "iface", each: "接口", min: 1 },
    gpu: { metric: "gpu.usage", labelKey: "gpu", each: "显卡", min: 1 },
  };

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

function railSummary(r: ResourceDef, rings: ReadonlyMap<string, Ring>): string {
  switch (r.id) {
    case "cpu": {
      const v = latestSum(rings, "cpu.usage");
      return v === null ? "—" : fmtPct(v / 100);
    }
    case "mem": {
      const used = latestSum(rings, "mem.used");
      const total = latestSum(rings, "mem.total");
      return used !== null && total ? `${fmtBytes(used)} / ${fmtBytes(total)}` : "—";
    }
    case "disk": {
      const rd = latestSum(rings, "disk.read_bytes");
      const wr = latestSum(rings, "disk.write_bytes");
      const busiest = latestMax(rings, "disk.util");
      if (rd !== null || wr !== null) {
        const parts: string[] = [];
        if (busiest !== null) parts.push(`最忙 ${fmtPct(busiest / 100)}`);
        parts.push(`${fmtBytes((rd ?? 0) + (wr ?? 0))}/s`);
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
      const base = `↓${fmtRateBits(rx ?? 0)} ↑${fmtRateBits(tx ?? 0)}`;
      return errs && errs >= 1 ? `${base} · 异常 ${Math.round(errs)}/s` : base;
    }
    case "gpu": {
      const v = latestMax(rings, "gpu.usage");
      const mem = latestSum(rings, "gpu.mem_used");
      if (v === null) return "—";
      return mem !== null
        ? `最忙 ${fmtPct(v / 100)} · ${fmtBytes(mem)}`
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

/**
 * 走势图的纵轴上限。百分比类固定 0..100；内存固定 0..总量，否则几 GiB 的波动会被拉满
 * 整个格子，看着像要爆了；吞吐类没有天然上限，只能自适应。
 */
function railSparkMax(id: string, rings: ReadonlyMap<string, Ring>): number | undefined {
  switch (id) {
    case "cpu":
    case "gpu":
      return 100;
    case "mem":
      return latestSum(rings, "mem.total") ?? undefined;
    default:
      return undefined;
  }
}

/** 页头副标题：这台机器上该资源的一句话概述。 */
function subtitleOf(
  id: string,
  info: ReturnType<typeof useSystemInfo>["data"],
  d: ReturnType<typeof useDiscovery>["data"],
  rings: ReadonlyMap<string, Ring>,
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
      const n = d
        ? liveMembers(rings, "disk.util", "dev", d.members("disk.util", "dev")).length
        : 0;
      return n > 0 ? `${n} 个块设备` : "";
    }
    case "net": {
      const n = d
        ? liveMembers(rings, "net.tx_bytes", "iface", d.members("net.tx_bytes", "iface")).length
        : 0;
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
  // 视图选择按资源各记各的：从 CPU 的逐核切回总体，再去磁盘页，不该带着别人的选择
  const [views, setViews] = useState<Record<string, PerfView>>({});

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
          <p className={s.note}>暂无序列。</p>
        </div>
      </>
    );
  }
  const current = visible.find((r) => r.id === res);
  if (!current) return <Navigate to={`/performance/${first.id}`} replace />;

  const rangeDef = RANGES.find((r) => r.key === range);

  const splitDef = member ? undefined : VIEW_SPLIT[current.id];
  const splitMembers =
    splitDef && discovery.data
      ? liveMembers(
          rings,
          splitDef.metric,
          splitDef.labelKey,
          discovery.data.members(splitDef.metric, splitDef.labelKey),
        )
      : [];
  const canSplit = splitDef !== undefined && splitMembers.length >= splitDef.min;
  // 够不着门槛时强制回总体,切换器也不出现
  const view: PerfView = canSplit ? (views[current.id] ?? "all") : "all";

  return (
    <>
      <header className={chrome.head}>
        <span className={s.crumb}>
          <b>性能</b>
          {" / "}
          {current.label}
          {member ? ` / ${member}` : ""}
          <span className={s.crumbSub}>
            {subtitleOf(current.id, info.data, discovery.data, rings)}
          </span>
        </span>
        <span className={chrome.spacer} />
        {/* 总体⇄逐设备只切图表区,切换器贴着时间档(08 §6.6) */}
        {splitDef && canSplit && (
          <Segmented
            label="视图"
            value={view}
            onChange={(v) => setViews((prev) => ({ ...prev, [current.id]: v }))}
            options={[
              { value: "all", label: "总体" },
              { value: "each", label: `${splitDef.each} ${splitMembers.length}` },
            ]}
          />
        )}
        <span className={s.layerNote} title="区间带 = min–max · 实线 = avg · 虚线 = med">
          {range === "60s" ? "live · 2s" : (layer ?? "…")}
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
                ? liveMembers(
                    rings,
                    r.memberMetric,
                    r.memberLabel,
                    discovery.data.members(r.memberMetric, r.memberLabel),
                  ).length
                : 0;
            return (
              <button
                key={r.id}
                type="button"
                className={cx(s.rrow, r.id === current.id && s.rrowOn)}
                style={{ "--tone": `var(${r.tone})` } as React.CSSProperties}
                onClick={() => navigate(`/performance/${r.id}`)}
              >
                {/* 尺寸由 .rrSpark 给（窄视口要换一档），组件属性只留作没有数据时的兜底 */}
                <span className={s.rrSpark}>
                  <Sparkline
                    data={railSpark(rings, RAIL_SPARK_METRICS[r.id] ?? [])}
                    tone={r.tone}
                    max={railSparkMax(r.id, rings)}
                    width={100}
                    height={48}
                    label={`${r.label} 走势`}
                  />
                </span>
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

        <div className={s.main} style={{ "--tone": `var(${current.tone})` } as React.CSSProperties}>
          <div className={s.body}>
            {/* 区块竖向堆叠:首图是唯一会长的弹性项,吃掉一屏里其余区块用剩的高度 */}
            <div className={s.stack}>
              {member && current.memberLabel ? (
                <MemberDetail resource={current} member={member} range={range} onLayer={setLayer} />
              ) : (
                <>
                  {current.id === "cpu" && (
                    <CpuSection
                      range={range}
                      rangeSecs={rangeDef?.secs ?? 60}
                      view={view}
                      onLayer={setLayer}
                    />
                  )}
                  {current.id === "mem" && <MemSection range={range} onLayer={setLayer} />}
                  {current.id === "disk" && (
                    <DiskSection range={range} view={view} onLayer={setLayer} />
                  )}
                  {current.id === "net" && (
                    <NetSection range={range} view={view} onLayer={setLayer} />
                  )}
                  {current.id === "gpu" && (
                    <GpuSection range={range} view={view} onLayer={setLayer} />
                  )}
                </>
              )}
            </div>
          </div>
        </div>
      </div>
    </>
  );
}
