import type { ReactNode } from "react";
import s from "./KeyValue.module.css";

export interface KeyValueItem {
  k: string;
  v: ReactNode;
  /** 机器给的字符串（主机名、路径、版本、指标 id）一律等宽（spec §3.1） */
  mono?: boolean;
}

export function KeyValueGrid({ items }: { items: readonly KeyValueItem[] }) {
  return (
    <dl className={s.grid}>
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
