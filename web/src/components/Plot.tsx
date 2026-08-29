import { useEffect, useRef } from "react";
import uPlot, { type AlignedData } from "uplot";
import "uplot/dist/uPlot.min.css";
import { useTheme } from "@/theme/useTheme";
import s from "./Plot.module.css";

export interface PlotSeries {
  /** CSS 颜色。band 的 min/max 边界序列传 "transparent" + width 0 */
  stroke: string;
  width?: number;
  dash?: number[];
  /** 面积填充色（60 秒原始视图用） */
  fill?: string;
}

export interface PlotBand {
  /** [上界序列号, 下界序列号]，1 起 */
  series: [number, number];
  fill: string;
}

export interface PlotProps {
  /** [xs, ...ys]，与 series 一一对应 */
  data: AlignedData;
  series: readonly PlotSeries[];
  bands?: readonly PlotBand[];
  /** y 上限；不传则自动 = 数据最大值 × 1.15（至少 1） */
  yMax?: number;
  height: number;
  /** 资源色 CSS 变量名（"--cpu"），用于网格 / 末点 / 游标线 */
  tone: string;
  cornerTL?: string;
  cornerTR?: string;
  cornerBL?: string;
  cornerBR?: string;
  /** 关掉游标（sparkline / 成员格小图） */
  noCursor?: boolean;
  className?: string;
}

export function cssVar(name: string): string {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";
}

/** 资源色转带透明度的 rgba（网格用）。仅支持 #rrggbb。 */
export function withAlpha(hex: string, alpha: number): string {
  const m = /^#([0-9a-f]{6})$/i.exec(hex);
  if (!m?.[1]) return hex;
  const n = Number.parseInt(m[1], 16);
  return `rgba(${(n >> 16) & 255}, ${(n >> 8) & 255}, ${n & 255}, ${alpha})`;
}

/**
 * uPlot 包装（roadmap/08 §8.1）：无轴、无图例、固定像素网格、末点方块、四角 HTML 标注。
 * 数据更新走 setData 直接重绘，不做动画（spec §7）。
 */
export function Plot({
  data,
  series,
  bands,
  yMax,
  height,
  tone,
  cornerTL,
  cornerTR,
  cornerBL,
  cornerBR,
  noCursor,
  className,
}: PlotProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const plotRef = useRef<uPlot | null>(null);
  const dataRef = useRef(data);
  dataRef.current = data;
  const mode = useTheme((t) => t.mode);

  // 结构性参数变化（序列形状 / 主题 / 上限模式）→ 重建实例
  const shape = JSON.stringify({
    n: series.length,
    st: series.map((x) => [x.stroke, x.width, x.dash, x.fill]),
    bands,
    yMax,
    height,
    tone,
    mode,
    noCursor,
  });

  // biome-ignore lint/correctness/useExhaustiveDependencies: shape 串已覆盖全部结构参数，逐项列出会导致每次渲染都重建实例
  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;

    const hue = cssVar(tone);
    const surface = cssVar("--surface");
    host.style.setProperty("--plot-hue", hue);

    const opts: uPlot.Options = {
      width: host.clientWidth || 300,
      height,
      padding: [0, 0, 0, 0],
      legend: { show: false },
      cursor: noCursor
        ? { show: false }
        : { x: true, y: false, points: { show: false }, drag: { setScale: false } },
      scales: {
        x: { time: false },
        y: {
          range: (u): [number, number] => {
            if (yMax !== undefined) return [0, yMax];
            let max = 1e-9;
            for (let i = 1; i < u.data.length; i++) {
              const ys = u.data[i];
              if (!ys) continue;
              for (const v of ys) if (typeof v === "number" && v > max) max = v;
            }
            return [0, max * 1.15];
          },
        },
      },
      axes: [{ show: false }, { show: false }],
      series: [
        {},
        ...series.map((x) => ({
          stroke: x.stroke,
          width: x.width ?? 2,
          dash: x.dash ? [...x.dash] : undefined,
          fill: x.fill,
          points: { show: false },
        })),
      ],
      bands: bands?.map((b) => ({ series: [...b.series] as [number, number], fill: b.fill })),
      hooks: {
        drawClear: [
          (u) => {
            // 固定像素网格：步长不随刻度走（08 §8.1）
            const ctx = u.ctx;
            const dpr = devicePixelRatio;
            const { left, top, width, height: h } = u.bbox;
            const step = Math.max(18, width / dpr / 18) * dpr;
            ctx.save();
            ctx.strokeStyle = withAlpha(hue, 0.13);
            ctx.lineWidth = 1;
            ctx.beginPath();
            for (let x = left + step; x < left + width; x += step) {
              ctx.moveTo(Math.round(x) + 0.5, top);
              ctx.lineTo(Math.round(x) + 0.5, top + h);
            }
            for (let y = top + step; y < top + h; y += step) {
              ctx.moveTo(left, Math.round(y) + 0.5);
              ctx.lineTo(left + width, Math.round(y) + 0.5);
            }
            ctx.stroke();
            ctx.restore();
          },
        ],
        draw: [
          (u) => {
            // 末点 6px 方块 + surface 描边（直角语言，不用圆点）
            const ctx = u.ctx;
            const dpr = devicePixelRatio;
            const xs = u.data[0];
            if (!xs || xs.length === 0) return;
            for (let si = 1; si < u.data.length; si++) {
              const cfg = series[si - 1];
              if (!cfg || (cfg.width ?? 2) === 0) continue; // band 边界不画
              const ys = u.data[si];
              if (!ys) continue;
              let li = ys.length - 1;
              while (li >= 0 && typeof ys[li] !== "number") li--;
              if (li < 0) continue;
              const xv = xs[li];
              const yv = ys[li];
              if (typeof xv !== "number" || typeof yv !== "number") continue;
              const cx = u.valToPos(xv, "x", true);
              const cy = u.valToPos(yv, "y", true);
              const sz = 6 * dpr;
              ctx.save();
              ctx.fillStyle = cfg.stroke;
              ctx.strokeStyle = surface;
              ctx.lineWidth = 1.5 * dpr;
              ctx.fillRect(cx - sz / 2, cy - sz / 2, sz, sz);
              ctx.strokeRect(cx - sz / 2, cy - sz / 2, sz, sz);
              ctx.restore();
            }
          },
        ],
      },
    };

    const u = new uPlot(opts, dataRef.current, host);
    plotRef.current = u;

    const ro = new ResizeObserver(() => {
      if (host.clientWidth > 0) u.setSize({ width: host.clientWidth, height });
    });
    ro.observe(host);

    return () => {
      ro.disconnect();
      u.destroy();
      plotRef.current = null;
    };
  }, [shape]);

  // 数据更新：直接 setData 重绘
  useEffect(() => {
    plotRef.current?.setData(data);
  }, [data]);

  return (
    <div className={`${s.wrap} ${className ?? ""}`} ref={hostRef} style={{ height }}>
      {cornerTL && <span className={`${s.corner} ${s.tl}`}>{cornerTL}</span>}
      {cornerTR && <span className={`${s.corner} ${s.tr}`}>{cornerTR}</span>}
      {cornerBL && <span className={`${s.corner} ${s.bl}`}>{cornerBL}</span>}
      {cornerBR && <span className={`${s.corner} ${s.br}`}>{cornerBR}</span>}
    </div>
  );
}
