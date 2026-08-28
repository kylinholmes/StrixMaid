import { useId } from "react";

export interface SparklineProps {
  data: readonly number[];
  /** 数据层的 CSS 变量名，例如 "--cpu" */
  tone: string;
  /** y 轴上限。占用率类固定 0–100；吞吐类传 undefined 走自动缩放 */
  max?: number;
  width?: number;
  height?: number;
  label: string;
}

/**
 * 小曲线图。**只用于成员格与导航行里的缩略图**——正式图表走 uPlot。
 *
 * 末点画 6px 方块而不是圆点（spec §4：radius 0 全局无例外）。
 */
export function Sparkline({ data, tone, max, width = 76, height = 34, label }: SparklineProps) {
  const id = useId();
  if (data.length < 2) return <svg width={width} height={height} aria-label={label} role="img" />;

  const ceiling = max ?? (Math.max(...data) * 1.15 || 1);
  const pts = data.map((v, i) => {
    const x = (i / (data.length - 1)) * 100;
    const y = 100 - (Math.min(v, ceiling) / ceiling) * 100;
    return `${x.toFixed(2)},${y.toFixed(2)}`;
  });
  const last = pts[pts.length - 1]!.split(",").map(Number) as [number, number];

  return (
    <svg
      width={width}
      height={height}
      viewBox="0 0 100 100"
      preserveAspectRatio="none"
      role="img"
      aria-label={label}
    >
      <title id={id}>{label}</title>
      <polygon points={`0,100 ${pts.join(" ")} 100,100`} fill={`var(${tone})`} fillOpacity="0.2" />
      <polyline
        points={pts.join(" ")}
        fill="none"
        stroke={`var(${tone})`}
        strokeWidth="1.6"
        strokeLinejoin="miter"
        vectorEffect="non-scaling-stroke"
      />
      <rect
        x={last[0] - 3}
        y={last[1] - 3}
        width="6"
        height="6"
        fill={`var(${tone})`}
        stroke="var(--surface)"
        strokeWidth="1.5"
        vectorEffect="non-scaling-stroke"
      />
    </svg>
  );
}
