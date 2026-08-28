import type { CSSProperties } from "react";
import s from "./Meter.module.css";

export interface MeterProps {
  /** 0–1 */
  value: number;
  label: string;
  /** 取自数据层的 CSS 变量名，例如 "--disk"。不传则用 ink-2（中性） */
  tone?: string;
  showValue?: boolean;
  width?: number;
}

/** 使用率条。方头、无圆角、无渐变（spec §4）。 */
export function Meter({ value, label, tone, showValue = true, width }: MeterProps) {
  const pct = Math.min(1, Math.max(0, value));
  return (
    <span className={s.wrap}>
      {/* 条本身是纯视觉的；数值由旁边的文字承担，读屏读到「根分区使用率 66%」
          比读一个 role="meter" 更直接。showValue=false 时用视觉隐藏的文字补上。 */}
      <span
        className={s.track}
        style={width ? ({ "--meter-w": `${width}px` } as CSSProperties) : undefined}
        aria-hidden="true"
      >
        <span
          className={s.fill}
          style={{ width: `${pct * 100}%`, background: tone ? `var(${tone})` : "var(--ink-2)" }}
        />
      </span>
      <span className={showValue ? s.value : s.srOnly}>
        {showValue ? "" : `${label} `}
        {Math.round(pct * 100)}%
      </span>
    </span>
  );
}
