import type { ReactNode } from "react";
import { cx } from "@/lib/cx";
import s from "./KeyValue.module.css";

export interface KeyValueItem {
  k: string;
  v: ReactNode;
  /** 机器给的字符串（主机名、路径、版本、指标 id）一律等宽（spec §3.1） */
  mono?: boolean;
}

export interface KeyValueGridProps {
  items: readonly KeyValueItem[];
  /**
   * 列数。默认 `"auto"` 按可用宽度自动铺列,概览这类整页宽的场合要的就是它。
   *
   * 侧边抽屉只有 360–440px,auto-fit 会在里面挤出两三列:一段描述被压在半宽里
   * 折成十几行,注册表路径折成三行。那里传 `1` 强制单列——列数是调用方的版面
   * 决策,不是这个组件的,所以做成属性而不是改默认值。
   */
  columns?: "auto" | 1;
}

export function KeyValueGrid({ items, columns = "auto" }: KeyValueGridProps) {
  return (
    <dl className={cx(s.grid, columns === 1 && s.one)}>
      {items.map((it) => (
        <div key={it.k} className={s.item}>
          <dt className={s.k}>{it.k}</dt>
          <dd className={`${s.v} ${it.mono ? s.mono : ""}`} style={{ margin: 0 }}>
            {it.v}
          </dd>
        </div>
      ))}
    </dl>
  );
}
