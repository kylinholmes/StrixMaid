import { cssVar, withAlpha } from "@/components/Plot";
import { cx } from "@/lib/cx";
import { fmtBytes, fmtPct } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { latestOf, latestSum, seriesKey, useLive } from "@/metrics/live";
import { useTheme } from "@/theme/useTheme";
import s from "../Perf.module.css";

import {
  H_SUB,
  Legend,
  type NumItem,
  PsiChart,
  type SectionProps,
  SeriesChart,
  StatFact,
  useSystemInfo,
} from "./shared";

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
    ["mem.cached", "已缓存"],
    ["mem.swap_used", "交换已用"],
  ] as const) {
    const v = latestOf(rings, seriesKey(metric, ""));
    if (v !== null) stats.push({ k: label, v: fmtBytes(v) });
  }
  const swapTotalNow = latestOf(rings, seriesKey("mem.swap_total", ""));
  if (swapTotalNow !== null) {
    const swapUsedNow = latestOf(rings, seriesKey("mem.swap_used", ""));
    stats.push({
      k: "交换总量",
      v: fmtBytes(swapTotalNow),
      sub:
        swapUsedNow !== null && swapTotalNow > 0
          ? `（${fmtPct(swapUsedNow / swapTotalNow)}）`
          : undefined,
    });
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
        grow
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
