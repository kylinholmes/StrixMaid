import { cssVar } from "@/components/Plot";
import { fmtBytes, fmtPct } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { latestMax, latestOf, latestSum, liveMembers, seriesKey, useLive } from "@/metrics/live";
import { useTheme } from "@/theme/useTheme";
import s from "../Perf.module.css";
import { GPU_RESOURCE, MemberGrid } from "./members";
import {
  AggChart,
  ChartFrame,
  H_SUB,
  Legend,
  type NumItem,
  type PerfView,
  type SectionProps,
  SeriesChart,
  StatFact,
  useSystemInfo,
} from "./shared";

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

export function GpuSection({ range, view, onLayer }: SectionProps & { view: PerfView }) {
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
  const split = view === "each" && gpus.length > 0;

  return (
    <>
      {split ? (
        /* 与总体图同一个 grow 外框,切换前后区块高度不变。不给标题行也不给图例:
           每一格自己写着设备名与读数,一眼看得出是逐设备 */
        <ChartFrame grow noFrame>
          {(h) => (
            <MemberGrid
              resource={GPU_RESOURCE}
              members={gpus}
              height={h}
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
        </ChartFrame>
      ) : only !== undefined ? (
        <SeriesChart
          expr={`gpu.usage{gpu=${only}}`}
          metric="gpu.usage"
          labels={`gpu=${only}`}
          title="GPU · gpu.usage"
          tone="--gpu"
          yMax={100}
          grow
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
          grow
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />
      )}
      <GpuComposition gpus={gpus} />
      {/* 副图行:只剩温度。引擎细分归到单卡 Detail(整组取最大没法定位是哪张卡在忙),
          显存的量在下面的 GPU 内存块里,不用再占一张时序图 */}
      {discovery.data?.has("gpu.temp") && (
        <div className={s.subRow}>
          {only !== undefined ? (
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
          )}
        </div>
      )}
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

        const cards = info.data?.gpus ?? [];
        const card = cards.length === 1 ? cards[0] : undefined;
        // 只有一张卡时把它的静态事实逐条摊开;多卡时一张卡一行,挤在一起才对得上号
        const facts: NumItem[] = card
          ? [
              { k: "型号", v: card.model || "—" },
              ...(card.driver ? [{ k: "驱动", v: card.driver }] : []),
              ...(card.vram_bytes ? [{ k: "显存", v: fmtBytes(card.vram_bytes) }] : []),
              ...(card.bus ? [{ k: "总线", v: card.bus }] : []),
            ]
          : cards.map((g) => ({
              k: g.card || "GPU",
              v: g.model || "—",
              sub: [g.driver, g.vram_bytes ? fmtBytes(g.vram_bytes) : null, g.bus]
                .filter(Boolean)
                .join(" · "),
            }));
        return <StatFact stats={stats} facts={facts} />;
      })()}
    </>
  );
}
