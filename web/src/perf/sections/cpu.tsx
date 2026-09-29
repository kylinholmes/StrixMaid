import { useEffect, useRef } from "react";
import { cssVar, Plot, withAlpha } from "@/components/Plot";
import { fmtPct, fmtUptime } from "@/lib/fmt";
import { useDiscovery } from "@/metrics/discovery";
import { latestOf, liveMembers, type Ring, seriesKey, useLive } from "@/metrics/live";
import { useTheme } from "@/theme/useTheme";
import { liveSeries, liveSingle } from "../chart";
import s from "../Perf.module.css";
import {
  CORE_CHROME_H,
  CORE_MIN_PLOT_H,
  CORE_MIN_W,
  GRID_GAP,
  gridLayout,
  useBoxWidth,
} from "./layout";
import {
  ChartFrame,
  type NumItem,
  type PerfView,
  PsiChart,
  type SectionProps,
  SeriesChart,
  StatFact,
  useSystemInfo,
} from "./shared";

/* ================= CPU ================= */

/**
 * CPU 段。视图切换只换图表区（总体大图 ⇄ 逻辑处理器网格/热力图）,
 * 数字与静态事实不随之消失;切换器由页头渲染（靠近时间档）,状态在 PerfPage。
 */
export function CpuSection({
  range,
  rangeSecs,
  view,
  onLayer,
}: SectionProps & { rangeSecs: number; view: PerfView }) {
  const rings = useLive((st) => st.rings);
  const discovery = useDiscovery();
  const info = useSystemInfo();

  // 与页头切换器上那个数出自同一处口径,两边不会差一个
  const cores = liveMembers(
    rings,
    "cpu.core.usage",
    "core",
    discovery.data?.members("cpu.core.usage", "core") ?? [],
  );
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
      {view === "all" || cores.length < 2 ? (
        <SeriesChart
          expr="cpu.usage"
          metric="cpu.usage"
          labels=""
          title="CPU · cpu.usage"
          tone="--cpu"
          yMax={100}
          grow
          range={range}
          onLayer={onLayer}
          unitFmt={(v) => fmtPct(v / 100)}
        />
      ) : (
        /* 与总体图同一个 grow 外框,切换前后区块高度不变,下方数字|静态事实不跳。
           不给标题行也不给图例:每一格自己写着「核 N」与读数,一眼看得出是逐核,
           再加两行字是白说。格子摊满容器,摊不下才转热力图 */
        <ChartFrame grow noFrame>
          {(h) => <CoresView cores={cores} ringOf={ringOf} height={h} rangeSecs={rangeSecs} />}
        </ChartFrame>
      )}

      {/* 副图行:主图之外的第二层信息。视图切换(总体⇄逐核)不影响这里 */}
      {discovery.data?.has("psi.cpu.some") && (
        <div className={s.subRow}>
          <PsiChart
            kind="cpu"
            title="CPU · psi.cpu（压力）"
            tone="--cpu"
            range={range}
            onLayer={onLayer}
          />
        </div>
      )}

      <StatFact stats={stats} facts={facts} />
    </>
  );
}

/**
 * 逐核视图（08 §6.6）：容器高度由外层弹性布局给定，**格子反过来适应容器**——
 * 列数由宽度与目标宽高比定，行高由容器高度整除，摊满整块面积。
 * 摊不下（核太多，行高低于下限）时转热力图；「超多核」不是写死的 32，
 * 是几何上装不装得下。
 *
 * 逐核与成员格长得像，但不是一回事，不合并：核是一颗 CPU 内部的构成，
 * 成员是若干同类设备，后者每一格都能点进去看自己的详情，前者不能。
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
  const w = useBoxWidth(hostRef);
  const mode = useTheme((t) => t.mode);
  void mode; // 主题切换时 cssVar 解析值变化，线色要跟着重算

  const ready = w > 0 && cores.length > 0;
  const grid = ready
    ? gridLayout(cores.length, w, height, CORE_MIN_W, CORE_CHROME_H + CORE_MIN_PLOT_H)
    : null;

  return (
    <div ref={hostRef} className={s.coreArea} style={{ height }}>
      {!ready ? null : grid ? (
        <div
          className={s.cores}
          style={{
            gridTemplateColumns: `repeat(${grid.cols}, minmax(0, 1fr))`,
            gridAutoRows: `${grid.cellH}px`,
            gap: GRID_GAP,
          }}
        >
          {cores.map((core) => {
            const ring = ringOf(core);
            const latest = ring?.v[ring.v.length - 1];
            return (
              <div key={core} className={s.core}>
                <div className={s.coreLabel}>
                  <span className={s.coreName}>核 {core}</span>
                  <span className={s.coreVal}>
                    {typeof latest === "number" ? fmtPct(latest / 100) : "—"}
                  </span>
                </div>
                <Plot
                  data={liveSingle(ring, Math.min(rangeSecs, 180))}
                  series={liveSeries(cssVar("--cpu"))}
                  yMax={100}
                  height={grid.cellH - CORE_CHROME_H}
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
